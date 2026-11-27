//! Child processes whose stdout lines a test waits for, and OS-level process checks that work
//! on PIDs the test did not create itself (the dialer's `ssh` children, sshd's session
//! children).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
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

/// The child PIDs a [`Proc`] saw right before it was killed (see [`Proc::track_children`]).
#[derive(Clone, Default)]
pub struct ChildList(Arc<Mutex<Vec<u32>>>);

impl ChildList {
    fn add(&self, pids: Vec<u32>) {
        let mut list = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for pid in pids {
            if !list.contains(&pid) {
                list.push(pid);
            }
        }
    }

    pub fn pids(&self) -> Vec<u32> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// How long a killed process's stdout reader may take to pass the last lines on. A child that
/// shares the killed process's stdout (the tunnel ssh shares the dialer's) keeps the pipe
/// open, so the reader may not end within it.
const OUTPUT_END_LIMIT: Duration = Duration::from_secs(2);

/// A child process with piped stdio. Its stdout lines come through [`Proc::expect`]; stdout
/// and stderr both go to its [`Transcript`]. Dropping it kills and reaps the child.
pub struct Proc {
    pub name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    pub transcript: Transcript,
    status: Option<ExitStatus>,
    stdout_reader: Option<JoinHandle<()>>,
    children: Option<ChildList>,
}

fn relay(
    name: String,
    from: impl Read + Send + 'static,
    transcript: Transcript,
    tx: Option<mpsc::Sender<String>>,
) -> JoinHandle<()> {
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
    })
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
        let stdout_reader = relay(
            name.into(),
            child.stdout.take().expect("stdout"),
            transcript.clone(),
            Some(tx),
        );
        // The stderr reader is not joined; the transcript is read for stdout lines.
        drop(relay(
            name.into(),
            child.stderr.take().expect("stderr"),
            transcript.clone(),
            None,
        ));
        let stdin = child.stdin.take();
        Self {
            name: name.into(),
            child,
            stdin,
            lines,
            transcript,
            status: None,
            stdout_reader: Some(stdout_reader),
            children: None,
        }
    }

    /// From now on, [`Proc::kill`] lists the process's children right before it kills it and
    /// keeps their PIDs in the returned list, so children that outlive it can be ended too.
    pub fn track_children(&mut self) -> ChildList {
        self.children.get_or_insert_with(ChildList::default).clone()
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

    /// Whether the process has exited, without waiting; reaps it when it has.
    pub fn has_exited(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.try_wait().expect("try_wait");
        }
        self.status.is_some()
    }

    /// Kills the process and reaps it. With [`Proc::track_children`], first lists its
    /// children. Afterwards waits up to [`OUTPUT_END_LIMIT`] for the stdout reader to pass the
    /// last lines to the transcript.
    pub fn kill(&mut self) {
        if self.status.is_none() {
            if let Some(list) = &self.children {
                let Children { pids, error } = children_of(self.pid());
                list.add(pids);
                if let Some(e) = error {
                    log(&format!("listing the children of {}: {e}", self.name));
                }
            }
            let _killed = self.child.kill().is_ok();
            self.status = self.child.wait().ok();
            self.join_stdout_reader();
        }
    }

    fn join_stdout_reader(&mut self) {
        let deadline = Instant::now() + OUTPUT_END_LIMIT;
        while self
            .stdout_reader
            .as_ref()
            .is_some_and(|reader| !reader.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(reader) = self.stdout_reader.take_if(|reader| reader.is_finished())
            && reader.join().is_err()
        {
            log(&format!("the stdout reader of {} panicked", self.name));
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
    if let Err(e) = try_terminate(pid) {
        log(&e);
    }
}

/// [`terminate`], returning a failed end command (one that did not start, or exited with an
/// error, as for a process that is already gone) as an error.
pub fn try_terminate(pid: u32) -> Result<(), String> {
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
        Ok(s) if s.success() => Ok(()),
        other => Err(format!("ending pid {pid} failed: {other:?}")),
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

/// What [`children_of`] found: the children it saw, and the error of a query that failed on
/// the way. The PIDs found before (or apart from) the error stay in it.
pub struct Children {
    pub pids: Vec<u32>,
    pub error: Option<String>,
}

/// The running children of `pid`. A query that fails is an error; `pgrep`'s "no match"
/// (exit 1) is an empty list, and its exit 2 or more an error. When the state of one child
/// cannot be read, the child stays in the list (it may run) with the error, and the others
/// are still checked.
pub fn children_of(pid: u32) -> Children {
    #[cfg(unix)]
    let listed = {
        let mut pgrep = Command::new("pgrep");
        pgrep.args(["-P", &pid.to_string()]);
        query(pgrep, &[0, 1], no_stderr)
    };
    #[cfg(windows)]
    let listed = {
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
        query(ps, &[0], |_| true)
    };
    let mut found = Children {
        pids: Vec::new(),
        error: None,
    };
    let stdout = match listed {
        Ok((_, stdout)) => stdout,
        Err(e) => {
            found.error = Some(e);
            return found;
        }
    };
    for word in stdout.split_whitespace() {
        let Ok(child) = word.parse::<u32>() else {
            found
                .error
                .get_or_insert_with(|| format!("unexpected child PID {word:?} of {pid}"));
            continue;
        };
        match is_running(child) {
            Ok(true) => found.pids.push(child),
            Ok(false) => {}
            Err(e) => {
                found.pids.push(child);
                found.error.get_or_insert(e);
            }
        }
    }
    found
}

/// One line about `pid` for a failure log: its PID, parent PID, state and command line, or
/// what the query said when there is no such process or the query failed.
pub fn describe(pid: u32) -> String {
    #[cfg(unix)]
    let mut command = {
        let mut ps = Command::new("ps");
        ps.args(["-o", "pid=,ppid=,stat=,args=", "-p", &pid.to_string()]);
        ps
    };
    #[cfg(windows)]
    let mut command = {
        let mut ps = Command::new("powershell");
        ps.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$ProgressPreference = 'SilentlyContinue'; \
                 Get-CimInstance Win32_Process -Filter 'ProcessId={pid}' | \
                 ForEach-Object {{ \"$($_.ProcessId) $($_.ParentProcessId) $($_.CommandLine)\" }}"
            ),
        ]);
        ps
    };
    match command.stdin(Stdio::null()).output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                format!("pid {pid}: no such process ({})", out.status)
            } else {
                text
            }
        }
        Err(e) => format!("pid {pid}: the query did not start: {e}"),
    }
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
