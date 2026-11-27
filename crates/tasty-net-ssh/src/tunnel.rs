//! Starting and reaping `ssh -L` tunnels, and the retry interval.

use std::error::Error;
use std::fmt;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cancel::{kill_and_reap, recover_mutex};
use crate::listener::{ListenerVerdict, StderrMarks, VerdictTiming, relay_stderr};
use crate::settings::SshSettings;
use crate::{SshLauncher, SshTarget};

/// Returns a free local port and the listener holding it. Between dropping the listener and
/// ssh binding the port, another process can take it.
fn reserve_local_port() -> io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Why a tunnel did not come up.
#[derive(Debug)]
pub enum TunnelError {
    /// No local port could be reserved for the forward.
    LocalPort(io::Error),
    /// The ssh child could not be created.
    Spawn(io::Error),
    /// ssh exited before the local end was ready (authentication or forward failure, for
    /// example). Its stderr was passed on before this error returned.
    ExitedBeforeReady { status: ExitStatus },
    /// The local end did not accept connections in time.
    ReadyTimeout { local_port: u16, waited: Duration },
    /// The state of the ssh child could not be read.
    Wait(io::Error),
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalPort(_) => f.write_str("Could not reserve a local port for the SSH tunnel."),
            Self::Spawn(_) => f.write_str("Could not start ssh for the tunnel."),
            Self::ExitedBeforeReady { status } => write!(
                f,
                "ssh exited before the tunnel was ready ({status}); authentication or the port forward may have failed."
            ),
            Self::ReadyTimeout { local_port, waited } => write!(
                f,
                "The SSH tunnel on local port {local_port} was not ready within {waited:?}."
            ),
            Self::Wait(_) => f.write_str("Could not read the state of the ssh tunnel process."),
        }
    }
}

impl Error for TunnelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LocalPort(e) | Self::Spawn(e) | Self::Wait(e) => Some(e),
            Self::ExitedBeforeReady { .. } | Self::ReadyTimeout { .. } => None,
        }
    }
}

/// Owns an `ssh -L` child. Dropping it kills and reaps the child.
///
/// The remote process the tunnel forwards to runs on its own. All methods take `&self`, so
/// several channels can share one tunnel through an `Arc`; the last reference reaps it.
pub struct SshTunnel {
    child: Mutex<Child>,
    local_port: u16,
    remote_port: u16,
    /// What the stderr relay marked ([`crate::ListenerVerdict`]).
    marks: Arc<StderrMarks>,
    /// The waits of the verdicts of this tunnel, from its launcher's settings.
    verdict_timing: VerdictTiming,
}

/// Options every tunnel forces. They go before the target's extra options, and ssh keeps the
/// first value of an option, so the target's options and the ssh config cannot undo them.
/// - `ExitOnForwardFailure=yes`: exit at once if the forward fails, so the ready probe sees the
///   child die.
/// - `ControlMaster=no`, `ControlPath=none`: no connection sharing. A multiplexed forward would
///   belong to the master process, not to this child.
/// - `ClearAllForwardings=no`: a `ClearAllForwardings=yes` in the target's options or the ssh
///   config would drop the `-L` forward too (OpenSSH 9.6 `ssh -G` then shows no `localforward`).
const TUNNEL_FORCED_OPTIONS: [&str; 4] = [
    "ExitOnForwardFailure=yes",
    "ControlMaster=no",
    "ControlPath=none",
    "ClearAllForwardings=no",
];

/// Keeps ssh in the foreground, so the forward lives in the child this tunnel owns and reaps.
/// OpenSSH before 8.7 does not know the option (and cannot fork without `-f`); when it refuses
/// the option, the tunnel starts again without it.
const NO_FORK_OPTION: &str = "ForkAfterAuthentication=no";

/// The ssh command of a tunnel from `local_port` to `remote_port`.
fn tunnel_command(
    launcher: &SshLauncher,
    target: &SshTarget,
    local_port: u16,
    remote_port: u16,
    no_fork_option: bool,
) -> std::process::Command {
    let mut forced: Vec<&str> = TUNNEL_FORCED_OPTIONS.to_vec();
    if no_fork_option {
        forced.push(NO_FORK_OPTION);
    }
    // Bind only to loopback. That does not keep other users of the same host out.
    let forward = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
    // No remote command, only the tunnel.
    let flags = ["-N".to_string(), "-L".to_string(), forward];
    launcher.command(target, &forced, &flags, &[])
}

/// Why one start attempt failed.
enum AttemptError {
    Failed(TunnelError),
    /// ssh refused [`NO_FORK_OPTION`] and exited; start again without it.
    ForkOptionRefused(TunnelError),
}

