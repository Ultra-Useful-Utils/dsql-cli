use crate::{
    app::{
        BoxFuture, CancellationCapability, ConnectedSession, ConnectionIntent, ExecutionEvent,
        ExecutionSink, SessionCancellation, SessionConnector, SessionHandle, SessionLiveness,
        SessionMetadata, SessionSetting, TransactionState,
    },
    db::{
        auth::generate_auth_token,
        execute::{StreamEvent, emit_stream_events},
        tls::make_rustls_connect,
    },
    error::{
        ApplicationError, bounded_error_chain_text, dsql_connection_failure, dsql_database_failure,
        redact_diagnostic,
    },
};
use futures::{StreamExt, future::poll_fn, pin_mut};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};
use tokio_postgres::{
    AsyncMessage, Client, Config, Error,
    config::SslMode,
    types::{ToSql, Type},
};
use tokio_postgres_rustls::MakeRustlsConnect;

const REQUEST_QUEUE_DIAGNOSTIC: &str =
    "could not submit database statement to the connection request queue";
const CONNECTION_LOST_DIAGNOSTIC: &str = "database connection was lost after statement submission; statement outcome is unknown and was not replayed";
const CONNECTION_LOST_AFTER_COMPLETION_DIAGNOSTIC: &str = "database connection was lost after statement completion; statement outcome is known but the transaction cannot continue";
const CANCELLATION_UNAVAILABLE_DIAGNOSTIC: &str =
    "database connection is no longer available for cancellation";
const CANCELLATION_TIMEOUT_DIAGNOSTIC: &str = "database cancellation timed out";
const CANCELLATION_DIAGNOSTIC: &str = "could not cancel database statement";
const SESSION_SETTING_CAPTURE_DIAGNOSTIC: &str =
    "could not capture database session settings for reconnect";
const SESSION_SETTING_RESTORE_DIAGNOSTIC: &str =
    "could not safely restore database session settings";
// Bound queued notices so a slow terminal sink backpressures the connection
// driver rather than allowing unbounded diagnostic memory growth.
const DRIVER_EVENT_CHANNEL_CAPACITY: usize = 16;
const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(5);
const RESTORABLE_SESSION_SETTINGS: [&str; 16] = [
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
];

fn validate_restorable_settings(settings: &[SessionSetting]) -> Result<(), ApplicationError> {
    validate_session_setting_names(settings)?;
    if settings.len() != RESTORABLE_SESSION_SETTINGS.len() {
        return Err(ApplicationError::runtime(
            "cannot restore incomplete database session settings",
        ));
    }
    Ok(())
}

fn validate_session_setting_names(settings: &[SessionSetting]) -> Result<(), ApplicationError> {
    let mut seen = Vec::with_capacity(settings.len());
    for setting in settings {
        if !RESTORABLE_SESSION_SETTINGS.contains(&setting.name()) {
            return Err(ApplicationError::runtime(
                "cannot restore unsupported database session setting",
            ));
        }
        if seen.contains(&setting.name()) {
            return Err(ApplicationError::runtime(
                "cannot restore duplicate database session setting",
            ));
        }
        seen.push(setting.name());
    }
    Ok(())
}

async fn capture_session_settings(
    client: &Client,
) -> Result<Vec<SessionSetting>, ApplicationError> {
    let settings = capture_available_session_settings(client, &RESTORABLE_SESSION_SETTINGS).await?;
    validate_restorable_settings(&settings)
        .map_err(|_| ApplicationError::runtime(SESSION_SETTING_CAPTURE_DIAGNOSTIC))?;
    Ok(settings)
}

async fn capture_available_session_settings(
    client: &Client,
    names: &[&str],
) -> Result<Vec<SessionSetting>, ApplicationError> {
    let mut settings = Vec::with_capacity(names.len());
    for name in names {
        let row = client
            .query_one("SELECT current_setting($1, true)", &[&name])
            .await
            .map_err(|_| ApplicationError::runtime(SESSION_SETTING_CAPTURE_DIAGNOSTIC))?;
        let value = row
            .try_get::<_, Option<String>>(0)
            .map_err(|_| ApplicationError::runtime(SESSION_SETTING_CAPTURE_DIAGNOSTIC))?;
        if let Some(value) = value {
            settings.push(SessionSetting::new(*name, value));
        }
    }
    Ok(settings)
}

