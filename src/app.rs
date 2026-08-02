#![allow(dead_code)] // Foundation contracts are consumed starting in Milestone 1.

use crate::error::ApplicationError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClusterId(String);

impl ClusterId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClusterStatus {
    Creating,
    Active,
    Idle,
    Inactive,
    Updating,
    Deleting,
    Deleted,
    Failed,
    PendingSetup,
    PendingDelete,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoverableCluster {
    id: ClusterId,
    arn: Option<String>,
    region: String,
    endpoint: Option<String>,
    status: Option<ClusterStatus>,
    display_name: Option<String>,
    enrichment: EnrichmentState,
}

impl DiscoverableCluster {
    pub(crate) fn new(
        id: ClusterId,
        region: impl Into<String>,
        endpoint: Option<String>,
        status: Option<ClusterStatus>,
        display_name: Option<String>,
    ) -> Self {
        Self {
            id,
            arn: None,
            region: region.into(),
            endpoint,
            status,
            display_name,
            enrichment: EnrichmentState::Complete,
        }
    }

    pub(crate) fn inventory(
        id: ClusterId,
        arn: String,
        region: impl Into<String>,
        endpoint: Option<String>,
        status: Option<ClusterStatus>,
        display_name: Option<String>,
        enrichment: EnrichmentState,
    ) -> Self {
        Self {
            id,
            arn: Some(arn),
            region: region.into(),
            endpoint,
            status,
            display_name,
            enrichment,
        }
    }

