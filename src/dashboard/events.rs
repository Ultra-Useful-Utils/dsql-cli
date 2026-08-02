use std::{
    future::Future,
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::{Stream, StreamExt, channel::mpsc};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    app::{MetricsRange, MetricsSnapshot},
    error::ApplicationError,
};

use super::view::{self, DashboardData, DashboardView};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DashboardAction {
    Quit,
    Refresh,
    Range(MetricsRange),
}

enum DashboardEvent {
    Terminal(crossterm::event::Event),
    Terminate,
}

fn action_for_key(key: KeyEvent) -> Option<DashboardAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || key.modifiers != KeyModifiers::NONE
    {
        return None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(DashboardAction::Quit),
        KeyCode::Char('r') => Some(DashboardAction::Refresh),
        KeyCode::Char('1') => Some(DashboardAction::Range(MetricsRange::FifteenMinutes)),
        KeyCode::Char('2') => Some(DashboardAction::Range(MetricsRange::OneHour)),
        KeyCode::Char('3') => Some(DashboardAction::Range(MetricsRange::SixHours)),
        KeyCode::Char('4') => Some(DashboardAction::Range(MetricsRange::TwentyFourHours)),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) async fn run<F, Fut>(cluster_id: &str, fetch: F) -> Result<(), ApplicationError>
where
    F: FnMut(MetricsRange) -> Fut + Send,
    Fut: Future<Output = Result<MetricsSnapshot, ApplicationError>> + Send,
{
    run_monitored(cluster_id, fetch, true).await
}

pub(crate) async fn run_in_shell<F, Fut>(cluster_id: &str, fetch: F) -> Result<(), ApplicationError>
where
    F: FnMut(MetricsRange) -> Fut + Send,
    Fut: Future<Output = Result<MetricsSnapshot, ApplicationError>> + Send,
{
    run_monitored(cluster_id, fetch, false).await
}

async fn run_monitored<F, Fut>(
    cluster_id: &str,
    fetch: F,
    monitor_termination: bool,
) -> Result<(), ApplicationError>
where
    F: FnMut(MetricsRange) -> Fut + Send,
    Fut: Future<Output = Result<MetricsSnapshot, ApplicationError>> + Send,
{
    let _terminal_guard = TerminalGuard::enter()?;
    let ui = CrosstermUi::new()?;
    let (sender, events) = mpsc::unbounded();
    let _input_monitor = InputMonitor::new(sender.clone())?;
    let _termination_monitor = monitor_termination
        .then(|| TerminationMonitor::new(sender))
        .transpose()?;
    run_with(ui, cluster_id, fetch, events, REFRESH_INTERVAL).await
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, ApplicationError> {
        enable_raw_mode().map_err(|_| {
            ApplicationError::runtime("could not enable raw mode for the metrics dashboard")
        })?;
        if execute!(io::stdout(), EnterAlternateScreen, Hide).is_err() {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
            return Err(ApplicationError::runtime(
                "could not enter the metrics dashboard terminal view",
            ));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut output = io::stdout();
        let _ = execute!(output, LeaveAlternateScreen, Show);
        let _ = output.flush();
    }
}

struct CrosstermUi {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl CrosstermUi {
    fn new() -> Result<Self, ApplicationError> {
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
            .map_err(|_| ApplicationError::runtime("could not initialize the metrics dashboard"))?;
        Ok(Self { terminal })
    }
}

impl DashboardUi for CrosstermUi {
    fn draw(&mut self, cluster_id: &str, data: DashboardData<'_>) -> Result<(), ApplicationError> {
        self.terminal
            .draw(|frame| view::render(frame, DashboardView { cluster_id, data }))
            .map(|_| ())
            .map_err(|_| ApplicationError::runtime("could not draw the metrics dashboard"))
    }
}

struct InputMonitor {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputMonitor {
    fn new(
        sender: mpsc::UnboundedSender<Result<DashboardEvent, ApplicationError>>,
    ) -> Result<Self, ApplicationError> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("dsql-dashboard-input".into())
            .spawn(move || monitor_input(&thread_stop, &sender))
            .map_err(|_| {
                ApplicationError::runtime("could not start the metrics dashboard input monitor")
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn monitor_input(
    stop: &AtomicBool,
    sender: &mpsc::UnboundedSender<Result<DashboardEvent, ApplicationError>>,
) {
    while !stop.load(Ordering::Acquire) {
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if sender
                        .unbounded_send(Ok(DashboardEvent::Terminal(event)))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.unbounded_send(Err(ApplicationError::runtime(
                        "could not read metrics dashboard input",
                    )));
                    break;
                }
            },
            Ok(false) => {}
            Err(_) => {
                let _ = sender.unbounded_send(Err(ApplicationError::runtime(
                    "could not monitor metrics dashboard input",
                )));
                break;
            }
        }
    }
}

struct TerminationMonitor {
    task: tokio::task::JoinHandle<()>,
}

impl TerminationMonitor {
    fn new(
        sender: mpsc::UnboundedSender<Result<DashboardEvent, ApplicationError>>,
    ) -> Result<Self, ApplicationError> {
        #[cfg(unix)]
        let task = {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(|_| ApplicationError::runtime("could not monitor SIGTERM"))?;
            let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .map_err(|_| ApplicationError::runtime("could not monitor SIGHUP"))?;
            tokio::spawn(async move {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = hangup.recv() => {}
                }
                let _ = sender.unbounded_send(Ok(DashboardEvent::Terminate));
            })
        };
        #[cfg(not(unix))]
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(sender);
        });
        Ok(Self { task })
    }
}