async fn restore_session_settings(
    client: &Client,
    settings: &[SessionSetting],
) -> Result<(), ApplicationError> {
    validate_restorable_settings(settings)?;
    restore_available_session_settings(client, settings).await
}

async fn restore_available_session_settings(
    client: &Client,
    settings: &[SessionSetting],
) -> Result<(), ApplicationError> {
    validate_session_setting_names(settings)?;
    for setting in settings {
        let row = client
            .query_one(
                "SELECT set_config($1, $2, false)",
                &[&setting.name(), &setting.value()],
            )
            .await
            .map_err(|_| ApplicationError::runtime(SESSION_SETTING_RESTORE_DIAGNOSTIC))?;
        let restored = row
            .try_get::<_, String>(0)
            .map_err(|_| ApplicationError::runtime(SESSION_SETTING_RESTORE_DIAGNOSTIC))?;
        if restored != setting.value() {
            return Err(ApplicationError::runtime(
                SESSION_SETTING_RESTORE_DIAGNOSTIC,
            ));
        }
    }
    Ok(())
}

/// Connects application intents to Aurora DSQL using the configured AWS identity.
#[derive(Clone)]
pub(crate) struct DsqlSessionConnector {
    sdk_config: aws_types::SdkConfig,
}

impl DsqlSessionConnector {
    pub(crate) fn new(sdk_config: aws_types::SdkConfig) -> Self {
        Self { sdk_config }
    }

    fn connect_with_settings<'a>(
        &'a self,
        intent: &'a ConnectionIntent,
        settings: Option<&'a [SessionSetting]>,
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        Box::pin(async move {
            let endpoint = intent.target().endpoint().ok_or_else(|| {
                ApplicationError::runtime(
                    "connection target does not include an Aurora DSQL endpoint",
                )
            })?;
            let token = generate_auth_token(&self.sdk_config, endpoint, intent.role()).await?;
            let tls = make_rustls_connect(intent.tls_roots())?;
            let mut config = Config::new();
            config
                .host(endpoint)
                .port(5432)
                .dbname("postgres")
                .user(intent.role().name())
                .password(token.as_str())
                .application_name(intent.application_name())
                .ssl_mode(SslMode::Require);
            connect_postgres(config, tls, intent, settings).await
        })
    }
}

impl SessionConnector for DsqlSessionConnector {
    fn connect<'a>(
        &'a self,
        intent: &'a ConnectionIntent,
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        self.connect_with_settings(intent, None)
    }

    fn connect_restoring<'a>(
        &'a self,
        intent: &'a ConnectionIntent,
        settings: &'a [SessionSetting],
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        self.connect_with_settings(intent, Some(settings))
    }
}

async fn connect_postgres(
    config: Config,
    tls: MakeRustlsConnect,
    intent: &ConnectionIntent,
    settings: Option<&[SessionSetting]>,
) -> Result<ConnectedSession, ApplicationError> {
    connect_postgres_inner(config, tls, intent, settings, true).await
}

#[cfg(test)]
async fn connect_postgres_without_session_settings(
    config: Config,
    tls: MakeRustlsConnect,
    intent: &ConnectionIntent,
) -> Result<ConnectedSession, ApplicationError> {
    connect_postgres_inner(config, tls, intent, None, false).await
}

async fn connect_postgres_inner(
    config: Config,
    tls: MakeRustlsConnect,
    intent: &ConnectionIntent,
    settings: Option<&[SessionSetting]>,
    capture_settings: bool,
) -> Result<ConnectedSession, ApplicationError> {
    let (client, connection) = config
        .connect(tls.clone())
        .await
        .map_err(|error| postgres_connection_failure(&error))?;
    let cancellation = Arc::new(DsqlSessionCancellation {
        token: client.cancel_token(),
        tls,
        alive: Arc::new(AtomicBool::new(true)),
    });
    let (driver_sender, driver_events) = mpsc::channel(DRIVER_EVENT_CHANNEL_CAPACITY);
    let driver = tokio::spawn(drive_connection(
        connection,
        driver_sender,
        cancellation.alive.clone(),
    ));
    if let Some(settings) = settings
        && let Err(error) = restore_session_settings(&client, settings).await
    {
        driver.abort();
        return Err(error);
    }
    let session_settings = if capture_settings {
        match capture_session_settings(&client).await {
            Ok(settings) => settings,
            Err(error) => {
                driver.abort();
                return Err(error);
            }
        }
    } else {
        Vec::new()
    };

    let metadata = SessionMetadata::new(
        intent.clone(),
        SystemTime::now(),
        CancellationCapability::Available,
        TransactionState::Idle,
        session_settings,
    );
    Ok(ConnectedSession::new(
        metadata,
        Box::new(DsqlSessionHandle {
            client,
            driver_events,
            cancellation,
            driver: Some(driver),
        }),
    ))
}

