//! One test's processes for the two-process SSH tests: the test SSH server, `demo-server`,
//! `demo-dialer` and the dialer's ssh children, with the checks that none of them is left
//! (docs/design/done-criteria.md D2, D3).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::process::{
    ChildList, Children, Proc, Transcript, children_of, describe, is_running, log, terminate,
    try_terminate, wait_all_gone,
};
use super::sshd::SshServer;
use super::{TempDir, ssh_path};
use tasty_net_demo::field;
use tasty_net_demo::table::decode_value;

/// Link read timeout of both binaries; a Ping goes every sixth of it.
pub const READ_TIMEOUT_MS: &str = "3000";
/// Bound of one expected event that needs SSH (discovery, tunnel, handshake).
pub const SSH_STEP: Duration = Duration::from_secs(60);
/// Bound of one expected event without SSH.
pub const LOCAL_STEP: Duration = Duration::from_secs(20);
/// How long a dialer's ssh children may outlive the dialer after `quit`. The dialer ends them
/// before it exits, so this only absorbs the time a reaped process takes to vanish.
pub const SSH_END_AFTER_QUIT: Duration = Duration::from_secs(5);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a `server ready` line announced.
pub struct Ready {
    pub pid: u32,
    pub port: u16,
    pub instance: String,
}

pub fn number<T: std::str::FromStr>(line: &str, key: &str) -> T {
    field(line, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {key}= in {line:?}"))
}

/// One test's processes. Every PID it learns is checked at the end. When a test fails
/// before [`Scene::finish`], dropping it ends what still runs: the dialers, then every ssh
/// child of a dialer (they outlive a killed dialer), then the other tracked processes, and
/// last the SSH server with its sessions (its field drops after this).
pub struct Scene {
    sshd: Option<SshServer>,
    pub dir: Option<TempDir>,
    pub record: PathBuf,
    tracked: Vec<(String, u32)>,
    /// What each dialer left to find its ssh children by, read again on drop: a test that
    /// fails while waiting for a line has not tracked the ssh children of the dialer yet.
    dialers: Vec<DialerTrace>,
    /// Filled on drop, before the SSH server ends, for a test of the failure path.
    pub drop_report: Option<Arc<Mutex<Option<DropReport>>>>,
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
pub struct DropReport {
    pub tracked: Vec<(String, u32)>,
    pub left: Result<Vec<(String, u32)>, String>,
}

/// Ends the tracked processes that run, dialers first, then ssh children, then the rest, and
/// waits until they are gone. Returns what still runs, or the error of a failed query.
pub fn end_in_order(pids: &[(String, u32)]) -> Result<Vec<(String, u32)>, String> {
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
    pub fn new() -> Self {
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

    pub fn sshd(&self) -> &SshServer {
        self.sshd.as_ref().expect("the SSH server runs")
    }

    pub fn track(&mut self, what: &str, pid: u32) {
        if !self.tracked.iter().any(|(_, p)| *p == pid) {
            self.tracked.push((what.to_string(), pid));
        }
    }

    pub fn start_server(&mut self) -> (Proc, Ready) {
        self.start_server_with(&[])
    }

    /// [`Scene::start_server`] with extra arguments for the server.
    pub fn start_server_with(&mut self, args: &[&str]) -> (Proc, Ready) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_demo-server"));
        command
            .arg("--record")
            .arg(&self.record)
            .args(["--read-timeout-ms", READ_TIMEOUT_MS])
            .args(args);
        let mut server = Proc::spawn("demo-server", command);
        self.track("demo-server", server.pid());
        let line = server.expect_prefix("server ready ", LOCAL_STEP);
        let ready = Ready {
            pid: number(&line, "pid"),
            port: number(&line, "port"),
            instance: field(&line, "instance")
                .and_then(decode_value)
                .expect("instance"),
        };
        assert_eq!(ready.pid, server.pid(), "the ready line names the process");
        (server, ready)
    }

    pub fn start_dialer(&mut self, record: &Path) -> Proc {
        self.start_dialer_with(record, &[], &[])
    }

    /// [`Scene::start_dialer`] with extra arguments and environment variables for the dialer.
    pub fn start_dialer_with(
        &mut self,
        record: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Proc {
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
        command.args(args).envs(env.iter().copied());
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
    pub fn connected(&mut self, dialer: &mut Proc) -> String {
        let line = dialer.expect_prefix("dialer connected ", SSH_STEP);
        self.track_ssh_children();
        line
    }

    /// Tracks the running children of the dialer `pid` (its ssh children).
    pub fn track_dialer_children(&mut self, pid: u32) {
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
    pub fn track_ssh_children(&mut self) {
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
    pub fn quit_dialer(&mut self, mut dialer: Proc) {
        self.before_quit(&dialer);
        dialer.send_line("quit");
        let status = dialer.wait_exit(LOCAL_STEP);
        self.after_quit(&dialer, status);
    }

    /// Before the dialer's `quit`: tracks its ssh children (reported and listed).
    pub fn before_quit(&mut self, dialer: &Proc) {
        self.track_ssh_children();
        let Children { pids, error } = children_of(dialer.pid());
        for child in pids {
            self.track("ssh child", child);
        }
        // After tracking what was found, so the drop ends it.
        if let Some(e) = error {
            panic!("listing the children of the dialer: {e}");
        }
    }

    /// After the dialer exited on `quit` with `status`: checks a clean exit and that its ssh
    /// children are gone.
    pub fn after_quit(&mut self, dialer: &Proc, status: std::process::ExitStatus) {
        self.track_ssh_children();
        assert!(
            status.success(),
            "the dialer did not end cleanly: {status}\n{}",
            dialer.transcript.render()
        );
        self.check_ssh_ended_with(dialer);
    }

    /// After the dialer's `quit`, and before the SSH server ends: every ssh child it had is
    /// gone. A dialer that exits while one still runs would leave that ssh, and its sshd
    /// session, behind. Logs what is left (with its parent PID) and the sshd sessions, then
    /// fails.
    pub fn check_ssh_ended_with(&self, dialer: &Proc) {
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
    pub fn stop_server(mut server: Proc) {
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
    /// run is ended and checked again, before the test fails. The scene counts as finished
    /// only when that check found all of them gone, so a panic before it (in the SSH server's
    /// stop), or a process still running or not checkable after it, leaves the cleanup to the
    /// drop.
    pub fn finish(mut self) {
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
        // Finished only when every one of them is known to be gone. Otherwise the failure below
        // leaves the scene unfinished, and its drop ends the tracked processes again in order.
        self.finished = after_end.left.is_empty() && after_end.errors.is_empty();
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
pub struct GoneCheck {
    pub left: Vec<(String, u32)>,
    pub errors: Vec<(u32, String)>,
}

/// Waits up to `timeout` until none of `pids` runs, checking each PID every round even when
/// the query of another fails.
pub fn check_all_gone(pids: &[(String, u32)], timeout: Duration) -> GoneCheck {
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
pub fn echo(dialer: &mut Proc, text: &str) {
    dialer.send_line(&format!("send {text}"));
    let want = format!("dialer echo {text}");
    dialer.expect(&format!("{want:?}"), LOCAL_STEP, |l| l == want);
}

/// Connects, checks the two PIDs and the snapshot, and echoes once. Returns the tunnel PID.
pub fn connect_and_check(
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
    // The dialer reads the link after it opened what `--channels` asks for, which discovers
    // the record over SSH again, so the snapshot may take as long as an SSH step.
    let control = dialer.expect_prefix("dialer control ", SSH_STEP);
    assert!(
        control.contains(&ready.instance),
        "the snapshot names the server instance: {control}"
    );
    server.expect_prefix("server attached ", LOCAL_STEP);
    echo(dialer, "hello-1");
    tunnel
}

/// The first line of `proc` that starts with `prefix`, waiting for it up to [`SSH_STEP`].
pub fn transcript_line(proc: &Proc, prefix: &str) -> String {
    let deadline = std::time::Instant::now() + SSH_STEP;
    loop {
        if let Some(line) = proc
            .transcript
            .lines()
            .into_iter()
            .find(|l| l.starts_with(prefix))
        {
            return line;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no line {prefix:?} within {SSH_STEP:?}\n{}",
            proc.transcript.render()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
