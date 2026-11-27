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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use support::process::{
    ChildList, Children, Proc, Transcript, children_of, describe, is_running, log, terminate,
    try_terminate, wait_all_gone,
};
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
/// How long a dialer's ssh children may outlive the dialer after `quit`. The dialer ends them
/// before it exits, so this only absorbs the time a reaped process takes to vanish.
const SSH_END_AFTER_QUIT: Duration = Duration::from_secs(5);

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

/// One test's processes. Every PID it learns is checked at the end. When a test fails
/// before [`Scene::finish`], dropping it ends what still runs: the dialers, then every ssh
/// child of a dialer (they outlive a killed dialer), then the other tracked processes, and
/// last the SSH server with its sessions (its field drops after this).
struct Scene {
    sshd: Option<SshServer>,
    dir: Option<TempDir>,
    record: PathBuf,
    tracked: Vec<(String, u32)>,
    /// What each dialer left to find its ssh children by, read again on drop: a test that
    /// fails while waiting for a line has not tracked the ssh children of the dialer yet.
    dialers: Vec<DialerTrace>,
    /// Filled on drop, before the SSH server ends, for a test of the failure path.
    drop_report: Option<Arc<Mutex<Option<DropReport>>>>,
    finished: bool,
}

/// A dialer's transcript (its `ssh-spawned` lines) and the children it had when it was
/// killed ([`Proc::track_children`]).
struct DialerTrace {
    transcript: Transcript,
    children: ChildList,
}

/// What [`Scene`]'s drop tracked and what still ran after it ended them, before the SSH
/// server ended.
struct DropReport {
    tracked: Vec<(String, u32)>,
    left: Result<Vec<(String, u32)>, String>,
}

/// Ends the tracked processes that run, dialers first, then ssh children, then the rest, and
/// waits until they are gone. Returns what still runs, or the error of a failed query.
fn end_in_order(pids: &[(String, u32)]) -> Result<Vec<(String, u32)>, String> {
    let rank = |what: &str| match what {
        "demo-dialer" => 0,
        w if w.starts_with("ssh ") => 1,
        _ => 2,
    };
    let mut order = pids.to_vec();
    order.sort_by_key(|(what, _)| rank(what));
    for (what, pid) in &order {
        // A failed query counts as running: ending a process that is gone is harmless.
        if is_running(*pid).unwrap_or(true) {
            log(&format!("ending {what} {pid} left by a failed test"));
            terminate(*pid);
        }
    }
    wait_all_gone(&order, Duration::from_secs(10))
}

impl Drop for Scene {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A dialer that still runs: its children, before it is ended.
        let dialers: Vec<u32> = self
            .tracked
            .iter()
            .filter(|(what, _)| what == "demo-dialer")
            .map(|(_, pid)| *pid)
            .collect();
        for pid in dialers {
            if is_running(pid).unwrap_or(false) {
                self.track_dialer_children(pid);
            }
        }
        self.track_ssh_children();
        let mut left = end_in_order(&self.tracked);
        // With the ssh children gone, the killed dialers' stdout ends; an `ssh-spawned` line
        // still in the pipe shows now.
        let before = self.tracked.len();
        self.track_ssh_children();
        if self.tracked.len() > before {
            left = end_in_order(&self.tracked);
        }
        match &left {
            Ok(left) if left.is_empty() => {}
            other => log(&format!("still running after a failed test: {other:?}")),
        }
        if let Some(report) = &self.drop_report {
            *report.lock().unwrap_or_else(PoisonError::into_inner) = Some(DropReport {
                tracked: self.tracked.clone(),
                left,
            });
        }
    }
}

impl Scene {
    fn new() -> Self {
        let sshd = SshServer::start();
        let dir = TempDir::new("scene");
        let record = dir.path().join("record.json");
        Self {
            sshd: Some(sshd),
            dir: Some(dir),
            record,
            tracked: Vec::new(),
            dialers: Vec::new(),
            drop_report: None,
            finished: false,
        }
    }

    fn sshd(&self) -> &SshServer {
        self.sshd.as_ref().expect("the SSH server runs")
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
        self.start_dialer_with(record, &[])
    }