impl From<TunnelError> for AttemptError {
    fn from(e: TunnelError) -> Self {
        Self::Failed(e)
    }
}

/// The tunnel child holds no invariant a panic could break; recover it so it is still reaped.
const TUNNEL_CHILD_WHAT: &str = "the ssh tunnel child";
static TUNNEL_CHILD_POISON_REPORTED: AtomicBool = AtomicBool::new(false);

impl SshTunnel {
    /// Starts `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<remote_port>` for `target` in the
    /// background, then polls until the local end accepts connections. The forced options
    /// ([`TUNNEL_FORCED_OPTIONS`], `ForkAfterAuthentication=no` where ssh knows it) win over the
    /// target's options and the ssh config.
    ///
    /// ssh's stderr goes on to this process's stderr (authentication messages stay visible;
    /// a first connection's host key and passphrase prompts go to the terminal directly). The
    /// child is created through the launcher's [`crate::SpawnHook`]. It blocks for up to
    /// `tunnel.ready_timeout` of the launcher's settings (5 s by default).
    pub fn establish(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
    ) -> Result<Self, TunnelError> {
        Self::establish_relaying_to(launcher, target, remote_port, io::stderr)
    }

    /// [`Self::establish`], passing ssh's stderr to what `sink` makes (a sink per attempt).
    fn establish_relaying_to<W: io::Write + 'static>(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
        sink: impl Fn() -> W + Send + Sync + 'static,
    ) -> Result<Self, TunnelError> {
        let sink = Arc::new(sink);
        let first = {
            let sink = Arc::clone(&sink);
            let no_fork_option = launcher.fork_option_supported();
            Self::attempt(launcher, target, remote_port, no_fork_option, move || {
                sink()
            })
        };
        match first {
            Ok(tunnel) => Ok(tunnel),
            Err(AttemptError::Failed(e)) => Err(e),
            Err(AttemptError::ForkOptionRefused(_)) => {
                let tunnel = Self::attempt(launcher, target, remote_port, false, move || sink())
                    .map_err(|e| match e {
                        AttemptError::Failed(e) | AttemptError::ForkOptionRefused(e) => e,
                    })?;
                // Remembered only once the start without it worked: a retry that fails for
                // another reason says nothing about the option.
                launcher.mark_fork_option_refused();
                Ok(tunnel)
            }
        }
    }

    /// Starts one ssh and waits until it is ready.
    fn attempt<W: io::Write + 'static>(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
        no_fork_option: bool,
        sink: impl FnOnce() -> W + Send + 'static,
    ) -> Result<Self, AttemptError> {
        // Release the reservation so ssh can bind it. Another process can take it meanwhile.
        let (reservation, local_port) = reserve_local_port().map_err(TunnelError::LocalPort)?;
        drop(reservation);
        let settings = launcher.settings();
        let max_line = settings.stderr_max_line();
        let mut cmd = tunnel_command(launcher, target, local_port, remote_port, no_fork_option);
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        let mut child = launcher
            .spawn_hook()
            .spawn(cmd)
            .map_err(TunnelError::Spawn)?;
        let marks = Arc::new(StderrMarks::default());
        let mut stderr_relay = child.stderr.take().and_then(|stderr| {
            let seen = Arc::clone(&marks);
            // The thread ends when ssh exits and stderr closes. Nobody waits for it after the
            // tunnel is ready.
            std::thread::Builder::new()
                .name("ssh-tunnel-stderr".into())
                .spawn(move || relay_stderr(stderr, sink(), &seen, max_line))
                .map_err(|e| tracing::warn!(%e, "ssh tunnel stderr relay thread could not start"))
                .ok()
        });

        let tunnel = SshTunnel {
            child: Mutex::new(child),
            local_port,
            remote_port,
            marks,
            verdict_timing: VerdictTiming::new(settings),
        };
        // On an error the tunnel drops here and the child is killed and reaped.
        match tunnel.wait_ready(&mut stderr_relay, settings) {
            Ok(()) => Ok(tunnel),
            Err(e @ TunnelError::ExitedBeforeReady { .. })
                if no_fork_option && tunnel.marks.fork_option_refused.load(Ordering::Acquire) =>
            {
                tracing::debug!("ssh does not know ForkAfterAuthentication; starting without it");
                Err(AttemptError::ForkOptionRefused(e))
            }
            Err(e) => Err(AttemptError::Failed(e)),
        }
    }

    fn lock_child(&self) -> MutexGuard<'_, Child> {
        recover_mutex(
            self.child.lock(),
            TUNNEL_CHILD_WHAT,
            &TUNNEL_CHILD_POISON_REPORTED,
        )
    }

    /// Polls the local end every `tunnel.ready_poll` (cut to the time left) until
    /// `tunnel.ready_timeout`, checking the child's exit and the deadline before each attempt, so
    /// no attempt starts after the deadline. A single `TcpStream::connect` has no timeout of its
    /// own.
    fn wait_ready(
        &self,
        stderr_relay: &mut Option<JoinHandle<()>>,
        settings: &SshSettings,
    ) -> Result<(), TunnelError> {
        let ready_timeout = settings.ready_timeout();
        let deadline = Instant::now() + ready_timeout;
        loop {
            let exited = self.lock_child().try_wait().map_err(TunnelError::Wait)?;
            if let Some(status) = exited {
                drain_stderr_relay(
                    stderr_relay.take(),
                    settings.stderr_drain_limit(),
                    settings.stderr_drain_poll(),
                );
                return Err(TunnelError::ExitedBeforeReady { status });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(TunnelError::ReadyTimeout {
                    local_port: self.local_port,
                    waited: ready_timeout,
                });
            }
            if TcpStream::connect(("127.0.0.1", self.local_port)).is_ok() {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(settings.ready_poll().min(left));
        }
    }

    /// Local end: connect to `127.0.0.1:local_port`.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The remote port the tunnel forwards to.
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Kills this tunnel and returns its child's wait result. A host calls it on its own
    /// retirement worker; dropping the tunnel does the same synchronously.
    pub fn terminate_and_reap(&self) -> io::Result<ExitStatus> {
        let mut child = self.lock_child();
        if let Err(error) = child.kill() {
            tracing::debug!(%error, "SSH tunnel kill returned before wait");
        }
        child.wait()
    }

    /// Whether the ssh child still runs (process-level drop detection).
    pub fn is_alive(&self) -> bool {
        matches!(self.lock_child().try_wait(), Ok(None))
    }

    /// The absent-listener verdict for the next channel through this tunnel. Take it right
    /// before opening that channel; it counts only ssh refusals after this call
    /// ([`ListenerVerdict`]). It stays usable after the tunnel moved elsewhere or dropped.
    pub fn listener_verdict(&self) -> ListenerVerdict {
        ListenerVerdict::new(
            self.remote_port,
            Arc::clone(&self.marks),
            self.verdict_timing,
        )
    }
}