enum DriverEvent {
    Notice(String),
    Failed,
}

enum DrainPhase {
    BeforeSubmission,
    AfterCompletion,
}

struct DriverLiveness(Arc<AtomicBool>);

impl Drop for DriverLiveness {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn drive_connection<S>(
    mut connection: tokio_postgres::Connection<tokio_postgres::Socket, S>,
    sender: mpsc::Sender<DriverEvent>,
    alive: Arc<AtomicBool>,
) where
    S: tokio_postgres::tls::TlsStream + Unpin,
{
    let _liveness = DriverLiveness(alive);
    loop {
        match poll_fn(|context| connection.poll_message(context)).await {
            Some(Ok(AsyncMessage::Notice(notice))) => {
                if sender
                    .send(DriverEvent::Notice(redact_diagnostic(notice.message())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                let _ = sender.send(DriverEvent::Failed).await;
                return;
            }
        }
    }
}

struct DsqlSessionCancellation {
    token: tokio_postgres::CancelToken,
    tls: MakeRustlsConnect,
    alive: Arc<AtomicBool>,
}

impl SessionCancellation for DsqlSessionCancellation {
    fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>> {
        Box::pin(async move {
            if !cancellation_is_available(&self.alive) {
                return Err(ApplicationError::runtime(
                    CANCELLATION_UNAVAILABLE_DIAGNOSTIC,
                ));
            }
            match timeout(
                CANCELLATION_TIMEOUT,
                self.token.cancel_query(self.tls.clone()),
            )
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(ApplicationError::runtime(CANCELLATION_DIAGNOSTIC)),
                Err(_) => Err(ApplicationError::runtime(CANCELLATION_TIMEOUT_DIAGNOSTIC)),
            }
        })
    }
}

struct DsqlSessionHandle {
    client: Client,
    driver_events: mpsc::Receiver<DriverEvent>,
    cancellation: Arc<DsqlSessionCancellation>,
    driver: Option<JoinHandle<()>>,
}

impl Drop for DsqlSessionHandle {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

impl DsqlSessionHandle {
    fn drain_driver_events(
        &mut self,
        sink: &mut dyn ExecutionSink,
        phase: DrainPhase,
    ) -> Result<(), ApplicationError> {
        loop {
            match self.driver_events.try_recv() {
                Ok(DriverEvent::Notice(notice)) => sink.emit(ExecutionEvent::Notice(notice))?,
                Ok(DriverEvent::Failed) | Err(mpsc::error::TryRecvError::Disconnected) => {
                    return match phase {
                        DrainPhase::BeforeSubmission => forward_connection_failure(sink, false),
                        DrainPhase::AfterCompletion => forward_completed_connection_failure(sink),
                    };
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            }
        }
    }
}

impl SessionHandle for DsqlSessionHandle {
    fn execute<'a>(
        &'a mut self,
        statement: &'a str,
        sink: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>> {
        Box::pin(async move {
            // Drain bounded notice traffic before enqueueing the query so a
            // full idle-session channel cannot stall the connection driver.
            self.drain_driver_events(sink, DrainPhase::BeforeSubmission)?;
            let stream = match self.client.simple_query_raw(statement).await {
                Ok(stream) => stream,
                Err(_) => return forward_connection_failure(sink, false),
            };
            pin_mut!(stream);
            loop {
                tokio::select! {
                    biased;
                    message = stream.next() => match message {
                        Some(Ok(message)) => {
                            let event = match message {
                                tokio_postgres::SimpleQueryMessage::RowDescription(description) => {
                                    StreamEvent::Columns(description.iter().map(|column| column.name().to_owned()).collect())
                                }
                                tokio_postgres::SimpleQueryMessage::Row(row) => StreamEvent::Row(
                                    (0..row.len()).map(|index| row.get(index).map(str::to_owned)).collect(),
                                ),
                                tokio_postgres::SimpleQueryMessage::CommandComplete(rows) => StreamEvent::CommandComplete(rows),
                                _ => continue,
                            };
                            emit_stream_events(std::iter::once(event), sink)?;
                        }
                        Some(Err(error)) => return self.forward_query_error(error, sink, false),
                        None => {
                            return self.drain_driver_events(sink, DrainPhase::AfterCompletion);
                        }
                    },
                    driver_event = self.driver_events.recv() => match driver_event {
                        Some(DriverEvent::Notice(notice)) => sink.emit(ExecutionEvent::Notice(notice))?,
                        Some(DriverEvent::Failed) | None => {
                            return forward_stream_connection_failure(sink);
                        }
                    },
                }
            }
        })
    }

    fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        Some(self.cancellation.clone())
    }

    fn liveness(&self) -> SessionLiveness {
        if cancellation_is_available(&self.cancellation.alive) {
            SessionLiveness::Alive
        } else {
            SessionLiveness::Lost
        }
    }

    fn capture_session_settings(
        &self,
    ) -> BoxFuture<'_, Result<Vec<SessionSetting>, ApplicationError>> {
        Box::pin(capture_session_settings(&self.client))
    }

    fn execute_params<'a>(
        &'a mut self,
        statement: &'a str,
        params: &'a [String],
        sink: &'a mut dyn ExecutionSink,
    ) -> BoxFuture<'a, Result<(), ApplicationError>> {
        Box::pin(async move {
            self.drain_driver_events(sink, DrainPhase::BeforeSubmission)?;
            let parameter_types = vec![Type::TEXT; params.len()];
            let description = match self.client.prepare_typed(statement, &parameter_types).await {
                Ok(description) => description,
                Err(error) => return self.forward_query_error(error, sink, true),
            };
            let stream = match self
                .client
                .query_raw(
                    &description,
                    params
                        .iter()
                        .map(|parameter| parameter as &(dyn ToSql + Sync)),
                )
                .await
            {
                Ok(stream) => stream,
                Err(error) => return self.forward_query_error(error, sink, true),
            };
            let columns = description
                .columns()
                .iter()
                .map(|column| column.name().to_owned())
                .collect();
            emit_stream_events(std::iter::once(StreamEvent::Columns(columns)), sink)?;
            pin_mut!(stream);
            let mut rows = 0;
            loop {
                tokio::select! {
                    biased;
                    message = stream.next() => match message {
                        Some(Ok(row)) => {
                            let values = (0..row.len())
                                .map(|index| row.try_get::<_, Option<String>>(index).map_err(|_| ApplicationError::runtime("could not decode catalog result")))
                                .collect::<Result<Vec<_>, _>>()?;
                            rows += 1;
                            emit_stream_events(std::iter::once(StreamEvent::Row(values)), sink)?;
                        }
                        Some(Err(error)) => return self.forward_query_error(error, sink, true),
                        None => {
                            emit_stream_events(std::iter::once(StreamEvent::CommandComplete(rows)), sink)?;
                            return self.drain_driver_events(sink, DrainPhase::AfterCompletion);
                        }
                    },
                    driver_event = self.driver_events.recv() => match driver_event {
                        Some(DriverEvent::Notice(notice)) => sink.emit(ExecutionEvent::Notice(notice))?,
                        Some(DriverEvent::Failed) | None => {
                            return forward_connection_failure(sink, true);
                        }
                    },
                }
            }
        })
    }
}

