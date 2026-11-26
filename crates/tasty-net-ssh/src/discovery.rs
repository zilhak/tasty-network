//! Remote port discovery: run a remote command over ssh and decode what it prints.
//!
//! The discovery loop does not know the record format. The caller passes a decoder for the
//! command's stdout ([`decode_port`] reads a bare port number). Program, directory, and file
//! names of the remote side come from [`PortDiscoveryConfig`]; this crate has no defaults.

use std::fmt;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cancel::{SshCancel, cancel_requested, current_cancel, kill_and_reap};
use crate::{SSH_CONNECT_TIMEOUT, SpawnHook, SshLauncher, SshTarget};

/// Upper bound for one discovery step: the spawn hook, the wait for the child, and output
/// collection share it. When it expires the child is killed and reaped. Spawning itself and the
/// reap after a kill have no bound of their own.
pub const PORT_DISCOVERY_STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// Time budget the steps of one discovery call share. Each step gets the smaller of the step
/// bound and what is left. When the budget is used up no new child is started. Spawning itself
/// and the reap after a kill are not guaranteed to fit in it.
pub const PORT_DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(45);

/// Exit code of the remote subcommand when the instance that wrote the record has exited.
/// It differs from the general failure 1 and the usage error 2, so discovery can settle on
/// "the remote instance is not running" instead of guessing.
pub const PORT_COMMAND_EXITED_INSTANCE_CODE: i32 = 3;

/// How the remote record is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortMode {
    /// `ssh <dest> <remote_program> <remote_args...>`. Works on Unix shells and Git Bash. A
    /// Windows GUI build of the remote program can print nothing under PowerShell or cmd.
    Subcommand,
    /// Unix remote: `cat ~/<data_dir>/<port_file>`.
    FileUnix,
    /// Windows remote: `type "%USERPROFILE%\<data_dir>\<port_file>"`.
    FileWindows,
    /// Subcommand, then the Unix file, then the Windows file. Covers PowerShell, cmd, bash,
    /// and zsh as the remote default shell.
    Auto,
}

impl PortMode {
    /// Parses `auto`, `subcommand`, `file-unix`, or `file-windows`.
    pub fn parse(s: &str) -> Result<Self, PortModeParseError> {
        Ok(match s {
            "auto" => Self::Auto,
            "subcommand" => Self::Subcommand,
            "file-unix" => Self::FileUnix,
            "file-windows" => Self::FileWindows,
            other => {
                return Err(PortModeParseError::Unknown {
                    value: other.to_string(),
                });
            }
        })
    }

    /// The string form ([`PortMode::parse`] reads it back).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Subcommand => "subcommand",
            Self::FileUnix => "file-unix",
            Self::FileWindows => "file-windows",
        }
    }
}

impl FromStr for PortMode {
    type Err = PortModeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for PortMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A port mode string that is not one of the four modes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PortModeParseError {
    /// The string is none of the mode names.
    Unknown {
        /// The string that was not recognized.
        value: String,
    },
}

impl PortModeParseError {
    /// The string that was not recognized.
    pub fn value(&self) -> &str {
        match self {
            Self::Unknown { value } => value,
        }
    }
}

impl fmt::Display for PortModeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { value } => write!(
                f,
                "Unknown port mode '{value}' (expected auto, subcommand, file-unix, or file-windows)."
            ),
        }
    }
}

impl std::error::Error for PortModeParseError {}

/// Names of the remote side that discovery uses. The consumer gives every value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortDiscoveryConfig {
    /// Program the subcommand mode runs on the remote host (a name on the remote `PATH` or a
    /// full path).
    pub remote_program: String,
    /// Arguments after `remote_program` in the subcommand mode.
    pub remote_args: Vec<String>,
    /// Directory under the remote home directory that holds the record file.
    pub data_dir: String,
    /// File name of the record inside `data_dir`.
    pub port_file: String,
    /// A remote path to read instead. When set, discovery reads only this path (`cat <path>`,
    /// then `type <path>`), before and instead of the mode chain. The remote shell expands
    /// `~`, so an absolute path is safer.
    pub explicit_port_file: Option<String>,
}

