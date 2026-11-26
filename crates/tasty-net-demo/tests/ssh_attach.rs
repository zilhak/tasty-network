//! SSH forward attach between two OS processes through a real `ssh -L` tunnel
//! (docs/design/done-criteria.md D2).
//!
//! Each test starts a test SSH server ([`support::sshd`]: OpenSSH sshd, or the test-only Rust
//! server when OpenSSH cannot serve), a `demo-server` process and a `demo-dialer` process. The
//! dialer reaches the server with the system `ssh`: record discovery over SSH, the `-L`
//! tunnel, handshake v2. The tests check what the dialer and the server print, and at the end
//! that no sshd, ssh or demo process is left. They run one at a time: the SSH crate reserves a
//! local port and releases it before ssh binds it, so two tunnels at once could meet on one
//! port.
//!
//! Evidence lines go to stderr unconditionally (`[ssh_attach] ...`): `sshd=<server>`, the
//! process IDs, and the tunnel PIDs before and after a reconnect.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use support::process::{Proc, log, terminate, wait_all_gone};
use support::sshd::SshServer;
use support::{TempDir, ssh_path};
use tasty_net_demo::field;
use tasty_net_identity::{ConnectionRecord, ConnectionToken};

/// Link read timeout of both binaries; a Ping goes every sixth of it.
const READ_TIMEOUT_MS: &str = "3000";
/// Bound of one expected event that needs SSH (discovery, tunnel, handshake).
const SSH_STEP: Duration = Duration::from_secs(60);
/// Bound of one expected event without SSH.
const LOCAL_STEP: Duration = Duration::from_secs(20);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a `server ready` line announced.
struct Ready {
    pid: u32,
    port: u16,
    instance: String,
}

