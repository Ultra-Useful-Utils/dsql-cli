use crate::{
    app::{
        ClusterStatus, ClusterTarget, ConnectedSession, ConnectionIntent, DatabaseRole,
        ExecutionEvent, ExecutionSink, ManagedSession, MetricsFetchStatus, MetricsProvider,
        MetricsRange, SessionConnector,
    },
    aws::{
        clusters::discover_aws_clusters,
        config::{AwsConfigRequest, RegionPrompt, load_aws_configuration},
        identity::resolve_aws_caller_identity,
        metrics::cloudwatch_metrics_provider,
    },
    db::session::DsqlSessionConnector,
    error::ApplicationError,
    shell::invalidate_after_schema_change,
    sql::metadata::load_managed_snapshot,
};
use futures::FutureExt;
use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    sync::{Arc, OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const OPT_IN: &str = "AURORA_DSQL_LIVE_TEST";
const CLUSTER_ID: &str = "AURORA_DSQL_LIVE_CLUSTER_ID";
const REGION: &str = "AURORA_DSQL_LIVE_REGION";
const CUSTOM_ROLE: &str = "AURORA_DSQL_LIVE_CUSTOM_ROLE";
const MUTATING_OPT_IN: &str = "AURORA_DSQL_LIVE_MUTATING";
const MUTATING_CLUSTER_ID: &str = "AURORA_DSQL_LIVE_MUTATING_CLUSTER_ID";
const ACCOUNT_ID: &str = "AURORA_DSQL_LIVE_ACCOUNT_ID";
const READ_ONLY_TIMEOUT: Duration = Duration::from_secs(90);
const MUTATING_TIMEOUT: Duration = Duration::from_secs(180);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const OWNED_TABLE_PREFIX: &str = "dsql_cli_live_";

#[derive(Debug, Eq, PartialEq)]
struct LiveConfig {
    cluster_id: String,
    region: String,
    custom_role: Option<String>,
}

fn parse_live_config(get: impl Fn(&str) -> Option<String>) -> Result<LiveConfig, String> {
    if get(OPT_IN).as_deref() != Some("1") {
        return Err("set AURORA_DSQL_LIVE_TEST=1 to authorize live Aurora DSQL tests".into());
    }
    let cluster_id = required(&get, CLUSTER_ID)?;
    if cluster_id.len() != 26
        || !cluster_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err("AURORA_DSQL_LIVE_CLUSTER_ID must be a 26-character cluster ID".into());
    }
    let region = required(&get, REGION)?;
    if !crate::target::is_region(&region) {
        return Err("AURORA_DSQL_LIVE_REGION has invalid Region syntax".into());
    }
    let custom_role = get(CUSTOM_ROLE).filter(|value| !value.is_empty());
    if let Some(custom_role) = custom_role.as_deref() {
        if custom_role == "admin" {
            return Err("AURORA_DSQL_LIVE_CUSTOM_ROLE must name a custom role, not admin".into());
        }
        if !is_identifier(custom_role) {
            return Err("AURORA_DSQL_LIVE_CUSTOM_ROLE must be a PostgreSQL identifier".into());
        }
    }
    Ok(LiveConfig {
        cluster_id,
        region,
        custom_role,
    })
}

fn required(get: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    get(name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("set {name} for live Aurora DSQL tests"))
}

fn is_identifier(value: &str) -> bool {
    value.len() <= 63
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

struct NoRegionPrompt;

impl RegionPrompt for NoRegionPrompt {
    fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
        Ok(None)
    }
}

#[derive(Default)]
struct Sink {
    events: Vec<ExecutionEvent>,
}

impl Sink {
    fn rows(&self) -> Vec<Vec<Option<String>>> {
        self.events
            .iter()
            .filter_map(|event| match event {
                ExecutionEvent::Row(values) => Some(values.clone()),
                _ => None,
            })
            .collect()
    }