impl PortDiscoveryConfig {
    /// The remote command that reads the record file at the conventional place, quoted for
    /// the shell that runs it ([`sh_path`] for `cat`, [`cmd_quote`] for `type`).
    fn file_command(&self, windows: bool) -> String {
        if windows {
            let path = format!("%USERPROFILE%\\{}\\{}", self.data_dir, self.port_file);
            format!("type {}", cmd_quote(&path))
        } else {
            let path = format!("~/{}/{}", self.data_dir, self.port_file);
            format!("cat {}", sh_path(&path))
        }
    }
}

/// Whether `s` reads as one literal word, unquoted, in a POSIX shell and in PowerShell (the
/// shells that run `cat` and the subcommand). Such values go to the remote shell unchanged, as
/// tasty sent every value. Left out on purpose: `\` (an escape in sh), `,` (an array in
/// PowerShell), `=` (an assignment as sh's first word), `~` (home expansion; only a path's
/// leading `~/` is meant to expand, see [`sh_path`]), and `@` (splatting in PowerShell).
fn sh_plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:%+".contains(c))
}

/// Quotes `s` for a POSIX shell: single quotes, with `'` written as `'\''`. A plain value
/// ([`sh_plain`]) is left as is. PowerShell reads single quotes too, but writes `'` inside them
/// as `''`; a value with `'` is quoted for sh only.
fn sh_quote(s: &str) -> String {
    if sh_plain(s) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// [`sh_quote`] for a record path, keeping a leading `~/` outside the quotes so the remote shell
/// still expands it. Only a path's leading `~/` expands; a `~` anywhere else is quoted.
fn sh_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", sh_quote(rest)),
        None => sh_quote(path),
    }
}

/// Quotes `s` for cmd, always: double quotes keep one word through spaces, `,`, `=`, and `;`
/// (cmd's built-ins split on all of them), and cmd still expands `%VAR%` inside them, whose
/// value can itself hold a space (`%USERPROFILE%` of `C:\Users\Jane Doe`). cmd has no escape
/// for `"` inside double quotes, so such a value cannot be read.
fn cmd_quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// Failure classes of port discovery. They come from exit codes and elapsed time, not from
/// locale-dependent stderr text, so some of them are estimates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortDiscoveryFailureKind {
    /// The SSH connection or authentication failed (exit 255, killed by a signal, or the
    /// local spawn failed).
    SshConnectionFailed,
    /// SSH and the remote command ran, but no record came back. Probably no instance is
    /// running on the remote host (no record file, the subcommand failed).
    RemoteInstanceNotRunning,
    /// The remote subcommand found that the instance which wrote the record has exited
    /// ([`PORT_COMMAND_EXITED_INSTANCE_CODE`]). This is the remote side's finding, not a guess.
    RemoteInstanceExited,
    /// The command succeeded (exit 0) but the decoder did not accept its stdout.
    PortParseFailed,
    /// The polling budget ran out, or a connection failure took at least the connect timeout.
    /// Cancellation is reported separately.
    TimedOut,
    /// The caller stopped the lookup with [`SshCancel::cancel`]. It wins over a timeout.
    Cancelled,
}

impl fmt::Display for PortDiscoveryFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SshConnectionFailed => {
                "SSH connection failed (this may be a network or authentication problem)."
            }
            Self::RemoteInstanceNotRunning => {
                "No instance appears to be running on the remote host."
            }
            Self::RemoteInstanceExited => {
                "The remote instance is not running; its port file was left by an instance that has exited."
            }
            Self::PortParseFailed => "Could not parse the port record reported by the remote host.",
            Self::TimedOut => {
                "The remote host did not respond in time (it may be unreachable, or the connection may be too slow)."
            }
            Self::Cancelled => "The port lookup was stopped.",
        })
    }
}