impl DsqlSessionHandle {
    fn forward_query_error(
        &self,
        error: Error,
        sink: &mut dyn ExecutionSink,
        metadata: bool,
    ) -> Result<(), ApplicationError> {
        let Some(database_error) = error.as_db_error() else {
            return forward_connection_failure(sink, true);
        };
        let sqlstate = database_error.code().code().to_owned();
        let diagnostic = redact_diagnostic(database_error.message());
        let failure = if metadata {
            metadata_failure(&sqlstate, &diagnostic)
        } else {
            database_failure(&sqlstate, &diagnostic)
        };
        sink.emit(database_error_event(&sqlstate, diagnostic))?;
        Err(failure)
    }
}

fn cancellation_is_available(alive: &AtomicBool) -> bool {
    alive.load(Ordering::Acquire)
}

fn query_failure_diagnostic(submitted: bool) -> &'static str {
    if submitted {
        CONNECTION_LOST_DIAGNOSTIC
    } else {
        REQUEST_QUEUE_DIAGNOSTIC
    }
}

fn forward_connection_failure(
    sink: &mut dyn ExecutionSink,
    submitted: bool,
) -> Result<(), ApplicationError> {
    let diagnostic = query_failure_diagnostic(submitted);
    sink.emit(ExecutionEvent::Error {
        sqlstate: None,
        diagnostic: diagnostic.to_owned(),
    })?;
    Err(ApplicationError::runtime(diagnostic))
}