    fn sqlstate(&self) -> Option<&str> {
        self.events.iter().find_map(|event| match event {
            ExecutionEvent::Error { sqlstate, .. } => sqlstate.as_deref(),
            _ => None,
        })
    }
}

impl ExecutionSink for Sink {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        self.events.push(event);
        Ok(())
    }
}

struct LiveContext {
    config: LiveConfig,
    target: ClusterTarget,
    connector: DsqlSessionConnector,
    aws: crate::aws::config::AwsConfiguration,
}

async fn live_context() -> Result<LiveContext, String> {
    let config = parse_live_config(|name| std::env::var(name).ok())?;
    let mut prompt = NoRegionPrompt;
    let aws = load_aws_configuration(
        AwsConfigRequest::new(None, Some(config.region.clone()), None, false),
        &mut prompt,
    )
    .await
    .map_err(|error| error.to_string())?;
    let clusters = discover_aws_clusters(&aws)
        .await
        .map_err(|error| error.to_string())?;
    let cluster = clusters
        .iter()
        .find(|cluster| cluster.id().as_str() == config.cluster_id)
        .ok_or_else(|| {
            format!(
                "configured cluster {} is not discoverable in {}",
                config.cluster_id, config.region
            )
        })?;
    if cluster.status() != Some(ClusterStatus::Active) {
        return Err(format!(
            "configured cluster {} is not active",
            config.cluster_id
        ));
    }
    if cluster.endpoint().is_none() {
        return Err(format!(
            "configured cluster {} has no discoverable endpoint",
            config.cluster_id
        ));
    }
    let target = ClusterTarget::from_discovered(cluster);
    let connector = DsqlSessionConnector::new(aws.sdk_config().clone());
    Ok(LiveContext {
        config,
        target,
        connector,
        aws,
    })
}

fn connection_intent(target: &ClusterTarget, role: DatabaseRole) -> ConnectionIntent {
    ConnectionIntent::new(target.clone(), role, Vec::new(), "dsql-cli-live-test")
}

async fn connect(context: &LiveContext, role: DatabaseRole) -> Result<ConnectedSession, String> {
    context
        .connector
        .connect(&connection_intent(&context.target, role))
        .await
        .map_err(|error| error.to_string())
}

async fn execute(
    session: &mut ConnectedSession,
    statement: &str,
) -> Result<Sink, ApplicationError> {
    let mut sink = Sink::default();
    session.execute(statement, &mut sink).await?;
    Ok(sink)
}