/// A port discovery failure. `kind` is for branching and user messages; `detail` keeps the
/// diagnostic text for logs.
///
/// `Display` prints only the English sentence of the kind. It does not show remote stderr,
/// internal commands, or the command's stdout. The detail (and `Debug`) never holds the stdout
/// or the decoder's error either, because a record can carry a token; remote stderr goes in only
/// cut short and with token-like runs removed.
#[derive(Debug)]
pub struct PortDiscoveryError {
    kind: PortDiscoveryFailureKind,
    detail: String,
    /// Exit code of the remote command. `None` if it did not finish or ended by a signal.
    exit_code: Option<i32>,
}

impl PortDiscoveryError {
    fn new(kind: PortDiscoveryFailureKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        tracing::debug!(?kind, %detail, "remote port discovery failed");
        Self {
            kind,
            detail,
            exit_code: None,
        }
    }

    pub fn kind(&self) -> PortDiscoveryFailureKind {
        self.kind
    }

    /// Diagnostic text for logs. It is not part of `Display`.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Exit code of the remote command, when it finished with one.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
}

impl fmt::Display for PortDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.kind, f)
    }
}

impl std::error::Error for PortDiscoveryError {}

/// Why [`decode_port`] refused the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PortDecodeError {
    /// The output has no non-empty line.
    Empty,
    /// The last non-empty line is not a number from 0 to 65535.
    NotAPort,
}

impl fmt::Display for PortDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "The output has no line to read a port from.",
            Self::NotAPort => "The last output line is not a port number.",
        })
    }
}

impl std::error::Error for PortDecodeError {}

/// Decoder for a bare port number: the last non-empty line, trimmed (a trailing CR from `type`
/// and a banner before it are fine).
pub fn decode_port(stdout: &str) -> Result<u16, PortDecodeError> {
    let last = stdout
        .trim()
        .lines()
        .next_back()
        .ok_or(PortDecodeError::Empty)?;
    last.trim()
        .parse::<u16>()
        .map_err(|_| PortDecodeError::NotAPort)
}

/// Maps an exit code to a failure class. 255 or a signal means the connection failed; other
/// codes are taken as "no remote instance". The remote command can also return 255, and other
/// codes do not prove that the instance is missing.
fn classify_by_exit_code(code: Option<i32>) -> PortDiscoveryFailureKind {
    match code {
        Some(255) | None => PortDiscoveryFailureKind::SshConnectionFailed,
        Some(_) => PortDiscoveryFailureKind::RemoteInstanceNotRunning,
    }
}

/// A connection failure that took at least the connect timeout is reported as a timeout. This
/// is an estimate from exit code and time.
fn refine_with_elapsed(
    kind: PortDiscoveryFailureKind,
    elapsed: Duration,
) -> PortDiscoveryFailureKind {
    if kind == PortDiscoveryFailureKind::SshConnectionFailed && elapsed >= SSH_CONNECT_TIMEOUT {
        return PortDiscoveryFailureKind::TimedOut;
    }
    kind
}

/// Expiry of the total budget of one discovery call. It hands each step what is left, so the
/// total wait does not grow with the number of steps.
#[derive(Clone, Copy, Debug)]
struct Deadline {
    at: Instant,
}

impl Deadline {
    fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// The bound for this step: the smaller of the step bound and the rest of the budget.
    /// `Duration::ZERO` means the budget is used up (no ssh is started).
    fn step_budget(self) -> Duration {
        PORT_DISCOVERY_STEP_TIMEOUT.min(self.remaining())
    }
}

/// The error of an attempt stopped by cancellation. `what` goes only into the detail.
fn cancelled_error(what: &str) -> PortDiscoveryError {
    PortDiscoveryError::new(
        PortDiscoveryFailureKind::Cancelled,
        format!("{what}: stopped by cancellation"),
    )
}

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
                    tracing::debug!("ssh try_wait failed; polling stops: {e}");
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
    /// `stop` said to cut the child off (the caller kills it).
    Stopped,
}