/// Waits up to `limit` (`tunnel.stderr_drain_limit`), looking every `poll` (cut to the time
/// left), for the relay of an
/// ssh that exited before ready, so its last stderr (an authentication refusal and so on)
/// reaches the user before the error. The pipe closes when ssh exits, so the relay thread ends
/// soon. Another process that inherited the pipe (a ProxyCommand and so on) keeps it open, hence
/// the bound.
fn drain_stderr_relay(relay: Option<JoinHandle<()>>, limit: Duration, poll: Duration) {
    let Some(relay) = relay else {
        return;
    };
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if relay.is_finished() || left.is_zero() {
            break;
        }
        std::thread::sleep(poll.min(left));
    }
    if !relay.is_finished() {
        tracing::debug!("ssh tunnel stderr relay still open after the tunnel exited");
        return;
    }
    if relay.join().is_err() {
        tracing::warn!("ssh tunnel stderr relay thread panicked");
    }
}

/// Total time spent in `SshTunnel::drop` (ns). See [`tunnel_drop_totals`].
static TUNNEL_DROP_NANOS: AtomicU64 = AtomicU64::new(0);
/// Number of `SshTunnel::drop` calls. See [`tunnel_drop_totals`].
static TUNNEL_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Total time spent in `SshTunnel::drop` and how often it ran, since the process started.
/// Ordinary tunnel releases count too; measure a shutdown phase by the difference before and
/// after it.
pub fn tunnel_drop_totals() -> (Duration, u64) {
    (
        Duration::from_nanos(TUNNEL_DROP_NANOS.load(Ordering::Relaxed)),
        TUNNEL_DROP_COUNT.load(Ordering::Relaxed),
    )
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let t_drop = Instant::now();
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        kill_and_reap(child);
        TUNNEL_DROP_NANOS.fetch_add(
            u64::try_from(t_drop.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        TUNNEL_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reconnect wait that grows by a factor up to a maximum. [`Backoff::reset`] returns to the
/// minimum after a success. When to reconnect is the consumer's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backoff {
    cur: Duration,
    min: Duration,
    max: Duration,
    factor: u32,
}

impl Backoff {
    /// The default settings: starts at 500 ms, doubles, at most 30 s.
    pub fn new() -> Self {
        Self::from_settings(&SshSettings::default())
    }

    /// Starts at `backoff.initial`, multiplies by `backoff.multiplier`, at most
    /// `backoff.max_delay`. The cap holds for every wait: a first wait above it starts (and
    /// resets) at the cap.
    pub fn from_settings(settings: &SshSettings) -> Self {
        let max = settings.backoff_max_delay();
        let min = settings.backoff_initial().min(max);
        Self {
            cur: min,
            min,
            max,
            factor: settings.backoff_multiplier(),
        }
    }

    /// Sleeps for the current interval, then multiplies it by the factor (up to the maximum).
    pub fn sleep(&mut self) {
        std::thread::sleep(self.cur);
        self.advance();
    }

    /// The current interval, without sleeping. A scheduler that cannot block (a GUI main loop)
    /// uses it to compute the next retry time.
    pub fn current(&self) -> Duration {
        self.cur
    }

    /// Moves to the next interval without sleeping (multiplies by the factor, up to the
    /// maximum). [`Backoff::sleep`] is this plus a blocking sleep.
    pub fn advance(&mut self) {
        self.cur = self
            .cur
            .checked_mul(self.factor)
            .unwrap_or(self.max)
            .min(self.max);
    }

    /// Returns to the minimum after a successful connection.
    pub fn reset(&mut self) {
        self.cur = self.min;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpawnHook;
    use std::sync::atomic::AtomicUsize;

    /// A first wait above the cap starts, and resets, at the cap.
    #[test]
    fn backoff_caps_its_first_and_reset_wait() {
        use crate::settings::Value;
        let mut settings = SshSettings::new();
        settings
            .set("backoff.initial", Value::Duration(Duration::from_secs(60)))
            .unwrap()
            .set(
                "backoff.max_delay",
                Value::Duration(Duration::from_millis(1)),
            )
            .unwrap();
        let mut b = Backoff::from_settings(&settings);
        assert_eq!(b.current(), Duration::from_millis(1));
        b.advance();
        assert_eq!(b.current(), Duration::from_millis(1));
        b.reset();
        assert_eq!(b.current(), Duration::from_millis(1));
    }

    /// A drain limit of 100 ms with a poll of 1 s waits about 100 ms for a relay that stays open.
    #[test]
    fn the_stderr_drain_waits_the_limit_even_with_a_longer_poll() {
        let (keep_open, closed) = std::sync::mpsc::channel::<()>();
        let relay = std::thread::spawn(move || {
            let _ended = closed.recv_timeout(Duration::from_secs(10));
        });
        let limit = Duration::from_millis(100);
        let started = Instant::now();
        drain_stderr_relay(Some(relay), limit, Duration::from_secs(1));
        let took = started.elapsed();
        drop(keep_open);
        assert!(took >= limit, "{took:?}");
        assert!(took < Duration::from_millis(600), "{took:?}");
    }

    /// `backoff.*` give the first wait, the factor and the cap.
    #[test]
    fn backoff_takes_its_settings() {
        use crate::settings::Value;
        let mut settings = SshSettings::new();
        settings
            .set(
                "backoff.initial",
                Value::Duration(Duration::from_millis(100)),
            )
            .unwrap()
            .set("backoff.multiplier", Value::Count(3))
            .unwrap()
            .set(
                "backoff.max_delay",
                Value::Duration(Duration::from_millis(500)),
            )
            .unwrap();
        let mut b = Backoff::from_settings(&settings);
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(b.current());
            b.advance();
        }
        let ms = Duration::from_millis;
        assert_eq!(seen, [ms(100), ms(300), ms(500), ms(500)]);
        b.reset();
        assert_eq!(b.current(), ms(100));
        assert_eq!(
            Backoff::from_settings(&SshSettings::default()),
            Backoff::new()
        );
    }

    #[test]
    fn backoff_grows_and_resets() {
        let mut b = Backoff::new();
        assert_eq!(b.current(), Duration::from_millis(500));
        b.advance();
        assert_eq!(b.current(), Duration::from_secs(1));
        for _ in 0..10 {
            b.advance();
        }
        assert_eq!(
            b.current(),
            Duration::from_secs(30),
            "capped at the maximum"
        );
        b.reset();
        assert_eq!(b.current(), Duration::from_millis(500));
        assert_eq!(Backoff::default(), Backoff::new());
    }

    #[test]
    fn reserve_local_port_holds_a_usable_port() {
        let (listener, p) = reserve_local_port().unwrap();
        assert!(p > 0);
        assert!(
            TcpListener::bind(("127.0.0.1", p)).is_err(),
            "reserved port {p} must be held while the listener is alive"
        );
        drop(listener);
    }

    #[test]
    fn tunnel_errors_are_english_sentences_with_io_sources() {
        let spawn = TunnelError::Spawn(io::Error::from(io::ErrorKind::NotFound));
        assert!(spawn.source().is_some());
        let timeout = TunnelError::ReadyTimeout {
            local_port: 4000,
            waited: Duration::from_secs(5),
        };
        assert!(timeout.source().is_none());
        for e in [spawn, timeout] {
            let text = e.to_string();
            assert!(text.is_ascii() && text.ends_with('.'), "{text}");
        }
    }

    /// Counts hook calls and records the pid of each child the hook made.
    fn counting_hook() -> (SpawnHook, Arc<AtomicUsize>, Arc<Mutex<Vec<u32>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let pids = Arc::new(Mutex::new(Vec::new()));
        let (c, p) = (Arc::clone(&calls), Arc::clone(&pids));
        let hook = SpawnHook::new(move |mut cmd| {
            c.fetch_add(1, Ordering::SeqCst);
            let child = cmd.spawn()?;
            p.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(child.id());
            Ok(child)
        });
        (hook, calls, pids)
    }

    fn args_of(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The forced options come before the target's options, so ssh keeps them, and the tunnel
    /// stays in the foreground without connection sharing.
    #[test]
    fn forced_tunnel_options_come_before_the_target_options() {
        let mut target = SshTarget::parse("me@box");
        target.extra_options = vec![
            "ExitOnForwardFailure=no".into(),
            "ControlMaster=auto".into(),
            "ControlPath=~/.ssh/cm-%C".into(),
            "ForkAfterAuthentication=yes".into(),
            "ClearAllForwardings=yes".into(),
        ];
        let launcher = SshLauncher::new("ssh");
        let args = args_of(&tunnel_command(&launcher, &target, 4001, 4002, true));
        let at = |v: &str| {
            args.iter()
                .position(|a| a == v)
                .unwrap_or_else(|| panic!("{v} missing from {args:?}"))
        };
        for (forced, user) in [
            ("ExitOnForwardFailure=yes", "ExitOnForwardFailure=no"),
            ("ControlMaster=no", "ControlMaster=auto"),
            ("ControlPath=none", "ControlPath=~/.ssh/cm-%C"),
            (NO_FORK_OPTION, "ForkAfterAuthentication=yes"),
            ("ClearAllForwardings=no", "ClearAllForwardings=yes"),
        ] {
            assert!(
                at(forced) < at(user),
                "{forced} must win over {user}: {args:?}"
            );
            assert_eq!(args[at(forced) - 1], "-o");
        }
        assert_eq!(args[at("-L") + 1], "127.0.0.1:4001:127.0.0.1:4002");
        assert!(at("-N") < at("me@box"));
        assert_eq!(args.last().map(String::as_str), Some("me@box"));

        let without = args_of(&tunnel_command(&launcher, &target, 4001, 4002, false));
        assert!(!without.iter().any(|a| a == NO_FORK_OPTION), "{without:?}");
        assert!(without.iter().any(|a| a == "ExitOnForwardFailure=yes"));
    }

    /// `establish` creates its child through the hook, once.
    #[test]
    fn establish_spawns_through_the_hook_once() {
        let (hook, calls, _) = counting_hook();
        let launcher = SshLauncher::new("/tasty-net-ssh-test/no-such-ssh").with_spawn_hook(hook);
        let result = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), 45123);
        assert!(
            matches!(result, Err(TunnelError::Spawn(_))),
            "{:?}",
            result.err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Fake `ssh` tests. The early-exit fake is a shell script. The forwarding fake is this
    /// test binary run as `fake_ssh_forwarder_entry`: it listens on the `-L` local port of real
    /// TCP and forwards to the remote port, or writes ssh's channel failure line if nothing
    /// listens there.
    #[cfg(unix)]
    mod fake {
        use super::*;
        use crate::listener::RemoteListenerAbsent;
        use std::io::{Read, Write};
        use std::path::{Path, PathBuf};

        const FAKE_ARGS_ENV: &str = "TASTY_NET_SSH_FAKE_FORWARDER_ARGS";
        const FORWARDER_TEST: &str = "tunnel::tests::fake::fake_ssh_forwarder_entry";

        struct Script(PathBuf);

        impl Script {
            fn new(name: &str, body: &str) -> Self {
                static SEQ: AtomicUsize = AtomicUsize::new(0);
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tasty-net-ssh-tunnel-{}-{seq}-{name}",
                    std::process::id()
                ));
                crate::write_test_script(&path, body);
                Self(path)
            }

            fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Script {
            fn drop(&mut self) {
                if let Err(e) = std::fs::remove_file(&self.0) {
                    tracing::warn!("fake ssh cleanup failed: {e}");
                }
            }
        }

        /// The forwarding fake ssh: a script that runs this test binary in forwarder mode.
        fn forwarder() -> Script {
            forwarder_after("forwarder", "")
        }

        /// The forwarding fake, running `prelude` (shell lines) first.
        fn forwarder_after(name: &str, prelude: &str) -> Script {
            let exe = std::env::current_exe().expect("test binary path");
            Script::new(
                name,
                &format!(
                    "#!/bin/sh\n{prelude}{FAKE_ARGS_ENV}=\"$*\"\nexport {FAKE_ARGS_ENV}\n\
                     exec '{}' {FORWARDER_TEST} --exact --nocapture --test-threads=1 >/dev/null\n",
                    exe.display()
                ),
            )
        }

        /// An ssh before OpenSSH 8.7 refuses `ForkAfterAuthentication`. The tunnel starts again
        /// without it, and the launcher (and its clones) leave it out from then on.
        #[test]
        fn an_ssh_without_the_fork_option_gets_a_tunnel_without_it() {
            let old_ssh = forwarder_after(
                "old-ssh",
                "case \" $* \" in *\" ForkAfterAuthentication=no \"*)\n\
                 echo 'command-line: line 0: Bad configuration option: forkafterauthentication' >&2\n\
                 exit 255;;\nesac\n",
            );
            let argvs = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
            let seen = Arc::clone(&argvs);
            let hook = SpawnHook::new(move |mut cmd| {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(
                        cmd.get_args()
                            .map(|a| a.to_string_lossy().into_owned())
                            .collect(),
                    );
                cmd.spawn()
            });
            let launcher = SshLauncher::new(old_ssh.path()).with_spawn_hook(hook);
            let remote = echo_server();
            let first = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), remote)
                .expect("the second start works");
            let again =
                SshTunnel::establish(&launcher.clone(), &SshTarget::parse("fake-host"), remote)
                    .expect("starts without the option at once");
            let argvs = argvs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let has_option = |argv: &Vec<String>| argv.iter().any(|a| a == NO_FORK_OPTION);
            assert_eq!(argvs.len(), 3, "{argvs:?}");
            assert!(has_option(&argvs[0]));
            assert!(
                !has_option(&argvs[1]) && !has_option(&argvs[2]),
                "{argvs:?}"
            );
            drop((first, again));
        }

        /// Runs `establish` once on `ssh` and returns the argvs it started and whether the
        /// launcher still uses the option.
        fn establish_recording(ssh: &Script) -> (Vec<Vec<String>>, bool) {
            let argvs = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
            let seen = Arc::clone(&argvs);
            let hook = SpawnHook::new(move |mut cmd| {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(
                        cmd.get_args()
                            .map(|a| a.to_string_lossy().into_owned())
                            .collect(),
                    );
                cmd.spawn()
            });
            let launcher = SshLauncher::new(ssh.path()).with_spawn_hook(hook);
            let (held, remote) = reserve_local_port().expect("port");
            drop(held);
            let established =
                SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), remote);
            assert!(established.is_err(), "this ssh never forwards");
            let argvs = argvs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            (argvs, launcher.fork_option_supported())
        }

        /// A newer ssh refusing another option whose name starts the same is not taken for an
        /// old ssh: no retry, nothing remembered.
        #[test]
        fn a_longer_option_name_refusal_is_not_the_fork_option() {
            let ssh = forwarder_after(
                "other-option",
                "echo 'command-line: line 0: Bad configuration option: \
                 forkafterauthenticationextra' >&2\nexit 255\n",
            );
            let (argvs, supported) = establish_recording(&ssh);
            assert_eq!(argvs.len(), 1, "{argvs:?}");
            assert!(supported);
        }

        /// The option is not remembered as unsupported when the start without it fails too.
        #[test]
        fn a_failed_retry_does_not_remember_the_refusal() {
            let ssh = forwarder_after(
                "old-and-down",
                "case \" $* \" in *\" ForkAfterAuthentication=no \"*)\n\
                 echo 'command-line: line 0: Bad configuration option: forkafterauthentication' >&2\n\
                 exit 255;;\nesac\nexit 255\n",
            );
            let (argvs, supported) = establish_recording(&ssh);
            assert_eq!(argvs.len(), 2, "{argvs:?}");
            assert!(supported);
        }

        /// Whether no process (not even a zombie) has this pid.
        fn pid_is_gone(pid: u32) -> bool {
            let alive = std::process::Command::new("sh")
                .args(["-c", "kill -0 \"$1\" 2>/dev/null", "sh"])
                .arg(pid.to_string())
                .status()
                .expect("sh");
            !alive.success()
        }

        fn only_pid(pids: &Mutex<Vec<u32>>) -> u32 {
            let pids = pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(pids.len(), 1, "{pids:?}");
            pids[0]
        }

        /// In forwarder mode (the env var is set) this acts as `ssh -N -L` and never returns. In
        /// a normal test run it returns at once.
        #[test]
        fn fake_ssh_forwarder_entry() {
            let Ok(args) = std::env::var(FAKE_ARGS_ENV) else {
                return;
            };
            let words: Vec<&str> = args.split_whitespace().collect();
            let spec = words
                .iter()
                .position(|w| *w == "-L")
                .and_then(|i| words.get(i + 1))
                .expect("-L spec");
            let parts: Vec<&str> = spec.split(':').collect();
            let local: u16 = parts[1].parse().expect("local port");
            let remote: u16 = parts[3].parse().expect("remote port");
            let listener = match TcpListener::bind(("127.0.0.1", local)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("bind [127.0.0.1]:{local}: {e}");
                    std::process::exit(255);
                }
            };
            for (n, conn) in listener.incoming().enumerate() {
                let Ok(conn) = conn else { continue };
                std::thread::spawn(move || forward(n, conn, remote));
            }
        }

        fn forward(n: usize, client: TcpStream, remote: u16) {
            let server = match TcpStream::connect(("127.0.0.1", remote)) {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("channel {n}: open failed: connect failed: Connection refused");
                    return;
                }
            };
            let (mut c_read, mut s_write) = (
                client.try_clone().expect("clone"),
                server.try_clone().expect("clone"),
            );
            let up = std::thread::spawn(move || {
                let _copied = io::copy(&mut c_read, &mut s_write);
                let _closed = s_write.shutdown(std::net::Shutdown::Write);
            });
            let (mut s_read, mut c_write) = (server, client);
            let _copied = io::copy(&mut s_read, &mut c_write);
            let _closed = c_write.shutdown(std::net::Shutdown::Write);
            let _joined = up.join();
        }

        /// An echo server on a real port. It serves until the test process ends.
        fn echo_server() -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    std::thread::spawn(move || {
                        let mut reader = conn.try_clone().expect("clone");
                        let _copied = io::copy(&mut reader, &mut conn);
                    });
                }
            });
            port
        }

        /// A tunnel carries bytes over real ports to the remote listener, and dropping it reaps
        /// the child.
        #[test]
        fn a_tunnel_forwards_to_the_remote_port_and_drop_reaps_it() {
            let fake = forwarder();
            let (hook, calls, pids) = counting_hook();
            let launcher = SshLauncher::new(fake.path()).with_spawn_hook(hook);
            let remote = echo_server();
            let tunnel = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), remote)
                .expect("tunnel");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(tunnel.remote_port(), remote);
            assert_ne!(tunnel.local_port(), remote);
            assert!(tunnel.is_alive());

            let verdict = tunnel.listener_verdict();
            let mut conn = TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
            conn.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            conn.write_all(b"hello").expect("write");
            let mut back = [0u8; 5];
            conn.read_exact(&mut back).expect("echo");
            assert_eq!(&back, b"hello");
            // A listener is present, so a dropped connection is not explained as absent.
            assert!(!verdict.remote_listener_absent(&connection_closed()));

            let pid = only_pid(&pids);
            let (_, drops_before) = tunnel_drop_totals();
            drop(tunnel);
            assert!(tunnel_drop_totals().1 > drops_before);
            assert!(pid_is_gone(pid), "the tunnel child {pid} was not reaped");
        }

        fn connection_closed() -> io::Error {
            crate::connection_closed_error("eof")
        }

        /// Opens a channel through `tunnel` and reads until it ends without an answer (EOF or a
        /// reset). A read timeout would be a different failure and fails the test.
        fn unanswered_channel(tunnel: &SshTunnel) -> io::Error {
            let mut conn = TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
            conn.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut buf = [0u8; 1];
            match conn.read(&mut buf) {
                Ok(0) => connection_closed(),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    e
                }
                other => panic!("expected EOF or a reset, got {other:?}"),
            }
        }

        /// With nothing on the remote port, the tunnel still comes up, the first channel closes,
        /// and its verdict says the listener is absent. Once a listener accepts there, a later
        /// channel that closes without an answer is not blamed on the earlier refusals.
        #[test]
        fn an_empty_remote_port_is_judged_absent_for_that_channel_only() {
            let fake = forwarder();
            let launcher = SshLauncher::new(fake.path());
            let (held, remote) = reserve_local_port().expect("port");
            drop(held);
            let tunnel = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), remote)
                .expect("tunnel");
            let first = tunnel.listener_verdict();
            let failure = unanswered_channel(&tunnel);
            assert!(first.remote_listener_absent(&failure));
            assert_eq!(
                first.explain_failure(&failure),
                Some(RemoteListenerAbsent {
                    remote_port: remote
                })
            );

            // A listener that accepts and closes at once, so the channel opens but gets no answer.
            let listener =
                TcpListener::bind(("127.0.0.1", remote)).expect("rebind the remote port");
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    drop(conn);
                }
            });
            // Let the refusal lines of the ready probe and the first channel arrive.
            std::thread::sleep(Duration::from_millis(300));
            let second = tunnel.listener_verdict();
            let failure = unanswered_channel(&tunnel);
            assert!(
                !second.remote_listener_absent(&failure),
                "the listener is there now"
            );
            // The first verdict still covers the first channel's refusal.
            assert!(first.remote_listener_absent(&failure));

            let verdict = first;
            let status = tunnel.terminate_and_reap().expect("reaped");
            assert!(!status.success());
            assert!(!tunnel.is_alive());
            drop(tunnel);
            // The verdict handle outlives the tunnel.
            assert!(verdict.remote_listener_absent(&failure));
        }

        /// Writes each stderr chunk late, so the relay ends after ssh exits.
        struct SlowSink(Arc<Mutex<Vec<u8>>>);

        impl Write for SlowSink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(100));
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        /// A ready timeout of 100 ms with a poll of 1 s gives up after about 100 ms, and the
        /// error says the time it waited.
        #[test]
        fn the_ready_wait_is_not_stretched_by_the_poll() {
            use crate::settings::Value;
            let ssh = Script::new("never-ready-poll", "#!/bin/sh\nexec sleep 10\n");
            let mut settings = crate::SshSettings::new();
            settings
                .set(
                    "tunnel.ready_timeout",
                    Value::Duration(Duration::from_millis(100)),
                )
                .unwrap()
                .set("tunnel.ready_poll", Value::Duration(Duration::from_secs(1)))
                .unwrap();
            let launcher = SshLauncher::new(ssh.path()).with_settings(settings);
            let started = Instant::now();
            let error = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), 45123)
                .err()
                .expect("the fake never forwards");
            let took = started.elapsed();
            match error {
                TunnelError::ReadyTimeout { waited, .. } => {
                    assert_eq!(waited, Duration::from_millis(100));
                }
                other => panic!("unexpected error: {other:?}"),
            }
            assert!(took >= Duration::from_millis(100), "{took:?}");
            assert!(took < Duration::from_millis(700), "{took:?}");
        }

        /// `tunnel.ready_timeout` of the launcher bounds the wait for the local end.
        #[test]
        fn the_ready_timeout_comes_from_the_settings() {
            let ssh = Script::new("never-ready", "#!/bin/sh\nexec sleep 10\n");
            let mut settings = crate::SshSettings::new();
            settings
                .set(
                    "tunnel.ready_timeout",
                    crate::settings::Value::Duration(Duration::from_millis(300)),
                )
                .unwrap();
            let launcher = SshLauncher::new(ssh.path()).with_settings(settings);
            let started = Instant::now();
            let error = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), 45123)
                .err()
                .expect("the fake never forwards");
            match error {
                TunnelError::ReadyTimeout { waited, .. } => {
                    assert_eq!(waited, Duration::from_millis(300));
                }
                other => panic!("unexpected error: {other:?}"),
            }
            assert!(started.elapsed() < Duration::from_secs(5));
        }

        /// An ssh that exits at once gives an error, its last stderr lines are passed on before
        /// the error, and its child is reaped.
        #[test]
        fn an_early_exit_is_an_error_after_its_stderr_and_the_child_is_reaped() {
            let ssh = Script::new(
                "early-exit",
                "#!/bin/sh\necho 'first line' >&2\nsleep 0.05\necho 'Permission denied (publickey).' >&2\nexit 255\n",
            );
            let (hook, calls, pids) = counting_hook();
            let launcher = SshLauncher::new(ssh.path()).with_spawn_hook(hook);
            let relayed = Arc::new(Mutex::new(Vec::new()));
            let into = Arc::clone(&relayed);
            let result = SshTunnel::establish_relaying_to(
                &launcher,
                &SshTarget::parse("fake-host"),
                45123,
                move || SlowSink(Arc::clone(&into)),
            );
            let error = result
                .err()
                .expect("the fake ssh exits before the tunnel is ready");
            match error {
                TunnelError::ExitedBeforeReady { status } => {
                    assert_eq!(status.code(), Some(255));
                }
                other => panic!("unexpected error: {other:?}"),
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let relayed = String::from_utf8(
                relayed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )
            .unwrap();
            assert_eq!(relayed, "first line\nPermission denied (publickey).\n");
            let pid = only_pid(&pids);
            assert!(pid_is_gone(pid), "the exited child {pid} was not reaped");
        }
    }
}