    pub(crate) fn id(&self) -> &ClusterId {
        &self.id
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn arn(&self) -> Option<&str> {
        self.arn.as_deref()
    }

    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    pub(crate) fn status(&self) -> Option<ClusterStatus> {
        self.status
    }

    pub(crate) fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub(crate) fn enrichment(&self) -> EnrichmentState {
        self.enrichment
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrichmentState {
    Complete,
    Unavailable(EnrichmentErrorCategory),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrichmentErrorCategory {
    AccessDenied,
    Throttled,
    NotFound,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClusterTarget {
    id: ClusterId,
    region: String,
    endpoint: Option<String>,
    arn: Option<String>,
    status: Option<ClusterStatus>,
    display_name: Option<String>,
}

impl ClusterTarget {
    pub(crate) fn new(
        id: impl Into<String>,
        region: impl Into<String>,
        endpoint: Option<String>,
    ) -> Self {
        Self {
            id: ClusterId::new(id),
            region: region.into(),
            endpoint,
            arn: None,
            status: None,
            display_name: None,
        }
    }

    pub(crate) fn resolved(
        id: impl Into<String>,
        region: impl Into<String>,
        endpoint: Option<String>,
        arn: Option<String>,
    ) -> Self {
        Self {
            id: ClusterId::new(id),
            region: region.into(),
            endpoint,
            arn,
            status: None,
            display_name: None,
        }
    }

    pub(crate) fn id(&self) -> &ClusterId {
        &self.id
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    pub(crate) fn arn(&self) -> Option<&str> {
        self.arn.as_deref()
    }

    pub(crate) fn status(&self) -> Option<ClusterStatus> {
        self.status
    }

    pub(crate) fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub(crate) fn from_discovered(cluster: &DiscoverableCluster) -> Self {
        Self {
            id: cluster.id.clone(),
            region: cluster.region.clone(),
            endpoint: cluster.endpoint.clone(),
            arn: cluster.arn.clone(),
            status: cluster.status,
            display_name: cluster.display_name.clone(),
        }
    }
}

/// AWS configuration resolved before any service adapter is called. The SDK
/// configuration remains inside the AWS adapter; this value is safe to share
/// with the application seams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedAwsContext {
    region: String,
    profile_label: Option<String>,
    caller_identity: Option<CallerIdentity>,
}

impl ResolvedAwsContext {
    pub(crate) fn new(
        region: impl Into<String>,
        profile_label: Option<String>,
        caller_identity: Option<CallerIdentity>,
    ) -> Self {
        Self {
            region: region.into(),
            profile_label,
            caller_identity,
        }
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn profile_label(&self) -> Option<&str> {
        self.profile_label.as_deref()
    }

    pub(crate) fn caller_identity(&self) -> Option<&CallerIdentity> {
        self.caller_identity.as_ref()
    }
}

/// Best-effort caller information. Either field can be unavailable without
/// preventing discovery or connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CallerIdentity {
    account_id: Option<String>,
    principal: Option<String>,
}

impl CallerIdentity {
    pub(crate) fn new(account_id: Option<String>, principal: Option<String>) -> Self {
        Self {
            account_id,
            principal,
        }
    }

    pub(crate) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(crate) fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DatabaseRole {
    Admin,
    Custom(String),
}

impl DatabaseRole {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Admin => "admin",
            Self::Custom(name) => name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionIntent {
    target: ClusterTarget,
    role: DatabaseRole,
    tls_roots: Vec<String>,
    application_name: String,
}

impl ConnectionIntent {
    pub(crate) fn new(
        target: ClusterTarget,
        role: DatabaseRole,
        tls_roots: Vec<String>,
        application_name: impl Into<String>,
    ) -> Self {
        Self {
            target,
            role,
            tls_roots,
            application_name: application_name.into(),
        }
    }

    pub(crate) fn target(&self) -> &ClusterTarget {
        &self.target
    }

    pub(crate) fn role(&self) -> &DatabaseRole {
        &self.role
    }

    pub(crate) fn tls_roots(&self) -> &[String] {
        &self.tls_roots
    }

    pub(crate) fn application_name(&self) -> &str {
        &self.application_name
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationCapability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionState {
    Idle,
    Active,
    Failed,
    Uncertain,
}

/// Typed liveness signal supplied by the database adapter.  Callers must not
/// infer connection loss from rendered diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionLiveness {
    Alive,
    Lost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconnectState {
    Connected,
    Due,
    Deferred,
    Required,
    Uncertain,
}

impl ReconnectState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Due => "due before next statement",
            Self::Deferred => "deferred until transaction ends",
            Self::Required => "required before next statement",
            Self::Uncertain => "blocked: session outcome uncertain",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionSetting {
    name: String,
    value: String,
}

impl SessionSetting {
    pub(crate) fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

/// Application-owned session state. A database adapter holds its PostgreSQL
/// client privately and supplies this state at the integration seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionMetadata {
    intent: ConnectionIntent,
    connected_at: SystemTime,
    cancellation: CancellationCapability,
    transaction_state: TransactionState,
    session_settings: Vec<SessionSetting>,
}

impl SessionMetadata {
    pub(crate) fn new(
        intent: ConnectionIntent,
        connected_at: SystemTime,
        cancellation: CancellationCapability,
        transaction_state: TransactionState,
        session_settings: Vec<SessionSetting>,
    ) -> Self {
        Self {
            intent,
            connected_at,
            cancellation,
            transaction_state,
            session_settings,
        }
    }

    pub(crate) fn intent(&self) -> &ConnectionIntent {
        &self.intent
    }

    pub(crate) fn connected_at(&self) -> SystemTime {
        self.connected_at
    }

    pub(crate) fn cancellation(&self) -> CancellationCapability {
        self.cancellation
    }

    pub(crate) fn transaction_state(&self) -> TransactionState {
        self.transaction_state
    }

    pub(crate) fn session_settings(&self) -> &[SessionSetting] {
        &self.session_settings
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionEvent {
    Columns(Vec<String>),
    Row(Vec<Option<String>>),
    CommandComplete {
        rows: u64,
    },
    Notice(String),
    Error {
        sqlstate: Option<String>,
        diagnostic: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetricsRange {
    FifteenMinutes,
    OneHour,
    SixHours,
    TwentyFourHours,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetricsFetchStatus {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricSeries {
    pub(crate) metric: String,
    pub(crate) samples: Vec<Option<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricsSnapshot {
    pub(crate) range: MetricsRange,
    pub(crate) fetched_at: Option<SystemTime>,
    pub(crate) series: Vec<MetricSeries>,
    pub(crate) status: MetricsFetchStatus,
}

impl MetricsSnapshot {
    pub(crate) fn empty(range: MetricsRange) -> Self {
        Self {
            range,
            fetched_at: None,
            series: Vec::new(),
            status: MetricsFetchStatus::Unavailable,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelationName {
    schema: String,
    relation: String,
}

impl RelationName {
    pub(crate) fn new(schema: impl Into<String>, relation: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            relation: relation.into(),
        }
    }

    pub(crate) fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) fn relation(&self) -> &str {
        &self.relation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ColumnName {
    schema: String,
    relation: String,
    column: String,
}

impl ColumnName {
    pub(crate) fn new(
        schema: impl Into<String>,
        relation: impl Into<String>,
        column: impl Into<String>,
    ) -> Self {
        Self {
            schema: schema.into(),
            relation: relation.into(),
            column: column.into(),
        }
    }

    pub(crate) fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) fn relation(&self) -> &str {
        &self.relation
    }

    pub(crate) fn column(&self) -> &str {
        &self.column
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MetadataSnapshot {
    schemas: Vec<String>,
    relations: Vec<RelationName>,
    columns: Vec<ColumnName>,
    roles: Vec<DatabaseRole>,
    loaded_at: Option<SystemTime>,
    stale: bool,
}

impl MetadataSnapshot {
    pub(crate) fn new(
        schemas: Vec<String>,
        relations: Vec<RelationName>,
        columns: Vec<ColumnName>,
        roles: Vec<DatabaseRole>,
        loaded_at: Option<SystemTime>,
        stale: bool,
    ) -> Self {
        Self {
            schemas,
            relations,
            columns,
            roles,
            loaded_at,
            stale,
        }
    }

    pub(crate) fn empty() -> Self {
        Self::new(Vec::new(), Vec::new(), Vec::new(), Vec::new(), None, false)
    }

    pub(crate) fn schemas(&self) -> &[String] {
        &self.schemas
    }

    pub(crate) fn relations(&self) -> &[RelationName] {
        &self.relations
    }

    pub(crate) fn columns(&self) -> &[ColumnName] {
        &self.columns
    }

    pub(crate) fn roles(&self) -> &[DatabaseRole] {
        &self.roles
    }

    pub(crate) fn loaded_at(&self) -> Option<SystemTime> {
        self.loaded_at
    }

    pub(crate) fn stale(&self) -> bool {
        self.stale
    }

    pub(crate) fn invalidate(&mut self) {
        self.schemas.clear();
        self.relations.clear();
        self.columns.clear();
        self.roles.clear();
        self.loaded_at = None;
        self.stale = true;
    }
}

pub(crate) trait ClusterDiscovery {
    fn discover(
        &self,
        context: &ResolvedAwsContext,
    ) -> Result<Vec<DiscoverableCluster>, ApplicationError>;
}

pub(crate) trait TargetSelector {
    fn select(&self, clusters: &[DiscoverableCluster]) -> Result<ClusterTarget, ApplicationError>;
}

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait ExecutionSink: Send {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError>;
}

pub(crate) trait SessionCancellation: Send + Sync {
    fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>>;
}

pub(crate) trait SessionHandle: Send {
    fn execute<'a>(
        &'a mut self,
        statement: &'a str,
        sink: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>>;

    fn execute_params<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a [String],
        _: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>> {
        Box::pin(async {
            Err(ApplicationError::runtime(
                "parameterized execution is not supported by this session",
            ))
        })
    }

    fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>>;

    fn liveness(&self) -> SessionLiveness {
        SessionLiveness::Alive
    }

    fn capture_session_settings(
        &self,
    ) -> BoxFuture<'_, Result<Vec<SessionSetting>, ApplicationError>> {
        Box::pin(async {
            Err(ApplicationError::runtime(
                "session setting capture is not supported by this session",
            ))
        })
    }
}

pub(crate) struct ConnectedSession {
    metadata: SessionMetadata,
    handle: Box<dyn SessionHandle>,
}

impl ConnectedSession {
    pub(crate) fn new(metadata: SessionMetadata, handle: Box<dyn SessionHandle>) -> Self {
        Self { metadata, handle }
    }

    pub(crate) fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    pub(crate) fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        self.handle.cancellation_handle()
    }

    pub(crate) fn liveness(&self) -> SessionLiveness {
        self.handle.liveness()
    }

    pub(crate) fn execute<'a>(
        &'a mut self,
        statement: &'a str,
        sink: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>> {
        self.handle.execute(statement, sink)
    }

    pub(crate) fn execute_params<'a>(
        &'a mut self,
        statement: &'a str,
        params: &'a [String],
        sink: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>> {
        self.handle.execute_params(statement, params, sink)
    }

    pub(crate) fn capture_session_settings(
        &self,
    ) -> BoxFuture<'_, Result<Vec<SessionSetting>, ApplicationError>> {
        self.handle.capture_session_settings()
    }
}

/// Owns reconnect eligibility and transaction state for every SQL entry path.
/// It deliberately replaces a session only after a fully restored replacement
/// has connected, and never retries the submitted statement.
pub(crate) struct ManagedSession<'a> {
    session: ConnectedSession,
    connector: &'a dyn SessionConnector,
    connected_at: SystemTime,
    state: TransactionState,
    settings: Vec<SessionSetting>,
    settings_current: bool,
    reconnect_required: bool,
    reconnected: bool,
}

impl<'a> ManagedSession<'a> {
    pub(crate) fn new(
        session: ConnectedSession,
        connector: &'a dyn SessionConnector,
        now: SystemTime,
    ) -> Self {
        let state = session.metadata().transaction_state();
        let settings = session.metadata().session_settings().to_vec();
        Self {
            session,
            connector,
            connected_at: now,
            state,
            settings,
            settings_current: true,
            reconnect_required: false,
            reconnected: false,
        }
    }

    pub(crate) fn state(&self) -> TransactionState {
        self.state
    }

    pub(crate) fn metadata(&self) -> &SessionMetadata {
        self.session.metadata()
    }

    pub(crate) fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        self.session.cancellation_handle()
    }

    pub(crate) fn reconnect_required(&self) -> bool {
        self.reconnect_required
    }

    pub(crate) fn reconnect_state(&self, now: SystemTime) -> ReconnectState {
        if self.state == TransactionState::Uncertain {
            ReconnectState::Uncertain
        } else if self.reconnect_required || self.session.liveness() == SessionLiveness::Lost {
            ReconnectState::Required
        } else if connection_is_due(self.connected_at, now) {
            if self.state == TransactionState::Idle {
                ReconnectState::Due
            } else {
                ReconnectState::Deferred
            }
        } else {
            ReconnectState::Connected
        }
    }

    pub(crate) fn take_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.reconnected)
    }

    pub(crate) fn mark_uncertain(&mut self) {
        self.state = TransactionState::Uncertain;
    }

    pub(crate) fn require_reconnect(&mut self) {
        if self.state == TransactionState::Idle {
            self.reconnect_required = true;
        } else {
            self.mark_uncertain();
        }
    }

    pub(crate) async fn execute(
        &mut self,
        statement: &str,
        sink: &mut dyn ExecutionSink,
    ) -> Result<(), ApplicationError> {
        self.execute_at(statement, sink, SystemTime::now()).await
    }

    pub(crate) async fn execute_at(
        &mut self,
        statement: &str,
        sink: &mut dyn ExecutionSink,
        now: SystemTime,
    ) -> Result<(), ApplicationError> {
        let control = crate::sql::scanner::classify_transaction_control(statement);
        self.prepare(now).await?;
        let result = self.session.execute(statement, sink).await;
        self.finalize(result, control).await
    }

    pub(crate) async fn execute_params(
        &mut self,
        statement: &str,
        params: &[String],
        sink: &mut dyn ExecutionSink,
    ) -> Result<(), ApplicationError> {
        let control = crate::sql::scanner::classify_transaction_control(statement);
        self.prepare(SystemTime::now()).await?;
        let result = self.session.execute_params(statement, params, sink).await;
        self.finalize(result, control).await
    }

    async fn prepare(&mut self, now: SystemTime) -> Result<(), ApplicationError> {
        if self.state == TransactionState::Uncertain {
            return Err(ApplicationError::runtime(
                "database session state is uncertain; statement was not submitted",
            ));
        }

        let reconnect_required =
            self.reconnect_required || self.session.liveness() == SessionLiveness::Lost;
        if reconnect_required && self.state != TransactionState::Idle {
            return Err(ApplicationError::runtime(
                "database connection requires reconnect but transaction state is not idle; statement was not submitted",
            ));
        }
        if reconnect_required
            || (self.state == TransactionState::Idle && connection_is_due(self.connected_at, now))
        {
            if !self.settings_current {
                return Err(ApplicationError::runtime(
                    "current session settings could not be captured; automatic reconnect is unsafe and the statement was not submitted",
                ));
            }
            self.reconnect(now).await?;
        }
        Ok(())
    }

    async fn finalize(
        &mut self,
        result: Result<(), ApplicationError>,
        control: crate::sql::scanner::TransactionControl,
    ) -> Result<(), ApplicationError> {
        match result {
            Ok(()) => {
                self.state = transition_transaction_state(self.state, control);
                match self.session.capture_session_settings().await {
                    Ok(settings) => {
                        self.settings = settings;
                        self.settings_current = true;
                    }
                    Err(_) => self.settings_current = false,
                }
                Ok(())
            }
            Err(error) => {
                if self.session.liveness() == SessionLiveness::Lost {
                    self.reconnect_required = true;
                    if self.state != TransactionState::Idle
                        || control == crate::sql::scanner::TransactionControl::Begin
                    {
                        self.state = TransactionState::Uncertain;
                    }
                } else if self.state == TransactionState::Active {
                    self.state = TransactionState::Failed;
                }
                Err(error)
            }
        }
    }

    async fn reconnect(&mut self, now: SystemTime) -> Result<(), ApplicationError> {
        // Only called from an Idle boundary. The old session remains owned until
        // a replacement has restored every SES-001 setting successfully.
        let intent = self.session.metadata().intent().clone();
        let mut last_error = None;
        let jitter_seed = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| u64::from(duration.subsec_nanos()));
        for attempt in 0..3 {
            match self
                .connector
                .connect_restoring(&intent, &self.settings)
                .await
            {
                Ok(replacement) => {
                    self.session = replacement;
                    self.connected_at = now;
                    self.reconnect_required = false;
                    self.reconnected = true;
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(reconnect_delay(attempt, jitter_seed)).await;
                    }
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| ApplicationError::runtime("could not reconnect database session")))
    }
}

const PROACTIVE_RECONNECT_AFTER: Duration = Duration::from_secs(55 * 60);

fn connection_is_due(connected_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(connected_at)
        .is_ok_and(|age| age >= PROACTIVE_RECONNECT_AFTER)
}

fn reconnect_delay(attempt: u32, jitter_seed: u64) -> Duration {
    let base = 50_u64.saturating_mul(1_u64 << attempt);
    let jitter = jitter_seed.wrapping_mul(u64::from(attempt) + 1) % (base / 4 + 1);
    Duration::from_millis(base + jitter)
}

pub(crate) fn transition_transaction_state(
    state: TransactionState,
    control: crate::sql::scanner::TransactionControl,
) -> TransactionState {
    use crate::sql::scanner::TransactionControl;
    match control {
        TransactionControl::Begin => TransactionState::Active,
        TransactionControl::Commit | TransactionControl::Rollback
            if state != TransactionState::Uncertain =>
        {
            TransactionState::Idle
        }
        TransactionControl::Savepoint
        | TransactionControl::Release
        | TransactionControl::RollbackTo
        | TransactionControl::Other => state,
        _ => state,
    }
}

pub(crate) trait SessionConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        intent: &'a ConnectionIntent,
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>>;

    fn connect_restoring<'a>(
        &'a self,
        _: &'a ConnectionIntent,
        _: &'a [SessionSetting],
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        Box::pin(async {
            Err(ApplicationError::runtime(
                "session setting restoration is not supported by this connector",
            ))
        })
    }
}

pub(crate) trait MetricsProvider: Send + Sync {
    fn snapshot<'a>(
        &'a self,
        context: &'a ResolvedAwsContext,
        target: &'a ClusterTarget,
        range: MetricsRange,
    ) -> BoxFuture<'a, Result<MetricsSnapshot, ApplicationError>>;
}

pub(crate) trait MetadataProvider {
    fn snapshot(&self, session: &SessionMetadata) -> Result<MetadataSnapshot, ApplicationError>;
}

/// Orchestrates stable domain seams; adapters supply AWS, PostgreSQL, and
/// CloudWatch integration without exposing their generated client types.
pub(crate) struct Application<'a> {
    discovery: &'a dyn ClusterDiscovery,
    selector: &'a dyn TargetSelector,
    connector: &'a dyn SessionConnector,
    metrics: &'a dyn MetricsProvider,
    metadata: &'a dyn MetadataProvider,
}

impl<'a> Application<'a> {
    pub(crate) fn new(
        discovery: &'a dyn ClusterDiscovery,
        selector: &'a dyn TargetSelector,
        connector: &'a dyn SessionConnector,
        metrics: &'a dyn MetricsProvider,
        metadata: &'a dyn MetadataProvider,
    ) -> Self {
        Self {
            discovery,
            selector,
            connector,
            metrics,
            metadata,
        }
    }

    pub(crate) fn discover_and_select(
        &self,
        context: &ResolvedAwsContext,
    ) -> Result<ClusterTarget, ApplicationError> {
        let clusters = self.discovery.discover(context)?;
        self.selector.select(&clusters)
    }

    pub(crate) async fn connect(
        &self,
        intent: &ConnectionIntent,
    ) -> Result<ConnectedSession, ApplicationError> {
        self.connector.connect(intent).await
    }

    pub(crate) async fn execute_statement(
        &self,
        session: &mut ConnectedSession,
        statement: &str,
        sink: &mut dyn ExecutionSink,
    ) -> Result<(), ApplicationError> {
        session.execute(statement, sink).await
    }

    pub(crate) async fn metrics_snapshot(
        &self,
        context: &ResolvedAwsContext,
        target: &ClusterTarget,
        range: MetricsRange,
    ) -> Result<MetricsSnapshot, ApplicationError> {
        self.metrics.snapshot(context, target, range).await
    }

    pub(crate) fn metadata_snapshot(
        &self,
        session: &ConnectedSession,
    ) -> Result<MetadataSnapshot, ApplicationError> {
        self.metadata.snapshot(session.metadata())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApplicationError;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::{Duration, UNIX_EPOCH};

    struct FakeDiscovery {
        clusters: Vec<DiscoverableCluster>,
        seen: Mutex<Vec<ResolvedAwsContext>>,
    }

    impl super::ClusterDiscovery for FakeDiscovery {
        fn discover(
            &self,
            context: &ResolvedAwsContext,
        ) -> Result<Vec<DiscoverableCluster>, ApplicationError> {
            self.seen
                .lock()
                .expect("discovery state")
                .push(context.clone());
            Ok(self.clusters.clone())
        }
    }

    struct FakeSelector {
        selected: ClusterTarget,
        seen: Mutex<Vec<DiscoverableCluster>>,
    }

    impl super::TargetSelector for FakeSelector {
        fn select(
            &self,
            clusters: &[DiscoverableCluster],
        ) -> Result<ClusterTarget, ApplicationError> {
            self.seen
                .lock()
                .expect("selector state")
                .clone_from(&clusters.to_vec());
            Ok(self.selected.clone())
        }
    }

    struct FakeSessionHandle {
        events: Vec<ExecutionEvent>,
        failure: Option<&'static str>,
        calls: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
        cancellation: Option<Arc<dyn SessionCancellation>>,
    }

    impl SessionHandle for FakeSessionHandle {
        fn execute<'a>(
            &'a mut self,
            statement: &'a str,
            sink: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.seen
                    .lock()
                    .expect("handle state")
                    .push(statement.into());
                for event in &self.events {
                    sink.emit(event.clone())?;
                }
                match self.failure {
                    Some(diagnostic) => Err(ApplicationError::runtime(diagnostic)),
                    None => Ok(()),
                }
            })
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            self.cancellation.clone()
        }
    }

    struct FakeCancellation {
        calls: AtomicUsize,
    }

    impl SessionCancellation for FakeCancellation {
        fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    struct RecordingSink {
        events: Vec<ExecutionEvent>,
    }

    impl ExecutionSink for RecordingSink {
        fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
            self.events.push(event);
            Ok(())
        }
    }

    struct FakeSessionConnector {
        metadata: SessionMetadata,
        events: Vec<ExecutionEvent>,
        failure: Option<&'static str>,
        calls: Arc<AtomicUsize>,
        statements: Arc<Mutex<Vec<String>>>,
        cancellation: Option<Arc<dyn SessionCancellation>>,
        seen: Mutex<Vec<ConnectionIntent>>,
        restored_seen: Mutex<Vec<Vec<SessionSetting>>>,
    }

    impl SessionConnector for FakeSessionConnector {
        fn connect<'a>(
            &'a self,
            intent: &'a ConnectionIntent,
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move {
                self.seen
                    .lock()
                    .expect("connector state")
                    .push(intent.clone());
                Ok(ConnectedSession::new(
                    self.metadata.clone(),
                    Box::new(FakeSessionHandle {
                        events: self.events.clone(),
                        failure: self.failure,
                        calls: self.calls.clone(),
                        seen: self.statements.clone(),
                        cancellation: self.cancellation.clone(),
                    }),
                ))
            })
        }

        fn connect_restoring<'a>(
            &'a self,
            intent: &'a ConnectionIntent,
            settings: &'a [SessionSetting],
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move {
                self.restored_seen
                    .lock()
                    .expect("restored connector state")
                    .push(settings.to_vec());
                self.connect(intent).await
            })
        }
    }

    struct ManagedTestHandle {
        calls: Arc<AtomicUsize>,
        statements: Arc<Mutex<Vec<String>>>,
        lost: Arc<AtomicBool>,
        lose_on_execute: bool,
        failure: Option<&'static str>,
        captured_settings: Vec<SessionSetting>,
        capture_failure: bool,
    }

    impl SessionHandle for ManagedTestHandle {
        fn execute<'a>(
            &'a mut self,
            statement: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.statements
                    .lock()
                    .expect("statement state")
                    .push(statement.into());
                if self.lose_on_execute {
                    self.lost.store(true, Ordering::SeqCst);
                }
                match self.failure {
                    Some(diagnostic) => Err(ApplicationError::runtime(diagnostic)),
                    None => Ok(()),
                }
            })
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            None
        }