fn forward_completed_connection_failure(
    sink: &mut dyn ExecutionSink,
) -> Result<(), ApplicationError> {
    sink.emit(ExecutionEvent::Error {
        sqlstate: None,
        diagnostic: CONNECTION_LOST_AFTER_COMPLETION_DIAGNOSTIC.to_owned(),
    })?;
    Err(ApplicationError::runtime(
        CONNECTION_LOST_AFTER_COMPLETION_DIAGNOSTIC,
    ))
}

fn forward_stream_connection_failure(sink: &mut dyn ExecutionSink) -> Result<(), ApplicationError> {
    forward_connection_failure(sink, true)
}

fn postgres_connection_failure(error: &Error) -> ApplicationError {
    if let Some(database_error) = error.as_db_error() {
        return dsql_connection_failure(
            Some(database_error.code().code()),
            database_error.message(),
        );
    }

    let diagnostic = bounded_error_chain_text(error);
    dsql_connection_failure(None, &diagnostic)
}

fn database_error_event(sqlstate: &str, diagnostic: String) -> ExecutionEvent {
    ExecutionEvent::Error {
        sqlstate: Some(sqlstate.to_owned()),
        diagnostic: redact_diagnostic(&diagnostic),
    }
}

fn database_failure(sqlstate: &str, diagnostic: &str) -> ApplicationError {
    dsql_database_failure(sqlstate, diagnostic)
}