fn number<T: std::str::FromStr>(line: &str, key: &str) -> T {
    field(line, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {key}= in {line:?}"))
}

/// One test's processes. Every PID it learns is checked at the end.
struct Scene {
    sshd: SshServer,
    dir: TempDir,
    record: PathBuf,
    tracked: Vec<(String, u32)>,
}

impl Scene {
    fn new() -> Self {
        let sshd = SshServer::start();
        let dir = TempDir::new("scene");
        let record = dir.path().join("record.json");
        Self {
            sshd,
            dir,
            record,
            tracked: Vec::new(),
        }
    }

    fn track(&mut self, what: &str, pid: u32) {
        if !self.tracked.iter().any(|(_, p)| *p == pid) {
            self.tracked.push((what.to_string(), pid));
        }
    }

    fn start_server(&mut self) -> (Proc, Ready) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_demo-server"));
        command
            .arg("--record")
            .arg(&self.record)
            .args(["--read-timeout-ms", READ_TIMEOUT_MS]);
        let mut server = Proc::spawn("demo-server", command);
        self.track("demo-server", server.pid());
        let line = server.expect_prefix("server ready ", LOCAL_STEP);
        let ready = Ready {
            pid: number(&line, "pid"),
            port: number(&line, "port"),
            instance: field(&line, "instance").expect("instance").to_string(),
        };
        assert_eq!(ready.pid, server.pid(), "the ready line names the process");
        (server, ready)
    }

    fn start_dialer(&mut self, record: &Path) -> Proc {
        let mut command = Command::new(env!("CARGO_BIN_EXE_demo-dialer"));
        command
            .arg("--destination")
            .arg(self.sshd.destination())
            .arg("--ssh-port")
            .arg(self.sshd.port.to_string())
            .arg("--identity")
            .arg(ssh_path(&self.sshd.identity))
            .arg("--record")
            .arg(ssh_path(record))
            .args(["--read-timeout-ms", READ_TIMEOUT_MS]);
        for option in self.sshd.client_options() {
            command.arg("--option").arg(option);
        }
        let mut dialer = Proc::spawn("demo-dialer", command);
        self.track("demo-dialer", dialer.pid());
        let line = dialer.expect_prefix("dialer pid=", LOCAL_STEP);
        assert_eq!(number::<u32>(&line, "pid"), dialer.pid());
        dialer
    }

    /// Waits for the dialer's next `connected` line and returns it. Tracks every ssh child the
    /// dialer reported up to it.
    fn connected(&mut self, dialer: &mut Proc) -> String {
        let line = dialer.expect_prefix("dialer connected ", SSH_STEP);
        self.track_ssh_children(dialer);
        line
    }

    fn track_ssh_children(&mut self, dialer: &Proc) {
        for line in dialer.transcript.lines() {
            if line.starts_with("dialer ssh-spawned ") {
                let role = field(&line, "role").unwrap_or("?").to_string();
                self.track(&format!("ssh {role}"), number(&line, "pid"));
            }
        }
    }

    /// Ends the dialer with `quit` and checks a clean exit.
    fn quit_dialer(&mut self, mut dialer: Proc) {
        dialer.send_line("quit");
        let status = dialer.wait_exit(LOCAL_STEP);
        self.track_ssh_children(&dialer);
        assert!(
            status.success(),
            "the dialer did not end cleanly: {status}\n{}",
            dialer.transcript.render()
        );
    }

    /// Ends the server by closing its stdin (it retires its record) and checks a clean exit.
    fn stop_server(mut server: Proc) {
        server.close_stdin();
        let status = server.wait_exit(LOCAL_STEP);
        assert!(
            status.success(),
            "the server did not end cleanly: {status}\n{}",
            server.transcript.render()
        );
    }

    /// Stops the SSH server and checks that no process of the test is left.
    fn finish(mut self) {
        let sshd_pids = self.sshd.stop();
        self.tracked.extend(sshd_pids);
        let left = wait_all_gone(&self.tracked, Duration::from_secs(15));
        log(&format!(
            "processes left after the test: {} of {} tracked ({})",
            left.len(),
            self.tracked.len(),
            self.tracked
                .iter()
                .map(|(what, pid)| format!("{what} {pid}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for (_, pid) in &left {
            terminate(*pid);
        }
        assert!(left.is_empty(), "processes left after the test: {left:?}");
        self.dir.remove();
    }
}

/// Sends `text` and waits for its echo.
fn echo(dialer: &mut Proc, text: &str) {
    dialer.send_line(&format!("send {text}"));
    let want = format!("dialer echo {text}");
    dialer.expect(&format!("{want:?}"), LOCAL_STEP, |l| l == want);
}

/// Connects, checks the two PIDs and the snapshot, and echoes once. Returns the tunnel PID.
fn connect_and_check(
    scene: &mut Scene,
    server: &mut Proc,
    ready: &Ready,
    dialer: &mut Proc,
) -> u32 {
    let line = scene.connected(dialer);
    assert_eq!(field(&line, "instance"), Some(ready.instance.as_str()));
    let tunnel: u32 = number(&line, "tunnel_pid");
    log(&format!(
        "pids server={} dialer={} tunnel_ssh={tunnel} (sshd={})",
        server.pid(),
        dialer.pid(),
        scene.sshd.kind()
    ));
    assert_ne!(server.pid(), dialer.pid(), "two separate processes");
    assert_ne!(tunnel, 0, "the dialer reported its tunnel ssh");
    let control = dialer.expect_prefix("dialer control ", LOCAL_STEP);
    assert!(
        control.contains(&ready.instance),
        "the snapshot names the server instance: {control}"
    );
    server.expect_prefix("server attached ", LOCAL_STEP);
    echo(dialer, "hello-1");
    tunnel
}

#[test]
fn connect_and_echo() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    log(&format!(
        "server pid={} port={} instance={}",
        ready.pid, ready.port, ready.instance
    ));
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    connect_and_check(&mut scene, &mut server, &ready, &mut dialer);
    echo(&mut dialer, "hello-2");
    scene.quit_dialer(dialer);
    let released = server.expect_prefix("server released ", LOCAL_STEP);
    assert_eq!(field(&released, "live"), Some("0"), "{released}");
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn reconnect_after_tunnel_kill() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    let before = connect_and_check(&mut scene, &mut server, &ready, &mut dialer);

    log(&format!("ending the tunnel ssh pid={before}"));
    terminate(before);
    let lost = dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    let failure = field(&lost, "failure").unwrap_or("");
    log(&format!("the dialer classified the loss as {failure}"));
    assert!(
        matches!(failure, "peer_closed" | "heartbeat_timeout"),
        "a dropped tunnel is a closed peer or a heartbeat timeout: {lost}"
    );

    let line = scene.connected(&mut dialer);
    let after: u32 = number(&line, "tunnel_pid");
    log(&format!("tunnel ssh pid before={before} after={after}"));
    assert_ne!(before, after, "the reconnect started a new tunnel ssh");
    assert_eq!(field(&line, "instance"), Some(ready.instance.as_str()));
    echo(&mut dialer, "hello-again");

    // The first link's registration is gone and only the new link is live.
    let released = server.expect("\"server released client=1\"", LOCAL_STEP, |l| {
        l.starts_with("server released client=1 ")
    });
    let lines = server.transcript.lines();
    let attached = lines
        .iter()
        .find(|l| l.starts_with("server attached client=2 "))
        .expect("the second attach");
    let released_at = lines.iter().position(|l| *l == released).expect("released");
    let attached_at = lines.iter().position(|l| l == attached).expect("attached");
    let live_now = if released_at > attached_at {
        field(&released, "live")
    } else {
        field(attached, "live")
    };
    log(&format!(
        "server links live after the reconnect: {live_now:?}"
    ));
    assert_eq!(live_now, Some("1"), "{}", server.transcript.render());

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn reconnect_after_server_restart() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    let tunnel_before = connect_and_check(&mut scene, &mut server, &ready, &mut dialer);

    log(&format!("killing demo-server pid={}", server.pid()));
    server.kill();
    let lost = dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    log(&format!("the dialer saw: {lost}"));

    let (mut restarted, ready2) = scene.start_server();
    log(&format!(
        "server instance before={} after={} pid before={} after={}",
        ready.instance, ready2.instance, ready.pid, ready2.pid
    ));
    assert_ne!(ready.instance, ready2.instance);
    // Attempts that still read the old record fail and are retried; the one that reads the
    // new record reaches the new instance.
    let line = scene.connected(&mut dialer);
    assert_eq!(
        field(&line, "instance"),
        Some(ready2.instance.as_str()),
        "the reconnect used the newly discovered record\n{}",
        dialer.transcript.render()
    );
    let tunnel_after: u32 = number(&line, "tunnel_pid");
    log(&format!(
        "tunnel ssh pid before={tunnel_before} after={tunnel_after}"
    ));
    assert_ne!(
        tunnel_before, tunnel_after,
        "a new record gives a new tunnel"
    );
    restarted.expect_prefix("server attached client=1 live=1", LOCAL_STEP);
    echo(&mut dialer, "hello-restarted");

    scene.quit_dialer(dialer);
    Scene::stop_server(restarted);
    drop(server);
    scene.finish();
}

#[test]
fn wrong_token_is_rejected_without_retry() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    // A record with the server's port and instance but another token.
    let wrong = ConnectionRecord {
        port: ready.port,
        token: ConnectionToken::issue().expect("token"),
        instance_id: ready.instance.clone(),
    };
    let bad_record = scene.dir.path().join("wrong-token.json");
    std::fs::write(&bad_record, wrong.to_line()).expect("write the wrong record");
    let mut dialer = scene.start_dialer(&bad_record);

    let rejected = dialer.expect_prefix("dialer rejected ", SSH_STEP);
    log(&format!("the dialer ended with: {rejected}"));
    assert_eq!(
        field(&rejected, "reason"),
        Some("token_invalid"),
        "{rejected}"
    );
    let status = dialer.wait_exit(LOCAL_STEP);
    assert_eq!(
        status.code(),
        Some(3),
        "the dialer ends with the rejection exit code"
    );
    scene.track_ssh_children(&dialer);

    server.expect_prefix("server refused reason=token_invalid", LOCAL_STEP);
    // The dialer has exited, so nothing more can come; the server saw exactly one attempt.
    std::thread::sleep(Duration::from_secs(1));
    let lines = server.transcript.lines();
    let refused = lines
        .iter()
        .filter(|l| l.starts_with("server refused "))
        .count();
    let attached = lines
        .iter()
        .filter(|l| l.starts_with("server attached "))
        .count();
    let attempts = dialer
        .transcript
        .lines()
        .iter()
        .filter(|l| l.starts_with("dialer attempt "))
        .count();
    log(&format!(
        "attempts: dialer {attempts}, server refusals {refused}, server attaches {attached}"
    ));
    assert_eq!(
        (attempts, refused, attached),
        (1, 1, 0),
        "{}",
        server.transcript.render()
    );

    drop(dialer);
    Scene::stop_server(server);
    scene.finish();
}
