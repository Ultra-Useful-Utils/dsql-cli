use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::{Child, ChildStdin, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const PAGER_EXIT_TIMEOUT: Duration = Duration::from_millis(250);
const PAGER_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// An already-tokenized pager command. It is never evaluated by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PagerCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl PagerCommand {
    pub(crate) fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    pub(crate) fn program(&self) -> &OsStr {
        &self.program
    }

    pub(crate) fn args(&self) -> &[OsString] {
        &self.args
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PagerStart {
    Pager,
    Fallback,
}

enum PagerTarget<W> {
    Fallback(W),
    Pager {
        child: Child,
        stdin: Option<ChildStdin>,
        closed: bool,
    },
}

/// A writer that uses a pager when it can be spawned, otherwise writes directly.
///
/// Call [`Self::finish`] before returning to the prompt so the pager's child is
/// reaped. `Drop` also makes a best effort to do that on error paths.
pub(crate) struct OptionalPager<W> {
    target: PagerTarget<W>,
}

impl<W: Write> OptionalPager<W> {
    pub(crate) fn new(fallback: W) -> Self {
        Self {
            target: PagerTarget::Fallback(fallback),
        }
    }

    /// Attempts a direct process spawn. Missing or unspawnable pagers retain
    /// the fallback writer rather than turning interactive output into an error.
    pub(crate) fn start(&mut self, command: Option<&PagerCommand>) -> PagerStart {
        let Some(command) = command else {
            return PagerStart::Fallback;
        };
        let mut process = Command::new(command.program());
        process
            .args(command.args())
            .stdin(Stdio::piped())
            .env_clear();
        for name in ["PATH", "TERM", "LANG", "LC_ALL"] {
            if let Some(value) = env::var_os(name) {
                process.env(name, value);
            }
        }
        let Ok(mut child) = process.spawn() else {
            return PagerStart::Fallback;
        };
        let Some(stdin) = child.stdin.take() else {
            let _ = child.wait();
            return PagerStart::Fallback;
        };
        self.target = PagerTarget::Pager {
            child,
            stdin: Some(stdin),
            closed: false,
        };
        PagerStart::Pager
    }

    /// Closes the pager input and reaps it. A pager that has already exited is
    /// a normal interactive outcome (for example, after a user quits `less`).
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        let PagerTarget::Pager { child, stdin, .. } = &mut self.target else {
            return Ok(());
        };
        drop(stdin.take());
        reap_child(child)
    }

    #[cfg(test)]
    pub(crate) fn fallback(&self) -> Option<&W> {
        match &self.target {
            PagerTarget::Fallback(writer) => Some(writer),
            PagerTarget::Pager { .. } => None,
        }
    }
}

impl<W: Write> Write for OptionalPager<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match &mut self.target {
            PagerTarget::Fallback(writer) => writer.write(buffer),
            PagerTarget::Pager { stdin, closed, .. } => {
                if *closed {
                    return Ok(buffer.len());
                }
                let Some(stdin) = stdin.as_mut() else {
                    return Ok(buffer.len());
                };
                match stdin.write(buffer) {
                    Ok(written) => Ok(written),
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        *closed = true;
                        Ok(buffer.len())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.target {
            PagerTarget::Fallback(writer) => writer.flush(),
            PagerTarget::Pager { stdin, closed, .. } => {
                if *closed {
                    return Ok(());
                }
                let Some(stdin) = stdin.as_mut() else {
                    return Ok(());
                };
                match stdin.flush() {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        *closed = true;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

impl<W> Drop for OptionalPager<W> {
    fn drop(&mut self) {
        if let PagerTarget::Pager { child, stdin, .. } = &mut self.target {
            drop(stdin.take());
            let _ = reap_child(child);
        }
    }
}

fn reap_child(child: &mut Child) -> io::Result<()> {
    let deadline = Instant::now() + PAGER_EXIT_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            return child.wait().map(|_| ());
        }
        thread::sleep(PAGER_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::{OptionalPager, PagerCommand, PagerStart};
    use std::io::Write;

    #[test]
    fn missing_pager_uses_the_fallback_writer() {
        let mut pager = OptionalPager::new(Vec::new());
        assert_eq!(
            pager.start(Some(&PagerCommand::new(
                "dsql-pager-that-does-not-exist",
                std::iter::empty::<&str>(),
            ))),
            PagerStart::Fallback
        );
        pager.write_all(b"result\n").expect("fallback writes");
        assert_eq!(pager.fallback().expect("fallback writer"), b"result\n");
    }

    #[test]
    fn argv_is_not_interpreted_by_a_shell() {
        let command = PagerCommand::new("pager", ["--flag; touch /tmp/not-run", "$(whoami)"]);
        assert_eq!(command.program(), "pager");
        assert_eq!(command.args(), ["--flag; touch /tmp/not-run", "$(whoami)"]);
    }

    #[test]
    fn an_early_pager_exit_does_not_fail_output() {
        let mut pager = OptionalPager::new(Vec::new());
        assert_eq!(
            pager.start(Some(&PagerCommand::new("true", std::iter::empty::<&str>()))),
            PagerStart::Pager
        );
        pager.write_all(b"result\n").expect("closed pager is clean");
        pager.finish().expect("closed pager is clean");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_exiting_pager_is_killed_within_a_bound() {
        let started = std::time::Instant::now();
        let mut pager = OptionalPager::new(Vec::new());
        assert_eq!(
            pager.start(Some(&PagerCommand::new("sleep", ["30"]))),
            PagerStart::Pager
        );
        pager.finish().expect("hung pager is reaped");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