        fn execute_params<'a>(
            &'a mut self,
            statement: &'a str,
            _: &'a [String],
            sink: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            self.execute(statement, sink)
        }

        fn liveness(&self) -> SessionLiveness {
            if self.lost.load(Ordering::SeqCst) {
                SessionLiveness::Lost
            } else {
                SessionLiveness::Alive
            }
        }

        fn capture_session_settings(
            &self,
        ) -> BoxFuture<'_, Result<Vec<SessionSetting>, ApplicationError>> {
            Box::pin(async {
                if self.capture_failure {
                    Err(ApplicationError::runtime(
                        "could not capture session settings",
                    ))
                } else {
                    Ok(self.captured_settings.clone())
                }
            })
        }
    }

    struct ScriptedReconnectConnector {
        metadata: SessionMetadata,
        failures_before_success: usize,
        attempts: Arc<AtomicUsize>,
        restored_seen: Arc<Mutex<Vec<Vec<SessionSetting>>>>,
        replacement_calls: Arc<AtomicUsize>,
        replacement_statements: Arc<Mutex<Vec<String>>>,
        replacement_settings: Vec<SessionSetting>,
    }

    impl ScriptedReconnectConnector {
        fn replacement(&self) -> ConnectedSession {
            ConnectedSession::new(
                self.metadata.clone(),
                Box::new(ManagedTestHandle {
                    calls: self.replacement_calls.clone(),
                    statements: self.replacement_statements.clone(),
                    lost: Arc::new(AtomicBool::new(false)),
                    lose_on_execute: false,
                    failure: None,
                    captured_settings: self.replacement_settings.clone(),
                    capture_failure: false,
                }),
            )
        }
    }

    impl SessionConnector for ScriptedReconnectConnector {
        fn connect<'a>(
            &'a self,
            _: &'a ConnectionIntent,
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move { Ok(self.replacement()) })
        }

        fn connect_restoring<'a>(
            &'a self,
            _: &'a ConnectionIntent,
            settings: &'a [SessionSetting],
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move {
                self.restored_seen
                    .lock()
                    .expect("restored state")
                    .push(settings.to_vec());
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < self.failures_before_success {
                    Err(ApplicationError::runtime("replacement unavailable"))
                } else {
                    Ok(self.replacement())
                }
            })
        }
    }

    fn managed_test_session<'a>(
        intent: &ConnectionIntent,
        state: TransactionState,
        connector: &'a ScriptedReconnectConnector,
        connected_at: SystemTime,
        lose_on_execute: bool,
        failure: Option<&'static str>,
        captured_settings: Vec<SessionSetting>,
    ) -> (
        ManagedSession<'a>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let statements = Arc::new(Mutex::new(Vec::new()));
        let initial = ConnectedSession::new(
            metadata(intent.clone(), CancellationCapability::Unavailable, state),
            Box::new(ManagedTestHandle {
                calls: calls.clone(),
                statements: statements.clone(),
                lost: Arc::new(AtomicBool::new(false)),
                lose_on_execute,
                failure,
                captured_settings,
                capture_failure: false,
            }),
        );
        (
            ManagedSession::new(initial, connector, connected_at),
            calls,
            statements,
        )
    }

    struct FakeMetrics {
        snapshot: MetricsSnapshot,
        seen: Mutex<Vec<(ResolvedAwsContext, ClusterTarget, MetricsRange)>>,
    }

    impl super::MetricsProvider for FakeMetrics {
        fn snapshot<'a>(
            &'a self,
            context: &'a ResolvedAwsContext,
            target: &'a ClusterTarget,
            range: MetricsRange,
        ) -> BoxFuture<'a, Result<MetricsSnapshot, ApplicationError>> {
            self.seen
                .lock()
                .expect("metrics state")
                .push((context.clone(), target.clone(), range));
            let snapshot = self.snapshot.clone();
            Box::pin(async move { Ok(snapshot) })
        }
    }

    struct FakeMetadata {
        snapshot: MetadataSnapshot,
        seen: Mutex<Vec<SessionMetadata>>,
    }

    impl super::MetadataProvider for FakeMetadata {
        fn snapshot(
            &self,
            session: &SessionMetadata,
        ) -> Result<MetadataSnapshot, ApplicationError> {
            self.seen
                .lock()
                .expect("metadata state")
                .push(session.clone());
            Ok(self.snapshot.clone())
        }
    }

    fn metadata(
        intent: ConnectionIntent,
        cancellation: CancellationCapability,
        state: TransactionState,
    ) -> SessionMetadata {
        SessionMetadata::new(
            intent,
            UNIX_EPOCH + Duration::from_secs(1),
            cancellation,
            state,
            vec![SessionSetting::new("timezone", "UTC")],
        )
    }

    fn target() -> ClusterTarget {
        ClusterTarget::new(
            "cluster-1",
            "us-east-1",
            Some("cluster-1.dsql.us-east-1.on.aws".into()),
        )
    }

    #[tokio::test]
    async fn streamed_events_are_observed_once_and_failure_is_not_replayed() {
        let context = ResolvedAwsContext::new("us-east-1", Some("development".into()), None);
        let selected = target();
        let discovery = FakeDiscovery {
            clusters: vec![DiscoverableCluster::new(
                ClusterId::new("cluster-1"),
                "us-east-1",
                Some("cluster-1.dsql.us-east-1.on.aws".into()),
                Some(ClusterStatus::Active),
                Some("orders".into()),
            )],
            seen: Mutex::new(Vec::new()),
        };
        let selector = FakeSelector {
            selected: selected.clone(),
            seen: Mutex::new(Vec::new()),
        };
        let intent = ConnectionIntent::new(
            selected.clone(),
            DatabaseRole::Custom("app_user".into()),
            vec!["test-root.pem".into()],
            "dsql test",
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let statements = Arc::new(Mutex::new(Vec::new()));
        let cancellation = Arc::new(FakeCancellation {
            calls: AtomicUsize::new(0),
        });
        let connector = FakeSessionConnector {
            metadata: metadata(
                intent.clone(),
                CancellationCapability::Available,
                TransactionState::Idle,
            ),
            events: vec![
                ExecutionEvent::Columns(vec!["id".into()]),
                ExecutionEvent::Row(vec![Some("1".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
            ],
            failure: Some("connection lost after submission"),
            calls: calls.clone(),
            statements: statements.clone(),
            cancellation: Some(cancellation.clone()),
            seen: Mutex::new(Vec::new()),
            restored_seen: Mutex::new(Vec::new()),
        };
        let metrics = FakeMetrics {
            snapshot: MetricsSnapshot::empty(MetricsRange::OneHour),
            seen: Mutex::new(Vec::new()),
        };
        let metadata = FakeMetadata {
            snapshot: MetadataSnapshot::empty(),
            seen: Mutex::new(Vec::new()),
        };
        let app = Application::new(&discovery, &selector, &connector, &metrics, &metadata);

        let target = app.discover_and_select(&context).expect("selected target");
        let mut session = app.connect(&intent).await.expect("fake session");
        let settings = session.metadata().session_settings().to_vec();
        let _restored = connector
            .connect_restoring(&intent, &settings)
            .await
            .expect("fake restored session");
        let mut sink = RecordingSink { events: Vec::new() };
        let error = app
            .execute_statement(&mut session, "SELECT id FROM orders", &mut sink)
            .await
            .expect_err("disconnect is returned without a replay");

        assert_eq!(target, selected);
        assert_eq!(
            discovery.seen.lock().expect("discovery state").as_slice(),
            &[context]
        );
        assert_eq!(
            connector.seen.lock().expect("connector state").as_slice(),
            &[intent.clone(), intent]
        );
        assert_eq!(
            connector
                .restored_seen
                .lock()
                .expect("restored connector state")
                .as_slice(),
            &[settings]
        );
        assert_eq!(
            sink.events,
            vec![
                ExecutionEvent::Columns(vec!["id".into()]),
                ExecutionEvent::Row(vec![Some("1".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
            ]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            statements.lock().expect("handle state").as_slice(),
            &["SELECT id FROM orders"]
        );
        assert!(!error.to_string().contains("SELECT id FROM orders"));

        session
            .cancellation_handle()
            .expect("cancellation handle")
            .cancel()
            .await
            .expect("cancellation forwards");
        assert_eq!(cancellation.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn connected_session_forwards_parameterized_execution() {
        type SeenParameters = Arc<Mutex<Vec<(String, Vec<String>)>>>;

        struct ParameterHandle {
            seen: SeenParameters,
        }

        impl SessionHandle for ParameterHandle {
            fn execute<'a>(
                &'a mut self,
                _: &'a str,
                _: &'a mut dyn ExecutionSink,
            ) -> BoxFuture<'a, Result<(), ApplicationError>> {
                Box::pin(async { Ok(()) })
            }

            fn execute_params<'a>(
                &'a mut self,
                statement: &'a str,
                params: &'a [String],
                _: &'a mut dyn ExecutionSink,
            ) -> BoxFuture<'a, Result<(), ApplicationError>> {
                Box::pin(async move {
                    self.seen
                        .lock()
                        .expect("parameter state")
                        .push((statement.into(), params.to_vec()));
                    Ok(())
                })
            }

            fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
                None
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let mut session = ConnectedSession::new(
            metadata(
                intent,
                CancellationCapability::Unavailable,
                TransactionState::Idle,
            ),
            Box::new(ParameterHandle { seen: seen.clone() }),
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_params("SELECT $1", &["untrusted".into()], &mut sink)
            .await
            .expect("parameter forwarding");

        assert_eq!(
            seen.lock().expect("parameter state").as_slice(),
            &[("SELECT $1".into(), vec!["untrusted".into()])]
        );
    }

    #[tokio::test]
    async fn metrics_and_metadata_keep_gaps_and_staleness_without_driver_state() {
        let context = ResolvedAwsContext::new("us-east-1", None, None);
        let target = target();
        let intent =
            ConnectionIntent::new(target.clone(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let discovery = FakeDiscovery {
            clusters: vec![],
            seen: Mutex::new(Vec::new()),
        };
        let selector = FakeSelector {
            selected: target.clone(),
            seen: Mutex::new(Vec::new()),
        };
        let connector = FakeSessionConnector {
            metadata: metadata(
                intent.clone(),
                CancellationCapability::Unavailable,
                TransactionState::Failed,
            ),
            events: vec![],
            failure: None,
            calls: Arc::new(AtomicUsize::new(0)),
            statements: Arc::new(Mutex::new(Vec::new())),
            cancellation: None,
            seen: Mutex::new(Vec::new()),
            restored_seen: Mutex::new(Vec::new()),
        };
        let metrics = FakeMetrics {
            snapshot: MetricsSnapshot {
                range: MetricsRange::OneHour,
                fetched_at: Some(UNIX_EPOCH + Duration::from_secs(2)),
                series: vec![MetricSeries {
                    metric: "active_connections".into(),
                    samples: vec![Some(3.0), None, Some(4.0)],
                }],
                status: MetricsFetchStatus::Stale,
            },
            seen: Mutex::new(Vec::new()),
        };
        let metadata = FakeMetadata {
            snapshot: MetadataSnapshot::new(
                vec!["public".into()],
                vec![RelationName::new("public", "orders")],
                vec![ColumnName::new("public", "orders", "id")],
                vec![DatabaseRole::Admin],
                Some(UNIX_EPOCH + Duration::from_secs(3)),
                true,
            ),
            seen: Mutex::new(Vec::new()),
        };
        let app = Application::new(&discovery, &selector, &connector, &metrics, &metadata);
        let session = app.connect(&intent).await.expect("fake session");

        let metrics_snapshot = app
            .metrics_snapshot(&context, &target, MetricsRange::OneHour)
            .await
            .expect("metrics snapshot");
        let metadata_snapshot = app.metadata_snapshot(&session).expect("metadata snapshot");

        assert_eq!(metrics_snapshot.status, MetricsFetchStatus::Stale);
        assert_eq!(
            metrics_snapshot.series[0].samples,
            vec![Some(3.0), None, Some(4.0)]
        );
        assert!(metadata_snapshot.stale());
        assert_eq!(
            metadata_snapshot.relations(),
            &[RelationName::new("public", "orders")]
        );
        assert_eq!(metadata_snapshot.roles(), &[DatabaseRole::Admin]);
        assert_eq!(
            metadata.seen.lock().expect("metadata state").as_slice(),
            &[session.metadata().clone()]
        );
    }

    fn reconnect_connector(
        intent: &ConnectionIntent,
        failures_before_success: usize,
    ) -> ScriptedReconnectConnector {
        ScriptedReconnectConnector {
            metadata: metadata(
                intent.clone(),
                CancellationCapability::Unavailable,
                TransactionState::Idle,
            ),
            failures_before_success,
            attempts: Arc::new(AtomicUsize::new(0)),
            restored_seen: Arc::new(Mutex::new(Vec::new())),
            replacement_calls: Arc::new(AtomicUsize::new(0)),
            replacement_statements: Arc::new(Mutex::new(Vec::new())),
            replacement_settings: vec![SessionSetting::new("timezone", "UTC")],
        }
    }

    #[tokio::test]
    async fn reconnects_at_fifty_five_minutes_but_not_one_second_before() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, initial_statements) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at(
                "SELECT 'before';",
                &mut sink,
                connected_at + Duration::from_secs(55 * 60 - 1),
            )
            .await
            .expect("statement before threshold");
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            session.reconnect_state(connected_at + Duration::from_secs(55 * 60)),
            ReconnectState::Due
        );

        session
            .execute_at(
                "SELECT 'boundary';",
                &mut sink,
                connected_at + Duration::from_secs(55 * 60),
            )
            .await
            .expect("statement at threshold");

        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            initial_statements
                .lock()
                .expect("initial statements")
                .as_slice(),
            &["SELECT 'before';"]
        );
        assert_eq!(
            connector
                .replacement_statements
                .lock()
                .expect("replacement statements")
                .as_slice(),
            &["SELECT 'boundary';"]
        );
        assert!(session.take_reconnected());
        assert!(!session.take_reconnected());
    }

    #[tokio::test]
    async fn proactive_reconnect_waits_for_active_or_failed_transactions_to_close() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        for (state, closing_statement) in [
            (TransactionState::Active, "COMMIT;"),
            (TransactionState::Failed, "ROLLBACK;"),
        ] {
            let connector = reconnect_connector(&intent, 0);
            let (mut session, calls, _) = managed_test_session(
                &intent,
                state,
                &connector,
                connected_at,
                false,
                None,
                vec![SessionSetting::new("timezone", "UTC")],
            );
            let mut sink = RecordingSink { events: Vec::new() };
            assert_eq!(
                session.reconnect_state(connected_at + PROACTIVE_RECONNECT_AFTER),
                ReconnectState::Deferred
            );

            session
                .execute_at(
                    closing_statement,
                    &mut sink,
                    connected_at + PROACTIVE_RECONNECT_AFTER,
                )
                .await
                .expect("transaction can close on the existing connection");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
            assert_eq!(session.state(), TransactionState::Idle);

            session
                .execute_at(
                    "SELECT 1;",
                    &mut sink,
                    connected_at + PROACTIVE_RECONNECT_AFTER + Duration::from_secs(1),
                )
                .await
                .expect("next statement reconnects at an idle boundary");
            assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn failed_setting_capture_blocks_automatic_reconnect_without_failing_the_statement() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let calls = Arc::new(AtomicUsize::new(0));
        let initial = ConnectedSession::new(
            metadata(
                intent.clone(),
                CancellationCapability::Unavailable,
                TransactionState::Idle,
            ),
            Box::new(ManagedTestHandle {
                calls: calls.clone(),
                statements: Arc::new(Mutex::new(Vec::new())),
                lost: Arc::new(AtomicBool::new(false)),
                lose_on_execute: false,
                failure: None,
                captured_settings: Vec::new(),
                capture_failure: true,
            }),
        );
        let mut session = ManagedSession::new(initial, &connector, connected_at);
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at("SET timezone = 'Europe/London';", &mut sink, connected_at)
            .await
            .expect("accepted SQL remains successful");
        let error = session
            .execute_at(
                "SELECT 1;",
                &mut sink,
                connected_at + PROACTIVE_RECONNECT_AFTER,
            )
            .await
            .expect_err("stale settings must not be restored");

        assert!(error.to_string().contains("session settings"));
        assert!(error.to_string().contains("statement was not submitted"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn uncertain_live_session_refuses_every_statement_without_submission() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, calls, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        session.mark_uncertain();
        assert_eq!(
            session.reconnect_state(connected_at),
            ReconnectState::Uncertain
        );
        let mut sink = RecordingSink { events: Vec::new() };

        let error = session
            .execute_at("SELECT 1;", &mut sink, connected_at)
            .await
            .expect_err("uncertain state must fail closed");

        assert!(error.to_string().contains("statement was not submitted"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lost_idle_session_reconnects_before_the_next_statement_without_replay() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, initial_statements) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            true,
            Some("connection lost after submission"),
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at("SELECT 'failed';", &mut sink, connected_at)
            .await
            .expect_err("submitted statement fails");
        assert!(session.reconnect_required());
        assert_eq!(
            session.reconnect_state(connected_at),
            ReconnectState::Required
        );
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);

        session
            .execute_at(
                "SELECT 'resubmitted explicitly';",
                &mut sink,
                connected_at + Duration::from_secs(1),
            )
            .await
            .expect("next statement uses replacement");

        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            initial_statements
                .lock()
                .expect("initial statements")
                .as_slice(),
            &["SELECT 'failed';"]
        );
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            connector
                .replacement_statements
                .lock()
                .expect("replacement statements")
                .as_slice(),
            &["SELECT 'resubmitted explicitly';"]
        );
    }

    #[tokio::test]
    async fn lost_session_during_begin_becomes_uncertain_from_idle() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            true,
            Some("connection lost after submission"),
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at("BEGIN;", &mut sink, connected_at)
            .await
            .expect_err("submitted BEGIN fails");

        assert_eq!(session.state(), TransactionState::Uncertain);
        assert_eq!(
            session.reconnect_state(connected_at),
            ReconnectState::Uncertain
        );
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lost_non_idle_session_becomes_uncertain_and_refuses_reconnect() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, _) = managed_test_session(
            &intent,
            TransactionState::Active,
            &connector,
            connected_at,
            true,
            Some("connection lost after submission"),
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at("UPDATE orders SET value = 1;", &mut sink, connected_at)
            .await
            .expect_err("submitted statement fails");
        assert_eq!(session.state(), TransactionState::Uncertain);

        let error = session
            .execute_at(
                "SELECT 1;",
                &mut sink,
                connected_at + Duration::from_secs(1),
            )
            .await
            .expect_err("uncertain session must not reconnect");
        assert!(error.to_string().contains("statement was not submitted"));
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_replacement_attempts_preserve_the_old_session() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, usize::MAX);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let old_metadata = session.metadata().clone();
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at(
                "SELECT 1;",
                &mut sink,
                connected_at + PROACTIVE_RECONNECT_AFTER,
            )
            .await
            .expect_err("all replacement attempts fail");

        assert_eq!(connector.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(initial_calls.load(Ordering::SeqCst), 0);
        assert_eq!(session.metadata(), &old_metadata);
        assert!(!session.take_reconnected());
    }

    #[tokio::test]
    async fn reconnect_retries_are_bounded_and_third_attempt_can_succeed() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 2);
        let connected_at = UNIX_EPOCH + Duration::from_nanos(123_456_789);
        let (mut session, _, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at(
                "SELECT 1;",
                &mut sink,
                connected_at + PROACTIVE_RECONNECT_AFTER,
            )
            .await
            .expect("third replacement succeeds");

        assert_eq!(connector.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(connector.replacement_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reconnect_backoff_is_deterministic_exponential_and_bounded() {
        for attempt in 0..3 {
            let base = Duration::from_millis(50 * (1 << attempt));
            let delay = reconnect_delay(attempt, 123_456_789);
            assert_eq!(delay, reconnect_delay(attempt, 123_456_789));
            assert!(delay >= base);
            assert!(delay <= base + base / 4);
        }
    }

    #[tokio::test]
    async fn successful_statement_refreshes_the_settings_used_by_reconnect() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let updated = vec![SessionSetting::new("timezone", "America/New_York")];
        let (mut session, _, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            updated.clone(),
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_at(
                "SET timezone = 'America/New_York';",
                &mut sink,
                connected_at,
            )
            .await
            .expect("setting update");
        session
            .execute_at(
                "SELECT 1;",
                &mut sink,
                connected_at + PROACTIVE_RECONNECT_AFTER,
            )
            .await
            .expect("reconnect with latest settings");

        assert_eq!(
            connector
                .restored_seen
                .lock()
                .expect("restored settings")
                .as_slice(),
            &[updated]
        );
    }

    #[tokio::test]
    async fn parameterized_metadata_execution_uses_the_same_reconnect_boundary() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_params("SELECT $1;", &["catalog".into()], &mut sink)
            .await
            .expect("parameterized execution");

        assert_eq!(initial_calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(connector.replacement_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parameterized_transaction_control_updates_session_state() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = SystemTime::now();
        let (mut session, _, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            Vec::new(),
        );
        let mut sink = RecordingSink { events: Vec::new() };

        session
            .execute_params("BEGIN;", &[], &mut sink)
            .await
            .expect("parameterized begin");

        assert_eq!(session.state(), TransactionState::Active);
    }

    #[tokio::test]
    async fn simulated_two_hour_session_reconnects_only_at_safe_boundaries() {
        let intent = ConnectionIntent::new(target(), DatabaseRole::Admin, Vec::new(), "dsql test");
        let connector = reconnect_connector(&intent, 0);
        let connected_at = UNIX_EPOCH + Duration::from_secs(10);
        let (mut session, initial_calls, _) = managed_test_session(
            &intent,
            TransactionState::Idle,
            &connector,
            connected_at,
            false,
            None,
            vec![SessionSetting::new("timezone", "UTC")],
        );
        let mut sink = RecordingSink { events: Vec::new() };

        for elapsed in [55 * 60 - 1, 55 * 60, 110 * 60 - 1, 110 * 60, 120 * 60] {
            session
                .execute_at(
                    "SELECT 1;",
                    &mut sink,
                    connected_at + Duration::from_secs(elapsed),
                )
                .await
                .expect("safe statement boundary");
        }

        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(connector.replacement_calls.load(Ordering::SeqCst), 4);
    }
}