impl Drop for TerminationMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

trait DashboardUi {
    fn draw(&mut self, cluster_id: &str, data: DashboardData<'_>) -> Result<(), ApplicationError>;
}

async fn run_with<U, F, Fut, E>(
    mut ui: U,
    cluster_id: &str,
    mut fetch: F,
    mut events: E,
    refresh_interval: Duration,
) -> Result<(), ApplicationError>
where
    U: DashboardUi,
    F: FnMut(MetricsRange) -> Fut,
    Fut: Future<Output = Result<MetricsSnapshot, ApplicationError>>,
    E: Stream<Item = Result<DashboardEvent, ApplicationError>> + Unpin,
{
    let mut range = MetricsRange::OneHour;
    let mut displayed = None;

    'dashboard: loop {
        let fetch_snapshot = fetch(range);
        tokio::pin!(fetch_snapshot);
        let fetched = loop {
            tokio::select! {
                biased;
                result = &mut fetch_snapshot => break result,
                event = events.next() => match event_action(event)? {
                    Some(DashboardAction::Quit) => return Ok(()),
                    Some(DashboardAction::Refresh) => continue 'dashboard,
                    Some(DashboardAction::Range(selected)) => {
                        range = selected;
                        continue 'dashboard;
                    }
                    None => {
                        if let Some(data) = displayed.as_ref() {
                            draw(&mut ui, cluster_id, data)?;
                        }
                    }
                }
            }
        };
        displayed = Some(fetched);
        draw(
            &mut ui,
            cluster_id,
            displayed.as_ref().expect("dashboard data was assigned"),
        )?;

        let refresh = tokio::time::sleep(refresh_interval);
        tokio::pin!(refresh);
        loop {
            tokio::select! {
                biased;
                event = events.next() => match event_action(event)? {
                    Some(DashboardAction::Quit) => return Ok(()),
                    Some(DashboardAction::Refresh) => continue 'dashboard,
                    Some(DashboardAction::Range(selected)) => {
                        range = selected;
                        continue 'dashboard;
                    }
                    None => draw(
                        &mut ui,
                        cluster_id,
                        displayed.as_ref().expect("dashboard data is displayed"),
                    )?,
                },
                () = &mut refresh => continue 'dashboard,
            }
        }
    }
}

fn event_action(
    event: Option<Result<DashboardEvent, ApplicationError>>,
) -> Result<Option<DashboardAction>, ApplicationError> {
    match event {
        Some(Ok(DashboardEvent::Terminal(crossterm::event::Event::Key(key)))) => {
            Ok(action_for_key(key))
        }
        Some(Ok(DashboardEvent::Terminal(_))) => Ok(None),
        Some(Ok(DashboardEvent::Terminate)) => Err(ApplicationError::runtime(
            "terminated while the metrics dashboard was active",
        )),
        Some(Err(error)) => Err(error),
        None => Err(ApplicationError::runtime(
            "metrics dashboard event monitor stopped unexpectedly",
        )),
    }
}

