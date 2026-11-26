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
use crate::listener::{ListenerVerdict, RemoteListenerAbsent, relay_stderr};
use crate::{SshLauncher, SshTarget};

/// How long `establish` waits for the local end to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an ssh that exited before ready may take to pass its last stderr on. The pipe closes
/// when ssh exits, so the relay thread ends soon. Another process that inherited the pipe
/// (a ProxyCommand and so on) keeps it open, hence the bound.
const STDERR_DRAIN_LIMIT: Duration = Duration::from_millis(500);

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
    /// ssh's stderr had a channel connect failure line ([`crate::ListenerVerdict`]).
    channel_connect_failed: Arc<AtomicBool>,
}

/// The tunnel child holds no invariant a panic could break; recover it so it is still reaped.
const TUNNEL_CHILD_WHAT: &str = "the ssh tunnel child";
static TUNNEL_CHILD_POISON_REPORTED: AtomicBool = AtomicBool::new(false);

impl SshTunnel {
    /// Starts `ssh -o ExitOnForwardFailure=yes -N -L 127.0.0.1:<local>:127.0.0.1:<remote_port>`
    /// for `target` in the background, then polls until the local end accepts connections.
    ///
    /// ssh's stderr goes on to this process's stderr (authentication messages stay visible;
    /// a first connection's host key and passphrase prompts go to the terminal directly). The
    /// child is created through the launcher's [`crate::SpawnHook`]. It blocks for up to a few
    /// seconds.
    pub fn establish(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
    ) -> Result<Self, TunnelError> {
        Self::establish_relaying_to(launcher, target, remote_port, io::stderr)
    }

