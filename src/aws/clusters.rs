#![allow(dead_code)] // Wired into CLI inventory by the integration owner.

use crate::aws::config::AwsConfiguration;
use crate::{
    app::{
        ClusterId, ClusterStatus, DiscoverableCluster, EnrichmentErrorCategory, EnrichmentState,
    },
    error::ApplicationError,
};
use futures::{StreamExt, stream};
use std::{collections::HashSet, future::Future, pin::Pin};

const LIST_CLUSTERS_PAGE_SIZE: i32 = 100;
const DETAIL_CONCURRENCY: usize = 8;
const MAX_DISCOVERABLE_CLUSTERS: usize = 10_000;

type ApiFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A DSQL `ListClusters` row without generated SDK types. The list ARN is
/// retained even when the corresponding detail request cannot be authorized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ListedCluster {
    identifier: String,
    arn: String,
}

impl ListedCluster {
    pub(crate) fn new(identifier: impl Into<String>, arn: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            arn: arn.into(),
        }
    }
}

/// One page returned by the DSQL list API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClusterPage {
    clusters: Vec<ListedCluster>,
    next_token: Option<String>,
}

impl ClusterPage {
    pub(crate) fn new(clusters: Vec<ListedCluster>, next_token: Option<String>) -> Self {
        Self {
            clusters,
            next_token,
        }
    }
}

/// Detail fields used by inventory. This is deliberately independent of the
/// AWS SDK output shape so deterministic fakes do not need generated types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClusterDetails {
    endpoint: Option<String>,
    lifecycle_status: String,
    display_name: Option<String>,
}

impl ClusterDetails {
    pub(crate) fn new(
        endpoint: Option<String>,
        lifecycle_status: impl Into<String>,
        display_name: Option<String>,
    ) -> Self {
        Self {
            endpoint,
            lifecycle_status: lifecycle_status.into(),
            display_name,
        }
    }
}

/// Stable, non-SDK error categories used both by the fakeable API and domain
/// enrichment state. No AWS diagnostic or request metadata is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClusterApiError {
    AccessDenied,
    Throttled,
    NotFound,
    Other,
}

/// Narrow asynchronous DSQL inventory boundary. Production uses SDK retries;
/// callers of this trait must not add application-level retries.
pub(crate) trait ClusterApi: Sync {
    fn list_clusters(
        &self,
        next_token: Option<String>,
        max_results: i32,
    ) -> ApiFuture<'_, Result<ClusterPage, ClusterApiError>>;

    fn get_cluster(
        &self,
        identifier: String,
    ) -> ApiFuture<'_, Result<ClusterDetails, ClusterApiError>>;
}

/// Real AWS SDK adapter. The generated client and its error types remain in
/// this module; the rest of the inventory pipeline sees only the records above.
pub(crate) struct AwsDsqlClusterApi {
    client: aws_sdk_dsql::Client,
}

impl AwsDsqlClusterApi {
    pub(crate) fn new(config: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_dsql::Client::new(config),
        }
    }
}

pub(crate) async fn discover_aws_clusters(
    configuration: &AwsConfiguration,
) -> Result<Vec<DiscoverableCluster>, ApplicationError> {
    let api = AwsDsqlClusterApi::new(configuration.sdk_config());
    discover_clusters(&api, configuration.context().region()).await
}

impl ClusterApi for AwsDsqlClusterApi {
    fn list_clusters(
        &self,
        next_token: Option<String>,
        max_results: i32,
    ) -> ApiFuture<'_, Result<ClusterPage, ClusterApiError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let response = client
                .list_clusters()
                .set_next_token(next_token)
                .max_results(max_results)
                .send()
                .await
                .map_err(map_list_error)?;
            let clusters = response
                .clusters()
                .iter()
                .map(|cluster| ListedCluster::new(cluster.identifier(), cluster.arn()))
                .collect();
            Ok(ClusterPage::new(
                clusters,
                response.next_token().map(str::to_owned),
            ))
        })
    }

    fn get_cluster(
        &self,
        identifier: String,
    ) -> ApiFuture<'_, Result<ClusterDetails, ClusterApiError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let response = client
                .get_cluster()
                .identifier(identifier)
                .send()
                .await
                .map_err(map_get_error)?;
            Ok(ClusterDetails::new(
                response.endpoint().map(str::to_owned),
                response.status().as_str(),
                response.tags().and_then(|tags| tags.get("Name")).cloned(),
            ))
        })
    }
}

