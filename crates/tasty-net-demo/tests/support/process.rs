//! Child processes whose stdout lines a test waits for, and OS-level process checks that work
//! on PIDs the test did not create itself (the dialer's `ssh` children, sshd's session
//! children).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Writes `line` to stderr directly. The test harness captures `print!`/`eprint!` of passing
/// tests, but the evidence lines (`sshd=`, PIDs) must reach the CI log of a passing test too.
pub fn log(line: &str) {
    let mut err = std::io::stderr().lock();
    let _written = writeln!(err, "[ssh_attach] {line}").is_ok();
}

/// The lines a process printed so far, its stderr included, for failure messages.
#[derive(Clone, Default)]
pub struct Transcript(Arc<Mutex<Vec<String>>>);

impl Transcript {
    fn push(&self, line: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn render(&self) -> String {
        self.lines().join("\n")
    }
}

/// A child process with piped stdio. Its stdout lines come through [`Proc::expect`]; stdout
/// and stderr both go to its [`Transcript`]. Dropping it kills and reaps the child.
pub struct Proc {
    pub name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    pub transcript: Transcript,
    status: Option<ExitStatus>,
}

fn relay(
    name: String,
    from: impl Read + Send + 'static,
    transcript: Transcript,
    tx: Option<mpsc::Sender<String>>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(from).lines() {
            let Ok(line) = line else { break };
            match &tx {
                Some(tx) => {
                    transcript.push(line.clone());
                    let _sent = tx.send(line).is_ok();
                }
                None => transcript.push(format!("[{name} stderr] {line}")),
            }
        }
    });
}

