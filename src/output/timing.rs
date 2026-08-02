use crate::{app::ExecutionSink, error::ApplicationError};
use std::{
    io::Write,
    time::{Duration, Instant},
};

/// Monotonic time source used by [`TimingExecutionSink`].
pub(crate) trait Clock: Send {
    fn now(&self) -> Instant;
    fn elapsed(&self, started: Instant) -> Duration;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn elapsed(&self, started: Instant) -> Duration {
        started.elapsed()
    }
}

/// Adds psql-style elapsed-time diagnostics without altering query output.
pub(crate) struct TimingExecutionSink<Sink, Diagnostics, Time = SystemClock> {
    sink: Sink,
    diagnostics: Diagnostics,
    clock: Time,
    started: Instant,
}

impl<Sink, Diagnostics> TimingExecutionSink<Sink, Diagnostics, SystemClock>
where
    Sink: ExecutionSink,
    Diagnostics: Write + Send,
{
    #[allow(dead_code)] // Shell wiring lands with SH-004's meta-command work.
    pub(crate) fn new(sink: Sink, diagnostics: Diagnostics) -> Self {
        Self::with_clock(sink, diagnostics, SystemClock)
    }
}

impl<Sink, Diagnostics, Time> TimingExecutionSink<Sink, Diagnostics, Time>
where
    Sink: ExecutionSink,
    Diagnostics: Write + Send,
    Time: Clock,
{
    pub(crate) fn with_clock(sink: Sink, diagnostics: Diagnostics, clock: Time) -> Self {
        let started = clock.now();
        Self {
            sink,
            diagnostics,
            clock,
            started,
        }
    }

    /// Starts timing a subsequent execution when a sink is retained by the shell.
    pub(crate) fn restart(&mut self) {
        self.started = self.clock.now();
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (Sink, Diagnostics) {
        (self.sink, self.diagnostics)
    }

    fn report(&mut self) -> Result<(), ApplicationError> {
        let elapsed = self.clock.elapsed(self.started);
        writeln!(
            self.diagnostics,
            "Time: {:.3} ms",
            elapsed.as_secs_f64() * 1_000.0
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                ApplicationError::broken_pipe("could not render query diagnostics")
            } else {
                ApplicationError::runtime("could not render query diagnostics")
            }
        })?;
        self.diagnostics.flush().map_err(|error| {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                ApplicationError::broken_pipe("could not render query diagnostics")
            } else {
                ApplicationError::runtime("could not render query diagnostics")
            }
        })?;
        self.restart();
        Ok(())
    }
}

impl<Sink, Diagnostics, Time> ExecutionSink for TimingExecutionSink<Sink, Diagnostics, Time>
where
    Sink: ExecutionSink,
    Diagnostics: Write + Send,
    Time: Clock,
{
    fn emit(&mut self, event: crate::app::ExecutionEvent) -> Result<(), ApplicationError> {
        let completed = matches!(event, crate::app::ExecutionEvent::CommandComplete { .. });
        let failed = matches!(event, crate::app::ExecutionEvent::Error { .. });
        self.sink.emit(event)?;
        if completed || failed {
            self.report()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{ExecutionEvent, ExecutionSink},
        output::expanded::ExpandedExecutionSink,
        output::timing::{Clock, TimingExecutionSink},
    };
    use std::time::{Duration, Instant};

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            Instant::now()
        }

        fn elapsed(&self, _: Instant) -> Duration {
            Duration::from_micros(1_250)
        }
    }

    #[test]
    fn completion_reports_elapsed_time_to_diagnostics() {
        let mut sink = TimingExecutionSink::with_clock(
            ExpandedExecutionSink::new(Vec::new(), Vec::new()),
            Vec::new(),
            FixedClock,
        );
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("completion renders");

        let (_, diagnostics) = sink.into_parts();
        assert_eq!(
            String::from_utf8(diagnostics).expect("utf-8"),
            "Time: 1.250 ms\n"
        );
    }
}