    /// [`Self::establish`], passing ssh's stderr to what `sink` makes.
    fn establish_relaying_to<W: io::Write + 'static>(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
        sink: impl FnOnce() -> W + Send + 'static,
    ) -> Result<Self, TunnelError> {
        // Release the reservation so ssh can bind it. Another process can take it meanwhile.
        let (reservation, local_port) = reserve_local_port().map_err(TunnelError::LocalPort)?;
        drop(reservation);
        // Bind only to loopback. That does not keep other users of the same host out.
        let forward = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
        let before = [
            "-o".to_string(),
            // Exit at once if the forward fails, so the ready probe sees the child die.
            "ExitOnForwardFailure=yes".to_string(),
            // No remote command, only the tunnel.
            "-N".to_string(),
            "-L".to_string(),
            forward,
        ];
        let mut cmd = launcher.command(target, &before, &[]);
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        let mut child = launcher
            .spawn_hook()
            .spawn(cmd)
            .map_err(TunnelError::Spawn)?;
        let channel_connect_failed = Arc::new(AtomicBool::new(false));
        let mut stderr_relay = child.stderr.take().and_then(|stderr| {
            let seen = Arc::clone(&channel_connect_failed);
            // The thread ends when ssh exits and stderr closes. Nobody waits for it after the
            // tunnel is ready.
            std::thread::Builder::new()
                .name("ssh-tunnel-stderr".into())
                .spawn(move || relay_stderr(stderr, sink(), &seen))
                .map_err(|e| tracing::warn!(%e, "ssh tunnel stderr relay thread could not start"))
                .ok()
        });

        let tunnel = SshTunnel {
            child: Mutex::new(child),
            local_port,
            remote_port,
            channel_connect_failed,
        };
        // On an error the tunnel drops here and the child is killed and reaped.
        tunnel.wait_ready(&mut stderr_relay)?;
        Ok(tunnel)
    }

    fn lock_child(&self) -> MutexGuard<'_, Child> {
        recover_mutex(
            self.child.lock(),
            TUNNEL_CHILD_WHAT,
            &TUNNEL_CHILD_POISON_REPORTED,
        )
    }

    /// Polls the local end, checking the deadline and the child's exit between attempts. A
    /// single `TcpStream::connect` has no timeout of its own.
    fn wait_ready(&self, stderr_relay: &mut Option<JoinHandle<()>>) -> Result<(), TunnelError> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let exited = self.lock_child().try_wait().map_err(TunnelError::Wait)?;
            if let Some(status) = exited {
                drain_stderr_relay(stderr_relay.take());
                return Err(TunnelError::ExitedBeforeReady { status });
            }
            if TcpStream::connect(("127.0.0.1", self.local_port)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(TunnelError::ReadyTimeout {
                    local_port: self.local_port,
                    waited: READY_TIMEOUT,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
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

    /// The absent-listener verdict of this tunnel. Use it to judge failures after the tunnel
    /// moved elsewhere.
    pub fn listener_verdict(&self) -> ListenerVerdict {
        ListenerVerdict::new(self.remote_port, Arc::clone(&self.channel_connect_failed))
    }

    /// Whether a request through the tunnel failed with `failure` because nothing listens on the
    /// remote port ([`ListenerVerdict::remote_listener_absent`]).
    pub fn remote_listener_absent(&self, failure: &(dyn Error + 'static)) -> bool {
        self.listener_verdict().remote_listener_absent(failure)
    }

    /// [`RemoteListenerAbsent`] if nothing listens on the remote port
    /// ([`ListenerVerdict::explain_failure`]).
    pub fn explain_failure(&self, failure: &(dyn Error + 'static)) -> Option<RemoteListenerAbsent> {
        self.listener_verdict().explain_failure(failure)
    }
}

/// Waits up to [`STDERR_DRAIN_LIMIT`] for the relay of an ssh that exited before ready, so its
/// last stderr (an authentication refusal and so on) reaches the user before the error.
fn drain_stderr_relay(relay: Option<JoinHandle<()>>) {
    let Some(relay) = relay else {
        return;
    };
    let deadline = Instant::now() + STDERR_DRAIN_LIMIT;
    while !relay.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
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

/// Reconnect wait that doubles up to a maximum. [`Backoff::reset`] returns to the minimum after
/// a success. When to reconnect is the consumer's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backoff {
    cur: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    /// Starts at 500 ms, at most 30 s.
    pub fn new() -> Self {
        Self {
            cur: Duration::from_millis(500),
            min: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }

    /// Sleeps for the current interval, then doubles it (up to the maximum).
    pub fn sleep(&mut self) {
        std::thread::sleep(self.cur);
        self.advance();
    }

    /// The current interval, without sleeping. A scheduler that cannot block (a GUI main loop)
    /// uses it to compute the next retry time.
    pub fn current(&self) -> Duration {
        self.cur
    }

    /// Moves to the next interval without sleeping (doubles, up to the maximum).
    /// [`Backoff::sleep`] is this plus a blocking sleep.
    pub fn advance(&mut self) {
        self.cur = (self.cur * 2).min(self.max);
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
            waited: READY_TIMEOUT,
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
            let exe = std::env::current_exe().expect("test binary path");
            Script::new(
                "forwarder",
                &format!(
                    "#!/bin/sh\n{FAKE_ARGS_ENV}=\"$*\"\nexport {FAKE_ARGS_ENV}\n\
                     exec '{}' {FORWARDER_TEST} --exact --nocapture --test-threads=1 >/dev/null\n",
                    exe.display()
                ),
            )
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

            let mut conn = TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
            conn.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            conn.write_all(b"hello").expect("write");
            let mut back = [0u8; 5];
            conn.read_exact(&mut back).expect("echo");
            assert_eq!(&back, b"hello");
            // A listener is present, so a dropped connection is not explained as absent.
            assert!(!tunnel.remote_listener_absent(&connection_closed()));

            let pid = only_pid(&pids);
            let (_, drops_before) = tunnel_drop_totals();
            drop(tunnel);
            assert!(tunnel_drop_totals().1 > drops_before);
            assert!(pid_is_gone(pid), "the tunnel child {pid} was not reaped");
        }

        fn connection_closed() -> io::Error {
            crate::connection_closed_error("eof")
        }

        /// With nothing on the remote port, the tunnel still comes up, the first connection
        /// closes, and the verdict says the listener is absent.
        #[test]
        fn an_empty_remote_port_is_judged_absent() {
            let fake = forwarder();
            let launcher = SshLauncher::new(fake.path());
            let (held, remote) = reserve_local_port().expect("port");
            drop(held);
            let tunnel = SshTunnel::establish(&launcher, &SshTarget::parse("fake-host"), remote)
                .expect("tunnel");
            let mut conn = TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
            conn.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut buf = [0u8; 1];
            // EOF or a reset: either way the connection ended without an answer. A read
            // timeout would be a different failure and must not pass here.
            let failure = match conn.read(&mut buf) {
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
            };
            assert!(tunnel.remote_listener_absent(&failure));
            assert_eq!(
                tunnel.explain_failure(&failure),
                Some(RemoteListenerAbsent {
                    remote_port: remote
                })
            );
            let verdict = tunnel.listener_verdict();
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
            let sink = SlowSink(Arc::clone(&relayed));
            let result = SshTunnel::establish_relaying_to(
                &launcher,
                &SshTarget::parse("fake-host"),
                45123,
                move || sink,
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