    /// [`Scene::start_dialer`] with extra environment variables for the dialer.
    fn start_dialer_with(&mut self, record: &Path, env: &[(&str, &str)]) -> Proc {
        let sshd = self.sshd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_demo-dialer"));
        command
            .arg("--destination")
            .arg(sshd.destination())
            .arg("--ssh-port")
            .arg(sshd.port.to_string())
            .arg("--identity")
            .arg(ssh_path(&sshd.identity))
            // The path goes into the remote shell command (`cat`, then `type`), so it keeps
            // the OS's own form; cmd's `type` needs backslashes.
            .arg("--record")
            .arg(record)
            .args(["--read-timeout-ms", READ_TIMEOUT_MS]);
        for option in sshd.client_options() {
            command.arg("--option").arg(option);
        }
        command.envs(env.iter().copied());
        let mut dialer = Proc::spawn("demo-dialer", command);
        self.track("demo-dialer", dialer.pid());
        let children = dialer.track_children();
        self.dialers.push(DialerTrace {
            transcript: dialer.transcript.clone(),
            children,
        });
        let line = dialer.expect_prefix("dialer pid=", LOCAL_STEP);
        assert_eq!(number::<u32>(&line, "pid"), dialer.pid());
        dialer
    }

    /// Waits for the dialer's next `connected` line and returns it. Tracks every ssh child the
    /// dialer reported up to it.
    fn connected(&mut self, dialer: &mut Proc) -> String {
        let line = dialer.expect_prefix("dialer connected ", SSH_STEP);
        self.track_ssh_children();
        line
    }

    /// Tracks the running children of the dialer `pid` (its ssh children).
    fn track_dialer_children(&mut self, pid: u32) {
        let Children { pids, error } = children_of(pid);
        for child in pids {
            self.track("ssh child", child);
        }
        if let Some(e) = error {
            log(&format!("listing the children of demo-dialer {pid}: {e}"));
        }
    }

    /// Tracks every ssh child the dialers reported so far, and the children a killed dialer
    /// had.
    fn track_ssh_children(&mut self) {
        let lines: Vec<String> = self
            .dialers
            .iter()
            .flat_map(|d| d.transcript.lines())
            .collect();
        for line in lines {
            if line.starts_with("dialer ssh-spawned ") {
                let role = field(&line, "role").unwrap_or("?").to_string();
                if let Some(pid) = field(&line, "pid").and_then(|p| p.parse().ok()) {
                    self.track(&format!("ssh {role}"), pid);
                }
            }
        }
        // After the reported ones, so those keep their role in the name.
        let killed: Vec<u32> = self
            .dialers
            .iter()
            .flat_map(|d| d.children.pids())
            .collect();
        for pid in killed {
            self.track("ssh child", pid);
        }
    }

    /// Ends the dialer with `quit` and checks a clean exit.
    fn quit_dialer(&mut self, mut dialer: Proc) {
        self.track_ssh_children();
        let Children { pids, error } = children_of(dialer.pid());
        for child in pids {
            self.track("ssh child", child);
        }
        // After tracking what was found, so the drop ends it.
        if let Some(e) = error {
            panic!("listing the children of the dialer: {e}");
        }
        dialer.send_line("quit");
        let status = dialer.wait_exit(LOCAL_STEP);
        self.track_ssh_children();
        assert!(
            status.success(),
            "the dialer did not end cleanly: {status}\n{}",
            dialer.transcript.render()
        );
        self.check_ssh_ended_with(&dialer);
    }

