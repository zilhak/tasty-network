//! The system backend's port discovery: each remote command runs as an `ssh` child whose
//! output is read with the bounds of [`crate::discovery`].

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cancel::{SshCancel, cancel_requested, current_cancel};
use crate::discovery::{
    LOG_TARGET, Limits, PortDiscoveryConfig, PortDiscoveryError, PortDiscoveryFailureKind,
    PortMode, Probe, RemoteRun, cancelled_error, classify_by_exit_code, refine_with_elapsed,
    sanitize_stderr,
};
use crate::system::cancel::kill_and_reap;
use crate::{SpawnHook, SshLauncher, SshTarget};

/// Who holds the child of [`run_capture_with_budget`]: the cancel scope when one is installed
/// (another thread may kill it before the bound expires), or this function.
///
/// Either way the bound watch ([`wait_with_timeout`]) polls `try_wait` through this type, so
/// the watch and cancellation share one handle.
enum ChildOwner {
    Local(Child),
    Scoped(SshCancel, u64),
}

/// Result of [`ChildOwner::try_exited`].
enum ChildPoll {
    /// The child exited (or `try_wait` failed and nothing more can be seen).
    Exited,
    Running,
    /// Cancellation took the child; there is nothing left to wait for.
    Taken,
}

impl ChildOwner {
    fn adopt(child: Child, what: &str) -> Result<Self, PortDiscoveryError> {
        match current_cancel() {
            Some(handle) => match handle.register(child) {
                Ok(id) => Ok(Self::Scoped(handle, id)),
                Err(mut rejected) => {
                    kill_and_reap(&mut rejected);
                    Err(cancelled_error(what))
                }
            },
            None => Ok(Self::Local(child)),
        }
    }

    /// Whether cancellation was requested (a child not handed to a scope cannot be cancelled).
    fn is_cancelled(&self) -> bool {
        match self {
            Self::Local(_) => false,
            Self::Scoped(handle, _) => handle.is_cancelled(),
        }
    }

    fn try_exited(&mut self) -> ChildPoll {
        fn poll(child: &mut Child) -> ChildPoll {
            match child.try_wait() {
                Ok(Some(_)) => ChildPoll::Exited,
                Ok(None) => ChildPoll::Running,
                // Waiting longer cannot tell more. Treat it as exited and let the output
                // collection meet and classify the same error.
                Err(e) => {
                    tracing::debug!(target: LOG_TARGET, "ssh try_wait failed; polling stops: {e}");
                    ChildPoll::Exited
                }
            }
        }
        match self {
            Self::Local(child) => poll(child),
            Self::Scoped(handle, id) => handle.with_child(*id, poll).unwrap_or(ChildPoll::Taken),
        }
    }

    /// Cuts the child off at the bound: kill and wait, so no zombie is left. `false` means
    /// cancellation had already taken it (and killed and reaped it).
    fn kill_and_reap_now(self) -> bool {
        let child = match self {
            Self::Local(child) => Some(child),
            Self::Scoped(handle, id) => handle.reclaim(id),
        };
        match child {
            Some(mut child) => {
                kill_and_reap(&mut child);
                true
            }
            None => false,
        }
    }

    /// Hands the child to output collection. If cancellation took it, or the cancel flag is
    /// set, the result is [`PortDiscoveryFailureKind::Cancelled`], so a child killed by
    /// cancellation is not classified as a connection failure or a timeout.
    fn finish(self, what: &str) -> Result<Child, PortDiscoveryError> {
        match self {
            Self::Local(child) => Ok(child),
            Self::Scoped(handle, id) => match handle.reclaim(id) {
                None => Err(cancelled_error(what)),
                Some(mut child) if handle.is_cancelled() => {
                    kill_and_reap(&mut child);
                    Err(cancelled_error(what))
                }
                Some(child) => Ok(child),
            },
        }
    }
}

/// How [`wait_with_timeout`] ended.
#[derive(Debug, PartialEq, Eq)]
enum WaitEnd {
    /// The child ended, or cancellation took it or was requested. [`ChildOwner::finish`] settles
    /// the cancelled case as [`PortDiscoveryFailureKind::Cancelled`].
    Ended,
    /// The child ran past the deadline (the caller kills it).
    Expired,
    /// `stop` said to cut the child off. The caller still takes the child's own exit status if
    /// it has one ([`exit_after_overflow`]) and kills it otherwise.
    Stopped,
}

/// Waits for the child until `deadline`, asking `stop` on every poll.
///
/// The crate has no async runtime, so it polls `try_wait`. The interval starts at
/// `discovery.poll_min` (5 ms) and doubles up to `discovery.poll_max` (50 ms): little delay on
/// the normal path (hundreds of ms), few wakeups on long waits. A `poll_min` above `poll_max`
/// starts at `poll_max`, and every interval is cut to the time left.
fn wait_with_timeout(
    owner: &mut ChildOwner,
    deadline: Instant,
    stop: &mut dyn FnMut() -> bool,
    limits: Limits,
) -> WaitEnd {
    let mut nap = limits.poll_min.min(limits.poll_max);
    loop {
        if owner.is_cancelled() {
            return WaitEnd::Ended;
        }
        if stop() {
            return WaitEnd::Stopped;
        }
        match owner.try_exited() {
            ChildPoll::Exited | ChildPoll::Taken => return WaitEnd::Ended,
            ChildPoll::Running => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return WaitEnd::Expired;
        }
        std::thread::sleep(nap.min(left));
        nap = (nap * 2).min(limits.poll_max);
    }
}

/// What a reader thread read from one pipe.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// More than the limit arrived; `bytes` holds the first `limit` bytes.
    overflowed: bool,
}

/// What a reader does when more than its limit arrives.
#[derive(Clone, Copy)]
enum OnOverflow {
    /// Report at once and close the pipe (stdout: the output is refused anyway).
    Stop,
    /// Keep reading and dropping to the end, so the writer never blocks (stderr: ssh writes its
    /// own messages there and must not fail on a closed pipe).
    Drain,
}

/// Reads `pipe` on a thread, keeping the first `limit` bytes. The result arrives on the returned
/// channel when the pipe closes, or as soon as the limit is passed with [`OnOverflow::Stop`].
/// With [`OnOverflow::Drain`] the rest is read and dropped so the writer never blocks. A
/// descendant that holds the pipe keeps the thread waiting; the caller stops waiting at its
/// deadline and the thread ends when the pipe finally closes.
fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    limit: usize,
    on_overflow: OnOverflow,
    name: &str,
) -> std::io::Result<mpsc::Receiver<Captured>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut captured = Captured::default();
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = limit.saturating_sub(captured.bytes.len());
                        captured.bytes.extend_from_slice(&buf[..n.min(room)]);
                        captured.overflowed |= n > room;
                        if captured.overflowed && matches!(on_overflow, OnOverflow::Stop) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        tracing::debug!(target: LOG_TARGET, %e, "ssh output read ended");
                        break;
                    }
                }
            }
            // The receiver may have given up already.
            let _unwanted = tx.send(captured);
        })?;
    Ok(rx)
}

/// Readers for the child's stdout and stderr.
type Readers = (
    Option<mpsc::Receiver<Captured>>,
    Option<mpsc::Receiver<Captured>>,
);

/// stdout beyond `discovery.output_limit` is refused (a record is small); of stderr only
/// `discovery.stderr_limit` is kept for the detail, and the rest is read and dropped.
fn start_readers(child: &mut Child, limits: Limits) -> std::io::Result<Readers> {
    let stdout = child
        .stdout
        .take()
        .map(|p| {
            spawn_reader(
                p,
                limits.output_limit,
                OnOverflow::Stop,
                "ssh-discovery-stdout",
            )
        })
        .transpose()?;
    let stderr = child
        .stderr
        .take()
        .map(|p| {
            spawn_reader(
                p,
                limits.stderr_limit,
                OnOverflow::Drain,
                "ssh-discovery-stderr",
            )
        })
        .transpose()?;
    Ok((stdout, stderr))
}

/// The stdout reader's result, taken early when it is already there (an overflow is reported
/// while the child still runs).
struct Stdout {
    rx: Option<mpsc::Receiver<Captured>>,
    early: Option<Captured>,
}

impl Stdout {
    /// Whether the output already passed `discovery.output_limit`. It does not wait.
    fn overflowed(&mut self) -> bool {
        if self.early.is_none()
            && let Some(rx) = &self.rx
            && let Ok(captured) = rx.try_recv()
        {
            self.early = Some(captured);
        }
        self.early.as_ref().is_some_and(|c| c.overflowed)
    }

    fn collect(
        self,
        deadline: Instant,
        what: &str,
        poll: Duration,
    ) -> Result<Captured, PortDiscoveryError> {
        match (self.early, self.rx) {
            (Some(captured), _) => settled(captured, what),
            (None, Some(rx)) => collect(&rx, deadline, what, poll),
            (None, None) => Ok(Captured::default()),
        }
    }
}

/// Kills a child that ran past the deadline and says why. When cancellation took the child or
/// came meanwhile, the reason is `Cancelled`, not the timeout.
fn cut_off(owner: ChildOwner, what: &str, budget: Duration) -> PortDiscoveryError {
    let reclaimed = owner.kill_and_reap_now();
    if !reclaimed || cancel_requested() {
        cancelled_error(what)
    } else {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::TimedOut,
            format!("{what}: killed after not finishing within {budget:?}"),
        )
    }
}

/// The exit code `Child::kill` leaves: none on Unix (the child dies by SIGKILL), 1 on Windows
/// (`TerminateProcess` with exit code 1).
const KILLED_EXIT_CODE: Option<i32> = if cfg!(windows) { Some(1) } else { None };

/// Whether a child reaped after our kill ended on its own exit code rather than by the kill.
/// A child that exited just before the kill landed keeps its code (3: the instance exited); an
/// exit that looks like the kill's own (`killed_code`) is taken as the kill.
fn exited_on_its_own(code: Option<i32>, killed_code: Option<i32>) -> bool {
    code.is_some() && code != killed_code
}