fn expect_single_row(sink: &Sink, expected: &[Option<&str>]) -> Result<(), String> {
    let rows = sink.rows();
    let expected = expected
        .iter()
        .map(|value| value.map(str::to_owned))
        .collect::<Vec<_>>();
    if rows.as_slice() != [expected] {
        return Err(format!("unexpected query rows: {rows:?}"));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live Aurora DSQL opt-in and AWS credentials"]
async fn live_dsql_read_only_discovery_admin_and_metrics() {
    let result = tokio::time::timeout(READ_ONLY_TIMEOUT, async {
        let context = live_context().await?;

        let mut admin = connect(&context, DatabaseRole::Admin).await?;
        let admin_identity = execute(
            &mut admin,
            "SELECT current_user::text, current_database()::text",
        )
        .await
        .map_err(|error| error.to_string())?;
        expect_single_row(&admin_identity, &[Some("admin"), Some("postgres")])?;

        let provider = cloudwatch_metrics_provider(&context.aws);
        let snapshot = provider
            .snapshot(
                context.aws.context(),
                &context.target,
                MetricsRange::FifteenMinutes,
            )
            .await
            .map_err(|error| error.to_string())?;
        if snapshot.status != MetricsFetchStatus::Fresh || snapshot.fetched_at.is_none() {
            return Err("CloudWatch metrics snapshot was not fresh".into());
        }
        if snapshot.series.len() != 17 {
            return Err(format!(
                "expected 17 CloudWatch metric series, got {}",
                snapshot.series.len()
            ));
        }
        let unique_metrics = snapshot
            .series
            .iter()
            .map(|series| series.metric.as_str())
            .collect::<HashSet<_>>();
        let expected_metrics = HashSet::from([
            "total_transactions",
            "read_only_transactions",
            "commit_latency",
            "occ_conflicts",
            "query_timeouts",
            "total_dpu",
            "read_dpu",
            "write_dpu",
            "compute_dpu",
            "multi_region_write_dpu",
            "bytes_read",
            "bytes_written",
            "compute_time",
            "cluster_storage_size",
            "active_connections",
            "admin_connection_attempts",
            "custom_role_connection_attempts",
        ]);
        if unique_metrics != expected_metrics
            || snapshot
                .series
                .iter()
                .any(|series| series.samples.len() != 15)
        {
            return Err("CloudWatch metrics snapshot had duplicate or malformed series".into());
        }
        Ok::<_, String>(())
    })
    .await;

    match result {
        Ok(result) => result.expect("live Aurora DSQL read-only scenarios failed"),
        Err(_) => panic!("live Aurora DSQL read-only scenarios exceeded 90 seconds"),
    }
}

#[tokio::test]
#[ignore = "requires a pre-created custom database role mapped to the AWS identity"]
async fn live_dsql_custom_role_authentication() {
    let result = tokio::time::timeout(READ_ONLY_TIMEOUT, async {
        let context = live_context().await?;
        let custom_role = context.config.custom_role.as_deref().ok_or_else(|| {
            "set AURORA_DSQL_LIVE_CUSTOM_ROLE for custom-role authentication".to_owned()
        })?;
        let mut custom = connect(&context, DatabaseRole::Custom(custom_role.to_owned())).await?;
        let identity = execute(
            &mut custom,
            "SELECT current_user::text, current_database()::text",
        )
        .await
        .map_err(|error| error.to_string())?;
        expect_single_row(&identity, &[Some(custom_role), Some("postgres")])
    })
    .await;

    match result {
        Ok(result) => result.expect("live Aurora DSQL custom-role scenario failed"),
        Err(_) => panic!("live Aurora DSQL custom-role scenario exceeded 90 seconds"),
    }
}

struct OwnedTable {
    name: String,
    marker: String,
}

impl OwnedTable {
    fn unique() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let suffix = format!("{:x}_{timestamp:x}", std::process::id());
        Self {
            name: format!("{OWNED_TABLE_PREFIX}{suffix}"),
            marker: format!("dsql_cli_owner_{suffix}"),
        }
    }

    fn qualified(&self) -> String {
        format!("public.\"{}\"", self.name)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn marker(&self) -> &str {
        &self.marker
    }
}

async fn require_mutating_opt_in(context: &LiveContext) -> Result<(), String> {
    if std::env::var(MUTATING_OPT_IN).as_deref() != Ok("1") {
        return Err("set AURORA_DSQL_LIVE_MUTATING=1 to authorize live DDL and DML".into());
    }
    if std::env::var(MUTATING_CLUSTER_ID).as_deref() != Ok(&context.config.cluster_id) {
        return Err(format!(
            "set {MUTATING_CLUSTER_ID}={} to confirm the mutation target",
            context.config.cluster_id
        ));
    }
    let expected_account = std::env::var(ACCOUNT_ID)
        .ok()
        .filter(|value| value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| format!("set {ACCOUNT_ID} to the 12-digit target AWS account ID"))?;
    let lookup = resolve_aws_caller_identity(&context.aws).await;
    let actual_account = lookup
        .identity()
        .and_then(|identity| identity.account_id())
        .ok_or_else(|| "could not verify the AWS account before mutation".to_owned())?;
    if actual_account != expected_account {
        return Err(format!(
            "{ACCOUNT_ID} does not match the current AWS identity"
        ));
    }
    Ok(())
}

fn live_mutation_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn mutating_scenarios(context: &LiveContext, table: &OwnedTable) -> Result<(), String> {
    let now = SystemTime::now();
    let admin = connect(context, DatabaseRole::Admin).await?;
    let mut managed = ManagedSession::new(admin, &context.connector, now);

    let before = load_managed_snapshot(&mut managed).await;
    if before
        .relations()
        .iter()
        .any(|relation| relation.schema() == "public" && relation.relation() == table.name())
    {
        return Err(format!(
            "refusing to use pre-existing table {}",
            table.name()
        ));
    }
    let completion = Arc::new(RwLock::new(before));

    let qualified = table.qualified();
    let create = format!(
        "CREATE TABLE {qualified} (id bigint PRIMARY KEY, value bigint NOT NULL, \"{}\" boolean)",
        table.marker()
    );
    let mut sink = Sink::default();
    managed
        .execute(&create, &mut sink)
        .await
        .map_err(|error| error.to_string())?;
    let mut refresh_hint_shown = false;
    if !invalidate_after_schema_change(&completion, &create, true, &mut refresh_hint_shown)
        || !completion
            .read()
            .map_err(|_| "completion snapshot lock was poisoned")?
            .stale()
    {
        return Err("successful DDL did not invalidate completion metadata".into());
    }

    let refreshed = load_managed_snapshot(&mut managed).await;
    if refreshed.stale()
        || !refreshed
            .relations()
            .iter()
            .any(|relation| relation.schema() == "public" && relation.relation() == table.name())
    {
        return Err("catalog refresh did not observe the suite-owned table".into());
    }
    *completion
        .write()
        .map_err(|_| "completion snapshot lock was poisoned")? = refreshed;

    let mut sink = Sink::default();
    managed
        .execute(
            &format!("INSERT INTO {qualified} (id, value) VALUES (1, 0)"),
            &mut sink,
        )
        .await
        .map_err(|error| error.to_string())?;

    verify_occ_conflict(context, &qualified).await?;
    verify_row_limit(context, &qualified).await?;

    let mut sink = Sink::default();
    managed
        .execute(
            "SET application_name = 'dsql_cli_live_reconnect'",
            &mut sink,
        )
        .await
        .map_err(|error| error.to_string())?;
    let settings_query = settings_query();
    let mut before_reconnect = Sink::default();
    managed
        .execute(&settings_query, &mut before_reconnect)
        .await
        .map_err(|error| error.to_string())?;
    let mut after_reconnect = Sink::default();
    managed
        .execute_at(
            &settings_query,
            &mut after_reconnect,
            now + Duration::from_secs(55 * 60),
        )
        .await
        .map_err(|error| error.to_string())?;
    if !managed.take_reconnected() {
        return Err("session did not proactively reconnect at the 55-minute boundary".into());
    }
    if before_reconnect.rows() != after_reconnect.rows()
        || after_reconnect.rows().first().map(Vec::len) != Some(16)
    {
        return Err("session settings changed across proactive reconnect".into());
    }
    if managed.metadata().session_settings().len() != 16
        || !managed.metadata().session_settings().iter().any(|setting| {
            setting.name() == "disable_sync_create_index" && !setting.value().is_empty()
        })
    {
        return Err("reconnected session did not capture all Aurora DSQL settings".into());
    }
    Ok(())
}

fn settings_query() -> String {
    let settings = [
        "application_name",
        "client_encoding",
        "datestyle",
        "extra_float_digits",
        "intervalstyle",
        "timezone",
        "search_path",
        "enable_bitmapscan",
        "enable_hashjoin",
        "enable_indexonlyscan",
        "enable_indexscan",
        "enable_material",
        "enable_mergejoin",
        "enable_nestloop",
        "enable_seqscan",
        "disable_sync_create_index",
    ]
    .into_iter()
    .map(|name| format!("current_setting('{name}')::text"))
    .collect::<Vec<_>>()
    .join(", ");
    format!("SELECT {settings}")
}

async fn verify_occ_conflict(context: &LiveContext, table: &str) -> Result<(), String> {
    let mut first = connect(context, DatabaseRole::Admin).await?;
    let mut second = connect(context, DatabaseRole::Admin).await?;
    execute(&mut first, "BEGIN")
        .await
        .map_err(|error| error.to_string())?;
    execute(&mut second, "BEGIN")
        .await
        .map_err(|error| error.to_string())?;
    execute(
        &mut first,
        &format!("UPDATE {table} SET value = value + 1 WHERE id = 1"),
    )
    .await
    .map_err(|error| error.to_string())?;
    execute(
        &mut second,
        &format!("UPDATE {table} SET value = value + 1 WHERE id = 1"),
    )
    .await
    .map_err(|error| error.to_string())?;
    execute(&mut first, "COMMIT")
        .await
        .map_err(|error| error.to_string())?;

    let mut conflict = Sink::default();
    let error = second
        .execute("COMMIT", &mut conflict)
        .await
        .expect_err("the second conflicting transaction must fail");
    if conflict.sqlstate() != Some("40001")
        || !error.to_string().contains("OC000")
        || !error
            .to_string()
            .contains("retry the transaction explicitly")
    {
        return Err(format!("unexpected OCC conflict: {error}"));
    }
    let _ = execute(&mut second, "ROLLBACK").await;

    let result = execute(
        &mut first,
        &format!("SELECT value::text FROM {table} WHERE id = 1"),
    )
    .await
    .map_err(|error| error.to_string())?;
    expect_single_row(&result, &[Some("1")])
}

async fn verify_row_limit(context: &LiveContext, table: &str) -> Result<(), String> {
    let mut session = connect(context, DatabaseRole::Admin).await?;
    let values = (10_000..13_001)
        .map(|id| format!("({id}, 0)"))
        .collect::<Vec<_>>()
        .join(",");
    let mut limit = Sink::default();
    let error = session
        .execute(
            &format!("INSERT INTO {table} (id, value) VALUES {values}"),
            &mut limit,
        )
        .await
        .expect_err("mutating 3,001 rows must exceed the Aurora DSQL limit");
    if limit.sqlstate() != Some("54000") {
        return Err(format!("unexpected row-limit error: {error}"));
    }
    let count = execute(&mut session, &format!("SELECT count(*)::text FROM {table}"))
        .await
        .map_err(|error| error.to_string())?;
    expect_single_row(&count, &[Some("1")])
}

async fn cleanup_owned_table(context: &LiveContext, table: &OwnedTable) -> Result<(), String> {
    if !table.name().starts_with(OWNED_TABLE_PREFIX) {
        return Err("refusing to drop a table without the live-suite ownership prefix".into());
    }
    let mut session = connect(context, DatabaseRole::Admin).await?;
    execute(&mut session, "BEGIN")
        .await
        .map_err(|error| error.to_string())?;
    let mut ownership = Sink::default();
    let params = vec![table.name().to_owned(), table.marker().to_owned()];
    session
        .execute_params(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1)::text, EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2)::text",
            &params,
            &mut ownership,
        )
        .await
        .map_err(|error| error.to_string())?;
    match ownership.rows().as_slice() {
        [row] if row == &[Some("false".into()), Some("false".into())] => {
            execute(&mut session, "ROLLBACK")
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        [row] if row == &[Some("true".into()), Some("true".into())] => {
            execute(&mut session, &format!("DROP TABLE {}", table.qualified()))
                .await
                .map_err(|error| error.to_string())?;
            execute(&mut session, "COMMIT")
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        _ => {
            let _ = execute(&mut session, "ROLLBACK").await;
            Err("refusing to drop a table without the expected ownership marker".into())
        }
    }
}

#[tokio::test]
#[ignore = "requires separate authorization for live Aurora DSQL DDL and DML"]
async fn live_dsql_mutating_occ_ddl_reconnect_and_limits() {
    let context = tokio::time::timeout(READ_ONLY_TIMEOUT, live_context())
        .await
        .expect("live Aurora DSQL setup exceeded 90 seconds")
        .expect("live Aurora DSQL setup failed");
    require_mutating_opt_in(&context)
        .await
        .expect("live mutating Aurora DSQL tests are not authorized");
    let _mutation_guard = live_mutation_lock().lock().await;
    let table = OwnedTable::unique();
    let scenario = AssertUnwindSafe(tokio::time::timeout(
        MUTATING_TIMEOUT,
        mutating_scenarios(&context, &table),
    ))
    .catch_unwind()
    .await;

    match tokio::time::timeout(CLEANUP_TIMEOUT, cleanup_owned_table(&context, &table)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!(
            "live scenario cleanup failed; inspect table {} manually: {error}",
            table.name()
        ),
        Err(_) => panic!(
            "live scenario cleanup timed out; inspect table {} manually",
            table.name()
        ),
    }
    match scenario {
        Ok(Ok(result)) => result.expect("live Aurora DSQL mutating scenarios failed"),
        Ok(Err(_)) => panic!(
            "live Aurora DSQL mutating scenarios exceeded 180 seconds; check for table {}",
            table.name()
        ),
        Err(_) => panic!(
            "live Aurora DSQL mutating scenarios panicked; cleanup was attempted for table {}",
            table.name()
        ),
    }
}

#[test]
fn live_dsql_configuration_requires_explicit_opt_in() {
    let values = HashMap::from([
        (CLUSTER_ID, "0123456789abcdefghijklmnop"),
        (REGION, "us-east-1"),
        (CUSTOM_ROLE, "app_role"),
    ]);

    let error = parse_live_config(|name| values.get(name).map(ToString::to_string))
        .expect_err("missing opt-in must fail");

    assert_eq!(
        error,
        "set AURORA_DSQL_LIVE_TEST=1 to authorize live Aurora DSQL tests"
    );
}

#[test]
fn live_dsql_configuration_validates_target_before_aws_calls() {
    let valid = HashMap::from([
        (OPT_IN, "1"),
        (CLUSTER_ID, "0123456789abcdefghijklmnop"),
        (REGION, "us-east-1"),
        (CUSTOM_ROLE, "app_role"),
    ]);
    assert_eq!(
        parse_live_config(|name| valid.get(name).map(ToString::to_string)),
        Ok(LiveConfig {
            cluster_id: "0123456789abcdefghijklmnop".into(),
            region: "us-east-1".into(),
            custom_role: Some("app_role".into()),
        })
    );
    let mut without_custom_role = valid.clone();
    without_custom_role.remove(CUSTOM_ROLE);
    assert_eq!(
        parse_live_config(|name| without_custom_role.get(name).map(ToString::to_string)),
        Ok(LiveConfig {
            cluster_id: "0123456789abcdefghijklmnop".into(),
            region: "us-east-1".into(),
            custom_role: None,
        })
    );

    for (name, value, diagnostic) in [
        (
            CLUSTER_ID,
            "not-a-cluster",
            "AURORA_DSQL_LIVE_CLUSTER_ID must be a 26-character cluster ID",
        ),
        (
            REGION,
            "US-east-1",
            "AURORA_DSQL_LIVE_REGION has invalid Region syntax",
        ),
        (
            CUSTOM_ROLE,
            "app role",
            "AURORA_DSQL_LIVE_CUSTOM_ROLE must be a PostgreSQL identifier",
        ),
        (
            CUSTOM_ROLE,
            "admin",
            "AURORA_DSQL_LIVE_CUSTOM_ROLE must name a custom role, not admin",
        ),
    ] {
        let mut values = valid.clone();
        values.insert(name, value);
        assert_eq!(
            parse_live_config(|key| values.get(key).map(ToString::to_string)),
            Err(diagnostic.into())
        );
    }
}