impl Proc {
    pub fn spawn(name: &str, mut command: Command) -> Self {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("starting {name} ({command:?}) failed: {e}"));
        let transcript = Transcript::default();
        let (tx, lines) = mpsc::channel();
        relay(
            name.into(),
            child.stdout.take().expect("stdout"),
            transcript.clone(),
            Some(tx),
        );
        relay(
            name.into(),
            child.stderr.take().expect("stderr"),
            transcript.clone(),
            None,
        );
        let stdin = child.stdin.take();
        Self {
            name: name.into(),
            child,
            stdin,
            lines,
            transcript,
            status: None,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits up to `timeout` for the next stdout line that `matches` and returns it. Lines
    /// before it are skipped (they stay in the transcript). Panics with the transcript when
    /// none comes.
    pub fn expect(
        &mut self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if matches(&line) => return line,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!(
                    "{}: no line {what} within {timeout:?}\n--- {} transcript ---\n{}",
                    self.name,
                    self.name,
                    self.transcript.render()
                ),
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "{}: stdout ended before a line {what}\n--- {} transcript ---\n{}",
                    self.name,
                    self.name,
                    self.transcript.render()
                ),
            }
        }
    }

    /// [`Proc::expect`] for a line that starts with `prefix`.
    pub fn expect_prefix(&mut self, prefix: &str, timeout: Duration) -> String {
        self.expect(&format!("starting with {prefix:?}"), timeout, |l| {
            l.starts_with(prefix)
        })
    }

    pub fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .unwrap_or_else(|e| panic!("{}: writing {line:?} to stdin failed: {e}", self.name));
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Waits up to `timeout` for the process to exit. Panics with the transcript otherwise.
    pub fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        if let Some(status) = self.status {
            return status;
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                self.status = Some(status);
                return status;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{} did not exit within {timeout:?}\n--- {} transcript ---\n{}",
                    self.name,
                    self.name,
                    self.transcript.render()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills the process and reaps it.
    pub fn kill(&mut self) {
        if self.status.is_none() {
            let _killed = self.child.kill().is_ok();
            self.status = self.child.wait().ok();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Ends a process the test did not create: SIGTERM on Unix, `taskkill /F` on Windows.
pub fn terminate(pid: u32) {
    #[cfg(unix)]
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    #[cfg(windows)]
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        other => log(&format!("ending pid {pid} failed: {other:?}")),
    }
}

/// Runs a process query command. Returns its exit code and stdout when it ran, its exit code
/// is one of `ok_codes` and `quiet_enough` accepts its stderr; anything else (no start,
/// another code, other stderr output) is an error, so a failed query is never read as "no such
/// process".
fn query(
    mut command: Command,
    ok_codes: &[i32],
    quiet_enough: impl Fn(&str) -> bool,
) -> Result<(i32, String), String> {
    let what = format!("{command:?}");
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{what} did not start: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(code) if ok_codes.contains(&code) && quiet_enough(&stderr) => {
            Ok((code, String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        _ => Err(format!("{what} failed ({}): {}", out.status, stderr.trim())),
    }
}

/// The stderr of `ps` and `pgrep`: nothing.
#[cfg(unix)]
fn no_stderr(stderr: &str) -> bool {
    stderr.trim().is_empty()
}

/// `tasklist`'s "no match" notice (`INFO: No tasks ...`), which some Windows versions may
/// write to stderr, is not an error; other stderr is.
#[cfg(windows)]
fn only_tasklist_info(stderr: &str) -> bool {
    stderr
        .lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.starts_with("INFO:"))
}

/// Whether a process with `pid` runs. A Unix zombie does not count: it has ended and only
/// waits for its parent (or init) to reap it. A query that fails is an error.
pub fn is_running(pid: u32) -> Result<bool, String> {
    #[cfg(unix)]
    {
        let mut ps = Command::new("ps");
        ps.args(["-o", "stat=", "-p", &pid.to_string()]);
        // ps exits 1 with no output when no process has the PID.
        let (code, stdout) = query(ps, &[0, 1], no_stderr)?;
        let stat = stdout.trim();
        if code == 1 && !stat.is_empty() {
            return Err(format!("ps -p {pid} exited 1 with output {stat:?}"));
        }
        Ok(!stat.is_empty() && !stat.starts_with('Z'))
    }
    #[cfg(windows)]
    {
        let mut tasklist = Command::new("tasklist");
        tasklist.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        // With no match, stdout holds only the INFO notice (or nothing when the notice went
        // to stderr), so it has no quoted PID.
        let (_, stdout) = query(tasklist, &[0], only_tasklist_info)?;
        Ok(stdout.contains(&format!("\"{pid}\"")))
    }
}

/// The PIDs of the running children of `pid`. A query that fails is an error; `pgrep`'s
/// "no match" (exit 1) is an empty list, and its exit 2 or more an error.
pub fn children_of(pid: u32) -> Result<Vec<u32>, String> {
    #[cfg(unix)]
    let (_, stdout) = {
        let mut pgrep = Command::new("pgrep");
        pgrep.args(["-P", &pid.to_string()]);
        query(pgrep, &[0, 1], no_stderr)?
    };
    #[cfg(windows)]
    let (_, stdout) = {
        // `$ErrorActionPreference = 'Stop'` turns a failed query into exit 1, so the exit
        // code alone tells success. stderr is not read: Windows PowerShell can write progress
        // records there (a module's first load) on a query that worked, and
        // `$ProgressPreference` keeps most of them away.
        let mut ps = Command::new("powershell");
        ps.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$ProgressPreference = 'SilentlyContinue'; $ErrorActionPreference = 'Stop'; \
                 (Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}').ProcessId"
            ),
        ]);
        query(ps, &[0], |_| true)?
    };
    let mut children = Vec::new();
    for word in stdout.split_whitespace() {
        let child: u32 = word
            .parse()
            .map_err(|_| format!("unexpected child PID {word:?} of {pid}"))?;
        if is_running(child)? {
            children.push(child);
        }
    }
    Ok(children)
}

/// Waits up to `timeout` until none of `pids` runs. Returns the ones still running, or the
/// error of a query that failed.
pub fn wait_all_gone(
    pids: &[(String, u32)],
    timeout: Duration,
) -> Result<Vec<(String, u32)>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut left = Vec::new();
        for (what, pid) in pids {
            if is_running(*pid)? {
                left.push((what.clone(), *pid));
            }
        }
        if left.is_empty() || Instant::now() >= deadline {
            return Ok(left);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
