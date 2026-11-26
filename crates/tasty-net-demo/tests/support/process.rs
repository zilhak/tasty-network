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

/// Whether a process with `pid` runs. A Unix zombie does not count: it has ended and only
/// waits for its parent (or init) to reap it.
pub fn is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }
    #[cfg(windows)]
    {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
}

/// The PIDs of the running children of `pid`.
pub fn children_of(pid: u32) -> Vec<u32> {
    #[cfg(unix)]
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .expect("pgrep");
    #[cfg(windows)]
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}').ProcessId"),
        ])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|w| w.parse().ok())
        .filter(|&child| is_running(child))
        .collect()
}

/// Waits up to `timeout` until none of `pids` runs. Returns the ones still running.
pub fn wait_all_gone(pids: &[(String, u32)], timeout: Duration) -> Vec<(String, u32)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left: Vec<_> = pids
            .iter()
            .filter(|(_, pid)| is_running(*pid))
            .cloned()
            .collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