fn draw(
    ui: &mut impl DashboardUi,
    cluster_id: &str,
    data: &Result<MetricsSnapshot, ApplicationError>,
) -> Result<(), ApplicationError> {
    let data = match data {
        Ok(snapshot) => DashboardData::Snapshot(snapshot),
        Err(error) => DashboardData::Error(error),
    };
    ui.draw(cluster_id, data)
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    #[cfg(unix)]
    use expectrl::{Eof, Expect, Session, process::unix::Signal};
    use futures::{channel::mpsc, stream};
    use tokio::sync::Notify;

    use crate::{
        app::{MetricsRange, MetricsSnapshot},
        error::ApplicationError,
    };

    use super::{
        DashboardAction, DashboardData, DashboardEvent, DashboardUi, REFRESH_INTERVAL,
        action_for_key, run_with,
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Drawn {
        Snapshot(MetricsRange),
        Error(String),
    }

    struct FakeUi {
        drawn: Arc<Mutex<Vec<Drawn>>>,
        draw_notify: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    }

    impl DashboardUi for FakeUi {
        fn draw(&mut self, _: &str, data: DashboardData<'_>) -> Result<(), ApplicationError> {
            let drawn = match data {
                DashboardData::Snapshot(snapshot) => Drawn::Snapshot(snapshot.range),
                DashboardData::Error(error) => Drawn::Error(error.to_string()),
            };
            self.drawn.lock().expect("draw lock").push(drawn);
            self.draw_notify.notify_one();
            Ok(())
        }
    }

    impl Drop for FakeUi {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    type FakeUiFixture = (
        FakeUi,
        Arc<Mutex<Vec<Drawn>>>,
        Arc<Notify>,
        Arc<AtomicUsize>,
    );

    fn fake_ui() -> FakeUiFixture {
        let drawn = Arc::new(Mutex::new(Vec::new()));
        let draw_notify = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        (
            FakeUi {
                drawn: drawn.clone(),
                draw_notify: draw_notify.clone(),
                drops: drops.clone(),
            },
            drawn,
            draw_notify,
            drops,
        )
    }

    fn key(character: char) -> DashboardEvent {
        DashboardEvent::Terminal(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )))
    }

    #[test]
    fn quit_keys_return_to_the_shell() {
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(DashboardAction::Quit)
        );
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(DashboardAction::Quit)
        );
    }

    #[test]
    fn refresh_and_range_keys_request_new_snapshots() {
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(DashboardAction::Refresh)
        );
        for (key, range) in [
            ('1', MetricsRange::FifteenMinutes),
            ('2', MetricsRange::OneHour),
            ('3', MetricsRange::SixHours),
            ('4', MetricsRange::TwentyFourHours),
        ] {
            assert_eq!(
                action_for_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
                Some(DashboardAction::Range(range))
            );
        }
    }

    #[test]
    fn modified_or_unrelated_keys_are_ignored() {
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn automatic_refresh_interval_is_sixty_seconds() {
        assert_eq!(REFRESH_INTERVAL, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn manual_refresh_and_range_switch_fetch_and_redraw() {
        let (ui, drawn, draw_notify, drops) = fake_ui();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let fetch_ranges = ranges.clone();
        let (mut sender, events) = mpsc::unbounded();
        let task = tokio::spawn(run_with(
            ui,
            "cluster",
            move |range| {
                fetch_ranges.lock().expect("range lock").push(range);
                async move { Ok(MetricsSnapshot::empty(range)) }
            },
            events,
            Duration::from_secs(60),
        ));

        draw_notify.notified().await;
        sender.start_send(Ok(key('r'))).expect("manual refresh");
        draw_notify.notified().await;
        sender.start_send(Ok(key('3'))).expect("range switch");
        draw_notify.notified().await;
        sender.start_send(Ok(key('q'))).expect("quit");

        task.await.expect("dashboard task").expect("dashboard run");
        assert_eq!(
            *ranges.lock().expect("range lock"),
            vec![
                MetricsRange::OneHour,
                MetricsRange::OneHour,
                MetricsRange::SixHours,
            ]
        );
        assert_eq!(
            *drawn.lock().expect("draw lock"),
            vec![
                Drawn::Snapshot(MetricsRange::OneHour),
                Drawn::Snapshot(MetricsRange::OneHour),
                Drawn::Snapshot(MetricsRange::SixHours),
            ]
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetch_errors_are_rendered_until_the_user_quits() {
        let (ui, drawn, _, drops) = fake_ui();
        let events = stream::iter([Ok(key('q'))]);

        run_with(
            ui,
            "cluster",
            |_| async { Err(ApplicationError::runtime("metrics denied")) },
            events,
            Duration::from_secs(60),
        )
        .await
        .expect("dashboard run");

        assert_eq!(
            *drawn.lock().expect("draw lock"),
            vec![Drawn::Error("metrics denied".into())]
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resize_redraws_without_fetching_again() {
        let (ui, drawn, _, _) = fake_ui();
        let fetches = Arc::new(AtomicUsize::new(0));
        let fetch_count = fetches.clone();
        let events = stream::iter([
            Ok(DashboardEvent::Terminal(Event::Resize(100, 30))),
            Ok(key('q')),
        ]);

        run_with(
            ui,
            "cluster",
            move |range| {
                fetch_count.fetch_add(1, Ordering::SeqCst);
                async move { Ok(MetricsSnapshot::empty(range)) }
            },
            events,
            Duration::from_secs(60),
        )
        .await
        .expect("dashboard run");

        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(drawn.lock().expect("draw lock").len(), 2);
    }

    #[tokio::test]
    async fn refresh_timer_fetches_a_new_snapshot() {
        let (ui, _, _, drops) = fake_ui();
        let fetches = Arc::new(AtomicUsize::new(0));
        let fetch_count = fetches.clone();
        let second_fetch = Arc::new(Notify::new());
        let second_fetch_started = second_fetch.clone();
        let task = tokio::spawn(run_with(
            ui,
            "cluster",
            move |range| {
                if fetch_count.fetch_add(1, Ordering::SeqCst) == 1 {
                    second_fetch_started.notify_one();
                }
                async move { Ok(MetricsSnapshot::empty(range)) }
            },
            stream::pending(),
            Duration::from_millis(1),
        ));

        tokio::time::timeout(Duration::from_secs(1), second_fetch.notified())
            .await
            .expect("automatic refresh");
        task.abort();
        assert!(task.await.expect_err("task canceled").is_cancelled());
        assert!(fetches.load(Ordering::SeqCst) >= 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn termination_and_event_errors_drop_the_ui() {
        for event in [
            Ok(DashboardEvent::Terminate),
            Err(ApplicationError::runtime("event read failed")),
        ] {
            let (ui, _, _, drops) = fake_ui();
            let error = run_with(
                ui,
                "cluster",
                |range| async move { Ok(MetricsSnapshot::empty(range)) },
                stream::iter([event]),
                Duration::from_secs(60),
            )
            .await
            .expect_err("dashboard must stop");

            assert!(
                error.to_string().contains("terminated")
                    || error.to_string().contains("event read failed")
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn aborting_the_dashboard_task_drops_the_ui() {
        let (ui, _, draw_notify, drops) = fake_ui();
        let task = tokio::spawn(run_with(
            ui,
            "cluster",
            |range| async move { Ok(MetricsSnapshot::empty(range)) },
            stream::pending(),
            Duration::from_secs(60),
        ));
        draw_notify.notified().await;

        task.abort();
        assert!(task.await.expect_err("task canceled").is_cancelled());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_fetch_panic_drops_the_ui_during_unwind() {
        let (ui, _, _, drops) = fake_ui();
        let task = tokio::spawn(run_with(
            ui,
            "cluster",
            |_| async {
                panic!("simulated fetch panic");
                #[allow(unreachable_code)]
                Ok(MetricsSnapshot::empty(MetricsRange::OneHour))
            },
            stream::pending(),
            Duration::from_secs(60),
        ));

        assert!(task.await.expect_err("task panicked").is_panic());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    const PTY_DASHBOARD_CHILD: &str = "DSQL_PTY_DASHBOARD_CHILD";

    #[cfg(unix)]
    #[test]
    fn pty_dashboard_child() {
        let Some(mode) = env::var_os(PTY_DASHBOARD_CHILD) else {
            return;
        };
        let mode = mode.to_string_lossy();
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let outcome = match mode.as_ref() {
            "quit" | "escape" | "error" | "signal" | "sighup" => {
                let fail = mode == "error";
                match runtime.block_on(super::run("pty-cluster", move |range| async move {
                    if fail {
                        Err(ApplicationError::runtime("metrics denied"))
                    } else {
                        Ok(MetricsSnapshot::empty(range))
                    }
                })) {
                    Ok(()) => "ok".to_owned(),
                    Err(error) => format!("error={error}"),
                }
            }
            "panic" => runtime.block_on(async {
                let task = tokio::spawn(super::run("pty-cluster", |_| async {
                    println!("__PTY_DASHBOARD_FETCH__");
                    panic!("simulated dashboard panic");
                    #[allow(unreachable_code)]
                    Ok(MetricsSnapshot::empty(MetricsRange::OneHour))
                }));
                assert!(task.await.expect_err("dashboard panic").is_panic());
                "panic".to_owned()
            }),
            "cancel" => runtime.block_on(async {
                let task = tokio::spawn(super::run("pty-cluster", |_| async {
                    println!("__PTY_DASHBOARD_FETCH__");
                    std::future::pending::<Result<MetricsSnapshot, ApplicationError>>().await
                }));
                tokio::time::sleep(Duration::from_millis(100)).await;
                task.abort();
                assert!(task.await.expect_err("dashboard canceled").is_cancelled());
                "canceled".to_owned()
            }),
            other => panic!("unknown PTY dashboard mode {other}"),
        };
        println!(
            "__PTY_DASHBOARD_RETURN__ {outcome} raw={}",
            crossterm::terminal::is_raw_mode_enabled().unwrap_or(true)
        );
    }

    #[cfg(unix)]
    fn spawn_pty_dashboard(mode: &str) -> expectrl::session::OsSession {
        let executable = env::current_exe().expect("current test executable");
        let mut command = Command::new(executable);
        command
            .args([
                "--exact",
                "dashboard::events::tests::pty_dashboard_child",
                "--nocapture",
            ])
            .env(PTY_DASHBOARD_CHILD, mode);
        let mut session = Session::spawn(command).expect("spawn dashboard PTY child");
        session.set_expect_timeout(Some(Duration::from_secs(30)));
        session
    }

    #[cfg(unix)]
    fn expect_restored(session: &mut expectrl::session::OsSession, outcome: &str) {
        session
            .expect("\u{1b}[?1049l")
            .expect("leave alternate screen");
        session
            .expect(format!("__PTY_DASHBOARD_RETURN__ {outcome} raw=false"))
            .expect("restored dashboard outcome");
        session.expect(Eof).expect("dashboard child EOF");
    }

    #[cfg(unix)]
    #[test]
    fn pty_quit_and_escape_restore_the_terminal() {
        let _guard = crate::pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (mode, input) in [("quit", "q"), ("escape", "\u{1b}")] {
            let mut session = spawn_pty_dashboard(mode);
            session.expect("Aurora DSQL metrics").expect("dashboard");
            session.send(input).expect("exit key");
            expect_restored(&mut session, "ok");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pty_fetch_error_restores_the_terminal() {
        let _guard = crate::pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut session = spawn_pty_dashboard("error");
        session
            .expect("Aurora DSQL metrics")
            .expect("error dashboard");
        session.send("q").expect("quit");
        expect_restored(&mut session, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn pty_panic_and_task_cancellation_restore_the_terminal() {
        let _guard = crate::pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (mode, outcome) in [("panic", "panic"), ("cancel", "canceled")] {
            let mut session = spawn_pty_dashboard(mode);
            session
                .expect("__PTY_DASHBOARD_FETCH__")
                .expect("fetch started");
            expect_restored(&mut session, outcome);
        }
    }

    #[cfg(unix)]
    #[test]
    fn pty_termination_signals_restore_the_terminal() {
        let _guard = crate::pty_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (mode, signal) in [("signal", Signal::SIGTERM), ("sighup", Signal::SIGHUP)] {
            let mut session = spawn_pty_dashboard(mode);
            session.expect("Aurora DSQL metrics").expect("dashboard");
            session
                .get_process_mut()
                .kill(signal)
                .expect("send termination signal");
            expect_restored(
                &mut session,
                "error=terminated while the metrics dashboard was active",
            );
        }
    }
}