/// After stdout passed the limit: the child's own exit status if it has ended (or ends within
/// `discovery.overflow_exit_grace`, within the deadline, or just before the kill), so an exit
/// code such as 3 still decides the class. `None` means the kill ended it. The grace is there
/// because such a child usually fails writing to the pipe just closed and exits on its own.
fn exit_after_overflow(
    owner: ChildOwner,
    deadline: Instant,
    what: &str,
    limits: Limits,
) -> Result<Option<ExitStatus>, PortDiscoveryError> {
    let mut child = owner.finish(what)?;
    let until = deadline.min(Instant::now() + limits.overflow_exit_grace);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() < until && !cancel_requested() => {
                let left = until.saturating_duration_since(Instant::now());
                std::thread::sleep(limits.poll_min.min(limits.poll_max).min(left));
            }
            _ => break,
        }
    }
    if let Err(e) = child.kill() {
        tracing::debug!(
            target: LOG_TARGET,
            "ssh child kill failed (the child may already have exited): {e}",
        );
    }
    let reaped = child.wait();
    if cancel_requested() {
        return Err(cancelled_error(what));
    }
    match reaped {
        Ok(status) if exited_on_its_own(status.code(), KILLED_EXIT_CODE) => Ok(Some(status)),
        Ok(_) => Ok(None),
        Err(e) => {
            tracing::debug!(target: LOG_TARGET, "ssh child reaping failed: {e}");
            Ok(None)
        }
    }
}

/// The refusal of an output past `limit` (`discovery.output_limit`).
fn output_too_long(what: &str, limit: usize) -> PortDiscoveryError {
    PortDiscoveryError::new(
        PortDiscoveryFailureKind::PortParseFailed,
        format!("{what}: the output is longer than {limit} bytes"),
    )
}

/// Waits for a reader's result until `deadline`, stopping early on cancellation and looking at
/// the cancel flag at least every `poll`. A result that already arrived is taken even at the
/// deadline. The cancel flag is read after every receive attempt, whatever it gave, so a cancel
/// that meets the result or the deadline is `Cancelled`.
fn collect(
    rx: &mpsc::Receiver<Captured>,
    deadline: Instant,
    what: &str,
    poll: Duration,
) -> Result<Captured, PortDiscoveryError> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let received = match rx.recv_timeout(left.min(poll)) {
            Ok(captured) => captured,
            Err(mpsc::RecvTimeoutError::Disconnected) => Captured::default(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel_requested() {
                    return Err(cancelled_error(what));
                }
                if left.is_zero() {
                    return Err(PortDiscoveryError::new(
                        PortDiscoveryFailureKind::TimedOut,
                        format!("{what}: the output stayed open after the command exited"),
                    ));
                }
                continue;
            }
        };
        return settled(received, what);
    }
}

/// A received output, unless cancellation came while it arrived: a cancelled lookup does not
/// return its result, even a successful one.
fn settled(captured: Captured, what: &str) -> Result<Captured, PortDiscoveryError> {
    if cancel_requested() {
        Err(cancelled_error(what))
    } else {
        Ok(captured)
    }
}

/// Runs `cmd` through `spawn`, polls its exit, and applies cancellation and the bound. Returns
/// stdout on exit 0.
///
/// stdout and stderr are read while the child runs, so a large output cannot block it. stdout
/// over `discovery.output_limit` is refused as [`PortDiscoveryFailureKind::PortParseFailed`] as soon as
/// it passes the limit: the child is cut off then, even if it would write forever. The
/// bound and cancellation also cover the wait for the pipes to close after the child exits (a
/// descendant can hold them). One deadline, taken before the spawn, covers the spawn hook, the
/// wait for the child, and output collection; only spawning itself and reaping after a kill
/// have no bound of their own. When cancellation and the bound meet, the result is
/// `Cancelled`.
///
/// A failed command keeps the class and exit code its exit gives. Its stderr is only waited for
/// up to `discovery.stderr_grace` (within the deadline): stderr only feeds the diagnostic
/// detail, so a descendant holding it open does not keep the lookup. If it does not arrive, or
/// cancellation comes, the detail goes without it.
pub(crate) fn run_capture_with_budget(
    mut cmd: Command,
    budget: Duration,
    what: &str,
    spawn: &SpawnHook,
    limits: Limits,
) -> Result<String, PortDiscoveryError> {
    // Cancellation before the budget: when both hold, "the user stopped it" is the more exact
    // fact. Neither starts a new ssh.
    if cancel_requested() {
        return Err(cancelled_error(what));
    }
    if budget.is_zero() {
        return Err(PortDiscoveryError::new(
            PortDiscoveryFailureKind::TimedOut,
            format!("{what}: the discovery budget is used up; ssh is not started"),
        ));
    }
    let started = Instant::now();
    let deadline = started + budget;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn.spawn(cmd).map_err(|e| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            format!("{what}: spawn failed: {e}"),
        )
    })?;
    let (stdout, stderr_rx) = match start_readers(&mut child, limits) {
        Ok(rx) => rx,
        Err(e) => {
            kill_and_reap(&mut child);
            return Err(PortDiscoveryError::new(
                PortDiscoveryFailureKind::SshConnectionFailed,
                format!("{what}: the output reader could not start: {e}"),
            ));
        }
    };
    // Hand the child to the cancel scope, so the bound watch and an outside cancel share it
    // and whichever comes first can cut it off.
    let mut stdout = Stdout {
        rx: stdout,
        early: None,
    };
    let mut owner = ChildOwner::adopt(child, what)?;
    let status = match wait_with_timeout(&mut owner, deadline, &mut || stdout.overflowed(), limits)
    {
        WaitEnd::Expired => return Err(cut_off(owner, what, budget)),
        WaitEnd::Stopped => match exit_after_overflow(owner, deadline, what, limits)? {
            Some(status) => status,
            None => return Err(output_too_long(what, limits.output_limit)),
        },
        WaitEnd::Ended => {
            let mut child = owner.finish(what)?;
            child.wait().map_err(|e| {
                PortDiscoveryError::new(
                    PortDiscoveryFailureKind::SshConnectionFailed,
                    format!("{what}: reaping the command failed: {e}"),
                )
            })?
        }
    };
    let wait_for = |rx: Option<mpsc::Receiver<Captured>>, until: Instant| match rx {
        Some(rx) => collect(&rx, until, what, limits.poll_max),
        None => Ok(Captured::default()),
    };
    // A child that succeeded with too much output, or was killed by a signal after its stdout
    // closed (SIGPIPE), failed because of its output size. An exit code of its own (3: the
    // instance exited) still decides the class below.
    if stdout.overflowed() && (status.success() || status.code().is_none()) {
        return Err(if cancel_requested() {
            cancelled_error(what)
        } else {
            output_too_long(what, limits.output_limit)
        });
    }
    if !status.success() {
        // The exit settles the class; stderr is only a diagnostic and cannot override it.
        let code = status.code();
        let elapsed = started.elapsed();
        let stderr = match wait_for(
            stderr_rx,
            deadline.min(Instant::now() + limits.stderr_grace),
        ) {
            Ok(stderr) => sanitize_stderr(
                &String::from_utf8_lossy(&stderr.bytes),
                stderr.overflowed,
                limits.stderr_detail_limit,
            ),
            Err(e) => format!("(stderr not collected: {})", e.kind()),
        };
        let mut error = PortDiscoveryError::new(
            refine_with_elapsed(classify_by_exit_code(code), elapsed, limits.connect_timeout),
            format!("{what}: code={code:?} elapsed={elapsed:?}: {stderr}"),
        );
        error.exit_code = code;
        return Err(error);
    }
    let stdout = stdout.collect(deadline, what, limits.poll_max)?;
    if stdout.overflowed {
        return Err(output_too_long(what, limits.output_limit));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

/// Options every discovery child forces before the target's options: no connection sharing,
/// so the child does not run its command over a master process of the user's config
/// (docs/design/ssh-backends.md "시스템 ssh 백엔드: 기능마다 자식 하나").
pub(crate) const DISCOVERY_FORCED_OPTIONS: [&str; 2] = ["ControlMaster=no", "ControlPath=none"];

/// The system backend's [`RemoteRun`]: one `ssh` child per command, through the launcher's
/// [`SpawnHook`].
pub(crate) struct SystemRun<'a> {
    pub(crate) launcher: &'a SshLauncher,
    pub(crate) target: &'a SshTarget,
}

impl RemoteRun for SystemRun<'_> {
    fn what(&self) -> String {
        format!("ssh({})", self.launcher.program().display())
    }

    fn run(
        &self,
        remote_argv: &[&str],
        budget: Duration,
        what: &str,
        limits: Limits,
    ) -> Result<String, PortDiscoveryError> {
        let cmd = self
            .launcher
            .command(self.target, &DISCOVERY_FORCED_OPTIONS, &[], remote_argv);
        run_capture_with_budget(cmd, budget, what, self.launcher.spawn_hook(), limits)
    }
}

/// Reads the remote record and decodes it with `decode`.
///
/// An explicit path in `config` is read first and alone. Otherwise `mode` decides; `Auto`
/// returns the first success, stops at cancellation and at an exited instance, and otherwise
/// returns the most telling failure. A command that succeeds with output the decoder refuses
/// is a failure. The steps share `discovery.total_timeout` of the launcher's settings (by default
/// [`crate::PORT_DISCOVERY_TOTAL_TIMEOUT`]) and each is polled for at most
/// `discovery.step_timeout` (by default [`crate::PORT_DISCOVERY_STEP_TIMEOUT`]), output collection
/// included. Only spawning itself and
/// the reap after a kill have no bound of their own. It blocks; call it from a worker. A [`SshCancel`] installed on the
/// calling thread with [`SshCancel::scope`] stops it.
pub fn discover_remote_port<T, E>(
    launcher: &SshLauncher,
    target: &SshTarget,
    mode: PortMode,
    config: &PortDiscoveryConfig,
    decode: impl Fn(&str) -> Result<T, E>,
) -> Result<T, PortDiscoveryError> {
    let runner = SystemRun { launcher, target };
    let probe = Probe::new(&runner, launcher.settings(), target, config);
    crate::discovery::discover(&probe, mode, &decode)
}