fn metadata_failure(sqlstate: &str, diagnostic: &str) -> ApplicationError {
    if matches!(sqlstate, "42501" | "42P01" | "42883") {
        ApplicationError::runtime(format!(
            "metadata is unavailable for this database role ({sqlstate}); catalog access may be restricted"
        ))
    } else {
        database_failure(sqlstate, diagnostic)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONNECTION_LOST_DIAGNOSTIC, DriverLiveness, REQUEST_QUEUE_DIAGNOSTIC,
        RESTORABLE_SESSION_SETTINGS, cancellation_is_available, database_error_event,
        database_failure, forward_completed_connection_failure, forward_connection_failure,
        forward_stream_connection_failure, metadata_failure, query_failure_diagnostic,
        validate_restorable_settings,
    };
    use crate::{
        app::{ExecutionEvent, ExecutionSink, SessionSetting},
        error::ApplicationError,
    };
    use std::sync::{Arc, atomic::AtomicBool};

    #[test]
    fn restorable_session_settings_match_the_public_contract() {
        assert_eq!(
            RESTORABLE_SESSION_SETTINGS,
            [
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
        );
        assert!(!RESTORABLE_SESSION_SETTINGS.contains(&"role"));
    }

    #[test]
    fn restoration_rejects_settings_outside_the_allowlist() {
        let error = validate_restorable_settings(&[SessionSetting::new("role", "admin")])
            .expect_err("role must not be restored");

        assert_eq!(
            error.to_string(),
            "cannot restore unsupported database session setting"
        );
    }

    #[test]
    fn restoration_rejects_duplicate_settings() {
        let error = validate_restorable_settings(&[
            SessionSetting::new("timezone", "UTC"),
            SessionSetting::new("timezone", "Europe/London"),
        ])
        .expect_err("duplicate setting must not be restored ambiguously");

        assert_eq!(
            error.to_string(),
            "cannot restore duplicate database session setting"
        );
    }

    #[test]
    fn restoration_rejects_incomplete_settings() {
        let error = validate_restorable_settings(&[SessionSetting::new("timezone", "UTC")])
            .expect_err("partial restoration must fail closed");

        assert_eq!(
            error.to_string(),
            "cannot restore incomplete database session settings"
        );
    }

    #[test]
    fn database_errors_keep_a_sanitized_diagnostic_in_the_event_only() {
        let event = database_error_event(
            "42601",
            "syntax error at \\u{001b}[2J token=secret-token".into(),
        );
        assert_eq!(
            event,
            ExecutionEvent::Error {
                sqlstate: Some("42601".into()),
                diagnostic: "syntax error at \\u{001b}[2J token=[REDACTED]".into(),
            }
        );
        assert_eq!(
            database_failure("42601", "syntax error").to_string(),
            "database statement failed (42601)"
        );
    }

    #[test]
    fn dsql_serialization_errors_include_explicit_retry_guidance() {
        assert_eq!(
            database_failure("40001", "change conflicts with another transaction (OC000)")
                .to_string(),
            "database statement failed (40001, OC000); transaction conflicted with another transaction; retry the transaction explicitly"
        );
    }

    #[test]
    fn connection_loss_emits_a_final_structured_error_with_unknown_outcome_wording() {
        let mut sink = RecordingSink::default();

        let error = forward_connection_failure(&mut sink, true)
            .expect_err("connection loss must fail execution");

        assert_eq!(error.to_string(), CONNECTION_LOST_DIAGNOSTIC);
        assert_eq!(
            sink.0,
            vec![ExecutionEvent::Error {
                sqlstate: None,
                diagnostic: CONNECTION_LOST_DIAGNOSTIC.into(),
            }]
        );
    }

    #[test]
    fn catalog_permission_errors_get_metadata_guidance_without_changing_sql_errors() {
        assert!(
            metadata_failure("42501", "permission denied")
                .to_string()
                .contains("metadata is unavailable")
        );
        assert!(
            metadata_failure("42P01", "relation does not exist")
                .to_string()
                .contains("catalog access may be restricted")
        );
        assert_eq!(
            metadata_failure("42601", "syntax error").to_string(),
            database_failure("42601", "syntax error").to_string()
        );
    }

    #[test]
    fn request_queue_failures_do_not_claim_an_unknown_statement_outcome() {
        assert_eq!(query_failure_diagnostic(false), REQUEST_QUEUE_DIAGNOSTIC);
        assert_eq!(query_failure_diagnostic(true), CONNECTION_LOST_DIAGNOSTIC);
        assert!(!query_failure_diagnostic(false).contains("outcome is unknown"));
    }

    #[test]
    fn connection_loss_after_completion_does_not_claim_submission_or_unknown_outcome() {
        let mut sink = RecordingSink::default();

        let error = forward_completed_connection_failure(&mut sink)
            .expect_err("lost completed session must require reconnect");

        assert!(error.to_string().contains("after statement completion"));
        assert!(!error.to_string().contains("could not submit"));
        assert!(!error.to_string().contains("outcome is unknown"));
        assert!(matches!(sink.0.as_slice(), [ExecutionEvent::Error { .. }]));
    }

    #[test]
    fn connection_loss_before_the_stream_ends_always_has_an_unknown_outcome() {
        let mut sink = RecordingSink::default();
        let error = forward_stream_connection_failure(&mut sink)
            .expect_err("lost in-progress stream fails");

        assert!(error.to_string().contains("outcome is unknown"));
    }

    #[test]
    fn cancellation_availability_tracks_the_driver_lifecycle() {
        let alive = Arc::new(AtomicBool::new(true));
        {
            let _driver_liveness = DriverLiveness(alive.clone());
            assert!(cancellation_is_available(&alive));
        }
        assert!(!cancellation_is_available(&alive));
    }

    #[derive(Default)]
    struct RecordingSink(Vec<ExecutionEvent>);

    impl ExecutionSink for RecordingSink {
        fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
            self.0.push(event);
            Ok(())
        }
    }

    mod local_postgres_protocol {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/integration/local_postgres_protocol.rs"
        ));
    }
}
