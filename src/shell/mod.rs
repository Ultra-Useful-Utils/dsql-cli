mod commands;
mod completion;
mod editor;
mod prompt;

use crate::{
    app::{
        ExecutionEvent, ExecutionSink, ManagedSession, MetricsProvider, ResolvedAwsContext,
        TransactionState,
    },
    dashboard,
    error::ApplicationError,
    output::{
        expanded::ExpandedExecutionSink,
        pager::{OptionalPager, PagerCommand},
        table::TableExecutionSink,
        timing::TimingExecutionSink,
    },
    sql::{
        metadata::{MetadataQuery, load_managed_snapshot},
        scanner::{MAX_STATEMENT_BYTES, StatementStream, TransactionControl},
    },
};
use commands::{
    CommandAction, ExpandedMode, MetadataRequest, RefreshState, ShellCommandState, ShellSettings,
    execute_with_reconnect_state as execute_meta_command,
};
use completion::SharedCompletionSnapshot;
use editor::build_editor;
use prompt::ShellPrompt;
use reedline::Signal;
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub(crate) async fn run(
    session: &mut ManagedSession<'_>,
    aws_context: &ResolvedAwsContext,
    metrics: &dyn MetricsProvider,
    no_history: bool,
    history_file: Option<PathBuf>,
) -> Result<(), ApplicationError> {
    let _terminal_guard = TerminalGuard;
    let snapshot: SharedCompletionSnapshot =
        Arc::new(RwLock::new(crate::app::MetadataSnapshot::empty()));
    let mut editor = build_editor(no_history, history_file, snapshot.clone());
    let metadata = session.metadata();
    let cluster_id = metadata.intent().target().id().as_str().to_owned();
    let database_role = metadata.intent().role().name().to_owned();
    let mut command_state = ShellCommandState::default();
    let mut initial_metadata_load = true;
    let mut refresh_hint_shown = false;
    let mut termination = TerminationMonitor::new()?;

    loop {
        let prompt = ShellPrompt::new(&cluster_id, &database_role, session.state());
        let (read_sender, mut read) = oneshot::channel();
        std::thread::Builder::new()
            .name("dsql-line-editor".into())
            .spawn(move || {
                let signal = editor.read_line(&prompt);
                let _ = read_sender.send((editor, signal));
            })
            .map_err(|_| ApplicationError::runtime("could not start interactive shell editor"))?;
        if initial_metadata_load {
            // Reedline is already accepting input while this uses the one shared
            // connection. The completed load is installed before submitted SQL
            // can run, so session work never overlaps.
            let loaded = tokio::select! {
                loaded = load_managed_snapshot(session) => loaded,
                _ = termination.wait() => return Err(termination_error()),
            };
            *snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = loaded;
            announce_reconnect(session);
            initial_metadata_load = false;
        }
        let (next_editor, signal) = tokio::select! {
            result = &mut read => result.map_err(|_| ApplicationError::runtime("interactive shell editor stopped unexpectedly"))?,
            _ = termination.wait() => return Err(termination_error()),
        };
        editor = next_editor;
        if editor.sync_history().is_err() {
            eprintln!("warning: interactive shell history could not be written");
        }
        match signal {
            Ok(Signal::Success(input)) => {
                if input.trim().is_empty() {
                    continue;
                }
                if let Err(error) = ensure_submission_size(&input, MAX_STATEMENT_BYTES) {
                    eprintln!("error: {error}");
                    continue;
                }
                let now = std::time::SystemTime::now();
                match execute_meta_command(
                    &input,
                    false,
                    session.metadata(),
                    session.reconnect_state(now),
                    &mut command_state,
                    now,
                ) {
                    Ok(Some(result)) => {
                        if result.action == CommandAction::Dashboard {
                            let target = session.metadata().intent().target();
                            let cluster_id = target.id().as_str();
                            let dashboard = dashboard::events::run_in_shell(cluster_id, |range| {
                                metrics.snapshot(aws_context, target, range)
                            });
                            tokio::pin!(dashboard);
                            let result = tokio::select! {
                                biased;
                                _ = termination.wait() => return Err(termination_error()),
                                result = &mut dashboard => result,
                            };
                            if let Err(error) = result {
                                eprintln!("error: {error}");
                            }
                            continue;
                        }
                        if command_state.refresh == RefreshState::Requested {
                            command_state.refresh = RefreshState::NotRequested;
                            if !refresh_allowed(session.state()) {
                                eprintln!(
                                    "error: \\refresh is unavailable while a transaction is active, failed, or uncertain"
                                );
                                continue;
                            }
                            let loaded = load_managed_snapshot(session).await;
                            let stale = loaded.stale();
                            *snapshot
                                .write()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = loaded;
                            refresh_hint_shown = false;
                            announce_reconnect(session);
                            if stale {
                                println!(
                                    "completion metadata refreshed with unavailable catalog entries"
                                );
                            } else {
                                println!("completion metadata refreshed");
                            }
                            continue;
                        }
                        if let Some(request) = result.metadata {
                            let (query, pattern) = metadata_query(&request);
                            let params = query.params(pattern);
                            let mut sink = interactive_sink(command_state.settings);
                            let (monitor, mut signals) = query_signal_monitor()?;
                            let execution = execute_params_with_signals(
                                session,
                                query.sql(),
                                &params,
                                sink.as_mut(),
                                &mut signals,
                            )
                            .await;
                            monitor.abort();
                            announce_reconnect(session);
                            match execution {
                                Ok(result) if result.cancellation_failed => {
                                    session.mark_uncertain();
                                }
                                Ok(result) => {
                                    if let Err(error) = result.result {
                                        eprintln!("error: {error}");
                                    }
                                }
                                Err(error) => return Err(error),
                            }
                            continue;
                        }
                        if !result.message.is_empty() {
                            println!("{}", result.message);
                        }
                        if result.action == CommandAction::Exit {
                            return Ok(());
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("error: {error}");
                        continue;
                    }
                }
                let submissions = match frame_submission(&input) {
                    Ok(submissions) => submissions,
                    Err(error) => {
                        eprintln!("error: {error}");
                        continue;
                    }
                };
                for (statement, _) in submissions {
                    let mut sink = interactive_sink(command_state.settings);
                    let (monitor, mut signals) = query_signal_monitor()?;
                    let execution = execute_statement_with_signals(
                        session,
                        &statement,
                        sink.as_mut(),
                        &mut signals,
                    )
                    .await;
                    monitor.abort();

                    match execution {
                        Ok(result) => {
                            if result.cancellation_failed {
                                session.mark_uncertain();
                                break;
                            } else if let Err(error) = result.result {
                                eprintln!("error: {error}");
                                break;
                            } else if invalidate_after_schema_change(
                                &snapshot,
                                &statement,
                                true,
                                &mut refresh_hint_shown,
                            ) {
                                eprintln!("notice: completion metadata is stale; use \\refresh");
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                announce_reconnect(session);
            }
            Ok(Signal::CtrlC) => {}
            Ok(Signal::CtrlD) => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                return Err(ApplicationError::runtime(
                    "could not read interactive shell input",
                ));
            }
        }
    }
}

fn announce_reconnect(session: &mut ManagedSession<'_>) {
    if session.take_reconnected() {
        eprintln!("notice: reconnected to Aurora DSQL cluster");
    }
}

struct TerminationMonitor {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl TerminationMonitor {
    fn new() -> Result<Self, ApplicationError> {
        #[cfg(unix)]
        {
            let terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(|_| ApplicationError::runtime("could not monitor SIGTERM"))?;
            let hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .map_err(|_| ApplicationError::runtime("could not monitor SIGHUP"))?;
            Ok(Self { terminate, hangup })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn wait(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.terminate.recv() => {}
                _ = self.hangup.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn termination_error() -> ApplicationError {
    ApplicationError::runtime("terminated while the interactive shell was active")
}

fn metadata_query(request: &MetadataRequest) -> (MetadataQuery, Option<&str>) {
    match request {
        MetadataRequest::Relations(pattern) => (MetadataQuery::Relations, pattern.as_deref()),
        MetadataRequest::Tables(pattern) => (MetadataQuery::Tables, pattern.as_deref()),
        MetadataRequest::Schemas(pattern) => (MetadataQuery::Schemas, pattern.as_deref()),
        MetadataRequest::Roles => (MetadataQuery::Roles, None),
    }
}

enum BaseInteractiveSink {
    Table(TableExecutionSink<OptionalPager<io::Stdout>, io::Stderr>),
    Expanded(ExpandedExecutionSink<OptionalPager<io::Stdout>, io::Stderr>),
}

impl ExecutionSink for BaseInteractiveSink {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        match self {
            Self::Table(sink) => sink.emit(event),
            Self::Expanded(sink) => sink.emit(event),
        }
    }
}

fn interactive_sink(settings: ShellSettings) -> Box<dyn ExecutionSink> {
    let display_width = crossterm::terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(80);
    let mut output = OptionalPager::new(io::stdout());
    if settings.pager {
        output.start(Some(&PagerCommand::new("less", ["-FRX"])));
    }
    let expanded = match settings.expanded {
        ExpandedMode::On => true,
        ExpandedMode::Off => false,
        ExpandedMode::Auto => display_width < 80,
    };
    let sink = if expanded {
        BaseInteractiveSink::Expanded(ExpandedExecutionSink::new(output, io::stderr()))
    } else {
        BaseInteractiveSink::Table(TableExecutionSink::new(output, io::stderr(), display_width))
    };
    if settings.timing {
        Box::new(TimingExecutionSink::new(sink, io::stderr()))
    } else {
        Box::new(sink)
    }
}

fn frame_submission(input: &str) -> Result<Vec<(String, TransactionControl)>, ApplicationError> {
    frame_submission_with_limit(input, MAX_STATEMENT_BYTES)
}

fn frame_submission_with_limit(
    input: &str,
    max_statement_bytes: usize,
) -> Result<Vec<(String, TransactionControl)>, ApplicationError> {
    ensure_submission_size(input, max_statement_bytes)?;
    let mut stream = StatementStream::new();
    let statements = stream
        .push_bounded(input, max_statement_bytes)
        .map_err(|()| {
            ApplicationError::usage(format!(
                "interactive input contains a SQL statement larger than {} MiB",
                MAX_STATEMENT_BYTES / (1024 * 1024)
            ))
        })?
        .into_iter()
        .map(|statement| {
            let control = statement.transaction_control();
            (statement.into_text(), control)
        })
        .collect();
    Ok(statements)
}

fn ensure_submission_size(input: &str, max_input_bytes: usize) -> Result<(), ApplicationError> {
    if input.len() > max_input_bytes {
        return Err(ApplicationError::usage(format!(
            "interactive input is larger than {} MiB",
            MAX_STATEMENT_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum QuerySignal {
    Interrupt,
    Terminate,
}

struct StatementExecution {
    result: Result<(), ApplicationError>,
    cancellation_failed: bool,
}

struct InterruptibleExecutionSink<'a> {
    sink: &'a mut dyn ExecutionSink,
    interrupted: Arc<AtomicBool>,
}

impl ExecutionSink for InterruptibleExecutionSink<'_> {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if self.interrupted.load(Ordering::Acquire) {
            Ok(())
        } else {
            self.sink.emit(event)
        }
    }
}

async fn execute_statement_with_signals(
    session: &mut ManagedSession<'_>,
    statement: &str,
    sink: &mut dyn crate::app::ExecutionSink,
    signals: &mut mpsc::Receiver<QuerySignal>,
) -> Result<StatementExecution, ApplicationError> {
    execute_with_signals(session, Execution::Statement(statement), sink, signals).await
}

async fn execute_params_with_signals(
    session: &mut ManagedSession<'_>,
    statement: &str,
    params: &[String],
    sink: &mut dyn crate::app::ExecutionSink,
    signals: &mut mpsc::Receiver<QuerySignal>,
) -> Result<StatementExecution, ApplicationError> {
    execute_with_signals(
        session,
        Execution::Parameters { statement, params },
        sink,
        signals,
    )
    .await
}

enum Execution<'a> {
    Statement(&'a str),
    Parameters {
        statement: &'a str,
        params: &'a [String],
    },
}

async fn execute_with_signals(
    session: &mut ManagedSession<'_>,
    execution: Execution<'_>,
    sink: &mut dyn crate::app::ExecutionSink,
    signals: &mut mpsc::Receiver<QuerySignal>,
) -> Result<StatementExecution, ApplicationError> {
    let outcome = {
        let cancellation = session.cancellation_handle();
        let output_interrupted = Arc::new(AtomicBool::new(false));
        let mut sink = InterruptibleExecutionSink {
            sink,
            interrupted: output_interrupted.clone(),
        };
        let execution: crate::app::BoxFuture<'_, Result<(), ApplicationError>> = match execution {
            Execution::Statement(statement) => Box::pin(session.execute(statement, &mut sink)),
            Execution::Parameters { statement, params } => {
                Box::pin(session.execute_params(statement, params, &mut sink))
            }
        };
        tokio::pin!(execution);
        let mut interrupted = false;
        let mut cancellation_failed = false;

        'coordinate: loop {
            tokio::select! {
                result = &mut execution => break Ok(StatementExecution { result, cancellation_failed }),
                signal = signals.recv() => match signal {
                    Some(QuerySignal::Terminate) => {
                        break Err(ApplicationError::runtime("terminated during database statement; statement outcome may be unknown and was not replayed"));
                    }
                    Some(QuerySignal::Interrupt) if interrupted => {
                        break Err(ApplicationError::runtime("database statement is still running after cancellation; interrupt again ended the interactive shell without replaying the statement"));
                    }
                    Some(QuerySignal::Interrupt) => {
                        interrupted = true;
                        output_interrupted.store(true, Ordering::Release);
                        match cancellation.as_ref() {
                            Some(handle) => {
                                tokio::select! {
                                    biased;
                                    result = handle.cancel() => {
                                        if let Err(error) = result {
                                            cancellation_failed = true;
                                            eprintln!("error: {error}");
                                        }
                                    }
                                    signal = signals.recv() => match signal {
                                        Some(QuerySignal::Terminate) => break 'coordinate Err(ApplicationError::runtime("terminated during database statement; statement outcome may be unknown and was not replayed")),
                                        Some(QuerySignal::Interrupt) => break 'coordinate Err(ApplicationError::runtime("database statement is still running after cancellation; interrupt again ended the interactive shell without replaying the statement")),
                                        None => {
                                            cancellation_failed = true;
                                            break 'coordinate Ok(StatementExecution {
                                                result: Err(ApplicationError::runtime("interactive shell signal monitor stopped unexpectedly")),
                                                cancellation_failed,
                                            });
                                        }
                                    },
                                }
                            }
                            None => {
                                cancellation_failed = true;
                                eprintln!("error: database connection is unavailable for cancellation");
                                break Ok(StatementExecution {
                                    result: Err(ApplicationError::runtime("database connection is unavailable for cancellation")),
                                    cancellation_failed,
                                });
                            }
                        }
                    }
                    None => break Err(ApplicationError::runtime("interactive shell signal monitor stopped unexpectedly")),
                },
            }
        }
    };
    if match &outcome {
        Ok(execution) => execution.cancellation_failed,
        Err(_) => true,
    } {
        session.mark_uncertain();
    }
    outcome
}

fn query_signal_monitor() -> Result<(JoinHandle<()>, mpsc::Receiver<QuerySignal>), ApplicationError>
{
    let (sender, receiver) = mpsc::channel(2);
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|_| {
        ApplicationError::runtime("could not monitor Ctrl-C during database statement")
    })?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| {
        ApplicationError::runtime("could not monitor SIGTERM during database statement")
    })?;
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|_| {
            ApplicationError::runtime("could not monitor SIGHUP during database statement")
        })?;
    let monitor = tokio::spawn(async move {
        loop {
            #[cfg(unix)]
            let signal = tokio::select! {
                _ = interrupt.recv() => QuerySignal::Interrupt,
                _ = terminate.recv() => QuerySignal::Terminate,
                _ = hangup.recv() => QuerySignal::Terminate,
            };
            #[cfg(not(unix))]
            let signal = match tokio::signal::ctrl_c().await {
                Ok(()) => QuerySignal::Interrupt,
                Err(_) => return,
            };

            if sender.send(signal).await.is_err() || matches!(signal, QuerySignal::Terminate) {
                return;
            }
        }
    });
    Ok((monitor, receiver))
}

fn refresh_allowed(state: TransactionState) -> bool {
    state == TransactionState::Idle
}

pub(crate) fn invalidate_after_schema_change(
    snapshot: &SharedCompletionSnapshot,
    statement: &str,
    succeeded: bool,
    hint_shown: &mut bool,
) -> bool {
    if !succeeded || !crate::sql::metadata::is_schema_changing(statement) {
        return false;
    }
    snapshot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .invalidate();
    if *hint_shown {
        false
    } else {
        *hint_shown = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QuerySignal, ensure_submission_size, execute_statement_with_signals, frame_submission,
        frame_submission_with_limit, invalidate_after_schema_change, refresh_allowed,
    };
    #[cfg(unix)]
    use crate::pty_test_lock;
    use crate::{
        app::{
            BoxFuture, CancellationCapability, ClusterTarget, ConnectedSession, ConnectionIntent,
            DatabaseRole, ExecutionEvent, ExecutionSink, ManagedSession, MetricsProvider,
            ResolvedAwsContext, SessionCancellation, SessionConnector, SessionHandle,
            SessionMetadata, TransactionState,
        },
        error::ApplicationError,
        sql::scanner::TransactionControl,
    };
    #[cfg(unix)]
    use expectrl::{Any, ControlCode, Eof, Expect, Session, process::unix::Signal};
    #[cfg(unix)]
    use std::{env, fs, process::Command};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::SystemTime,
    };
    use tokio::sync::{Notify, mpsc};

    struct NoReconnect;
    impl SessionConnector for NoReconnect {
        fn connect<'a>(
            &'a self,
            _: &'a ConnectionIntent,
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async { Err(ApplicationError::runtime("unexpected reconnect")) })
        }
    }

    #[test]
    fn transaction_control_updates_prompt_state_without_parsing_other_sql() {
        assert_eq!(
            crate::app::transition_transaction_state(
                TransactionState::Idle,
                TransactionControl::Begin
            ),
            TransactionState::Active
        );
        assert_eq!(
            crate::app::transition_transaction_state(
                TransactionState::Failed,
                TransactionControl::Rollback
            ),
            TransactionState::Idle
        );
        assert_eq!(
            crate::app::transition_transaction_state(
                TransactionState::Uncertain,
                TransactionControl::Commit
            ),
            TransactionState::Uncertain
        );
        assert_eq!(
            crate::app::transition_transaction_state(
                TransactionState::Active,
                TransactionControl::Other
            ),
            TransactionState::Active
        );
    }

    #[test]
    fn interactive_submission_enforces_the_statement_size_limit() {
        assert!(frame_submission_with_limit("SELECT 1;", 16).is_ok());
        assert!(frame_submission_with_limit("SELECT 123456789;", 8).is_err());
        assert!(frame_submission_with_limit("A;B;C;D;", 8).is_ok());
        assert!(frame_submission_with_limit("A;B;C;D;E;", 8).is_err());
        assert!(ensure_submission_size("\\dt abc", 8).is_ok());
        assert!(ensure_submission_size("\\dt abcdef", 8).is_err());
    }

    #[test]
    fn refresh_is_only_allowed_outside_a_transaction() {
        assert!(refresh_allowed(TransactionState::Idle));
        assert!(!refresh_allowed(TransactionState::Active));
        assert!(!refresh_allowed(TransactionState::Failed));
        assert!(!refresh_allowed(TransactionState::Uncertain));
    }

    #[test]
    fn submitted_buffer_is_framed_and_state_advances_for_each_statement() {
        let statements = frame_submission("BEGIN; SELECT 1; COMMIT;").expect("statements frame");
        let mut state = TransactionState::Idle;
        let mut submitted = Vec::new();

        for (statement, control) in statements {
            submitted.push(statement);
            state = crate::app::transition_transaction_state(state, control);
        }

        assert_eq!(submitted, ["BEGIN;", " SELECT 1;", " COMMIT;"]);
        assert_eq!(state, TransactionState::Idle);
    }

    #[test]
    fn successful_schema_changes_invalidate_completion_and_hint_once() {
        let snapshot = Arc::new(std::sync::RwLock::new(crate::app::MetadataSnapshot::new(
            vec!["public".into()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some(SystemTime::UNIX_EPOCH),
            false,
        )));
        let mut hint_shown = false;

        assert!(invalidate_after_schema_change(
            &snapshot,
            "CREATE TABLE orders (id bigint);",
            true,
            &mut hint_shown,
        ));
        assert!(!invalidate_after_schema_change(
            &snapshot,
            "ALTER TABLE orders ADD COLUMN total bigint;",
            true,
            &mut hint_shown,
        ));
        assert!(!invalidate_after_schema_change(
            &snapshot,
            "DROP TABLE orders;",
            false,
            &mut hint_shown,
        ));
        assert!(snapshot.read().expect("snapshot lock").stale());
        assert_eq!(snapshot.read().expect("snapshot lock").loaded_at(), None);
    }

    struct WaitingHandle {
        calls: Arc<AtomicUsize>,
        canceled: Arc<Notify>,
        cancellation: Arc<dyn SessionCancellation>,
    }

    impl SessionHandle for WaitingHandle {
        fn execute<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.canceled.notified().await;
                Err(ApplicationError::runtime("statement canceled"))
            })
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            Some(self.cancellation.clone())
        }
    }

    struct NotifyCancellation {
        calls: AtomicUsize,
        canceled: Arc<Notify>,
    }

    impl SessionCancellation for NotifyCancellation {
        fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.canceled.notify_one();
                Ok(())
            })
        }
    }

    struct Sink;

    impl ExecutionSink for Sink {
        fn emit(&mut self, _: ExecutionEvent) -> Result<(), ApplicationError> {
            Ok(())
        }
    }

    struct PendingHandle {
        cancellation: Arc<dyn SessionCancellation>,
    }

    impl SessionHandle for PendingHandle {
        fn execute<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(std::future::pending())
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            Some(self.cancellation.clone())
        }
    }

    struct UnavailableCancellationHandle;

    impl SessionHandle for UnavailableCancellationHandle {
        fn execute<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(std::future::pending())
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            None
        }
    }

    #[tokio::test]
    async fn first_interrupt_cancels_and_awaits_the_original_statement_without_replay() {
        let canceled = Arc::new(Notify::new());
        let cancellation = Arc::new(NotifyCancellation {
            calls: AtomicUsize::new(0),
            canceled: canceled.clone(),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let intent = ConnectionIntent::new(
            ClusterTarget::new("cluster-1", "us-east-1", None),
            DatabaseRole::Custom("app_user".into()),
            Vec::new(),
            "dsql test",
        );
        let metadata = SessionMetadata::new(
            intent,
            SystemTime::now(),
            CancellationCapability::Available,
            TransactionState::Idle,
            Vec::new(),
        );
        let session = ConnectedSession::new(
            metadata,
            Box::new(WaitingHandle {
                calls: calls.clone(),
                canceled,
                cancellation: cancellation.clone(),
            }),
        );
        let connector = NoReconnect;
        let mut session = ManagedSession::new(session, &connector, SystemTime::now());
        let (sender, mut receiver) = mpsc::channel(2);
        sender
            .send(QuerySignal::Interrupt)
            .await
            .expect("signal send");
        let mut sink = Sink;

        let result = execute_statement_with_signals(
            &mut session,
            "SELECT pg_sleep(30);",
            &mut sink,
            &mut receiver,
        )
        .await
        .expect("execution coordination");

        assert_eq!(cancellation.calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(result.result.is_err());
        assert!(!result.cancellation_failed);
    }

    #[tokio::test]
    async fn repeated_interrupt_ends_a_statement_that_did_not_stop_after_cancellation() {
        let cancellation = Arc::new(NotifyCancellation {
            calls: AtomicUsize::new(0),
            canceled: Arc::new(Notify::new()),
        });
        let intent = ConnectionIntent::new(
            ClusterTarget::new("cluster-1", "us-east-1", None),
            DatabaseRole::Custom("app_user".into()),
            Vec::new(),
            "dsql test",
        );
        let metadata = SessionMetadata::new(
            intent,
            SystemTime::now(),
            CancellationCapability::Available,
            TransactionState::Idle,
            Vec::new(),
        );
        let session = ConnectedSession::new(
            metadata,
            Box::new(PendingHandle {
                cancellation: cancellation.clone(),
            }),
        );
        let connector = NoReconnect;
        let mut session = ManagedSession::new(session, &connector, SystemTime::now());
        let (sender, mut receiver) = mpsc::channel(2);
        sender
            .send(QuerySignal::Interrupt)
            .await
            .expect("first signal send");
        sender
            .send(QuerySignal::Interrupt)
            .await
            .expect("second signal send");
        let mut sink = Sink;

        let error = execute_statement_with_signals(
            &mut session,
            "SELECT pg_sleep(30);",
            &mut sink,
            &mut receiver,
        )
        .await
        .err()
        .expect("second interrupt ends the shell");

        assert_eq!(cancellation.calls.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("interrupt again ended"));
        assert_eq!(session.state(), TransactionState::Uncertain);
    }

    #[tokio::test]
    async fn unavailable_cancellation_marks_the_live_session_uncertain_and_blocks_more_sql() {
        let intent = ConnectionIntent::new(
            ClusterTarget::new("cluster-1", "us-east-1", None),
            DatabaseRole::Custom("app_user".into()),
            Vec::new(),
            "dsql test",
        );
        let metadata = SessionMetadata::new(
            intent,
            SystemTime::now(),
            CancellationCapability::Unavailable,
            TransactionState::Idle,
            Vec::new(),
        );
        let session = ConnectedSession::new(metadata, Box::new(UnavailableCancellationHandle));
        let connector = NoReconnect;
        let mut session = ManagedSession::new(session, &connector, SystemTime::now());
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .send(QuerySignal::Interrupt)
            .await
            .expect("signal send");
        let mut sink = Sink;

        let result = execute_statement_with_signals(
            &mut session,
            "SELECT pg_sleep(30);",
            &mut sink,
            &mut receiver,
        )
        .await
        .expect("execution coordination");
        assert!(result.cancellation_failed);
        assert_eq!(session.state(), TransactionState::Uncertain);

        let error = session
            .execute("SELECT 1;", &mut sink)
            .await
            .expect_err("uncertain live session must reject more SQL");
        assert!(error.to_string().contains("statement was not submitted"));
    }

    // These tests deliberately run Reedline in a child test process.  Replacing this
    // process's file descriptors would make the test harness itself a terminal client.
    #[cfg(unix)]
    const PTY_CHILD: &str = "DSQL_PTY_SHELL_CHILD";
    #[cfg(unix)]
    const PTY_HISTORY: &str = "DSQL_PTY_SHELL_HISTORY";
    #[cfg(unix)]
    const PTY_NO_HISTORY: &str = "DSQL_PTY_SHELL_NO_HISTORY";
    #[cfg(unix)]
    const PTY_METRICS_DENIED: &str = "DSQL_PTY_SHELL_METRICS_DENIED";
    #[cfg(unix)]
    const PTY_PROMPT: &str = "pty-cluster/app_user=> ";

    #[cfg(unix)]
    #[test]
    fn pty_shell_child() {
        if env::var_os(PTY_CHILD).is_none() {
            return;
        }

        let cancellation = Arc::new(PtyCancellation::default());
        let metadata = SessionMetadata::new(
            ConnectionIntent::new(
                ClusterTarget::new("pty-cluster", "us-east-1", None),
                DatabaseRole::Custom("app_user".into()),
                Vec::new(),
                "dsql pty test",
            ),
            SystemTime::now(),
            CancellationCapability::Available,
            TransactionState::Idle,
            Vec::new(),
        );
        let session = ConnectedSession::new(
            metadata,
            Box::new(PtyHandle {
                cancellation: cancellation.clone(),
            }),
        );
        let connector = NoReconnect;
        let mut session = ManagedSession::new(session, &connector, SystemTime::now());
        let no_history = env::var_os(PTY_NO_HISTORY).is_some();
        let history = env::var_os(PTY_HISTORY).map(Into::into);
        let result = tokio::runtime::Runtime::new()
            .expect("test runtime")
            .block_on(super::run(
                &mut session,
                &ResolvedAwsContext::new("us-east-1", None, None),
                &PtyMetrics,
                no_history,
                history,
            ));
        match result {
            Ok(()) => println!("__PTY_SHELL_RETURN__ raw={}", raw_mode_enabled()),
            Err(error) => println!("__PTY_SHELL_ERROR__ {} raw={}", error, raw_mode_enabled()),
        }
        println!(
            "__PTY_CANCELLATIONS__{}",
            cancellation.calls.load(Ordering::SeqCst)
        );
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct PtyCancellation {
        calls: AtomicUsize,
        finished: Notify,
    }

    #[cfg(unix)]
    impl SessionCancellation for PtyCancellation {
        fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.finished.notify_waiters();
            Box::pin(async { Ok(()) })
        }
    }

    #[cfg(unix)]
    struct PtyHandle {
        cancellation: Arc<PtyCancellation>,
    }

    #[cfg(unix)]
    struct PtyMetrics;

    #[cfg(unix)]
    impl MetricsProvider for PtyMetrics {
        fn snapshot<'a>(
            &'a self,
            _: &'a ResolvedAwsContext,
            _: &'a ClusterTarget,
            range: crate::app::MetricsRange,
        ) -> BoxFuture<'a, Result<crate::app::MetricsSnapshot, ApplicationError>> {
            Box::pin(async move {
                if env::var_os(PTY_METRICS_DENIED).is_some() {
                    Err(ApplicationError::runtime(
                        "CloudWatch metrics are unavailable; allow cloudwatch:GetMetricData on *",
                    ))
                } else {
                    Ok(crate::app::MetricsSnapshot::empty(range))
                }
            })
        }
    }

    #[cfg(unix)]
    impl SessionHandle for PtyHandle {
        fn execute<'a>(
            &'a mut self,
            statement: &'a str,
            sink: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            let cancellation = self.cancellation.clone();
            Box::pin(async move {
                if statement.trim().eq_ignore_ascii_case("WAIT;") {
                    // Let the query signal monitor register its Ctrl-C handler before
                    // exposing the deterministic PTY synchronization marker.
                    tokio::task::yield_now().await;
                    println!("__PTY_WAIT_STARTED__");
                    cancellation.finished.notified().await;
                    return Err(ApplicationError::runtime("statement canceled"));
                }
                if statement.trim().eq_ignore_ascii_case("STREAM;") {
                    sink.emit(ExecutionEvent::Columns(vec!["result".into()]))?;
                    sink.emit(ExecutionEvent::Row(vec![Some("before_cancel".into())]))?;
                    println!("__PTY_STREAM_STARTED__");
                    while cancellation.calls.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    sink.emit(ExecutionEvent::Row(vec![Some(
                        "__PTY_OUTPUT_AFTER_CANCEL__".into(),
                    )]))?;
                    return sink.emit(ExecutionEvent::CommandComplete { rows: 2 });
                }
                println!(
                    "__PTY_EXECUTE__{}",
                    statement.trim().replace(char::is_whitespace, "_")
                );
                if statement.contains("FAIL_DDL") {
                    return Err(ApplicationError::runtime("pty statement failed"));
                }
                sink.emit(ExecutionEvent::Columns(vec!["result".into()]))?;
                sink.emit(ExecutionEvent::Row(vec![Some("pty_result".into())]))?;
                sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            })
        }

        fn execute_params<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a [String],
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async {
                println!("__PTY_PARAMS__");
                Ok(())
            })
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            Some(self.cancellation.clone())
        }
    }

    #[cfg(unix)]
    fn raw_mode_enabled() -> bool {
        crossterm::terminal::is_raw_mode_enabled().unwrap_or(true)
    }

    #[cfg(unix)]
    struct PtyChild {
        session: expectrl::session::OsSession,
    }

    #[cfg(unix)]
    impl PtyChild {
        fn spawn(history: Option<&std::path::Path>, no_history: bool) -> Self {
            Self::spawn_with_metrics_access(history, no_history, true)
        }

        fn spawn_with_metrics_access(
            history: Option<&std::path::Path>,
            no_history: bool,
            metrics_allowed: bool,
        ) -> Self {
            let executable = env::current_exe().expect("current test executable");
            let mut command = Command::new(executable);
            command
                .args(["--exact", "shell::tests::pty_shell_child", "--nocapture"])
                .env(PTY_CHILD, "1");
            if let Some(history) = history {
                command.env(PTY_HISTORY, history);
            }
            if no_history {
                command.env(PTY_NO_HISTORY, "1");
            }
            if !metrics_allowed {
                command.env(PTY_METRICS_DENIED, "1");
            }
            let mut session = Session::spawn(command).expect("spawn PTY test child");
            session.set_expect_timeout(Some(std::time::Duration::from_secs(30)));
            Self { session }
        }

        fn prompt(&mut self) {
            self.expect_prompt(PTY_PROMPT);
        }

        fn expect_prompt(&mut self, prompt: &str) {
            let mut answered_cursor_request = false;
            loop {
                let captures = self
                    .session
                    .expect(Any(["\u{1b}[6n", prompt]))
                    .unwrap_or_else(|error| panic!("expected shell prompt: {error}"));
                if captures.get(0) == Some(prompt.as_bytes()) {
                    if answered_cursor_request {
                        return;
                    }
                } else {
                    answered_cursor_request = true;
                    self.session
                        .send("\u{1b}[1;1R")
                        .expect("respond to cursor-position request");
                }
            }
        }

        fn expect_text(&mut self, text: &str) {
            loop {
                let captures = self
                    .session
                    // Prefer the requested output when both it and a cursor
                    // position request are already buffered. `expectrl::Any`
                    // selects needles by order rather than by position; putting
                    // the cursor request first can therefore consume output
                    // that appeared immediately before it.
                    .expect(Any([text, "\u{1b}[6n"]))
                    .unwrap_or_else(|error| panic!("expected terminal output {text:?}: {error}"));
                if captures.get(0) == Some(text.as_bytes()) {
                    return;
                }
                self.session
                    .send("\u{1b}[1;1R")
                    .expect("respond to cursor-position request");
            }
        }

        fn expect_without_text(&mut self, forbidden: &str, expected: &str) {
            let mut answered_cursor_request = false;
            loop {
                let captures = self
                    .session
                    .expect(Any(["\u{1b}[6n", forbidden, expected]))
                    .expect("terminal output");
                match captures.get(0) {
                    Some(value) if value == expected.as_bytes() => {
                        if answered_cursor_request {
                            return;
                        }
                    }
                    Some(value) if value == forbidden.as_bytes() => {
                        panic!("unexpected terminal output: {forbidden}")
                    }
                    _ => {
                        answered_cursor_request = true;
                        self.session
                            .send("\u{1b}[1;1R")
                            .expect("respond to cursor-position request");
                    }
                }
            }
        }

        fn exit(mut self, cancellations: usize) {
            self.session
                .send(ControlCode::EndOfTransmission)
                .expect("send Ctrl-D");
            self.expect_text("__PTY_SHELL_RETURN__ raw=false");
            self.expect_text(&format!("__PTY_CANCELLATIONS__{cancellations}"));
            self.session.expect(Eof).expect("child EOF");
        }
    }

    #[cfg(unix)]
    fn temporary_history_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dsql-pty-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_ddl_invalidates_once_and_refresh_stays_transaction_safe() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();

        child
            .session
            .send_line("CREATE TABLE orders (id bigint);")
            .expect("first DDL");
        child.expect_text("notice: completion metadata is stale; use \\refresh");
        child.session.send_line("\\refresh").expect("safe refresh");
        child.expect_text("completion metadata refreshed");
        child.prompt();

        child
            .session
            .send_line("CREATE TABLE FAIL_DDL (id bigint);")
            .expect("failed DDL");
        child.expect_text("error: pty statement failed");
        child.session.send_line("\\q").expect("quit shell");
        child.expect_text("__PTY_SHELL_RETURN__ raw=false");
        child.expect_text("__PTY_CANCELLATIONS__0");
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_metrics_returns_to_the_existing_session() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();

        child.session.send_line("\\metrics").expect("metrics");
        child.expect_text("Aurora DSQL metrics");
        child.session.send("q").expect("quit dashboard");
        child.prompt();

        child.session.send_line("SELECT 1;").expect("SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_1;");
        child.prompt();
        child.exit(0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_metrics_permission_failure_returns_to_the_existing_session() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn_with_metrics_access(None, true, false);
        child.prompt();

        child.session.send_line("\\metrics").expect("metrics");
        child.expect_text("Aurora DSQL metrics");
        child.expect_text("Metrics unavailable");
        child.session.send("q").expect("quit dashboard");
        child.prompt();

        child.session.send_line("SELECT 1;").expect("SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_1;");
        child.prompt();
        child.exit(0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_sigterm_during_metrics_terminates_and_restores_raw_mode() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send_line("\\metrics").expect("metrics");
        child.expect_text("Aurora DSQL metrics");

        child
            .session
            .get_process_mut()
            .kill(Signal::SIGTERM)
            .expect("send SIGTERM");
        child.expect_text(
            "__PTY_SHELL_ERROR__ terminated while the interactive shell was active raw=false",
        );
        child.expect_text("__PTY_CANCELLATIONS__0");
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_sighup_during_metrics_terminates_and_restores_raw_mode() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send_line("\\metrics").expect("metrics");
        child.expect_text("Aurora DSQL metrics");

        child
            .session
            .get_process_mut()
            .kill(Signal::SIGHUP)
            .expect("send SIGHUP");
        child.expect_text(
            "__PTY_SHELL_ERROR__ terminated while the interactive shell was active raw=false",
        );
        child.expect_text("__PTY_CANCELLATIONS__0");
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_refresh_in_transaction_submits_no_catalog_query() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send_line("BEGIN;").expect("begin");
        child.expect_text("__PTY_EXECUTE__BEGIN;");
        child.expect_prompt("pty-cluster/app_user=*> ");

        child
            .session
            .send_line("\\refresh")
            .expect("unsafe refresh");
        child.expect_text(
            "error: \\refresh is unavailable while a transaction is active, failed, or uncertain",
        );
        child.expect_without_text("__PTY_PARAMS__", "pty-cluster/app_user=*> ");
        child.exit(0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_multiline_sql_waits_for_a_terminator() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child
            .session
            .send_line("SELECT multiline")
            .expect("first line");
        child.expect_text("...> ");
        assert!(
            !child
                .session
                .is_matched("__PTY_EXECUTE__SELECT_multiline")
                .expect("check no early execution")
        );
        child.session.send_line("value;").expect("terminator");
        child.expect_text("__PTY_EXECUTE__SELECT_multiline_value;");
        child.prompt();
        child.exit(0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_idle_interrupt_clears_an_unsubmitted_buffer() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send("SELECT abandoned").expect("type buffer");
        child
            .session
            .send(ControlCode::EndOfText)
            .expect("send Ctrl-C");
        child.prompt();
        assert!(
            !child
                .session
                .is_matched("__PTY_EXECUTE__SELECT_abandoned")
                .expect("check no execution")
        );
        child.exit(0);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_history_is_private_and_excludes_leading_space_sql() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let history = temporary_history_path();
        let mut child = PtyChild::spawn(Some(&history), false);
        child.prompt();
        child
            .session
            .send_line(" SELECT hidden;")
            .expect("hidden SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_hidden;");
        child.prompt();
        child
            .session
            .send_line("SELECT remembered;")
            .expect("remembered SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_remembered;");
        child.prompt();
        child.exit(0);

        let contents = fs::read_to_string(&history).expect("read history");
        assert!(!contents.contains("SELECT hidden;"));
        assert!(contents.contains("SELECT remembered;"));
        assert_eq!(
            fs::metadata(&history)
                .expect("history metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_file(history).expect("remove history");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_history_search_recalls_and_submits_a_statement() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let history = temporary_history_path();
        let mut child = PtyChild::spawn(Some(&history), false);
        child.prompt();
        child
            .session
            .send_line("SELECT recalled;")
            .expect("history SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_recalled;");
        child.prompt();
        child
            .session
            .send(ControlCode::DeviceControl2)
            .expect("send Ctrl-R");
        child.expect_text("history search: ");
        child.session.send("recalled").expect("search term");
        child
            .session
            .send(ControlCode::CarriageReturn)
            .expect("accept search");
        child
            .session
            .send(ControlCode::CarriageReturn)
            .expect("submit recalled SQL");
        child.expect_text("__PTY_EXECUTE__SELECT_recalled;");
        child.prompt();
        child.exit(0);
        fs::remove_file(history).expect("remove history");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_no_history_does_not_create_a_file() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let history = temporary_history_path();
        let mut child = PtyChild::spawn(Some(&history), true);
        child.prompt();
        child.exit(0);
        assert!(!history.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_history_write_failure_warns_without_ending_the_session() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let history = temporary_history_path();
        let mut child = PtyChild::spawn(Some(&history), false);
        child.prompt();
        fs::remove_file(&history).expect("remove history file");
        fs::create_dir(&history).expect("replace history file with directory");
        child
            .session
            .send_line("SELECT after_history_failure;")
            .expect("SQL after history failure");
        child.expect_text("warning: interactive shell history could not be written");
        child.expect_text("__PTY_EXECUTE__SELECT_after_history_failure;");
        child.prompt();
        child.exit(0);
        fs::remove_dir(history).expect("remove history directory");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_sigterm_at_prompt_restores_raw_mode() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child
            .session
            .get_process_mut()
            .kill(Signal::SIGTERM)
            .expect("send SIGTERM");
        child.expect_text(
            "__PTY_SHELL_ERROR__ terminated while the interactive shell was active raw=false",
        );
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_sighup_at_prompt_restores_raw_mode() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child
            .session
            .get_process_mut()
            .kill(Signal::SIGHUP)
            .expect("send SIGHUP");
        child.expect_text(
            "__PTY_SHELL_ERROR__ terminated while the interactive shell was active raw=false",
        );
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_sighup_during_query_terminates_without_replay() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send_line("WAIT;").expect("WAIT statement");
        child.expect_text("__PTY_WAIT_STARTED__");
        child
            .session
            .get_process_mut()
            .kill(Signal::SIGHUP)
            .expect("send SIGHUP");
        child.expect_text("__PTY_SHELL_ERROR__ terminated during database statement; statement outcome may be unknown and was not replayed raw=false");
        child.expect_text("__PTY_CANCELLATIONS__0");
        child.session.expect(Eof).expect("child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_first_interrupt_cancels_wait_without_replay() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child.session.send_line("WAIT;").expect("WAIT statement");
        child.expect_text("__PTY_WAIT_STARTED__");
        child
            .session
            .send(ControlCode::EndOfText)
            .expect("send Ctrl-C");
        child.prompt();
        child
            .session
            .send_line("SELECT after_wait;")
            .expect("usable prompt");
        child.expect_text("__PTY_EXECUTE__SELECT_after_wait;");
        child.prompt();
        child.exit(1);
    }

    #[cfg(unix)]
    #[test]
    fn pty_shell_interrupt_stops_buffered_query_output() {
        let _guard = pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut child = PtyChild::spawn(None, true);
        child.prompt();
        child
            .session
            .send_line("STREAM;")
            .expect("streaming statement");
        child.expect_text("__PTY_STREAM_STARTED__");
        child
            .session
            .send(ControlCode::EndOfText)
            .expect("send Ctrl-C");
        child.expect_without_text("__PTY_OUTPUT_AFTER_CANCEL__", PTY_PROMPT);
        child.exit(1);
    }
}