/// Waits for the child until `deadline`, asking `stop` on every poll.
///
/// The crate has no async runtime, so it polls `try_wait`. The interval starts at 5 ms and
/// doubles up to 50 ms: little delay on the normal path (hundreds of ms), few wakeups on long
/// waits.
fn wait_with_timeout(
    owner: &mut ChildOwner,
    deadline: Instant,
    stop: &mut dyn FnMut() -> bool,
) -> WaitEnd {
    let mut nap = Duration::from_millis(5);
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
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

/// A discovery command's stdout beyond this many bytes is refused. A record is small.
const OUTPUT_LIMIT: usize = 64 * 1024;
/// Only this much of stderr is kept for the detail. The rest is read and dropped.
const STDERR_LIMIT: usize = 16 * 1024;

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
                        tracing::debug!(%e, "ssh output read ended");
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

fn start_readers(child: &mut Child) -> std::io::Result<Readers> {
    let stdout = child
        .stdout
        .take()
        .map(|p| spawn_reader(p, OUTPUT_LIMIT, OnOverflow::Stop, "ssh-discovery-stdout"))
        .transpose()?;
    let stderr = child
        .stderr
        .take()
        .map(|p| spawn_reader(p, STDERR_LIMIT, OnOverflow::Drain, "ssh-discovery-stderr"))
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
    /// Whether the output already passed [`OUTPUT_LIMIT`]. It does not wait.
    fn overflowed(&mut self) -> bool {
        if self.early.is_none()
            && let Some(rx) = &self.rx
            && let Ok(captured) = rx.try_recv()
        {
            self.early = Some(captured);
        }
        self.early.as_ref().is_some_and(|c| c.overflowed)
    }

    fn collect(self, deadline: Instant, what: &str) -> Result<Captured, PortDiscoveryError> {
        match (self.early, self.rx) {
            (Some(captured), _) => settled(captured, what),
            (None, Some(rx)) => collect(&rx, deadline, what),
            (None, None) => Ok(Captured::default()),
        }
    }
}

/// Kills a child the wait gave up on and says why. When cancellation took the child or came
/// meanwhile, the reason is `Cancelled`, not the timeout or the overflow.
fn cut_off(owner: ChildOwner, end: &WaitEnd, what: &str, budget: Duration) -> PortDiscoveryError {
    let reclaimed = owner.kill_and_reap_now();
    if !reclaimed || cancel_requested() {
        cancelled_error(what)
    } else if *end == WaitEnd::Stopped {
        output_too_long(what)
    } else {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::TimedOut,
            format!("{what}: killed after not finishing within {budget:?}"),
        )
    }
}

/// The refusal of an output past [`OUTPUT_LIMIT`].
fn output_too_long(what: &str) -> PortDiscoveryError {
    PortDiscoveryError::new(
        PortDiscoveryFailureKind::PortParseFailed,
        format!("{what}: the output is longer than {OUTPUT_LIMIT} bytes"),
    )
}