    /// After the dialer's `quit`, and before the SSH server ends: every ssh child it had is
    /// gone. A dialer that exits while one still runs would leave that ssh, and its sshd
    /// session, behind. Logs what is left (with its parent PID) and the sshd sessions, then
    /// fails.
    fn check_ssh_ended_with(&self, dialer: &Proc) {
        let ssh: Vec<(String, u32)> = self
            .tracked
            .iter()
            .filter(|(what, _)| what.starts_with("ssh "))
            .cloned()
            .collect();
        let left = wait_all_gone(&ssh, SSH_END_AFTER_QUIT)
            .unwrap_or_else(|e| panic!("checking the dialer's ssh children after quit: {e}"));
        if left.is_empty() {
            return;
        }
        for (what, pid) in &left {
            log(&format!("left after quit: {what}: {}", describe(*pid)));
        }
        log(&format!("sshd sessions: {}", self.sshd().session_listing()));
        let pids: Vec<String> = left.iter().map(|(_, pid)| pid.to_string()).collect();
        panic!(
            "the dialer left ssh {} after quit\n--- demo-dialer transcript ---\n{}",
            pids.join(", "),
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

    /// Stops the SSH server and checks that no process of the test is left. A failed query
    /// does not cut the check short: every tracked PID is still checked, and what may still
    /// run is ended, before the test fails. The scene counts as finished only after that, so
    /// a panic before it (in the SSH server's stop) leaves the cleanup to the drop.
    fn finish(mut self) {
        let sshd_pids = self.sshd.take().expect("sshd").stop();
        self.tracked.extend(sshd_pids);
        let GoneCheck { left, errors } = check_all_gone(&self.tracked, Duration::from_secs(15));
        log(&format!(
            "processes left after the test: {} of {} tracked ({}){}",
            left.len(),
            self.tracked.len(),
            self.tracked
                .iter()
                .map(|(what, pid)| format!("{what} {pid}"))
                .collect::<Vec<_>>()
                .join(", "),
            if errors.is_empty() {
                String::new()
            } else {
                format!("; failed checks: {errors:?}")
            }
        ));
        // What still ran, or could not be checked, is asked to end and checked again.
        let mut to_end = left.clone();
        to_end.extend(
            errors
                .iter()
                .map(|(pid, _)| ("unchecked".to_string(), *pid)),
        );
        let mut end_errors = Vec::new();
        for (what, pid) in &to_end {
            // A process that went between the check and the end command is not a failure.
            if let Err(e) = try_terminate(*pid)
                && is_running(*pid).unwrap_or(true)
            {
                end_errors.push(format!("{what}: {e}"));
            }
        }
        let after_end = if to_end.is_empty() {
            GoneCheck {
                left: Vec::new(),
                errors: Vec::new(),
            }
        } else {
            check_all_gone(&to_end, Duration::from_secs(10))
        };
        if !to_end.is_empty() {
            log(&format!(
                "after ending {} of them: {} still run, {} checks failed, {} end commands failed",
                to_end.len(),
                after_end.left.len(),
                after_end.errors.len(),
                end_errors.len()
            ));
        }
        self.finished = true;
        assert!(
            errors.is_empty(),
            "checking the processes of the test failed: {errors:?} (ending them: \
             {end_errors:?}; still running after that: {:?}; checks failed after that: {:?})",
            after_end.left,
            after_end.errors
        );
        assert!(
            left.is_empty(),
            "processes left after the test: {left:?} (ending them: {end_errors:?}; \
             still running after that: {:?}; checks failed after that: {:?})",
            after_end.left,
            after_end.errors
        );
        if let Some(dir) = self.dir.take() {
            dir.remove();
        }
    }
}

/// What [`check_all_gone`] found: the PIDs that still ran, and the PIDs whose last query
/// failed with its error (they may run).
struct GoneCheck {
    left: Vec<(String, u32)>,
    errors: Vec<(u32, String)>,
}

/// Waits up to `timeout` until none of `pids` runs, checking each PID every round even when
/// the query of another fails.
fn check_all_gone(pids: &[(String, u32)], timeout: Duration) -> GoneCheck {
    let deadline = Instant::now() + timeout;
    loop {
        let mut left = Vec::new();
        let mut errors = Vec::new();
        for (what, pid) in pids {
            match is_running(*pid) {
                Ok(true) => left.push((what.clone(), *pid)),
                Ok(false) => {}
                Err(e) => errors.push((*pid, e)),
            }
        }
        if (left.is_empty() && errors.is_empty()) || Instant::now() >= deadline {
            return GoneCheck { left, errors };
        }
        std::thread::sleep(Duration::from_millis(200));
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
        scene.sshd().kind()
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
    let bad_record = scene
        .dir
        .as_ref()
        .expect("dir")
        .path()
        .join("wrong-token.json");
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
    scene.track_ssh_children();

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

/// A test that fails while waiting for the dialer leaves no process behind: the wait runs out
/// (as a reconnect that does not come would) while the tunnel ssh runs and before any ssh
/// child was tracked, and dropping the scene ends the dialer, its ssh children and the server
/// before the SSH server, and then sshd.
#[test]
fn a_failed_wait_leaves_no_ssh_behind() {
    let _one = one_at_a_time();
    let mut dialer_lines: Option<Transcript> = None;
    let mut others: Vec<(String, u32)> = Vec::new();
    let mut tunnel: Option<u32> = None;
    let mut tunnel_ran_at_expiry: Option<bool> = None;
    let report: Arc<Mutex<Option<DropReport>>> = Arc::default();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut scene = Scene::new();
        scene.drop_report = Some(Arc::clone(&report));
        if let Some(pid) = scene.sshd().pid() {
            others.push(("sshd".into(), pid));
        }
        let (server, ready) = scene.start_server();
        others.push(("demo-server".into(), ready.pid));
        let record = scene.record.clone();
        let mut dialer = scene.start_dialer(&record);
        dialer_lines = Some(dialer.transcript.clone());
        let line = dialer.expect("\"role=tunnel\"", SSH_STEP, |l| {
            l.starts_with("dialer ssh-spawned ") && l.ends_with("role=tunnel")
        });
        let pid: u32 = number(&line, "pid");
        tunnel = Some(pid);
        // Never matches: this wait runs out, and the test body panics with its message.
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dialer.expect("that never comes", Duration::from_secs(3), |_| false)
        }));
        let Err(expiry) = waited else {
            unreachable!("a wait that matches nothing returned")
        };
        tunnel_ran_at_expiry =
            Some(is_running(pid).unwrap_or_else(|e| panic!("checking the tunnel ssh {pid}: {e}")));
        let _kept = (server, dialer);
        std::panic::resume_unwind(expiry);
    }));
    assert!(failed.is_err(), "the inner wait was meant to run out");
    let tunnel = tunnel.expect("the dialer started a tunnel ssh");
    assert_eq!(
        tunnel_ran_at_expiry,
        Some(true),
        "the tunnel ssh {tunnel} ran when the wait ran out"
    );

    // What the scene's drop saw before the SSH server ended.
    let seen = report
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("the scene's drop reported");
    assert!(
        seen.tracked.iter().any(|(_, pid)| *pid == tunnel),
        "the scene's drop tracked the tunnel ssh {tunnel}: {:?}",
        seen.tracked
    );
    let left_before_sshd = seen
        .left
        .unwrap_or_else(|e| panic!("checking the processes before sshd ended: {e}"));
    assert!(
        left_before_sshd.is_empty(),
        "still running before sshd ended: {left_before_sshd:?}"
    );

    let lines = dialer_lines.expect("the dialer started").lines();
    let mut pids = others;
    for line in &lines {
        if line.starts_with("dialer pid=") {
            pids.push(("demo-dialer".into(), number(line, "pid")));
        } else if line.starts_with("dialer ssh-spawned ") {
            let role = field(line, "role").unwrap_or("?");
            pids.push((format!("ssh {role}"), number(line, "pid")));
        }
    }
    let tunnels = pids.iter().filter(|(what, _)| what == "ssh tunnel").count();
    assert!(tunnels >= 1, "the dialer started a tunnel ssh: {lines:?}");
    let left = wait_all_gone(&pids, Duration::from_secs(15))
        .unwrap_or_else(|e| panic!("checking the processes: {e}"));
    log(&format!(
        "processes left after a failed wait: {} of {} ({} tunnel ssh, running at expiry; \
         tracked by the drop: {}; left before sshd ended: 0; {})",
        left.len(),
        pids.len(),
        tunnels,
        seen.tracked.len(),
        pids.iter()
            .map(|(what, pid)| format!("{what} {pid}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for (_, pid) in &left {
        terminate(*pid);
    }
    assert!(
        left.is_empty(),
        "processes left after a failed wait: {left:?}"
    );
}

/// The dialer waits on `quit` until its stdin thread has let the link's writer go, which ends
/// the tunnel ssh. The test hook keeps the writer 1.5 s longer: the dialer must still end
/// after that, and leave no ssh.
#[test]
fn quit_waits_for_the_detaching_writer() {
    const HOLD: Duration = Duration::from_millis(1500);
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let hold = HOLD.as_millis().to_string();
    let mut dialer =
        scene.start_dialer_with(&record, &[("TASTY_NET_DEMO_HOLD_WRITER_MS", hold.as_str())]);
    connect_and_check(&mut scene, &mut server, &ready, &mut dialer);
    let started = Instant::now();
    scene.quit_dialer(dialer);
    let took = started.elapsed();
    log(&format!(
        "the dialer ended {} ms after quit with the writer held {} ms",
        took.as_millis(),
        HOLD.as_millis()
    ));
    assert!(
        took >= HOLD,
        "the dialer ended {took:?} after quit, before its writer went ({HOLD:?})"
    );
    let released = server.expect_prefix("server released ", LOCAL_STEP);
    assert_eq!(field(&released, "live"), Some("0"), "{released}");
    Scene::stop_server(server);
    scene.finish();
}
