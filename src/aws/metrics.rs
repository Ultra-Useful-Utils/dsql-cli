use crate::{
    app::{
        BoxFuture, ClusterTarget, MetricSeries, MetricsFetchStatus, MetricsProvider, MetricsRange,
        MetricsSnapshot, ResolvedAwsContext,
    },
    aws::config::AwsConfiguration,
    error::ApplicationError,
};
use aws_sdk_cloudwatch::{
    error::ProvideErrorMetadata,
    primitives::DateTime,
    types::{
        Dimension, Metric, MetricDataQuery, MetricStat, ScanBy, StandardUnit,
        StatusCode as SdkStatusCode,
    },
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DSQL_NAMESPACE: &str = "AWS/AuroraDSQL";
const USAGE_NAMESPACE: &str = "AWS/Usage";
const MAX_METRIC_PAGES: usize = 1_000;
const MAX_CACHED_SNAPSHOTS: usize = 64;

type ApiFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Statistic {
    Average,
    Sum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unit {
    Bytes,
    Milliseconds,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetricQuery {
    id: String,
    namespace: String,
    metric_name: String,
    statistic: Statistic,
    unit: Option<Unit>,
    period: i32,
    dimensions: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
struct MetricDataRequest {
    start_time: SystemTime,
    end_time: SystemTime,
    queries: Vec<MetricQuery>,
    next_token: Option<String>,
    scan_ascending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetricResultStatus {
    Complete,
    PartialData,
    InternalError,
    Forbidden,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MetricPoint {
    timestamp: SystemTime,
    value: f64,
}

impl MetricPoint {
    fn new(timestamp: SystemTime, value: f64) -> Self {
        Self { timestamp, value }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MetricResult {
    id: String,
    points: Vec<MetricPoint>,
    status: MetricResultStatus,
}

impl MetricResult {
    fn new(id: impl Into<String>, points: Vec<MetricPoint>, status: MetricResultStatus) -> Self {
        Self {
            id: id.into(),
            points,
            status,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MetricPage {
    results: Vec<MetricResult>,
    next_token: Option<String>,
    incomplete: bool,
}

impl MetricPage {
    #[cfg(test)]
    fn new(results: Vec<MetricResult>, next_token: Option<String>) -> Self {
        Self {
            results,
            next_token,
            incomplete: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloudWatchApiError {
    AccessDenied,
    Throttled,
    Other,
}

trait CloudWatchApi: Send + Sync {
    fn get_metric_data(
        &self,
        request: MetricDataRequest,
    ) -> ApiFuture<'_, Result<MetricPage, CloudWatchApiError>>;
}

struct AwsCloudWatchApi {
    client: aws_sdk_cloudwatch::Client,
}

impl AwsCloudWatchApi {
    fn new(config: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_cloudwatch::Client::new(config),
        }
    }
}

impl CloudWatchApi for AwsCloudWatchApi {
    fn get_metric_data(
        &self,
        request: MetricDataRequest,
    ) -> ApiFuture<'_, Result<MetricPage, CloudWatchApiError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let queries = request.queries.into_iter().map(sdk_metric_query).collect();
            let response = client
                .get_metric_data()
                .set_metric_data_queries(Some(queries))
                .start_time(sdk_datetime(request.start_time)?)
                .end_time(sdk_datetime(request.end_time)?)
                .set_next_token(request.next_token)
                .scan_by(if request.scan_ascending {
                    ScanBy::TimestampAscending
                } else {
                    ScanBy::TimestampDescending
                })
                .send()
                .await
                .map_err(map_sdk_error)?;

            Ok(map_sdk_output(&response))
        })
    }
}

struct CloudWatchMetricsProvider<A> {
    api: A,
    cache: Mutex<MetricsCache>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MetricsCacheKey {
    region: String,
    cluster_id: String,
    range: u8,
}

#[derive(Default)]
struct MetricsCache {
    snapshots: HashMap<MetricsCacheKey, MetricsSnapshot>,
    insertion_order: VecDeque<MetricsCacheKey>,
}

impl MetricsCache {
    fn insert(&mut self, key: MetricsCacheKey, snapshot: MetricsSnapshot) {
        if !self.snapshots.contains_key(&key) {
            if self.snapshots.len() == MAX_CACHED_SNAPSHOTS
                && let Some(oldest) = self.insertion_order.pop_front()
            {
                self.snapshots.remove(&oldest);
            }
            self.insertion_order.push_back(key.clone());
        }
        self.snapshots.insert(key, snapshot);
    }

    fn get(&self, key: &MetricsCacheKey) -> Option<&MetricsSnapshot> {
        self.snapshots.get(key)
    }
}

impl<A> CloudWatchMetricsProvider<A> {
    fn new(api: A) -> Self {
        Self {
            api,
            cache: Mutex::new(MetricsCache::default()),
        }
    }
}

pub(crate) fn cloudwatch_metrics_provider(
    configuration: &AwsConfiguration,
) -> impl MetricsProvider + use<> {
    CloudWatchMetricsProvider::new(AwsCloudWatchApi::new(configuration.sdk_config()))
}

impl<A: CloudWatchApi> CloudWatchMetricsProvider<A> {
    async fn snapshot_at(
        &self,
        region: &str,
        cluster_id: &str,
        range: MetricsRange,
        now: SystemTime,
    ) -> Result<MetricsSnapshot, ApplicationError> {
        let key = MetricsCacheKey {
            region: region.into(),
            cluster_id: cluster_id.into(),
            range: range_key(range),
        };
        match fetch_metrics_raw(&self.api, cluster_id, range, now).await {
            Ok(snapshot) => {
                self.cache
                    .lock()
                    .map_err(|_| {
                        ApplicationError::runtime("CloudWatch metrics cache is unavailable")
                    })?
                    .insert(key, snapshot.clone());
                Ok(snapshot)
            }
            Err(MetricsFetchError::Throttled) => {
                let mut snapshot = self
                    .cache
                    .lock()
                    .map_err(|_| {
                        ApplicationError::runtime("CloudWatch metrics cache is unavailable")
                    })?
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| MetricsFetchError::Throttled.application_error())?;
                snapshot.status = MetricsFetchStatus::Stale;
                Ok(snapshot)
            }
            Err(error) => Err(error.application_error()),
        }
    }
}

impl<A: CloudWatchApi> MetricsProvider for CloudWatchMetricsProvider<A> {
    fn snapshot<'a>(
        &'a self,
        context: &'a ResolvedAwsContext,
        target: &'a ClusterTarget,
        range: MetricsRange,
    ) -> BoxFuture<'a, Result<MetricsSnapshot, ApplicationError>> {
        Box::pin(async move {
            self.snapshot_at(
                context.region(),
                target.id().as_str(),
                range,
                SystemTime::now(),
            )
            .await
        })
    }
}

impl MetricQuery {
    fn new(
        id: &str,
        namespace: &str,
        metric_name: &str,
        statistic: Statistic,
        unit: Option<Unit>,
        period: i32,
        dimensions: Vec<(String, String)>,
    ) -> Self {
        Self {
            id: id.into(),
            namespace: namespace.into(),
            metric_name: metric_name.into(),
            statistic,
            unit,
            period,
            dimensions,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RangeSpec {
    duration: Duration,
    period: Duration,
}

impl RangeSpec {
    fn for_range(range: MetricsRange) -> Self {
        match range {
            MetricsRange::FifteenMinutes => Self::new(15 * 60, 60),
            MetricsRange::OneHour => Self::new(60 * 60, 60),
            MetricsRange::SixHours => Self::new(6 * 60 * 60, 5 * 60),
            MetricsRange::TwentyFourHours => Self::new(24 * 60 * 60, 15 * 60),
        }
    }

    const fn new(duration_seconds: u64, period_seconds: u64) -> Self {
        Self {
            duration: Duration::from_secs(duration_seconds),
            period: Duration::from_secs(period_seconds),
        }
    }

    fn sample_count(self) -> usize {
        (self.duration.as_secs() / self.period.as_secs()) as usize
    }
}

fn metric_queries(cluster_id: &str, period: i32) -> Vec<MetricQuery> {
    let cluster_dimensions = || vec![("ClusterId".into(), cluster_id.into())];
    let dsql = [
        (
            "total_transactions",
            "TotalTransactions",
            Statistic::Sum,
            Some(Unit::None),
        ),
        (
            "read_only_transactions",
            "ReadOnlyTransactions",
            Statistic::Sum,
            Some(Unit::None),
        ),
        (
            "commit_latency",
            "CommitLatency",
            Statistic::Average,
            Some(Unit::Milliseconds),
        ),
        (
            "occ_conflicts",
            "OccConflicts",
            Statistic::Sum,
            Some(Unit::None),
        ),
        (
            "query_timeouts",
            "QueryTimeouts",
            Statistic::Sum,
            Some(Unit::None),
        ),
        // DPU is a custom unit, so CloudWatch's StandardUnit filter must be omitted.
        ("total_dpu", "TotalDPU", Statistic::Sum, None),
        ("read_dpu", "ReadDPU", Statistic::Sum, None),
        ("write_dpu", "WriteDPU", Statistic::Sum, None),
        ("compute_dpu", "ComputeDPU", Statistic::Sum, None),
        (
            "multi_region_write_dpu",
            "MultiRegionWriteDPU",
            Statistic::Sum,
            None,
        ),
        ("bytes_read", "BytesRead", Statistic::Sum, Some(Unit::Bytes)),
        (
            "bytes_written",
            "BytesWritten",
            Statistic::Sum,
            Some(Unit::Bytes),
        ),
        (
            "compute_time",
            "ComputeTime",
            Statistic::Sum,
            Some(Unit::Milliseconds),
        ),
        (
            "cluster_storage_size",
            "ClusterStorageSize",
            Statistic::Average,
            Some(Unit::Bytes),
        ),
    ];
    let mut queries = dsql
        .into_iter()
        .map(|(id, name, statistic, unit)| {
            MetricQuery::new(
                id,
                DSQL_NAMESPACE,
                name,
                statistic,
                unit,
                period,
                cluster_dimensions(),
            )
        })
        .collect::<Vec<_>>();
    queries.extend([
        MetricQuery::new(
            "active_connections",
            USAGE_NAMESPACE,
            "ResourceCount",
            Statistic::Average,
            None,
            period,
            usage_dimensions(cluster_id, "Resource", "ClusterConnectionCount"),
        ),
        MetricQuery::new(
            "admin_connection_attempts",
            USAGE_NAMESPACE,
            "CallCount",
            Statistic::Sum,
            None,
            period,
            usage_dimensions(cluster_id, "API", "DbConnectAdmin"),
        ),
        MetricQuery::new(
            "custom_role_connection_attempts",
            USAGE_NAMESPACE,
            "CallCount",
            Statistic::Sum,
            None,
            period,
            usage_dimensions(cluster_id, "API", "DbConnect"),
        ),
    ]);
    queries
}

fn usage_dimensions(cluster_id: &str, usage_type: &str, resource: &str) -> Vec<(String, String)> {
    vec![
        ("Service".into(), "AuroraDSQL".into()),
        ("Type".into(), usage_type.into()),
        ("Resource".into(), resource.into()),
        ("ResourceId".into(), format!("cluster/{cluster_id}")),
        ("Class".into(), "None".into()),
    ]
}

fn sdk_metric_query(query: MetricQuery) -> MetricDataQuery {
    let metric = Metric::builder()
        .namespace(query.namespace)
        .metric_name(query.metric_name)
        .set_dimensions(Some(
            query
                .dimensions
                .into_iter()
                .map(|(name, value)| Dimension::builder().name(name).value(value).build())
                .collect(),
        ))
        .build();
    let metric_stat = MetricStat::builder()
        .metric(metric)
        .period(query.period)
        .stat(match query.statistic {
            Statistic::Average => "Average",
            Statistic::Sum => "Sum",
        })
        .set_unit(query.unit.map(|unit| match unit {
            Unit::Bytes => StandardUnit::Bytes,
            Unit::Milliseconds => StandardUnit::Milliseconds,
            Unit::None => StandardUnit::None,
        }))
        .build();
    MetricDataQuery::builder()
        .id(query.id)
        .metric_stat(metric_stat)
        .return_data(true)
        .build()
}

fn sdk_datetime(timestamp: SystemTime) -> Result<DateTime, CloudWatchApiError> {
    let seconds = timestamp
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CloudWatchApiError::Other)?
        .as_secs();
    let seconds = i64::try_from(seconds).map_err(|_| CloudWatchApiError::Other)?;
    Ok(DateTime::from_secs(seconds))
}

fn system_time(timestamp: &DateTime) -> Option<SystemTime> {
    let seconds = u64::try_from(timestamp.secs()).ok()?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))?
        .checked_add(Duration::from_nanos(u64::from(timestamp.subsec_nanos())))
}

fn map_sdk_result(result: &aws_sdk_cloudwatch::types::MetricDataResult) -> MetricResult {
    let mut status = match result.status_code() {
        Some(SdkStatusCode::Complete) => MetricResultStatus::Complete,
        Some(SdkStatusCode::PartialData) => MetricResultStatus::PartialData,
        Some(SdkStatusCode::InternalError) => MetricResultStatus::InternalError,
        Some(SdkStatusCode::Forbidden) => MetricResultStatus::Forbidden,
        Some(_) | None => MetricResultStatus::Unknown,
    };
    if !result.messages().is_empty() || result.timestamps().len() != result.values().len() {
        status = match status {
            MetricResultStatus::Forbidden => MetricResultStatus::Forbidden,
            _ => MetricResultStatus::Unknown,
        };
    }
    let points = result
        .timestamps()
        .iter()
        .zip(result.values())
        .filter_map(|(timestamp, value)| {
            system_time(timestamp).map(|timestamp| MetricPoint::new(timestamp, *value))
        })
        .collect();
    MetricResult::new(result.id().unwrap_or_default(), points, status)
}

fn map_sdk_output(
    response: &aws_sdk_cloudwatch::operation::get_metric_data::GetMetricDataOutput,
) -> MetricPage {
    MetricPage {
        results: response
            .metric_data_results()
            .iter()
            .map(map_sdk_result)
            .collect(),
        next_token: response.next_token().map(str::to_owned),
        incomplete: !response.messages().is_empty(),
    }
}

fn map_sdk_error(
    error: aws_sdk_cloudwatch::error::SdkError<
        aws_sdk_cloudwatch::operation::get_metric_data::GetMetricDataError,
    >,
) -> CloudWatchApiError {
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error
        .as_service_error()
        .and_then(|error| error.code())
        .unwrap_or_default();
    classify_sdk_error(status, code)
}

fn classify_sdk_error(status: Option<u16>, code: &str) -> CloudWatchApiError {
    match (status, code) {
        (Some(403), _) => CloudWatchApiError::AccessDenied,
        (Some(429), _) => CloudWatchApiError::Throttled,
        (_, "AccessDenied" | "AccessDeniedException" | "UnauthorizedOperation") => {
            CloudWatchApiError::AccessDenied
        }
        (
            _,
            "Throttling"
            | "ThrottlingException"
            | "RequestLimitExceeded"
            | "LimitExceededException",
        ) => CloudWatchApiError::Throttled,
        _ => CloudWatchApiError::Other,
    }
}

fn range_key(range: MetricsRange) -> u8 {
    match range {
        MetricsRange::FifteenMinutes => 0,
        MetricsRange::OneHour => 1,
        MetricsRange::SixHours => 2,
        MetricsRange::TwentyFourHours => 3,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetricsFetchError {
    AccessDenied,
    Throttled,
    InvalidTime,
    PaginationDidNotAdvance,
    TooManyPages,
    Other,
}

impl MetricsFetchError {
    fn application_error(self) -> ApplicationError {
        match self {
            Self::AccessDenied => metrics_access_denied(),
            Self::Throttled => ApplicationError::runtime(
                "CloudWatch metrics request remained throttled after AWS SDK retries",
            ),
            Self::InvalidTime => ApplicationError::runtime("CloudWatch metrics time is invalid"),
            Self::PaginationDidNotAdvance => {
                ApplicationError::runtime("CloudWatch metrics pagination did not advance")
            }
            Self::TooManyPages => {
                ApplicationError::runtime("CloudWatch metrics returned too many pages")
            }
            Self::Other => ApplicationError::runtime("could not retrieve CloudWatch metrics"),
        }
    }
}

#[cfg(test)]
async fn fetch_metrics(
    api: &dyn CloudWatchApi,
    cluster_id: &str,
    range: MetricsRange,
    now: SystemTime,
) -> Result<MetricsSnapshot, ApplicationError> {
    fetch_metrics_raw(api, cluster_id, range, now)
        .await
        .map_err(MetricsFetchError::application_error)
}

async fn fetch_metrics_raw(
    api: &dyn CloudWatchApi,
    cluster_id: &str,
    range: MetricsRange,
    now: SystemTime,
) -> Result<MetricsSnapshot, MetricsFetchError> {
    let spec = RangeSpec::for_range(range);
    let end_time = align_to_period(now, spec.period)?;
    let start_time = end_time
        .checked_sub(spec.duration)
        .ok_or(MetricsFetchError::InvalidTime)?;
    let period =
        i32::try_from(spec.period.as_secs()).map_err(|_| MetricsFetchError::InvalidTime)?;
    let queries = metric_queries(cluster_id, period);
    let query_indexes = queries
        .iter()
        .enumerate()
        .map(|(index, query)| (query.id.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut values = vec![BTreeMap::new(); queries.len()];
    let mut next_token = None;
    let mut seen_tokens = HashSet::new();
    let mut incomplete_results = HashSet::new();
    let mut latest_sample = None;
    let mut stale = false;

    for _ in 0..MAX_METRIC_PAGES {
        let page = api
            .get_metric_data(MetricDataRequest {
                start_time,
                end_time,
                queries: queries.clone(),
                next_token: next_token.take(),
                scan_ascending: true,
            })
            .await
            .map_err(metrics_api_error)?;
        let page_incomplete = page.incomplete;

        for result in page.results {
            match result.status {
                MetricResultStatus::Complete => {
                    incomplete_results.remove(&result.id);
                }
                MetricResultStatus::Forbidden => return Err(MetricsFetchError::AccessDenied),
                MetricResultStatus::PartialData
                | MetricResultStatus::InternalError
                | MetricResultStatus::Unknown => {
                    incomplete_results.insert(result.id.clone());
                }
            }
            let Some(index) = query_indexes.get(&result.id).copied() else {
                continue;
            };
            for point in result.points {
                if !point.value.is_finite()
                    || point.timestamp < start_time
                    || point.timestamp >= end_time
                {
                    continue;
                }
                let bucket = align_from_start(point.timestamp, start_time, spec.period)?;
                values[index].insert(bucket, point.value);
                latest_sample =
                    Some(latest_sample.map_or(point.timestamp, |latest: SystemTime| {
                        latest.max(point.timestamp)
                    }));
            }
        }

        let Some(token) = page.next_token else {
            stale |= page_incomplete;
            break;
        };
        if !seen_tokens.insert(token.clone()) {
            return Err(MetricsFetchError::PaginationDidNotAdvance);
        }
        next_token = Some(token);
    }
    if next_token.is_some() {
        return Err(MetricsFetchError::TooManyPages);
    }

    stale |= !incomplete_results.is_empty();
    let stale_before = end_time
        .checked_sub(spec.period.saturating_mul(2))
        .unwrap_or(UNIX_EPOCH);
    stale |= latest_sample.is_some_and(|timestamp| timestamp < stale_before);
    let series = queries
        .into_iter()
        .zip(values)
        .map(|(query, points)| MetricSeries {
            metric: query.id,
            samples: (0..spec.sample_count())
                .map(|index| {
                    let timestamp = start_time + spec.period.saturating_mul(index as u32);
                    points.get(&timestamp).copied()
                })
                .collect(),
        })
        .collect();

    Ok(MetricsSnapshot {
        range,
        fetched_at: Some(now),
        series,
        status: if stale {
            MetricsFetchStatus::Stale
        } else {
            MetricsFetchStatus::Fresh
        },
    })
}

fn align_to_period(
    timestamp: SystemTime,
    period: Duration,
) -> Result<SystemTime, MetricsFetchError> {
    let seconds = timestamp
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MetricsFetchError::InvalidTime)?
        .as_secs();
    Ok(UNIX_EPOCH + Duration::from_secs(seconds - seconds % period.as_secs()))
}

fn align_from_start(
    timestamp: SystemTime,
    start: SystemTime,
    period: Duration,
) -> Result<SystemTime, MetricsFetchError> {
    let elapsed = timestamp
        .duration_since(start)
        .map_err(|_| MetricsFetchError::InvalidTime)?;
    Ok(start + Duration::from_secs(elapsed.as_secs() / period.as_secs() * period.as_secs()))
}

fn metrics_api_error(error: CloudWatchApiError) -> MetricsFetchError {
    match error {
        CloudWatchApiError::AccessDenied => MetricsFetchError::AccessDenied,
        CloudWatchApiError::Throttled => MetricsFetchError::Throttled,
        CloudWatchApiError::Other => MetricsFetchError::Other,
    }
}

fn metrics_access_denied() -> ApplicationError {
    ApplicationError::runtime(
        "CloudWatch metrics are unavailable; allow cloudwatch:GetMetricData on *",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct FakeApi {
        pages: Mutex<VecDeque<Result<MetricPage, CloudWatchApiError>>>,
        requests: Mutex<Vec<MetricDataRequest>>,
    }

    impl FakeApi {
        fn new(pages: Vec<Result<MetricPage, CloudWatchApiError>>) -> Self {
            Self {
                pages: Mutex::new(pages.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl CloudWatchApi for FakeApi {
        fn get_metric_data(
            &self,
            request: MetricDataRequest,
        ) -> ApiFuture<'_, Result<MetricPage, CloudWatchApiError>> {
            self.requests.lock().expect("request state").push(request);
            let response = self
                .pages
                .lock()
                .expect("page state")
                .pop_front()
                .expect("planned page");
            Box::pin(async move { response })
        }
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn result(id: &str, points: &[(u64, f64)]) -> MetricResult {
        MetricResult::new(
            id,
            points
                .iter()
                .map(|(timestamp, value)| MetricPoint::new(at(*timestamp), *value))
                .collect(),
            MetricResultStatus::Complete,
        )
    }

    #[test]
    fn metric_catalog_uses_the_documented_names_statistics_and_units() {
        let queries = metric_queries("cluster-123", 60);
        let actual = queries
            .iter()
            .map(|query| {
                (
                    query.id.as_str(),
                    query.namespace.as_str(),
                    query.metric_name.as_str(),
                    query.statistic,
                    query.unit,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                (
                    "total_transactions",
                    "AWS/AuroraDSQL",
                    "TotalTransactions",
                    Statistic::Sum,
                    Some(Unit::None)
                ),
                (
                    "read_only_transactions",
                    "AWS/AuroraDSQL",
                    "ReadOnlyTransactions",
                    Statistic::Sum,
                    Some(Unit::None)
                ),
                (
                    "commit_latency",
                    "AWS/AuroraDSQL",
                    "CommitLatency",
                    Statistic::Average,
                    Some(Unit::Milliseconds)
                ),
                (
                    "occ_conflicts",
                    "AWS/AuroraDSQL",
                    "OccConflicts",
                    Statistic::Sum,
                    Some(Unit::None)
                ),
                (
                    "query_timeouts",
                    "AWS/AuroraDSQL",
                    "QueryTimeouts",
                    Statistic::Sum,
                    Some(Unit::None)
                ),
                (
                    "total_dpu",
                    "AWS/AuroraDSQL",
                    "TotalDPU",
                    Statistic::Sum,
                    None
                ),
                (
                    "read_dpu",
                    "AWS/AuroraDSQL",
                    "ReadDPU",
                    Statistic::Sum,
                    None
                ),
                (
                    "write_dpu",
                    "AWS/AuroraDSQL",
                    "WriteDPU",
                    Statistic::Sum,
                    None
                ),
                (
                    "compute_dpu",
                    "AWS/AuroraDSQL",
                    "ComputeDPU",
                    Statistic::Sum,
                    None
                ),
                (
                    "multi_region_write_dpu",
                    "AWS/AuroraDSQL",
                    "MultiRegionWriteDPU",
                    Statistic::Sum,
                    None
                ),
                (
                    "bytes_read",
                    "AWS/AuroraDSQL",
                    "BytesRead",
                    Statistic::Sum,
                    Some(Unit::Bytes)
                ),
                (
                    "bytes_written",
                    "AWS/AuroraDSQL",
                    "BytesWritten",
                    Statistic::Sum,
                    Some(Unit::Bytes)
                ),
                (
                    "compute_time",
                    "AWS/AuroraDSQL",
                    "ComputeTime",
                    Statistic::Sum,
                    Some(Unit::Milliseconds)
                ),
                (
                    "cluster_storage_size",
                    "AWS/AuroraDSQL",
                    "ClusterStorageSize",
                    Statistic::Average,
                    Some(Unit::Bytes)
                ),
                (
                    "active_connections",
                    "AWS/Usage",
                    "ResourceCount",
                    Statistic::Average,
                    None
                ),
                (
                    "admin_connection_attempts",
                    "AWS/Usage",
                    "CallCount",
                    Statistic::Sum,
                    None
                ),
                (
                    "custom_role_connection_attempts",
                    "AWS/Usage",
                    "CallCount",
                    Statistic::Sum,
                    None
                ),
            ]
        );
    }

    #[test]
    fn metric_catalog_uses_complete_metric_dimensions() {
        let queries = metric_queries("cluster-123", 60);
        for query in &queries[..14] {
            assert_eq!(
                query.dimensions,
                vec![("ClusterId".into(), "cluster-123".into())]
            );
        }
        assert_eq!(
            queries[14].dimensions,
            usage_dimensions("cluster-123", "Resource", "ClusterConnectionCount")
        );
        assert_eq!(
            queries[15].dimensions,
            usage_dimensions("cluster-123", "API", "DbConnectAdmin")
        );
        assert_eq!(
            queries[16].dimensions,
            usage_dimensions("cluster-123", "API", "DbConnect")
        );
    }

    #[test]
    fn ranges_use_bounded_standard_resolution_windows() {
        let cases = [
            (MetricsRange::FifteenMinutes, 15 * 60, 60, 15),
            (MetricsRange::OneHour, 60 * 60, 60, 60),
            (MetricsRange::SixHours, 6 * 60 * 60, 5 * 60, 72),
            (MetricsRange::TwentyFourHours, 24 * 60 * 60, 15 * 60, 96),
        ];

        for (range, duration, period, samples) in cases {
            let spec = RangeSpec::for_range(range);
            assert_eq!(spec.duration, Duration::from_secs(duration));
            assert_eq!(spec.period, Duration::from_secs(period));
            assert_eq!(spec.sample_count(), samples);
        }
    }

    #[test]
    fn sdk_queries_preserve_metric_identity_and_omit_usage_units() {
        let mut queries = metric_queries("cluster-123", 60);
        let dpu = sdk_metric_query(queries[5].clone());
        let usage = sdk_metric_query(queries.remove(14));
        assert_eq!(dpu.metric_stat().expect("DPU metric stat").unit(), None);
        let metric_stat = usage.metric_stat().expect("metric stat");
        let metric = metric_stat.metric().expect("metric");

        assert_eq!(usage.id(), Some("active_connections"));
        assert_eq!(metric.namespace(), Some("AWS/Usage"));
        assert_eq!(metric.metric_name(), Some("ResourceCount"));
        assert_eq!(metric_stat.stat(), Some("Average"));
        assert_eq!(metric_stat.period(), Some(60));
        assert_eq!(metric_stat.unit(), None);
        assert_eq!(metric.dimensions().len(), 5);
    }

    #[test]
    fn snapshot_cache_evicts_oldest_entries_at_its_fixed_limit() {
        let mut cache = MetricsCache::default();
        for index in 0..=MAX_CACHED_SNAPSHOTS {
            cache.insert(
                MetricsCacheKey {
                    region: "us-east-1".into(),
                    cluster_id: format!("cluster-{index}"),
                    range: 0,
                },
                MetricsSnapshot::empty(MetricsRange::OneHour),
            );
        }

        assert_eq!(cache.snapshots.len(), MAX_CACHED_SNAPSHOTS);
        assert!(!cache.snapshots.contains_key(&MetricsCacheKey {
            region: "us-east-1".into(),
            cluster_id: "cluster-0".into(),
            range: 0,
        }));
        assert!(cache.snapshots.contains_key(&MetricsCacheKey {
            region: "us-east-1".into(),
            cluster_id: format!("cluster-{MAX_CACHED_SNAPSHOTS}"),
            range: 0,
        }));
    }

    #[test]
    fn sdk_result_and_operation_messages_mark_data_incomplete() {
        use aws_sdk_cloudwatch::types::{MessageData, MetricDataResult};

        let message = MessageData::builder()
            .code("Incomplete")
            .value("details intentionally discarded")
            .build();
        let result = MetricDataResult::builder()
            .id("total_transactions")
            .timestamps(DateTime::from_secs(3_600))
            .values(1.0)
            .status_code(SdkStatusCode::Complete)
            .messages(message.clone())
            .build();
        let output = aws_sdk_cloudwatch::operation::get_metric_data::GetMetricDataOutput::builder()
            .metric_data_results(result)
            .messages(message)
            .build();

        let page = map_sdk_output(&output);

        assert!(page.incomplete);
        assert_eq!(page.results[0].status, MetricResultStatus::Unknown);
        assert_eq!(
            page.results[0].points,
            vec![MetricPoint::new(at(3_600), 1.0)]
        );
    }

    #[test]
    fn sdk_error_classification_covers_http_and_service_throttling() {
        assert_eq!(
            classify_sdk_error(Some(403), ""),
            CloudWatchApiError::AccessDenied
        );
        assert_eq!(
            classify_sdk_error(Some(429), ""),
            CloudWatchApiError::Throttled
        );
        assert_eq!(
            classify_sdk_error(None, "AccessDeniedException"),
            CloudWatchApiError::AccessDenied
        );
        assert_eq!(
            classify_sdk_error(None, "ThrottlingException"),
            CloudWatchApiError::Throttled
        );
        assert_eq!(
            classify_sdk_error(Some(500), "InternalServiceError"),
            CloudWatchApiError::Other
        );
    }

    #[tokio::test]
    async fn fetch_batches_every_metric_in_one_ascending_request() {
        let api = FakeApi::new(vec![Ok(MetricPage::new(Vec::new(), None))]);

        fetch_metrics(&api, "cluster-123", MetricsRange::OneHour, at(7_234))
            .await
            .expect("metrics snapshot");

        let requests = api.requests.lock().expect("request state");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].start_time, at(3_600));
        assert_eq!(requests[0].end_time, at(7_200));
        assert_eq!(requests[0].queries.len(), 17);
        assert!(requests[0].scan_ascending);
        assert_eq!(requests[0].next_token, None);
    }

    #[tokio::test]
    async fn fetch_merges_paginated_results_in_time_order_and_preserves_gaps() {
        let api = FakeApi::new(vec![
            Ok(MetricPage::new(
                vec![MetricResult::new(
                    "total_transactions",
                    vec![
                        MetricPoint::new(at(3_720), 3.0),
                        MetricPoint::new(at(3_600), 1.0),
                    ],
                    MetricResultStatus::PartialData,
                )],
                Some("next".into()),
            )),
            Ok(MetricPage::new(
                vec![result("total_transactions", &[(3_660, 2.0), (7_140, 4.0)])],
                None,
            )),
        ]);

        let snapshot = fetch_metrics(&api, "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect("metrics snapshot");

        assert_eq!(snapshot.status, crate::app::MetricsFetchStatus::Fresh);
        assert_eq!(snapshot.fetched_at, Some(at(7_200)));
        assert_eq!(snapshot.series.len(), 17);
        assert_eq!(
            &snapshot.series[0].samples[..5],
            &[Some(1.0), Some(2.0), Some(3.0), None, None]
        );
        let requests = api.requests.lock().expect("request state");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].next_token.as_deref(), Some("next"));
    }

    #[tokio::test]
    async fn fetch_rejects_pagination_that_does_not_advance() {
        let api = FakeApi::new(vec![
            Ok(MetricPage::new(Vec::new(), Some("same".into()))),
            Ok(MetricPage::new(Vec::new(), Some("same".into()))),
        ]);

        let error = fetch_metrics(&api, "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect_err("repeated token rejected");

        assert!(error.to_string().contains("pagination did not advance"));
    }

    #[tokio::test]
    async fn successful_empty_response_is_no_data_instead_of_zeroes() {
        let api = FakeApi::new(vec![Ok(MetricPage::new(Vec::new(), None))]);

        let snapshot = fetch_metrics(&api, "cluster-123", MetricsRange::FifteenMinutes, at(7_200))
            .await
            .expect("metrics snapshot");

        assert_eq!(snapshot.status, crate::app::MetricsFetchStatus::Fresh);
        assert_eq!(snapshot.series.len(), 17);
        assert!(
            snapshot
                .series
                .iter()
                .all(|series| series.samples == vec![None; 15])
        );
    }

    #[tokio::test]
    async fn delayed_or_partial_results_are_marked_stale() {
        let delayed = FakeApi::new(vec![Ok(MetricPage::new(
            vec![result("total_transactions", &[(7_020, 1.0)])],
            None,
        ))]);
        let partial = FakeApi::new(vec![Ok(MetricPage::new(
            vec![MetricResult::new(
                "total_transactions",
                vec![MetricPoint::new(at(7_140), 1.0)],
                MetricResultStatus::PartialData,
            )],
            None,
        ))]);

        let delayed_snapshot =
            fetch_metrics(&delayed, "cluster-123", MetricsRange::OneHour, at(7_200))
                .await
                .expect("delayed snapshot");
        let partial_snapshot =
            fetch_metrics(&partial, "cluster-123", MetricsRange::OneHour, at(7_200))
                .await
                .expect("partial snapshot");

        assert_eq!(
            delayed_snapshot.status,
            crate::app::MetricsFetchStatus::Stale
        );
        assert_eq!(
            partial_snapshot.status,
            crate::app::MetricsFetchStatus::Stale
        );
    }

    #[tokio::test]
    async fn access_denial_names_the_required_action_without_raw_sdk_details() {
        let api = FakeApi::new(vec![Err(CloudWatchApiError::AccessDenied)]);

        let error = fetch_metrics(&api, "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect_err("permission failure");

        assert_eq!(
            error.to_string(),
            "CloudWatch metrics are unavailable; allow cloudwatch:GetMetricData on *"
        );
    }

    #[tokio::test]
    async fn result_level_forbidden_status_is_an_access_denial() {
        let api = FakeApi::new(vec![Ok(MetricPage::new(
            vec![MetricResult::new(
                "total_transactions",
                Vec::new(),
                MetricResultStatus::Forbidden,
            )],
            None,
        ))]);

        let error = fetch_metrics(&api, "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect_err("permission failure");

        assert!(error.to_string().contains("cloudwatch:GetMetricData"));
    }

    #[tokio::test]
    async fn provider_returns_the_last_successful_snapshot_when_throttled() {
        let api = FakeApi::new(vec![
            Ok(MetricPage::new(
                vec![result("total_transactions", &[(7_140, 4.0)])],
                None,
            )),
            Err(CloudWatchApiError::Throttled),
        ]);
        let provider = CloudWatchMetricsProvider::new(api);

        let fresh = provider
            .snapshot_at("us-east-1", "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect("fresh snapshot");
        let stale = provider
            .snapshot_at("us-east-1", "cluster-123", MetricsRange::OneHour, at(7_260))
            .await
            .expect("cached snapshot");

        assert_eq!(fresh.status, crate::app::MetricsFetchStatus::Fresh);
        assert_eq!(stale.status, crate::app::MetricsFetchStatus::Stale);
        assert_eq!(stale.fetched_at, fresh.fetched_at);
        assert_eq!(stale.series, fresh.series);
    }

    #[tokio::test]
    async fn provider_does_not_hide_access_denial_with_cached_data() {
        let api = FakeApi::new(vec![
            Ok(MetricPage::new(Vec::new(), None)),
            Err(CloudWatchApiError::AccessDenied),
        ]);
        let provider = CloudWatchMetricsProvider::new(api);
        provider
            .snapshot_at("us-east-1", "cluster-123", MetricsRange::OneHour, at(7_200))
            .await
            .expect("fresh snapshot");

        let error = provider
            .snapshot_at("us-east-1", "cluster-123", MetricsRange::OneHour, at(7_260))
            .await
            .expect_err("permission failure");

        assert!(error.to_string().contains("cloudwatch:GetMetricData"));
    }
}