/// Detects which mode reaches the remote side: the first mode of the Auto chain that succeeds.
///
/// A mode whose subcommand reports an exited instance ([`PortDiscoveryFailureKind::RemoteInstanceExited`])
/// counts as reached. The explicit path of `config` is not used; this finds a mode. It uses the
/// same budget and bounds as [`discover_remote_port`], blocks, and stops on cancellation.
pub fn detect_port_mode<T, E>(
    launcher: &SshLauncher,
    target: &SshTarget,
    config: &PortDiscoveryConfig,
    decode: impl Fn(&str) -> Result<T, E>,
) -> Result<PortMode, PortDiscoveryError> {
    let runner = SystemRun { launcher, target };
    let probe = Probe::new(&runner, launcher.settings(), target, config);
    crate::discovery::detect(&probe, &decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::*;
    use crate::settings::SshSettings;
    use crate::test_helper::{Orphan, helper_command, process_exists, process_runs};
    use crate::{PORT_DISCOVERY_STEP_TIMEOUT, PORT_DISCOVERY_TOTAL_TIMEOUT, SSH_CONNECT_TIMEOUT};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The default `discovery.stderr_detail_limit` and `discovery.stderr_limit`
    /// ([`the_test_limits_are_the_defaults`]).
    const STDERR_DETAIL_LIMIT: usize = 512;
    const STDERR_LIMIT: usize = 16 * 1024;

    /// Records the targets of the events a thread sends.
    struct Targets(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for Targets {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let target = event.metadata().target().to_string();
            self.0.lock().expect("targets").push(target);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A discovery failure logs under the target the module had before it moved into
    /// `system`, so a filter on `tasty_net_ssh::discovery` still selects it.
    #[test]
    fn a_discovery_failure_logs_under_the_old_module_target() {
        let targets = std::sync::Arc::default();
        let recorder = Targets(std::sync::Arc::clone(&targets));
        let _error = tracing::subscriber::with_default(recorder, || {
            PortDiscoveryError::new(PortDiscoveryFailureKind::TimedOut, "test")
        });
        let targets = targets.lock().expect("targets");
        assert_eq!(*targets, ["tasty_net_ssh::discovery"]);
    }

    /// The values of the default settings.
    fn limits() -> Limits {
        Limits::new(&SshSettings::default(), &SshTarget::default())
    }

    // The functions below run with the default settings.

    fn run_capture_with_budget(
        cmd: Command,
        budget: Duration,
        what: &str,
        spawn: &SpawnHook,
    ) -> Result<String, PortDiscoveryError> {
        super::run_capture_with_budget(cmd, budget, what, spawn, limits())
    }

    fn sanitize_stderr(stderr: &str, cut: bool) -> String {
        super::sanitize_stderr(stderr, cut, limits().stderr_detail_limit)
    }

    fn refine_with_elapsed(
        kind: PortDiscoveryFailureKind,
        elapsed: Duration,
    ) -> PortDiscoveryFailureKind {
        super::refine_with_elapsed(kind, elapsed, limits().connect_timeout)
    }

    fn collect(
        rx: &mpsc::Receiver<Captured>,
        deadline: Instant,
        what: &str,
    ) -> Result<Captured, PortDiscoveryError> {
        super::collect(rx, deadline, what, limits().poll_max)
    }

    fn wait_with_timeout(
        owner: &mut ChildOwner,
        deadline: Instant,
        stop: &mut dyn FnMut() -> bool,
    ) -> WaitEnd {
        super::wait_with_timeout(owner, deadline, stop, limits())
    }

    /// The connect timeout that classifies a slow failure is the one ssh runs with: a
    /// `ConnectTimeout` of the target wins over `ssh.connect_timeout`.
    #[test]
    fn a_slow_failure_is_judged_by_the_connect_timeout_in_effect() {
        let failed = PortDiscoveryFailureKind::SshConnectionFailed;
        let elapsed = Duration::from_secs(15);
        let mut target = SshTarget::parse("host");
        let judge = |settings: &SshSettings, target: &SshTarget| {
            super::refine_with_elapsed(
                failed,
                elapsed,
                Limits::new(settings, target).connect_timeout,
            )
        };
        assert_eq!(
            judge(&SshSettings::default(), &target),
            PortDiscoveryFailureKind::TimedOut
        );
        let mut longer = SshSettings::new();
        longer
            .set(
                "ssh.connect_timeout",
                crate::settings::Value::Duration(Duration::from_secs(30)),
            )
            .unwrap();
        assert_eq!(judge(&longer, &target), failed);
        target.extra_options = vec!["connecttimeout 60".into()];
        assert_eq!(judge(&SshSettings::default(), &target), failed);
        target.extra_options = vec!["ConnectTimeout=5".into()];
        assert_eq!(judge(&longer, &target), PortDiscoveryFailureKind::TimedOut);
    }

    /// A `poll_min` of 1 s with a `poll_max` of 1 ms polls at 1 ms. The child runs on, so the
    /// first exit check sees it running and the wait goes through its first sleep before the
    /// second look (where `stop` ends it).
    #[test]
    fn the_first_poll_is_cut_to_the_longest_poll() {
        let mut l = limits();
        l.poll_min = Duration::from_secs(1);
        l.poll_max = Duration::from_millis(1);
        let child = helper_command("sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut owner = ChildOwner::Local(child);
        let mut looks = Vec::new();
        let end = super::wait_with_timeout(
            &mut owner,
            Instant::now() + Duration::from_secs(5),
            &mut || {
                looks.push(Instant::now());
                looks.len() > 1
            },
            l,
        );
        assert_eq!(end, WaitEnd::Stopped);
        let first_sleep = looks[1].duration_since(looks[0]);
        assert!(first_sleep < Duration::from_millis(900), "{first_sleep:?}");
        if let Ok(mut child) = owner.finish("poll") {
            kill_and_reap(&mut child);
        }
    }

    /// After an overflow, a grace of 1 ms with polls of 1 s cuts a running child off after about
    /// the grace, not the poll.
    #[test]
    fn the_overflow_grace_is_not_stretched_by_the_poll() {
        let mut l = limits();
        l.overflow_exit_grace = Duration::from_millis(1);
        l.poll_min = Duration::from_secs(1);
        l.poll_max = Duration::from_secs(1);
        let child = helper_command("sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let started = Instant::now();
        let status = exit_after_overflow(
            ChildOwner::Local(child),
            started + Duration::from_secs(30),
            "overflow",
            l,
        )
        .expect("no cancel");
        let took = started.elapsed();
        assert!(status.is_none(), "the kill ended it: {status:?}");
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    #[test]
    fn the_test_limits_are_the_defaults() {
        let l = limits();
        assert_eq!(l.stderr_detail_limit, STDERR_DETAIL_LIMIT);
        assert_eq!(l.stderr_limit, STDERR_LIMIT);
        assert_eq!(l.step_timeout, PORT_DISCOVERY_STEP_TIMEOUT);
        assert_eq!(l.connect_timeout, SSH_CONNECT_TIMEOUT);
    }

    /// A command that fails if it is ever started. Tells an early return from a spawn.
    fn unspawnable_command() -> Command {
        Command::new("/tasty-net-ssh-test/this-path-must-not-exist")
    }

    /// A child that runs for 60 s and has no descendants (on every OS).
    fn never_returns_command() -> Command {
        helper_command("sleep")
    }

    /// A spawn hook that records the PID of the child it starts.
    fn recording_hook() -> (SpawnHook, std::sync::Arc<AtomicU32>) {
        let pid = std::sync::Arc::new(AtomicU32::new(0));
        let seen = std::sync::Arc::clone(&pid);
        let hook = SpawnHook::new(move |mut cmd: Command| {
            let child = cmd.spawn()?;
            seen.store(child.id(), Ordering::Release);
            Ok(child)
        });
        (hook, pid)
    }

    fn capture(cmd: Command, budget: Duration, what: &str) -> Result<String, PortDiscoveryError> {
        run_capture_with_budget(cmd, budget, what, &SpawnHook::default())
    }

    fn config() -> PortDiscoveryConfig {
        PortDiscoveryConfig {
            remote_program: "netprog".into(),
            remote_args: vec!["port".into()],
            data_dir: ".netprog-data".into(),
            port_file: "net.port".into(),
            explicit_port_file: None,
        }
    }

    #[test]
    fn port_discovery_times_out_instead_of_hanging() {
        const CEILING: Duration = Duration::from_secs(10);
        let started = Instant::now();
        let err = capture(
            never_returns_command(),
            Duration::from_millis(300),
            "no-hang test",
        )
        .expect_err("a child past the bound is an error");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut);
        let elapsed = started.elapsed();
        assert!(
            elapsed < CEILING,
            "did not return within the bound: {elapsed:?}"
        );
        assert!(!err.to_string().contains("no-hang test"));
    }

    #[test]
    fn exhausted_budget_skips_spawning_ssh() {
        // A started unspawnable command would give SshConnectionFailed.
        let err = capture(unspawnable_command(), Duration::ZERO, "exhausted")
            .expect_err("a zero budget is an error");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut, "{err}");
    }

    #[test]
    fn a_spawn_failure_is_a_connection_failure() {
        let err = capture(unspawnable_command(), Duration::from_secs(5), "spawn")
            .expect_err("cannot spawn");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::SshConnectionFailed);
        assert!(err.detail().contains("spawn failed"), "{}", err.detail());
    }

    #[test]
    fn capture_returns_stdout_within_budget() {
        let out = capture(
            helper_command("echo:45123"),
            Duration::from_secs(10),
            "echo",
        )
        .unwrap();
        assert_eq!(decode_port(&out).unwrap(), 45123);
    }

    #[test]
    fn a_failing_command_keeps_its_exit_code() {
        let err = capture(helper_command("fail:3:"), Duration::from_secs(10), "exit 3")
            .expect_err("exit 3");
        assert_eq!(err.exit_code(), Some(3));
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
        assert_eq!(
            refine_port_command_failure(err).kind(),
            PortDiscoveryFailureKind::RemoteInstanceExited
        );
    }

    #[test]
    fn the_capture_spawns_through_the_hook() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let hook = SpawnHook::new(move |mut cmd| {
            seen.fetch_add(1, Ordering::SeqCst);
            cmd.spawn()
        });
        let err =
            run_capture_with_budget(unspawnable_command(), Duration::from_secs(5), "hook", &hook)
                .expect_err("cannot spawn");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::SshConnectionFailed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_refused_output_is_a_parse_failure_without_its_text() {
        let stdout = format!("port=1\ntoken={TOKEN}\n");
        let err = decode_output("ssh(x)", &stdout, &|out: &str| {
            Err::<u16, _>(format!("token: not 64 lowercase hex in {out}"))
        })
        .expect_err("refused");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::PortParseFailed);
        assert!(err.detail().contains(&format!("{} bytes", stdout.len())));
        for text in [
            err.detail().to_string(),
            format!("{err:?}"),
            err.to_string(),
        ] {
            assert!(!text.contains(TOKEN), "{text}");
            assert!(!text.contains("token:"), "{text}");
        }
        assert_eq!(
            decode_output("ssh(x)", "45123\n", &decode_port).expect("a port"),
            45123
        );
    }

    #[test]
    fn stderr_details_drop_token_like_runs_and_keep_messages() {
        let stderr = format!("cat: /home/u/.data/net.port: No such file\ntoken: {TOKEN}\n");
        let kept = sanitize_stderr(&stderr, false);
        assert!(!kept.contains(TOKEN), "{kept}");
        assert!(kept.contains("<redacted 64 chars>"), "{kept}");
        assert!(
            kept.contains("cat: /home/u/.data/net.port: No such file"),
            "{kept}"
        );
        assert_eq!(
            sanitize_stderr("  Permission denied (publickey).\n", false),
            "Permission denied (publickey)."
        );
        let long = "x ".repeat(STDERR_DETAIL_LIMIT);
        let kept = sanitize_stderr(&long, false);
        assert_eq!(kept.chars().count(), STDERR_DETAIL_LIMIT);
        assert!(kept.ends_with("..."), "{kept}");
        let exact = "x".repeat(STDERR_DETAIL_LIMIT).replace("xx", "x ");
        assert_eq!(sanitize_stderr(&exact, false), exact.trim());
    }

    /// A stderr cut at the kept limit may end in the first part of a token: that part is
    /// redacted even though it is short.
    #[test]
    fn a_token_cut_at_the_stderr_limit_is_redacted() {
        let head = &TOKEN[..20];
        let cut = format!("no such file\ntoken: {head}");
        let kept = sanitize_stderr(&cut, true);
        assert!(!kept.contains(head), "{kept}");
        assert!(kept.ends_with("<redacted 20 chars>"), "{kept}");
        // Not cut: a short word at the end is an ordinary word.
        assert!(sanitize_stderr("exit code abc", false).ends_with("abc"));
        // Cut in whitespace: nothing to redact at the end.
        assert!(sanitize_stderr("exit code abc ", true).ends_with("abc"));
    }

    /// Through a real child: a long run fills the kept stderr and a token starts right before
    /// the cut. Neither part shows in the detail or the debug output, and the detail stays short.
    #[test]
    fn a_failing_command_keeps_no_token_part_at_the_stderr_cut() {
        // The filler, a space, then 20 token characters fill the kept bytes exactly.
        let filler = STDERR_LIMIT - 1 - 20;
        let err = capture(
            helper_command(&format!("stderr:3:{filler}:{TOKEN}")),
            Duration::from_secs(10),
            "stderr cut",
        )
        .expect_err("exit 3");
        let head = &TOKEN[..20];
        assert_eq!(err.exit_code(), Some(3));
        assert!(!err.detail().contains(head), "{}", err.detail());
        assert!(!format!("{err:?}").contains(head));
        assert!(
            err.detail().contains("<redacted 20 chars>"),
            "{}",
            err.detail()
        );
        let stderr_part = err.detail().rsplit(": ").next().expect("stderr part");
        assert!(stderr_part.chars().count() <= STDERR_DETAIL_LIMIT);
    }

    #[test]
    fn a_failing_command_keeps_no_token_from_stderr() {
        let err = capture(
            helper_command(&format!("fail:3:token: {TOKEN}")),
            Duration::from_secs(10),
            "stderr",
        )
        .expect_err("exit 3");
        assert_eq!(err.exit_code(), Some(3));
        assert!(
            err.detail().contains("<redacted 64 chars>"),
            "{}",
            err.detail()
        );
        assert!(!err.detail().contains(TOKEN), "{}", err.detail());
        assert!(!format!("{err:?}").contains(TOKEN));
    }

    #[test]
    fn deadline_step_budget_is_capped_by_remaining_total() {
        let fresh = Deadline::after(PORT_DISCOVERY_TOTAL_TIMEOUT);
        assert!(fresh.step_budget(PORT_DISCOVERY_STEP_TIMEOUT) <= PORT_DISCOVERY_STEP_TIMEOUT);
        let almost_gone = Deadline::after(Duration::from_millis(50));
        assert!(almost_gone.step_budget(PORT_DISCOVERY_STEP_TIMEOUT) <= Duration::from_millis(50));
        let expired = Deadline {
            at: Instant::now() - Duration::from_secs(1),
        };
        assert!(expired.step_budget(PORT_DISCOVERY_STEP_TIMEOUT).is_zero());
    }

    #[test]
    fn total_timeout_bounds_the_auto_chain() {
        // The step bound alone would let the three-step chain wait three times as long.
        assert!(
            PORT_DISCOVERY_TOTAL_TIMEOUT
                < PORT_DISCOVERY_STEP_TIMEOUT * AUTO_FALLBACK_CHAIN.len() as u32
        );
        // A ConnectTimeout failure ends on its own before the step bound kills it.
        assert!(PORT_DISCOVERY_STEP_TIMEOUT > SSH_CONNECT_TIMEOUT);
    }

    #[test]
    fn timed_out_display_is_not_the_raw_detail() {
        let err = PortDiscoveryError::new(
            PortDiscoveryFailureKind::TimedOut,
            "ssh(/usr/bin/ssh): killed after not finishing within 20s",
        );
        assert!(!err.to_string().contains("/usr/bin/ssh"));
        assert!(!err.to_string().contains("killed"));
        assert!(err.detail().contains("/usr/bin/ssh"));
    }

    #[test]
    fn every_kind_has_one_english_sentence() {
        for kind in [
            PortDiscoveryFailureKind::SshConnectionFailed,
            PortDiscoveryFailureKind::RemoteInstanceNotRunning,
            PortDiscoveryFailureKind::RemoteInstanceExited,
            PortDiscoveryFailureKind::PortParseFailed,
            PortDiscoveryFailureKind::TimedOut,
            PortDiscoveryFailureKind::Cancelled,
        ] {
            let text = kind.to_string();
            assert!(text.is_ascii(), "{kind:?}: {text}");
            assert!(text.ends_with('.'), "{kind:?}: {text}");
            assert_eq!(PortDiscoveryError::new(kind, "d").to_string(), text);
        }
    }

    #[test]
    fn ssh_self_timeout_is_refined_to_timed_out() {
        assert_eq!(
            refine_with_elapsed(
                PortDiscoveryFailureKind::SshConnectionFailed,
                SSH_CONNECT_TIMEOUT + Duration::from_millis(200),
            ),
            PortDiscoveryFailureKind::TimedOut
        );
        assert_eq!(
            refine_with_elapsed(
                PortDiscoveryFailureKind::SshConnectionFailed,
                Duration::from_millis(80),
            ),
            PortDiscoveryFailureKind::SshConnectionFailed
        );
        for kind in [
            PortDiscoveryFailureKind::RemoteInstanceNotRunning,
            PortDiscoveryFailureKind::PortParseFailed,
        ] {
            assert_eq!(refine_with_elapsed(kind, SSH_CONNECT_TIMEOUT * 3), kind);
        }
    }

    #[test]
    fn pick_most_informative_prefers_timed_out() {
        let errors = vec![
            PortDiscoveryError::new(PortDiscoveryFailureKind::SshConnectionFailed, "b"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::TimedOut, "a"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, "c"),
        ];
        assert_eq!(
            pick_most_informative(errors).kind(),
            PortDiscoveryFailureKind::TimedOut
        );
    }

    #[test]
    fn pick_most_informative_prefers_ssh_connection_failed() {
        let errors = vec![
            PortDiscoveryError::new(PortDiscoveryFailureKind::PortParseFailed, "a"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::SshConnectionFailed, "b"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, "c"),
        ];
        assert_eq!(
            pick_most_informative(errors).kind(),
            PortDiscoveryFailureKind::SshConnectionFailed
        );
    }

    #[test]
    fn pick_most_informative_prefers_instance_not_running_over_parse_failed() {
        let errors = vec![
            PortDiscoveryError::new(PortDiscoveryFailureKind::PortParseFailed, "empty output"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, "first"),
            PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, "second"),
        ];
        let picked = pick_most_informative(errors);
        assert_eq!(
            picked.kind(),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
        assert_eq!(
            picked.detail(),
            "first",
            "the earliest step wins in a class"
        );
        assert_eq!(
            pick_most_informative(Vec::new()).kind(),
            PortDiscoveryFailureKind::SshConnectionFailed
        );
    }

    #[test]
    fn port_mode_parse() {
        assert_eq!(PortMode::parse("auto").unwrap(), PortMode::Auto);
        assert_eq!(PortMode::parse("subcommand").unwrap(), PortMode::Subcommand);
        assert_eq!(PortMode::parse("file-unix").unwrap(), PortMode::FileUnix);
        assert_eq!(
            PortMode::parse("file-windows").unwrap(),
            PortMode::FileWindows
        );
        let err = PortMode::parse("nope").unwrap_err();
        assert!(matches!(&err, PortModeParseError::Unknown { value } if value == "nope"));
        assert_eq!(err.value(), "nope");
        assert!(err.to_string().contains("'nope'"));
        assert!(err.to_string().is_ascii());
        assert_eq!("file-unix".parse::<PortMode>(), Ok(PortMode::FileUnix));
    }

    #[test]
    fn port_mode_as_str_roundtrips_parse() {
        for m in [
            PortMode::Auto,
            PortMode::Subcommand,
            PortMode::FileUnix,
            PortMode::FileWindows,
        ] {
            assert_eq!(PortMode::parse(m.as_str()).unwrap(), m);
            assert_eq!(m.to_string(), m.as_str());
        }
    }

    #[test]
    fn decode_port_trims_and_takes_last_line() {
        assert_eq!(decode_port("45123\n"), Ok(45123));
        assert_eq!(decode_port("  45123  "), Ok(45123));
        assert_eq!(decode_port("45123\r\n"), Ok(45123));
        assert_eq!(decode_port("Last login: ...\n45123\n"), Ok(45123));
        assert_eq!(decode_port("not-a-port"), Err(PortDecodeError::NotAPort));
        assert_eq!(decode_port("70000"), Err(PortDecodeError::NotAPort));
        assert_eq!(decode_port(""), Err(PortDecodeError::Empty));
        assert_eq!(decode_port(" \r\n\n"), Err(PortDecodeError::Empty));
    }

    #[test]
    fn classify_exit_255_or_signal_is_ssh_connection_failed() {
        assert_eq!(
            classify_by_exit_code(Some(255)),
            PortDiscoveryFailureKind::SshConnectionFailed
        );
        assert_eq!(
            classify_by_exit_code(None),
            PortDiscoveryFailureKind::SshConnectionFailed
        );
    }

    #[test]
    fn classify_other_exit_codes_are_remote_instance_not_running() {
        assert_eq!(
            classify_by_exit_code(Some(1)),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
        assert_eq!(
            classify_by_exit_code(Some(127)),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
    }

    #[test]
    fn port_discovery_error_display_never_contains_raw_detail() {
        let raw = "cat: /home/user/.data/net.port: No such file or directory";
        let err = PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, raw);
        assert_eq!(err.detail(), raw);
        assert!(!err.to_string().contains("net.port"));
        assert!(!err.to_string().contains("cat:"));
    }

    #[test]
    fn only_the_port_command_exit_code_three_means_an_exited_instance() {
        let failure = |code| {
            let mut e =
                PortDiscoveryError::new(PortDiscoveryFailureKind::RemoteInstanceNotRunning, "x");
            e.exit_code = Some(code);
            refine_port_command_failure(e).kind()
        };
        assert_eq!(
            failure(PORT_COMMAND_EXITED_INSTANCE_CODE),
            PortDiscoveryFailureKind::RemoteInstanceExited
        );
        assert_eq!(
            failure(1),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
        assert!(stops_auto_chain(
            PortDiscoveryFailureKind::RemoteInstanceExited
        ));
        assert!(stops_auto_chain(PortDiscoveryFailureKind::Cancelled));
        assert!(!stops_auto_chain(
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        ));
        assert!(!stops_auto_chain(PortDiscoveryFailureKind::TimedOut));
    }

    #[test]
    fn auto_fallback_chain_order_covers_all_shells() {
        assert_eq!(
            AUTO_FALLBACK_CHAIN,
            [
                PortMode::Subcommand,
                PortMode::FileUnix,
                PortMode::FileWindows,
            ]
        );
        assert!(!AUTO_FALLBACK_CHAIN.contains(&PortMode::Auto));
    }

    /// Runs the Auto chain with every mode failing with `kind` and returns the modes it tried.
    fn auto_chain_tries(kind: PortDiscoveryFailureKind) -> (Vec<PortMode>, PortDiscoveryError) {
        let mut tried = Vec::new();
        let err = auto_chain::<(), _>(|m| {
            tried.push(m);
            Err(PortDiscoveryError::new(kind, format!("{m:?} failed")))
        })
        .unwrap_err();
        (tried, err)
    }

    #[test]
    fn the_auto_chain_stops_at_cancellation_and_an_exited_instance() {
        for kind in [
            PortDiscoveryFailureKind::Cancelled,
            PortDiscoveryFailureKind::RemoteInstanceExited,
        ] {
            let (tried, err) = auto_chain_tries(kind);
            assert_eq!(tried, [PortMode::Subcommand], "{kind:?}");
            assert_eq!(err.kind(), kind);
        }
        let (tried, err) = auto_chain_tries(PortDiscoveryFailureKind::RemoteInstanceNotRunning);
        assert_eq!(tried, AUTO_FALLBACK_CHAIN.to_vec());
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
    }

    #[test]
    fn the_auto_chain_returns_the_first_found_record() {
        let mut tried = Vec::new();
        let found = auto_chain(|m| {
            tried.push(m);
            if m == PortMode::FileUnix {
                Ok(45123)
            } else {
                Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::RemoteInstanceNotRunning,
                    "no record",
                ))
            }
        });
        assert_eq!(found.unwrap(), 45123);
        assert_eq!(tried, [PortMode::Subcommand, PortMode::FileUnix]);
    }

    #[test]
    fn detect_first_success_records_first_passing_mode() {
        let order = std::cell::RefCell::new(Vec::new());
        let mode = detect_first_success(|m| {
            order.borrow_mut().push(m);
            if m == PortMode::FileWindows {
                Ok(())
            } else {
                Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::RemoteInstanceNotRunning,
                    format!("probe {m:?} failed"),
                ))
            }
        })
        .unwrap();
        assert_eq!(mode, PortMode::FileWindows);
        assert_eq!(*order.borrow(), AUTO_FALLBACK_CHAIN.to_vec());
    }

    #[test]
    fn detect_first_success_takes_earliest_mode() {
        let mut calls = 0;
        let mode = detect_first_success(|_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(mode, PortMode::Subcommand);
        assert_eq!(calls, 1);
    }

    #[test]
    fn detect_first_success_counts_an_exited_instance_as_reached() {
        let mut calls = 0;
        let mode = detect_first_success(|_| {
            calls += 1;
            Err(PortDiscoveryError::new(
                PortDiscoveryFailureKind::RemoteInstanceExited,
                "exited",
            ))
        })
        .unwrap();
        assert_eq!(mode, PortMode::Subcommand);
        assert_eq!(calls, 1);
    }

    #[test]
    fn detect_first_success_stops_at_cancellation() {
        let mut calls = 0;
        let err = detect_first_success(|_| {
            calls += 1;
            Err(cancelled_error("detect"))
        })
        .unwrap_err();
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
        assert_eq!(calls, 1);
    }

    #[test]
    fn detect_first_success_all_fail_is_err() {
        let r = detect_first_success(|m| {
            Err(PortDiscoveryError::new(
                PortDiscoveryFailureKind::RemoteInstanceNotRunning,
                format!("{m:?} down"),
            ))
        });
        assert_eq!(
            r.unwrap_err().kind(),
            PortDiscoveryFailureKind::RemoteInstanceNotRunning
        );
    }

    #[test]
    fn explicit_port_file_prefers_cat_then_type() {
        let cmds = explicit_file_commands("/data/x/net.port");
        assert_eq!(cmds[0], "cat /data/x/net.port");
        assert_eq!(cmds[1], "type \"/data/x/net.port\"");
    }

    #[test]
    fn values_with_spaces_or_quotes_are_quoted_for_the_remote_shell() {
        assert_eq!(sh_quote("netprog"), "netprog");
        assert_eq!(sh_quote("/opt/app dir/netprog"), "'/opt/app dir/netprog'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_path("~/.data/net.port"), "~/.data/net.port");
        assert_eq!(sh_path("~/my data/net.port"), "~/'my data/net.port'");
        assert_eq!(sh_path("/srv/my data/net.port"), "'/srv/my data/net.port'");
        assert_eq!(cmd_quote("C:\\data\\net.port"), "\"C:\\data\\net.port\"");
        assert_eq!(
            cmd_quote("C:\\my data\\net.port"),
            "\"C:\\my data\\net.port\""
        );
        assert_eq!(
            explicit_file_commands("/srv/my data/net.port"),
            [
                "cat '/srv/my data/net.port'".to_string(),
                "type \"/srv/my data/net.port\"".to_string()
            ]
        );
        let mut c = config();
        c.data_dir = "App Data".into();
        assert_eq!(c.file_command(false), "cat ~/'App Data/net.port'");
        assert_eq!(
            c.file_command(true),
            "type \"%USERPROFILE%\\App Data\\net.port\""
        );
    }

    #[test]
    fn file_commands_use_the_configured_names() {
        let c = config();
        assert_eq!(c.file_command(false), "cat ~/.netprog-data/net.port");
        assert_eq!(
            c.file_command(true),
            "type \"%USERPROFILE%\\.netprog-data\\net.port\""
        );
    }

    /// The program word goes unquoted when it can, so a Windows path runs under PowerShell as
    /// tasty sent it; arguments keep the sh quoting.
    #[test]
    fn the_program_word_stays_plain_where_it_can() {
        let mut c = config();
        c.remote_program = "C:\\Tools\\netprog.exe".into();
        assert_eq!(
            c.subcommand_words().join(" "),
            "C:\\Tools\\netprog.exe port"
        );
        c.remote_program = "/opt/net/bin/netprog".into();
        c.remote_args = vec!["record".into(), "--json".into(), "~/x".into()];
        assert_eq!(
            c.subcommand_words().join(" "),
            "/opt/net/bin/netprog record --json '~/x'"
        );
        c.remote_program = "C:\\Program Files\\netprog.exe".into();
        c.remote_args = vec!["port".into()];
        assert_eq!(
            c.subcommand_words().join(" "),
            "'C:\\Program Files\\netprog.exe' port"
        );
        // Every character outside the program set quotes it, `+` and `%` too, though an argument
        // would keep them plain.
        for program in [
            "~/bin/netprog",
            "net,prog",
            "a=b",
            "@net",
            "net+prog",
            "net%prog",
        ] {
            c.remote_program = program.into();
            assert_eq!(c.subcommand_words()[0], format!("'{program}'"), "{program}");
        }
    }

    /// The characters each shell reads differently are quoted for that shell.
    #[test]
    fn shell_specific_characters_are_quoted() {
        // sh: `\` escapes, `=` assigns as the first word, `~` expands. PowerShell: `,` makes an
        // array, `@` splats.
        for word in ["/srv/a\\b", "a,b", "a=b", "~/x", "x~y", "@args"] {
            assert_eq!(sh_quote(word), format!("'{word}'"), "{word}");
        }
        assert_eq!(sh_quote("--json"), "--json");
        assert_eq!(sh_quote("/opt/net/bin/netprog"), "/opt/net/bin/netprog");
        // A record path keeps only its leading `~/` outside the quotes.
        assert_eq!(sh_path("~/a\\b/net.port"), "~/'a\\b/net.port'");
        assert_eq!(sh_path("~/a,b/net.port"), "~/'a,b/net.port'");
        assert_eq!(sh_path("/srv/a=b/net.port"), "'/srv/a=b/net.port'");
        // cmd: every `type` path is quoted, so `,` `=` `;` and an expanded `%USERPROFILE%` with a
        // space stay one word.
        for path in [
            "C:\\a,b\\net.port",
            "C:\\a=b\\net.port",
            "C:\\a;b\\net.port",
        ] {
            assert_eq!(cmd_quote(path), format!("\"{path}\""), "{path}");
        }
        let mut c = config();
        c.data_dir = "a=b,c".into();
        assert_eq!(c.file_command(false), "cat ~/'a=b,c/net.port'");
        assert_eq!(
            c.file_command(true),
            "type \"%USERPROFILE%\\a=b,c\\net.port\""
        );
    }

    /// The quoted words, run through a real `sh -c`, come out as the words that went in. The
    /// subcommand words stay literal (`~/x` too); a record path's leading `~/` expands to HOME.
    #[cfg(unix)]
    #[test]
    fn quoted_words_survive_a_real_shell() {
        let words = [
            "/srv/a\\b",
            "a,b",
            "a=b",
            "~/x",
            "it's",
            "two words",
            "%PATH%",
            "$HOME",
            "*",
            "",
        ];
        let quoted: Vec<String> = words.iter().map(|w| sh_quote(w)).collect();
        let script = format!("printf '%s\\n' {}", quoted.join(" "));
        let out = Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("sh");
        assert!(out.status.success());
        let got = String::from_utf8(out.stdout).expect("utf-8");
        let expected: String = words.iter().map(|w| format!("{w}\n")).collect();
        assert_eq!(got, expected);

        let home = std::env::var("HOME").expect("HOME");
        let path = sh_path("~/my dir/a\\b,c=d");
        let out = Command::new("sh")
            .args(["-c", &format!("printf '%s' {path}")])
            .output()
            .expect("sh");
        assert_eq!(
            String::from_utf8(out.stdout).expect("utf-8"),
            format!("{home}/my dir/a\\b,c=d")
        );
    }

    #[test]
    fn cancelled_scope_skips_spawn() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        handle.cancel();
        let err = capture(unspawnable_command(), Duration::from_secs(10), "cancelled")
            .expect_err("cancelled scope");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled, "{err}");
    }

    #[test]
    fn cancel_wins_over_exhausted_budget() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        handle.cancel();
        let err = capture(never_returns_command(), Duration::ZERO, "both")
            .expect_err("cancelled and no budget");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
    }

    #[test]
    fn uncancelled_scope_still_times_out() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let err = capture(
            never_returns_command(),
            Duration::from_millis(300),
            "still timeout",
        )
        .expect_err("past the bound");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut);
        assert!(!handle.is_cancelled());
    }

    /// An outside cancel cuts a running child before the bound.
    #[test]
    fn external_cancel_cuts_child_before_budget() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let killer = handle.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            killer.cancel();
        });
        const CEILING: Duration = Duration::from_secs(5);
        let started = Instant::now();
        let err = capture(
            never_returns_command(),
            Duration::from_secs(10),
            "external cancel",
        )
        .expect_err("cancelled lookup");
        t.join().expect("killer thread");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
        let elapsed = started.elapsed();
        assert!(elapsed < CEILING, "cut before the bound: {elapsed:?}");
    }

    /// A large output is drained while the child runs: the command ends at once and the output
    /// is refused for its size, not reported as a timeout.
    #[test]
    fn a_large_output_is_refused_not_timed_out() {
        let started = Instant::now();
        let err = capture(
            helper_command("flood:1048576"),
            Duration::from_secs(20),
            "flood",
        )
        .expect_err("too long");
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::PortParseFailed,
            "{}",
            err.detail()
        );
        assert!(err.detail().contains("longer than"), "{}", err.detail());
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    /// A child reaped after the kill keeps an exit code of its own; a death by the kill (a signal
    /// on Unix, the kill's exit code 1 on Windows) does not count as one.
    #[test]
    fn an_exit_just_before_the_kill_keeps_its_code() {
        // Unix: SIGKILL leaves no code.
        assert!(exited_on_its_own(Some(3), None));
        assert!(exited_on_its_own(Some(0), None));
        assert!(!exited_on_its_own(None, None));
        // Windows: TerminateProcess leaves 1.
        assert!(exited_on_its_own(Some(3), Some(1)));
        assert!(!exited_on_its_own(Some(1), Some(1)));
        assert!(!exited_on_its_own(None, Some(1)));
    }

    /// A command that writes past the limit and then exits 3 keeps its exit code: the overflow
    /// does not hide that the instance exited.
    #[test]
    fn an_overflow_does_not_hide_exit_3() {
        let started = Instant::now();
        let err = capture(
            helper_command("flood:200000:3"),
            Duration::from_secs(10),
            "flood exit 3",
        )
        .expect_err("exit 3");
        assert_eq!(err.exit_code(), Some(3), "{}", err.detail());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            refine_port_command_failure(err).kind(),
            PortDiscoveryFailureKind::RemoteInstanceExited
        );
    }

    /// An output that never ends is cut off as soon as it passes the limit, long before the
    /// budget.
    #[test]
    fn an_endless_output_is_refused_at_the_limit() {
        let started = Instant::now();
        let err = capture(
            helper_command("flood:forever"),
            Duration::from_secs(20),
            "endless",
        )
        .expect_err("too long");
        let elapsed = started.elapsed();
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::PortParseFailed,
            "{}",
            err.detail()
        );
        assert!(err.detail().contains("longer than"), "{}", err.detail());
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    /// A child that stays alive past the stdout limit is killed and reaped by the collecting side.
    #[test]
    fn a_child_past_the_stdout_limit_is_killed_and_reaped() {
        let (hook, child_pid) = recording_hook();
        let started = Instant::now();
        let err = run_capture_with_budget(
            helper_command("flood:hold"),
            Duration::from_secs(20),
            "held flood",
            &hook,
        )
        .expect_err("too long");
        let elapsed = started.elapsed();
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::PortParseFailed,
            "{}",
            err.detail()
        );
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        let pid = child_pid.load(Ordering::Acquire);
        assert_ne!(pid, 0);
        assert!(
            !process_exists(pid),
            "the child {pid} is left (running or a zombie)"
        );
    }

    /// A descendant that keeps stdout open after the command exits is bounded by the budget.
    #[test]
    fn an_output_held_open_by_a_descendant_times_out_within_the_budget() {
        let mut orphan = Orphan::new("held-stdout");
        let started = Instant::now();
        let err = capture(orphan.command(30, 0), Duration::from_millis(1500), "orphan")
            .expect_err("held open");
        let elapsed = started.elapsed();
        assert!(orphan.end(), "the grandchild is left running");
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::TimedOut,
            "{}",
            err.detail()
        );
        assert!(err.detail().contains("stayed open"), "{}", err.detail());
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    /// A command that exits 3 while a descendant holds its stderr keeps its exit code and class:
    /// the diagnostic stderr does not turn it into a timeout.
    #[test]
    fn a_held_stderr_does_not_override_the_exit_code() {
        let mut orphan = Orphan::new("held-stderr");
        let started = Instant::now();
        let err = capture(
            orphan.command(30, 3),
            Duration::from_secs(10),
            "exit 3 held",
        )
        .expect_err("exit 3");
        let elapsed = started.elapsed();
        assert!(orphan.end(), "the grandchild is left running");
        assert_eq!(err.exit_code(), Some(3), "{}", err.detail());
        assert!(
            err.detail().contains("stderr not collected"),
            "{}",
            err.detail()
        );
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(
            refine_port_command_failure(err).kind(),
            PortDiscoveryFailureKind::RemoteInstanceExited
        );
    }

    /// A result already on the channel is taken even when the deadline has passed.
    #[test]
    fn collect_takes_an_arrived_result_at_the_deadline() {
        let (tx, rx) = mpsc::channel();
        tx.send(Captured {
            bytes: b"45123".to_vec(),
            overflowed: false,
        })
        .expect("send");
        let past = Instant::now() - Duration::from_millis(1);
        let got = collect(&rx, past, "late").expect("an arrived result");
        assert_eq!(got.bytes, b"45123");
        let (_tx, empty) = mpsc::channel::<Captured>();
        assert_eq!(
            collect(&empty, past, "late")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            PortDiscoveryFailureKind::TimedOut
        );
    }

    /// An output that arrives together with cancellation is not returned.
    #[test]
    fn a_result_meeting_cancellation_is_cancelled() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let (tx, rx) = mpsc::channel();
        tx.send(Captured {
            bytes: b"45123".to_vec(),
            overflowed: false,
        })
        .expect("send");
        handle.request_cancel();
        let far = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            collect(&rx, far, "late cancel")
                .map(|_| ())
                .unwrap_err()
                .kind(),
            PortDiscoveryFailureKind::Cancelled
        );
        let early = Stdout {
            rx: None,
            early: Some(Captured::default()),
        };
        assert_eq!(
            early
                .collect(far, "late cancel", limits().poll_max)
                .map(|_| ())
                .unwrap_err()
                .kind(),
            PortDiscoveryFailureKind::Cancelled
        );
    }

    /// A child that cancellation took before the bound's kill is reported as cancelled.
    #[test]
    fn a_child_taken_by_cancellation_is_not_a_timeout() {
        let handle = SshCancel::new();
        let child = never_returns_command()
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn");
        let id = handle.register(child).expect("register");
        // Taken out the way cancel takes it, before the flag is read.
        let mut taken = handle.reclaim(id).expect("registered");
        kill_and_reap(&mut taken);
        let owner = ChildOwner::Scoped(handle, id);
        let err = cut_off(owner, "taken", Duration::from_secs(1));
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);

        let child = never_returns_command()
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn");
        let err = cut_off(ChildOwner::Local(child), "expired", Duration::from_secs(1));
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut);
    }

    /// A cancel set while collect waits on an empty channel, or already set at the deadline,
    /// gives `Cancelled`. The orders here can pass the flag check after an empty receive, but the
    /// test does not force the cancel to land between that check and the deadline's empty
    /// receive; it checks the result in the orders it can produce.
    #[test]
    fn a_cancel_while_collect_waits_gives_cancelled() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let (_tx, rx) = mpsc::channel::<Captured>();
        let killer = handle.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            killer.request_cancel();
        });
        let started = Instant::now();
        let err = collect(&rx, started + Duration::from_secs(5), "waiting")
            .map(|_| ())
            .unwrap_err();
        t.join().expect("killer thread");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));

        // At the deadline, a cancel already set wins over the timeout.
        let past = Instant::now() - Duration::from_millis(1);
        let err = collect(&rx, past, "at the deadline")
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
    }

    /// The spawn hook's time comes out of the budget: the child is not given a fresh one.
    #[test]
    fn the_spawn_hook_time_counts_against_the_budget() {
        let hook = SpawnHook::new(|mut cmd: Command| {
            std::thread::sleep(Duration::from_millis(600));
            cmd.spawn()
        });
        let started = Instant::now();
        let err = run_capture_with_budget(
            never_returns_command(),
            Duration::from_millis(700),
            "slow hook",
            &hook,
        )
        .expect_err("past the budget");
        let elapsed = started.elapsed();
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut);
        // A fresh budget after the hook would end near 1300 ms.
        assert!(elapsed < Duration::from_millis(1200), "{elapsed:?}");
    }

    /// A cancel after the direct child ended, while a descendant holds its output, gives
    /// `Cancelled` and returns well before the budget. The cancel may meet the wait for the held
    /// output, or the lookup just before it (taking the child back after its exit); the test does
    /// not force one of the two and checks the result for either.
    #[test]
    fn a_cancel_after_the_child_ended_with_a_held_output_gives_cancelled() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let mut orphan = Orphan::new("cancel-held");
        let (hook, child_pid) = recording_hook();
        let killer = handle.clone();
        let pid_file = orphan.pid_file().to_path_buf();
        // Cancel only once the grandchild's PID is out and the direct child has ended, so the
        // cancel does not kill the helper before it hands the grandchild's PID over.
        let t = std::thread::spawn(move || {
            let limit = Instant::now() + Duration::from_secs(10);
            loop {
                let pid = child_pid.load(Ordering::Acquire);
                if pid != 0 && pid_file.exists() && !process_runs(pid) {
                    break;
                }
                assert!(Instant::now() < limit, "the helper did not settle");
                std::thread::sleep(Duration::from_millis(10));
            }
            killer.cancel();
        });
        let started = Instant::now();
        let err = run_capture_with_budget(
            orphan.command(30, 0),
            Duration::from_secs(20),
            "orphan cancel",
            &hook,
        )
        .expect_err("cancelled");
        let elapsed = started.elapsed();
        t.join().expect("killer thread");
        assert!(orphan.end(), "the grandchild is left running");
        assert_eq!(
            err.kind(),
            PortDiscoveryFailureKind::Cancelled,
            "{}",
            err.detail()
        );
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    /// Two lookups under clones of one handle each get their own child's output, and one
    /// cancel stops both.
    #[test]
    fn one_handle_serves_concurrent_lookups() {
        let handle = SshCancel::new();
        let lookups: Vec<_> = ["45001", "45002"]
            .into_iter()
            .map(|port| {
                let h = handle.clone();
                std::thread::spawn(move || {
                    let _scope = h.scope();
                    let out = capture(
                        helper_command(&format!("echo:{port}")),
                        Duration::from_secs(20),
                        "concurrent",
                    )
                    .expect("echo");
                    (port, decode_port(&out).expect("a port"))
                })
            })
            .collect();
        for lookup in lookups {
            let (port, got) = lookup.join().expect("lookup thread");
            assert_eq!(got.to_string(), port);
        }

        let sleepers: Vec<_> = (0..2)
            .map(|_| {
                let h = handle.clone();
                std::thread::spawn(move || {
                    let _scope = h.scope();
                    capture(never_returns_command(), Duration::from_secs(20), "sleep")
                        .expect_err("cancelled")
                        .kind()
                })
            })
            .collect();
        // Cancel only once both children are registered, so the test covers taking them.
        let limit = Instant::now() + Duration::from_secs(10);
        while handle.registered() < 2 {
            assert!(Instant::now() < limit, "the children did not register");
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        handle.cancel();
        for sleeper in sleepers {
            assert_eq!(
                sleeper.join().expect("sleeper thread"),
                PortDiscoveryFailureKind::Cancelled
            );
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A child killed by cancellation is not reported as a timeout.
    #[test]
    fn cancelled_child_is_not_reported_as_timeout() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let child = never_returns_command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let mut owner = ChildOwner::adopt(child, "cancel vs timeout").expect("adopt");
        handle.cancel();
        assert_eq!(
            wait_with_timeout(
                &mut owner,
                Instant::now() + Duration::from_secs(5),
                &mut || false
            ),
            WaitEnd::Ended
        );
        let err = owner
            .finish("cancel vs timeout")
            .expect_err("cancelled child");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
    }

    /// Tests with a fake `ssh`: a shell script that logs the remote command (every argument after
    /// the destination `fake-host`, joined by spaces) and acts by it.
    #[cfg(unix)]
    mod fake_ssh {
        use super::*;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FakeSsh {
            script: PathBuf,
            log: PathBuf,
        }

        impl FakeSsh {
            /// `on_port` runs for the subcommand `netprog port` (the configured program and
            /// arguments) and the script exits after it; `on_file` runs for any other remote
            /// command. Both can read the remote command as `$tail`.
            fn new(name: &str, on_port: &str, on_file: &str) -> Self {
                static SEQ: AtomicUsize = AtomicUsize::new(0);
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir().join(format!(
                    "tasty-net-ssh-fake-{}-{seq}-{name}",
                    std::process::id()
                ));
                let log = base.with_extension("log");
                let script = format!(
                    "#!/bin/sh\nwhile [ $# -gt 0 ]; do a=\"$1\"; shift; [ \"$a\" = fake-host ] && break; done\n\
                     tail=\"$*\"\n\
                     printf '%s\\n' \"$tail\" >> '{}'\n\
                     if [ \"$tail\" = 'netprog port' ]; then {on_port}\nexit; fi\n{on_file}\n",
                    log.display()
                );
                crate::write_test_script(&base, &script);
                Self { script: base, log }
            }

            fn launcher(&self) -> SshLauncher {
                SshLauncher::new(&self.script)
            }

            /// The remote commands it was run with, in order.
            fn calls(&self) -> Vec<String> {
                std::fs::read_to_string(&self.log)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            }
        }

        impl Drop for FakeSsh {
            fn drop(&mut self) {
                for p in [&self.script, &self.log] {
                    if let Err(e) = std::fs::remove_file(p)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            target: "tasty_net_ssh::discovery::tests::fake_ssh",
                            "fake ssh cleanup failed: {e}",
                        );
                    }
                }
            }
        }

        fn target() -> SshTarget {
            SshTarget::parse("fake-host")
        }

        fn launcher_with(fake: &FakeSsh, values: &[(&str, crate::settings::Value)]) -> SshLauncher {
            let mut settings = SshSettings::new();
            for (key, value) in values {
                settings.set(key, *value).expect("a valid setting");
            }
            fake.launcher().with_settings(settings)
        }

        /// `discovery.output_limit` of the launcher decides which output is too long.
        #[test]
        fn the_output_limit_comes_from_the_settings() {
            use crate::settings::Value;
            let fake = FakeSsh::new(
                "output-limit",
                "head -c 2000 /dev/zero | tr '\\0' x",
                "exit 1",
            );
            let run = |launcher: &SshLauncher| {
                discover_remote_port(
                    launcher,
                    &target(),
                    PortMode::Subcommand,
                    &config(),
                    decode_port,
                )
                .expect_err("not a port")
            };
            let small = run(&launcher_with(
                &fake,
                &[("discovery.output_limit", Value::Bytes(1024))],
            ));
            assert_eq!(small.kind(), PortDiscoveryFailureKind::PortParseFailed);
            assert!(
                small.detail().contains("longer than 1024 bytes"),
                "{}",
                small.detail()
            );
            let default = run(&fake.launcher());
            assert_eq!(default.kind(), PortDiscoveryFailureKind::PortParseFailed);
            assert!(
                default.detail().contains("decoder refused"),
                "{}",
                default.detail()
            );
        }

        /// Runs Auto against a fake that never answers, with the given step and total bounds,
        /// and returns the remote commands it ran and how long the call took.
        fn auto_against_silence(name: &str, step: u64, total: u64) -> (Vec<String>, Duration) {
            use crate::settings::Value;
            let fake = FakeSsh::new(name, "exec sleep 30", "exec sleep 30");
            let launcher = launcher_with(
                &fake,
                &[
                    (
                        "discovery.step_timeout",
                        Value::Duration(Duration::from_secs(step)),
                    ),
                    (
                        "discovery.total_timeout",
                        Value::Duration(Duration::from_secs(total)),
                    ),
                ],
            );
            let started = Instant::now();
            let err =
                discover_remote_port(&launcher, &target(), PortMode::Auto, &config(), decode_port)
                    .expect_err("the fake never answers");
            let took = started.elapsed();
            assert_eq!(
                err.kind(),
                PortDiscoveryFailureKind::TimedOut,
                "{}",
                err.detail()
            );
            (fake.calls(), took)
        }

        /// A step bound of 1 s under a total of 1 h ends each of the three Auto steps after
        /// about a second. With the default step bound (20 s) the call would take far longer.
        #[test]
        fn the_step_bound_comes_from_the_settings() {
            let (calls, took) = auto_against_silence("step-first", 1, 3600);
            assert_eq!(calls.len(), AUTO_FALLBACK_CHAIN.len(), "{calls:?}");
            assert!(took >= Duration::from_secs(3), "took {took:?}");
            assert!(took < Duration::from_secs(10), "took {took:?}");
        }

        /// A total of 1 s under a step bound of 1 h ends the call after the first step: the
        /// budget is used up and no other ssh starts.
        #[test]
        fn the_total_budget_comes_from_the_settings() {
            let (calls, took) = auto_against_silence("total-first", 3600, 1);
            assert_eq!(calls, ["netprog port"]);
            assert!(took >= Duration::from_secs(1), "took {took:?}");
            assert!(took < Duration::from_secs(5), "took {took:?}");
        }

        /// An exit 3 after an output past the limit still stops Auto at the subcommand.
        #[test]
        fn auto_stops_at_an_exited_instance_after_a_long_output() {
            let exited = FakeSsh::new(
                "exited-long",
                "head -c 200000 /dev/zero | tr '\\0' x; exit 3",
                "echo 45123",
            );
            let err = discover_remote_port(
                &exited.launcher(),
                &target(),
                PortMode::Auto,
                &config(),
                decode_port,
            )
            .expect_err("exited instance");
            assert_eq!(
                err.kind(),
                PortDiscoveryFailureKind::RemoteInstanceExited,
                "{}",
                err.detail()
            );
            assert_eq!(err.exit_code(), Some(3));
            assert_eq!(exited.calls(), ["netprog port"]);
        }

        /// An exit 3 whose stderr a descendant holds still stops Auto at the subcommand.
        #[test]
        fn auto_stops_at_an_exited_instance_whose_stderr_stays_open() {
            let mut orphan = crate::test_helper::Orphan::new("auto-held-stderr");
            let on_port = format!(
                "sleep 30 & echo $! > '{}'; exit 3",
                orphan.pid_file().display()
            );
            let exited = FakeSsh::new("exited-held", &on_port, "echo 45123");
            let started = Instant::now();
            let err = discover_remote_port(
                &exited.launcher(),
                &target(),
                PortMode::Auto,
                &config(),
                decode_port,
            )
            .expect_err("exited instance");
            let elapsed = started.elapsed();
            assert!(orphan.end(), "the background sleep is left running");
            assert_eq!(
                err.kind(),
                PortDiscoveryFailureKind::RemoteInstanceExited,
                "{}",
                err.detail()
            );
            assert_eq!(err.exit_code(), Some(3));
            assert_eq!(exited.calls(), ["netprog port"]);
            assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        }

        /// An exited instance stops Auto: the file modes would return the record it left.
        /// An older remote (exit 1) still falls back to the file.
        #[test]
        fn auto_discovery_stops_at_an_exited_remote_instance() {
            let exited = FakeSsh::new("exited", "exit 3", "echo 45123");
            let err = discover_remote_port(
                &exited.launcher(),
                &target(),
                PortMode::Auto,
                &config(),
                decode_port,
            )
            .expect_err("exited instance");
            assert_eq!(err.kind(), PortDiscoveryFailureKind::RemoteInstanceExited);
            assert_eq!(exited.calls(), ["netprog port"]);

            let old = FakeSsh::new("old", "exit 1", "echo 45123");
            let port = discover_remote_port(
                &old.launcher(),
                &target(),
                PortMode::Auto,
                &config(),
                decode_port,
            )
            .expect("an old remote falls back to the file");
            assert_eq!(port, 45123);
            assert_eq!(
                old.calls(),
                ["netprog port", "cat ~/.netprog-data/net.port"]
            );
        }

        /// A cancelled lookup does not move on to the next mode.
        #[test]
        fn a_cancelled_auto_discovery_does_not_try_the_next_mode() {
            let slow = FakeSsh::new("slow", "exec sleep 60", "echo 45123");
            let handle = SshCancel::new();
            let _scope = handle.scope();
            let killer = handle.clone();
            let t = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                killer.cancel();
            });
            let started = Instant::now();
            let err = discover_remote_port(
                &slow.launcher(),
                &target(),
                PortMode::Auto,
                &config(),
                decode_port,
            )
            .expect_err("cancelled");
            t.join().expect("killer thread");
            assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(
                slow.calls(),
                ["netprog port"],
                "no file mode after the cancel"
            );
        }

        /// Detection counts the mode whose subcommand reports an exited instance as reached.
        #[test]
        fn detect_port_mode_returns_the_mode_that_reported_an_exited_instance() {
            let exited = FakeSsh::new("detect-exited", "exit 3", "echo 45123");
            let mode = detect_port_mode(&exited.launcher(), &target(), &config(), decode_port)
                .expect("reached");
            assert_eq!(mode, PortMode::Subcommand);
            assert_eq!(exited.calls(), ["netprog port"]);

            let windows_only = FakeSsh::new(
                "detect-windows",
                "exit 1",
                "case \"$tail\" in type*) echo 45123;; *) exit 1;; esac",
            );
            let mode =
                detect_port_mode(&windows_only.launcher(), &target(), &config(), decode_port)
                    .expect("the Windows file works");
            assert_eq!(mode, PortMode::FileWindows);
            assert_eq!(windows_only.calls().len(), 3);
        }

        /// An explicit path is read first and alone; `type` follows a failed `cat`.
        #[test]
        fn an_explicit_port_file_is_read_before_the_chain() {
            let fake = FakeSsh::new(
                "explicit",
                "echo 1",
                "case \"$tail\" in type*) echo 45124;; *) exit 1;; esac",
            );
            let mut c = config();
            c.explicit_port_file = Some("/srv/rec/net.port".into());
            let port =
                discover_remote_port(&fake.launcher(), &target(), PortMode::Auto, &c, decode_port)
                    .expect("type works");
            assert_eq!(port, 45124);
            assert_eq!(
                fake.calls(),
                ["cat /srv/rec/net.port", "type \"/srv/rec/net.port\""]
            );
        }

        /// A remote shell reads a quoted explicit path with a space as one path.
        #[test]
        fn an_explicit_path_with_a_space_reaches_the_remote_shell_as_one_path() {
            let dir =
                std::env::temp_dir().join(format!("tasty-net-ssh-quoted {}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            let file = dir.join("net.port");
            std::fs::write(&file, "45127\n").expect("record");
            // The fake runs the remote command line with sh, as sshd hands it to the shell.
            let fake = FakeSsh::new("quoted", "exit 1", "exec sh -c \"$tail\"");
            let mut c = config();
            c.explicit_port_file = Some(file.display().to_string());
            let found =
                discover_remote_port(&fake.launcher(), &target(), PortMode::Auto, &c, decode_port);
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(
                    target: "tasty_net_ssh::discovery::tests::fake_ssh",
                    "quoted test cleanup failed: {e}",
                );
            }
            assert_eq!(found.expect("cat reads the quoted path"), 45127);
            assert_eq!(fake.calls(), [format!("cat '{}'", file.display())]);
        }

        /// The subcommand runs `remote_program` with `remote_args`, right after the destination.
        #[test]
        fn the_subcommand_uses_the_configured_program_and_arguments() {
            let fake = FakeSsh::new("argv", "echo 45125", "exit 1");
            let mut c = config();
            c.remote_program = "/opt/net/bin/netprog".into();
            c.remote_args = vec!["record".into(), "--json".into()];
            let err = discover_remote_port(
                &fake.launcher(),
                &target(),
                PortMode::Subcommand,
                &c,
                decode_port,
            )
            .expect_err("only `netprog port` answers");
            assert_eq!(
                err.kind(),
                PortDiscoveryFailureKind::RemoteInstanceNotRunning
            );
            assert_eq!(fake.calls(), ["/opt/net/bin/netprog record --json"]);

            let port = discover_remote_port(
                &fake.launcher(),
                &target(),
                PortMode::Subcommand,
                &config(),
                decode_port,
            )
            .expect("subcommand");
            assert_eq!(port, 45125);
            assert_eq!(
                fake.calls().last().map(String::as_str),
                Some("netprog port")
            );
        }

        /// Neither the refused output nor the decoder's error text is copied into the error.
        #[test]
        fn a_refused_record_stays_out_of_the_error() {
            let fake = FakeSsh::new("secret", "echo secret-token-0123", "exit 1");
            let err = discover_remote_port(
                &fake.launcher(),
                &target(),
                PortMode::Subcommand,
                &config(),
                |out: &str| Err::<u16, _>(format!("bad record: {out}")),
            )
            .expect_err("refused");
            assert_eq!(err.kind(), PortDiscoveryFailureKind::PortParseFailed);
            for text in [
                err.detail().to_string(),
                format!("{err:?}"),
                err.to_string(),
            ] {
                assert!(!text.contains("secret-token"), "{text}");
            }
        }

        /// A custom decoder gets the whole stdout and its value comes back.
        #[test]
        fn the_decoder_value_is_returned() {
            let fake = FakeSsh::new("record", "printf 'a=1\\nb=2\\n'", "exit 1");
            let lines = discover_remote_port(
                &fake.launcher(),
                &target(),
                PortMode::Subcommand,
                &config(),
                |out: &str| Ok::<_, PortDecodeError>(out.lines().count()),
            )
            .expect("decoded");
            assert_eq!(lines, 2);
        }

        /// Discovery children go through the launcher's hook.
        #[test]
        fn discovery_spawns_through_the_hook() {
            let fake = FakeSsh::new("hook", "exit 1", "echo 45126");
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&calls);
            let launcher = fake
                .launcher()
                .with_spawn_hook(SpawnHook::new(move |mut cmd| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    cmd.spawn()
                }));
            let port =
                discover_remote_port(&launcher, &target(), PortMode::Auto, &config(), decode_port)
                    .expect("file mode");
            assert_eq!(port, 45126);
            assert_eq!(calls.load(Ordering::SeqCst), 2, "subcommand and file-unix");
        }
    }
}