/// How long a failed command's stderr is waited for after the command exited. stderr only
/// feeds the diagnostic detail, so a descendant holding it open does not keep the lookup.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// Waits for a reader's result until `deadline`, stopping early on cancellation. A result that
/// already arrived is taken even at the deadline.
fn collect(
    rx: &mpsc::Receiver<Captured>,
    deadline: Instant,
    what: &str,
) -> Result<Captured, PortDiscoveryError> {
    loop {
        if cancel_requested() {
            return Err(cancelled_error(what));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let received = match rx.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(captured) => captured,
            Err(mpsc::RecvTimeoutError::Disconnected) => Captured::default(),
            Err(mpsc::RecvTimeoutError::Timeout) if left.is_zero() => {
                return Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::TimedOut,
                    format!("{what}: the output stayed open after the command exited"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
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
/// over [`OUTPUT_LIMIT`] is refused as [`PortDiscoveryFailureKind::PortParseFailed`] as soon as
/// it passes the limit: the child is cut off then, even if it would write forever. The
/// bound and cancellation also cover the wait for the pipes to close after the child exits (a
/// descendant can hold them). One deadline, taken before the spawn, covers the spawn hook, the
/// wait for the child, and output collection; only spawning itself and reaping after a kill
/// have no bound of their own. When cancellation and the bound meet, the result is
/// `Cancelled`.
///
/// A failed command keeps the class and exit code its exit gives. Its stderr is only waited for
/// up to [`STDERR_GRACE`] (within the deadline); if it does not arrive, or cancellation comes,
/// the detail goes without it.
fn run_capture_with_budget(
    mut cmd: Command,
    budget: Duration,
    what: &str,
    spawn: &SpawnHook,
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
    let (stdout, stderr_rx) = match start_readers(&mut child) {
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
    let end = wait_with_timeout(&mut owner, deadline, &mut || stdout.overflowed());
    if end != WaitEnd::Ended {
        return Err(cut_off(owner, &end, what, budget));
    }
    let mut child = owner.finish(what)?;
    let status = child.wait().map_err(|e| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            format!("{what}: reaping the command failed: {e}"),
        )
    })?;
    let wait_for = |rx: Option<mpsc::Receiver<Captured>>, until: Instant| match rx {
        Some(rx) => collect(&rx, until, what),
        None => Ok(Captured::default()),
    };
    // A child that died writing to the closed stdout failed because of its output size.
    if stdout.overflowed() {
        return Err(output_too_long(what));
    }
    if !status.success() {
        // The exit settles the class; stderr is only a diagnostic and cannot override it.
        let code = status.code();
        let elapsed = started.elapsed();
        let stderr = match wait_for(stderr_rx, deadline.min(Instant::now() + STDERR_GRACE)) {
            Ok(stderr) => {
                sanitize_stderr(&String::from_utf8_lossy(&stderr.bytes), stderr.overflowed)
            }
            Err(e) => format!("(stderr not collected: {})", e.kind()),
        };
        let mut error = PortDiscoveryError::new(
            refine_with_elapsed(classify_by_exit_code(code), elapsed),
            format!("{what}: code={code:?} elapsed={elapsed:?}: {stderr}"),
        );
        error.exit_code = code;
        return Err(error);
    }
    let stdout = stdout.collect(deadline, what)?;
    if stdout.overflowed {
        return Err(output_too_long(what));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

/// Decodes the stdout of a successful command. A refusal is
/// [`PortDiscoveryFailureKind::PortParseFailed`]. The detail keeps only the output length: neither
/// the stdout nor the decoder's error goes into it, because both can quote a record with a token.
fn decode_output<T, E>(
    what: &str,
    stdout: &str,
    decode: &impl Fn(&str) -> Result<T, E>,
) -> Result<T, PortDiscoveryError> {
    decode(stdout).map_err(|_| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::PortParseFailed,
            format!(
                "{what}: the decoder refused the output ({} bytes)",
                stdout.len()
            ),
        )
    })
}

/// Runs of at least this many token-like characters are removed from stderr.
const SECRET_RUN: usize = 32;
/// The stderr kept in a detail is at most this many characters, the `...` of a cut included.
const STDERR_DETAIL_LIMIT: usize = 512;

/// The stderr text a detail may keep: trimmed, with every run of [`SECRET_RUN`] or more ASCII
/// letters, digits, `+`, `=`, `_`, or `-` replaced by `<redacted N chars>`, and cut to
/// [`STDERR_DETAIL_LIMIT`] characters. A failing remote command could print a record (and its
/// token) to stderr; ordinary messages (paths, "Permission denied") have no such runs.
///
/// `cut` says the text is the start of a longer stderr ([`STDERR_LIMIT`]). A run at its end may
/// then be the first part of a token, so it is redacted whatever its length.
fn sanitize_stderr(stderr: &str, cut: bool) -> String {
    fn token_like(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-')
    }
    fn flush(out: &mut String, run: &mut String) {
        if run.len() >= SECRET_RUN {
            out.push_str(&format!("<redacted {} chars>", run.len()));
        } else {
            out.push_str(run);
        }
        run.clear();
    }
    let cut_run = cut && stderr.chars().next_back().is_some_and(token_like);
    let text = if cut_run {
        stderr.trim_start()
    } else {
        stderr.trim()
    };
    let mut out = String::new();
    let mut run = String::new();
    for c in text.chars() {
        if token_like(c) {
            run.push(c);
        } else {
            flush(&mut out, &mut run);
            out.push(c);
        }
    }
    if cut_run && !run.is_empty() {
        out.push_str(&format!("<redacted {} chars>", run.len()));
    } else {
        flush(&mut out, &mut run);
    }
    const ELLIPSIS: &str = "...";
    if out.chars().count() <= STDERR_DETAIL_LIMIT {
        return out;
    }
    let keep = STDERR_DETAIL_LIMIT - ELLIPSIS.len();
    match out.char_indices().nth(keep) {
        Some((at, _)) => format!("{}{ELLIPSIS}", &out[..at]),
        None => out,
    }
}

/// One discovery call: launcher, target, names, and the shared deadline.
struct Probe<'a> {
    launcher: &'a SshLauncher,
    target: &'a SshTarget,
    config: &'a PortDiscoveryConfig,
    deadline: Deadline,
}

impl Probe<'_> {
    /// Runs one remote command and decodes its stdout ([`decode_output`]).
    fn run<T, E>(
        &self,
        remote_argv: &[&str],
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let cmd = self.launcher.command(self.target, &[], &[], remote_argv);
        // `what` goes only into the detail (logs). `Display` does not show it.
        let what = format!("ssh({})", self.launcher.program().display());
        let out = run_capture_with_budget(
            cmd,
            self.deadline.step_budget(),
            &what,
            self.launcher.spawn_hook(),
        )?;
        decode_output(&what, &out, decode)
    }

    /// The subcommand mode.
    fn subcommand<T, E>(
        &self,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        // ssh joins the words with spaces into one command line for the remote shell, so each
        // word is quoted for sh (see `sh_quote` for other shells).
        let words: Vec<String> = std::iter::once(&self.config.remote_program)
            .chain(&self.config.remote_args)
            .map(|w| sh_quote(w))
            .collect();
        let argv: Vec<&str> = words.iter().map(String::as_str).collect();
        self.run(&argv, decode).map_err(refine_port_command_failure)
    }

    /// The file modes at the conventional place.
    fn file<T, E>(
        &self,
        windows: bool,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        self.run(&[&self.config.file_command(windows)], decode)
    }

    /// The explicit path: `cat <path>` (Unix, PowerShell, Git Bash), then `type <path>` (cmd).
    fn explicit_file<T, E>(
        &self,
        path: &str,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let cmds = explicit_file_commands(path);
        match self.run(&[&cmds[0]], decode) {
            Ok(found) => return Ok(found),
            Err(e) if e.kind == PortDiscoveryFailureKind::Cancelled => return Err(e),
            Err(e) => tracing::debug!("port file `{}` failed ({e}); trying the next", cmds[0]),
        }
        self.run(&[&cmds[1]], decode)
    }

    /// One non-Auto mode.
    fn single_mode<T, E>(
        &self,
        mode: PortMode,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        match mode {
            PortMode::Subcommand => self.subcommand(decode),
            PortMode::FileUnix => self.file(false, decode),
            PortMode::FileWindows => self.file(true, decode),
            PortMode::Auto => self.auto(decode),
        }
    }

    /// The Auto chain: first success, or the most telling failure.
    fn auto<T, E>(&self, decode: &impl Fn(&str) -> Result<T, E>) -> Result<T, PortDiscoveryError> {
        auto_chain(|m| self.single_mode(m, decode))
    }
}

/// Runs the Auto chain: the first mode that finds the record wins, a stopping failure ends the
/// chain, and otherwise the most informative failure is returned.
fn auto_chain<T, F>(mut try_mode: F) -> Result<T, PortDiscoveryError>
where
    F: FnMut(PortMode) -> Result<T, PortDiscoveryError>,
{
    let mut errors = Vec::with_capacity(AUTO_FALLBACK_CHAIN.len());
    for m in AUTO_FALLBACK_CHAIN {
        match try_mode(m) {
            Ok(found) => return Ok(found),
            // Cancellation does not fall back: the next mode would start a new ssh for a
            // lookup that was just stopped. An exited instance stops too: the file modes
            // would read the record it left behind and report success.
            Err(e) if stops_auto_chain(e.kind) => return Err(e),
            Err(e) => {
                tracing::debug!("{m:?} port discovery failed ({e}); falling back");
                errors.push(e);
            }
        }
    }
    Err(pick_most_informative(errors))
}

/// The remote subcommand's exit code [`PORT_COMMAND_EXITED_INSTANCE_CODE`] settles on an
/// exited instance. Older remotes without that finding exit with other codes and keep the
/// estimated class.
fn refine_port_command_failure(mut error: PortDiscoveryError) -> PortDiscoveryError {
    if error.kind == PortDiscoveryFailureKind::RemoteInstanceNotRunning
        && error.exit_code == Some(PORT_COMMAND_EXITED_INSTANCE_CODE)
    {
        error.kind = PortDiscoveryFailureKind::RemoteInstanceExited;
    }
    error
}

/// Remote commands for an explicit record path, in the order they are tried.
fn explicit_file_commands(port_file: &str) -> [String; 2] {
    [
        format!("cat {}", sh_path(port_file)),
        format!("type {}", cmd_quote(port_file)),
    ]
}

/// Auto tries the subcommand, then the Unix file command, then the Windows file command. An
/// earlier way can fail depending on the shell and the program, hence several ways.
const AUTO_FALLBACK_CHAIN: [PortMode; 3] = [
    PortMode::Subcommand,
    PortMode::FileUnix,
    PortMode::FileWindows,
];

/// Failures after which Auto does not try the next mode.
fn stops_auto_chain(kind: PortDiscoveryFailureKind) -> bool {
    matches!(
        kind,
        PortDiscoveryFailureKind::Cancelled | PortDiscoveryFailureKind::RemoteInstanceExited
    )
}

/// Picks the representative of several failures: cancelled, exited instance, timed out,
/// connection failed, no instance (estimate), parse failed. Within one class the earliest step
/// wins.
fn pick_most_informative(mut errors: Vec<PortDiscoveryError>) -> PortDiscoveryError {
    const PRIORITY: [PortDiscoveryFailureKind; 6] = [
        PortDiscoveryFailureKind::Cancelled,
        PortDiscoveryFailureKind::RemoteInstanceExited,
        PortDiscoveryFailureKind::TimedOut,
        PortDiscoveryFailureKind::SshConnectionFailed,
        PortDiscoveryFailureKind::RemoteInstanceNotRunning,
        PortDiscoveryFailureKind::PortParseFailed,
    ];
    for kind in PRIORITY {
        if let Some(pos) = errors.iter().position(|e| e.kind == kind) {
            return errors.remove(pos);
        }
    }
    errors.pop().unwrap_or_else(|| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            "no discovery step ran",
        )
    })
}