fn map_list_error(
    error: aws_sdk_dsql::error::SdkError<aws_sdk_dsql::operation::list_clusters::ListClustersError>,
) -> ClusterApiError {
    map_error(error.as_service_error().map(|error| {
        if error.is_access_denied_exception() {
            ClusterApiError::AccessDenied
        } else if error.is_throttling_exception() {
            ClusterApiError::Throttled
        } else if error.is_resource_not_found_exception() {
            ClusterApiError::NotFound
        } else {
            ClusterApiError::Other
        }
    }))
}

fn map_get_error(
    error: aws_sdk_dsql::error::SdkError<aws_sdk_dsql::operation::get_cluster::GetClusterError>,
) -> ClusterApiError {
    map_error(error.as_service_error().map(|error| {
        if error.is_access_denied_exception() {
            ClusterApiError::AccessDenied
        } else if error.is_throttling_exception() {
            ClusterApiError::Throttled
        } else if error.is_resource_not_found_exception() {
            ClusterApiError::NotFound
        } else {
            ClusterApiError::Other
        }
    }))
}

fn map_error(category: Option<ClusterApiError>) -> ClusterApiError {
    category.unwrap_or(ClusterApiError::Other)
}

/// Discovers every `ListClusters` page, then enriches each row concurrently.
/// A list failure invalidates the inventory; a detail failure only degrades its
/// associated row. Results retain the API list order regardless of completion
/// order.
pub(crate) async fn discover_clusters(
    api: &dyn ClusterApi,
    region: &str,
) -> Result<Vec<DiscoverableCluster>, ApplicationError> {
    let mut listed = Vec::new();
    let mut next_token = None;
    let mut seen_tokens = HashSet::new();

    loop {
        let page = api
            .list_clusters(next_token.take(), LIST_CLUSTERS_PAGE_SIZE)
            .await
            .map_err(|_| ApplicationError::runtime("could not discover Aurora DSQL clusters"))?;
        extend_listed_clusters(&mut listed, page.clusters, MAX_DISCOVERABLE_CLUSTERS)?;

        let Some(token) = page.next_token else {
            break;
        };
        if !seen_tokens.insert(token.clone()) {
            return Err(ApplicationError::runtime(
                "cluster discovery pagination did not advance",
            ));
        }
        next_token = Some(token);
    }

    let mut enriched = stream::iter(listed.into_iter().enumerate())
        .map(|(index, listed)| async move {
            let detail = api.get_cluster(listed.identifier.clone()).await;
            (index, listed, detail)
        })
        .buffer_unordered(DETAIL_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    enriched.sort_by_key(|(index, _, _)| *index);

    Ok(enriched
        .into_iter()
        .map(|(_, listed, detail)| match detail {
            Ok(detail) => DiscoverableCluster::inventory(
                ClusterId::new(listed.identifier),
                listed.arn,
                region,
                detail.endpoint,
                Some(map_status(&detail.lifecycle_status)),
                detail.display_name,
                EnrichmentState::Complete,
            ),
            Err(error) => DiscoverableCluster::inventory(
                ClusterId::new(listed.identifier),
                listed.arn,
                region,
                None,
                None,
                None,
                EnrichmentState::Unavailable(map_enrichment_error(error)),
            ),
        })
        .collect())
}

fn extend_listed_clusters(
    listed: &mut Vec<ListedCluster>,
    page: Vec<ListedCluster>,
    limit: usize,
) -> Result<(), ApplicationError> {
    if listed
        .len()
        .checked_add(page.len())
        .is_none_or(|total| total > limit)
    {
        return Err(ApplicationError::runtime(
            "cluster discovery returned too many clusters",
        ));
    }
    listed.extend(page);
    Ok(())
}

fn map_enrichment_error(error: ClusterApiError) -> EnrichmentErrorCategory {
    match error {
        ClusterApiError::AccessDenied => EnrichmentErrorCategory::AccessDenied,
        ClusterApiError::Throttled => EnrichmentErrorCategory::Throttled,
        ClusterApiError::NotFound => EnrichmentErrorCategory::NotFound,
        ClusterApiError::Other => EnrichmentErrorCategory::Other,
    }
}

fn map_status(status: &str) -> ClusterStatus {
    match status {
        "ACTIVE" => ClusterStatus::Active,
        "CREATING" => ClusterStatus::Creating,
        "IDLE" => ClusterStatus::Idle,
        "INACTIVE" => ClusterStatus::Inactive,
        "UPDATING" => ClusterStatus::Updating,
        "DELETING" => ClusterStatus::Deleting,
        "DELETED" => ClusterStatus::Deleted,
        "FAILED" => ClusterStatus::Failed,
        "PENDING_SETUP" => ClusterStatus::PendingSetup,
        "PENDING_DELETE" => ClusterStatus::PendingDelete,
        _ => ClusterStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{
        StreamExt,
        channel::{mpsc, oneshot},
    };
    use std::{
        collections::{HashMap, VecDeque},
        sync::{Arc, Mutex},
    };

    #[test]
    fn discovery_inventory_has_an_explicit_row_limit() {
        let mut listed = vec![ListedCluster::new("one", "arn:one")];
        let error =
            extend_listed_clusters(&mut listed, vec![ListedCluster::new("two", "arn:two")], 1)
                .expect_err("inventory limit enforced");

        assert!(error.to_string().contains("too many clusters"));
        assert_eq!(listed.len(), 1);
    }

    enum DetailPlan {
        Immediate(Result<ClusterDetails, ClusterApiError>),
        Gate(oneshot::Receiver<Result<ClusterDetails, ClusterApiError>>),
    }

    struct State {
        pages: VecDeque<Result<ClusterPage, ClusterApiError>>,
        details: HashMap<String, VecDeque<DetailPlan>>,
        list_calls: Vec<(Option<String>, i32)>,
        detail_calls: Vec<String>,
    }

    struct FakeApi {
        state: Mutex<State>,
        started: mpsc::UnboundedSender<String>,
    }

    impl FakeApi {
        fn new(
            pages: Vec<Result<ClusterPage, ClusterApiError>>,
        ) -> (Self, mpsc::UnboundedReceiver<String>) {
            let (started, receiver) = mpsc::unbounded();
            (
                Self {
                    state: Mutex::new(State {
                        pages: pages.into(),
                        details: HashMap::new(),
                        list_calls: Vec::new(),
                        detail_calls: Vec::new(),
                    }),
                    started,
                },
                receiver,
            )
        }

        fn set_detail(&self, identifier: &str, plan: DetailPlan) {
            self.state
                .lock()
                .expect("fake state")
                .details
                .entry(identifier.into())
                .or_default()
                .push_back(plan);
        }
    }

    impl ClusterApi for FakeApi {
        fn list_clusters(
            &self,
            next_token: Option<String>,
            max_results: i32,
        ) -> ApiFuture<'_, Result<ClusterPage, ClusterApiError>> {
            let result = {
                let mut state = self.state.lock().expect("fake state");
                state.list_calls.push((next_token, max_results));
                state.pages.pop_front().expect("planned list response")
            };
            Box::pin(async move { result })
        }

        fn get_cluster(
            &self,
            identifier: String,
        ) -> ApiFuture<'_, Result<ClusterDetails, ClusterApiError>> {
            let (plan, started) = {
                let mut state = self.state.lock().expect("fake state");
                state.detail_calls.push(identifier.clone());
                let plan = state
                    .details
                    .get_mut(&identifier)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(DetailPlan::Immediate(Err(ClusterApiError::Other)));
                (plan, self.started.clone())
            };
            Box::pin(async move {
                let _ = started.unbounded_send(identifier);
                match plan {
                    DetailPlan::Immediate(result) => result,
                    DetailPlan::Gate(receiver) => receiver.await.expect("test releases detail"),
                }
            })
        }
    }

    fn listed(identifier: &str) -> ListedCluster {
        ListedCluster::new(
            identifier,
            format!("arn:aws:dsql:us-east-1:123:cluster/{identifier}"),
        )
    }

    fn detail(status: &str) -> ClusterDetails {
        ClusterDetails::new(
            Some("cluster.dsql.us-east-1.on.aws".into()),
            status,
            Some("orders".into()),
        )
    }

    #[tokio::test]
    async fn follows_more_than_one_hundred_rows_and_every_pagination_token() {
        let first = (0..100)
            .map(|index| listed(&format!("cluster-{index}")))
            .collect();
        let (api, mut started) = FakeApi::new(vec![
            Ok(ClusterPage::new(first, Some("second-page".into()))),
            Ok(ClusterPage::new(
                vec![listed("cluster-100")],
                Some("third-page".into()),
            )),
            Ok(ClusterPage::new(vec![listed("cluster-101")], None)),
        ]);
        for index in 0..102 {
            api.set_detail(
                &format!("cluster-{index}"),
                DetailPlan::Immediate(Ok(detail("ACTIVE"))),
            );
        }

        let clusters = discover_clusters(&api, "us-east-1")
            .await
            .expect("inventory");

        assert_eq!(clusters.len(), 102);
        assert_eq!(clusters[0].id().as_str(), "cluster-0");
        assert_eq!(clusters[101].id().as_str(), "cluster-101");
        assert_eq!(
            api.state.lock().expect("fake state").list_calls,
            vec![
                (None, LIST_CLUSTERS_PAGE_SIZE),
                (Some("second-page".into()), LIST_CLUSTERS_PAGE_SIZE),
                (Some("third-page".into()), LIST_CLUSTERS_PAGE_SIZE),
            ]
        );
        for _ in 0..102 {
            started.next().await.expect("detail started");
        }
    }

    #[tokio::test]
    async fn caps_detail_concurrency_at_eight_and_preserves_list_order_after_out_of_order_completion()
     {
        let identifiers: Vec<_> = (0..9).map(|index| format!("cluster-{index}")).collect();
        let (api, mut started) = FakeApi::new(vec![Ok(ClusterPage::new(
            identifiers
                .iter()
                .map(|identifier| listed(identifier))
                .collect(),
            None,
        ))]);
        let mut releases = Vec::new();
        for identifier in &identifiers {
            let (release, receiver) = oneshot::channel();
            api.set_detail(identifier, DetailPlan::Gate(receiver));
            releases.push(Some(release));
        }

        let api = Arc::new(api);
        let discovery_api = Arc::clone(&api);
        let task =
            tokio::spawn(
                async move { discover_clusters(discovery_api.as_ref(), "us-east-1").await },
            );
        for _ in 0..DETAIL_CONCURRENCY {
            started.next().await.expect("one of the first eight starts");
        }
        assert_eq!(
            api.state.lock().expect("fake state").detail_calls.len(),
            DETAIL_CONCURRENCY
        );

        for index in (0..DETAIL_CONCURRENCY).rev() {
            releases[index]
                .take()
                .expect("release is available")
                .send(Ok(detail("ACTIVE")))
                .expect("release detail");
        }
        started
            .next()
            .await
            .expect("ninth starts only after a slot frees");
        releases[8]
            .take()
            .expect("ninth release is available")
            .send(Ok(detail("ACTIVE")))
            .expect("release ninth detail");
        let clusters = task.await.expect("discovery task").expect("inventory");

        assert_eq!(
            clusters
                .iter()
                .map(|cluster| cluster.id().as_str())
                .collect::<Vec<_>>(),
            identifiers.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn detail_failures_degrade_rows_and_complete_rows_include_name_endpoint_arn_and_region() {
        let (api, _started) = FakeApi::new(vec![Ok(ClusterPage::new(
            vec![
                listed("denied"),
                listed("throttled"),
                listed("missing"),
                listed("unnamed"),
            ],
            None,
        ))]);
        api.set_detail(
            "denied",
            DetailPlan::Immediate(Err(ClusterApiError::AccessDenied)),
        );
        api.set_detail(
            "throttled",
            DetailPlan::Immediate(Err(ClusterApiError::Throttled)),
        );
        api.set_detail(
            "missing",
            DetailPlan::Immediate(Err(ClusterApiError::NotFound)),
        );
        api.set_detail(
            "unnamed",
            DetailPlan::Immediate(Ok(ClusterDetails::new(
                Some("unnamed.dsql.us-east-1.on.aws".into()),
                "IDLE",
                None,
            ))),
        );

        let clusters = discover_clusters(&api, "us-east-1")
            .await
            .expect("inventory");

        assert_eq!(
            clusters[0].enrichment(),
            EnrichmentState::Unavailable(EnrichmentErrorCategory::AccessDenied)
        );
        assert_eq!(
            clusters[1].enrichment(),
            EnrichmentState::Unavailable(EnrichmentErrorCategory::Throttled)
        );
        assert_eq!(
            clusters[2].enrichment(),
            EnrichmentState::Unavailable(EnrichmentErrorCategory::NotFound)
        );
        assert_eq!(clusters[3].enrichment(), EnrichmentState::Complete);
        assert_eq!(
            clusters[3].arn(),
            Some("arn:aws:dsql:us-east-1:123:cluster/unnamed")
        );
        assert_eq!(clusters[3].region(), "us-east-1");
        assert_eq!(
            clusters[3].endpoint(),
            Some("unnamed.dsql.us-east-1.on.aws")
        );
        assert_eq!(clusters[3].display_name(), None);
        assert_eq!(clusters[3].status(), Some(ClusterStatus::Idle));
    }

    #[test]
    fn maps_every_documented_lifecycle_status_and_unknown_values() {
        let cases = [
            ("ACTIVE", ClusterStatus::Active),
            ("CREATING", ClusterStatus::Creating),
            ("IDLE", ClusterStatus::Idle),
            ("INACTIVE", ClusterStatus::Inactive),
            ("UPDATING", ClusterStatus::Updating),
            ("DELETING", ClusterStatus::Deleting),
            ("DELETED", ClusterStatus::Deleted),
            ("FAILED", ClusterStatus::Failed),
            ("PENDING_SETUP", ClusterStatus::PendingSetup),
            ("PENDING_DELETE", ClusterStatus::PendingDelete),
            ("FUTURE_STATUS", ClusterStatus::Unknown),
        ];

        for (sdk_status, app_status) in cases {
            assert_eq!(map_status(sdk_status), app_status);
        }
    }

    #[tokio::test]
    async fn list_failure_stops_discovery_and_never_leaks_tokens_or_credentials() {
        let secret = "AQoDYXdzEJr-secret-token";
        let (api, _started) = FakeApi::new(vec![
            Ok(ClusterPage::new(Vec::new(), Some(secret.into()))),
            Err(ClusterApiError::AccessDenied),
        ]);

        let error = discover_clusters(&api, "us-east-1")
            .await
            .expect_err("list failure is total discovery failure");
        let rendered = format!("{error:?} {error}");

        assert_eq!(error.to_string(), "could not discover Aurora DSQL clusters");
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("AWS_SECRET_ACCESS_KEY"));
        assert!(
            api.state
                .lock()
                .expect("fake state")
                .detail_calls
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mixed_lifecycle_statuses_are_retained_per_cluster() {
        let (api, _started) = FakeApi::new(vec![Ok(ClusterPage::new(
            vec![
                listed("active"),
                listed("pending"),
                listed("deleted"),
                listed("future"),
            ],
            None,
        ))]);
        api.set_detail("active", DetailPlan::Immediate(Ok(detail("ACTIVE"))));
        api.set_detail(
            "pending",
            DetailPlan::Immediate(Ok(detail("PENDING_SETUP"))),
        );
        api.set_detail("deleted", DetailPlan::Immediate(Ok(detail("DELETED"))));
        api.set_detail("future", DetailPlan::Immediate(Ok(detail("FUTURE_STATUS"))));

        let clusters = discover_clusters(&api, "us-east-1")
            .await
            .expect("inventory");

        assert_eq!(clusters[0].status(), Some(ClusterStatus::Active));
        assert_eq!(clusters[1].status(), Some(ClusterStatus::PendingSetup));
        assert_eq!(clusters[2].status(), Some(ClusterStatus::Deleted));
        assert_eq!(clusters[3].status(), Some(ClusterStatus::Unknown));
    }
}