/// Reads the remote record and decodes it with `decode`.
///
/// An explicit path in `config` is read first and alone. Otherwise `mode` decides; `Auto`
/// returns the first success, stops at cancellation and at an exited instance, and otherwise
/// returns the most telling failure. A command that succeeds with output the decoder refuses
/// is a failure. The steps share [`PORT_DISCOVERY_TOTAL_TIMEOUT`] and each is polled for at
/// most [`PORT_DISCOVERY_STEP_TIMEOUT`], output collection included. Only spawning itself and
/// the reap after a kill have no bound of their own. It blocks; call it from a worker. A [`SshCancel`] installed on the
/// calling thread with [`SshCancel::scope`] stops it.
pub fn discover_remote_port<T, E>(
    launcher: &SshLauncher,
    target: &SshTarget,
    mode: PortMode,
    config: &PortDiscoveryConfig,
    decode: impl Fn(&str) -> Result<T, E>,
) -> Result<T, PortDiscoveryError> {
    let probe = Probe {
        launcher,
        target,
        config,
        deadline: Deadline::after(PORT_DISCOVERY_TOTAL_TIMEOUT),
    };
    if let Some(path) = &config.explicit_port_file {
        return probe.explicit_file(path, &decode);
    }
    probe.single_mode(mode, &decode)
}

/// Returns the first mode of the probe that reached the remote side. It does not find out the
/// shell itself.
fn detect_first_success<F>(mut try_mode: F) -> Result<PortMode, PortDiscoveryError>
where
    F: FnMut(PortMode) -> Result<(), PortDiscoveryError>,
{
    let mut errors = Vec::with_capacity(AUTO_FALLBACK_CHAIN.len());
    for m in AUTO_FALLBACK_CHAIN {
        match try_mode(m) {
            Ok(()) => return Ok(m),
            // The remote side answered with its finding, so this mode works. The instance is
            // just not running.
            Err(e) if e.kind == PortDiscoveryFailureKind::RemoteInstanceExited => return Ok(m),
            Err(e) if e.kind == PortDiscoveryFailureKind::Cancelled => return Err(e),
            Err(e) => {
                tracing::debug!("{m:?} detection probe failed ({e}); trying the next mode");
                errors.push(e);
            }
        }
    }
    Err(pick_most_informative(errors))
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
    let probe = Probe {
        launcher,
        target,
        config,
        deadline: Deadline::after(PORT_DISCOVERY_TOTAL_TIMEOUT),
    };
    detect_first_success(|m| probe.single_mode(m, &decode).map(|_| ()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helper::{Orphan, helper_command};

    /// A command that fails if it is ever started. Tells an early return from a spawn.
    fn unspawnable_command() -> Command {
        Command::new("/tasty-net-ssh-test/this-path-must-not-exist")
    }

    /// A child that runs for 60 s and has no descendants (on every OS).
    fn never_returns_command() -> Command {
        helper_command("sleep")
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
        assert!(fresh.step_budget() <= PORT_DISCOVERY_STEP_TIMEOUT);
        let almost_gone = Deadline::after(Duration::from_millis(50));
        assert!(almost_gone.step_budget() <= Duration::from_millis(50));
        let expired = Deadline {
            at: Instant::now() - Duration::from_secs(1),
        };
        assert!(expired.step_budget().is_zero());
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
                .collect(far, "late cancel")
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
        let err = cut_off(owner, &WaitEnd::Expired, "taken", Duration::from_secs(1));
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);

        let child = never_returns_command()
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn");
        let err = cut_off(
            ChildOwner::Local(child),
            &WaitEnd::Expired,
            "expired",
            Duration::from_secs(1),
        );
        assert_eq!(err.kind(), PortDiscoveryFailureKind::TimedOut);
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

    /// Cancellation also stops the wait for an output held open by a descendant.
    #[test]
    fn cancel_stops_the_wait_for_a_held_output() {
        let handle = SshCancel::new();
        let _scope = handle.scope();
        let killer = handle.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            killer.cancel();
        });
        let mut orphan = Orphan::new("cancel-held");
        let started = Instant::now();
        let err = capture(
            orphan.command(30, 0),
            Duration::from_secs(20),
            "orphan cancel",
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
                        tracing::warn!("fake ssh cleanup failed: {e}");
                    }
                }
            }
        }

        fn target() -> SshTarget {
            SshTarget::parse("fake-host")
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
                tracing::warn!("quoted test cleanup failed: {e}");
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
