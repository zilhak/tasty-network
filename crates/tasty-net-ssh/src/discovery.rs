//! Remote port discovery: run a remote command over ssh and decode what it prints.
//!
//! The discovery loop does not know the record format. The caller passes a decoder for the
//! command's stdout ([`decode_port`] reads a bare port number). Program, directory, and file
//! names of the remote side come from [`PortDiscoveryConfig`]; this crate has no defaults.

use std::fmt;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::cancel::{SshCancel, cancel_requested, current_cancel, kill_and_reap};
use crate::{SSH_CONNECT_TIMEOUT, SpawnHook, SshLauncher, SshTarget};

/// Upper bound for polling one discovery child. When it expires the child is killed and
/// reaped. It is not an absolute bound that includes spawning, reaping, and output collection.
pub const PORT_DISCOVERY_STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// Time budget the steps of one discovery call share. Each step gets the smaller of the step
/// bound and what is left. When the budget is used up no new child is started. Spawning,
/// reaping, and output collection are not guaranteed to fit in it.
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
    /// Windows remote: `type %USERPROFILE%\<data_dir>\<port_file>`.
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
                return Err(PortModeParseError {
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
pub struct PortModeParseError {
    value: String,
}

impl PortModeParseError {
    /// The string that was not recognized.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for PortModeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Unknown port mode '{}' (expected auto, subcommand, file-unix, or file-windows).",
            self.value
        )
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
    /// The remote command that reads the record file at the conventional place.
    fn file_command(&self, windows: bool) -> String {
        if windows {
            format!("type %USERPROFILE%\\{}\\{}", self.data_dir, self.port_file)
        } else {
            format!("cat ~/{}/{}", self.data_dir, self.port_file)
        }
    }
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
/// internal commands, or the command's stdout. The detail never holds the stdout either,
/// because a record can carry a token.
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

/// The last line of the output is not a port number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortDecodeError;

impl fmt::Display for PortDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The last output line is not a port number.")
    }
}

impl std::error::Error for PortDecodeError {}

/// Decoder for a bare port number: the last non-empty line, trimmed (a trailing CR from `type`
/// and a banner before it are fine).
pub fn decode_port(stdout: &str) -> Result<u16, PortDecodeError> {
    stdout
        .trim()
        .lines()
        .next_back()
        .unwrap_or("")
        .trim()
        .parse::<u16>()
        .map_err(|_| PortDecodeError)
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
    Scoped(SshCancel),
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
                Ok(()) => Ok(Self::Scoped(handle)),
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
            Self::Scoped(handle) => handle.is_cancelled(),
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
            Self::Scoped(handle) => handle.with_child(poll).unwrap_or(ChildPoll::Taken),
        }
    }

    /// Cuts the child off at the bound: kill and wait, so no zombie is left.
    fn kill_and_reap_now(self) {
        let child = match self {
            Self::Local(child) => Some(child),
            // If cancellation took it, the kill and wait are done.
            Self::Scoped(handle) => handle.reclaim(),
        };
        if let Some(mut child) = child {
            kill_and_reap(&mut child);
        }
    }

    /// Hands the child to output collection. If cancellation took it, or the cancel flag is
    /// set, the result is [`PortDiscoveryFailureKind::Cancelled`], so a child killed by
    /// cancellation is not classified as a connection failure or a timeout.
    fn finish(self, what: &str) -> Result<Child, PortDiscoveryError> {
        match self {
            Self::Local(child) => Ok(child),
            Self::Scoped(handle) => match handle.reclaim() {
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

/// `true` if the child ended within `budget`, `false` if it ran past it (the caller kills it).
/// A child taken by cancellation also gives `true`: there is nothing left to wait for, and
/// [`ChildOwner::finish`] settles the reason as [`PortDiscoveryFailureKind::Cancelled`].
///
/// The crate has no async runtime, so it polls `try_wait`. The interval starts at 5 ms and
/// doubles up to 50 ms: little delay on the normal path (hundreds of ms), few wakeups on long
/// waits.
fn wait_with_timeout(owner: &mut ChildOwner, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let mut nap = Duration::from_millis(5);
    loop {
        if owner.is_cancelled() {
            return true;
        }
        match owner.try_exited() {
            ChildPoll::Exited | ChildPoll::Taken => return true,
            ChildPoll::Running => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(nap.min(left));
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

/// Runs `cmd` through `spawn`, polls its exit, and applies cancellation and the bound. Returns
/// stdout on exit 0.
///
/// Output that fills the pipe and blocks the child is caught by the polling bound, but spawning,
/// reaping after a kill, and `wait_with_output` have no bound of their own. When cancellation
/// and the bound meet, the result is `Cancelled`.
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn.spawn(cmd).map_err(|e| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            format!("{what}: spawn failed: {e}"),
        )
    })?;
    // Hand the child to the cancel scope, so the bound watch and an outside cancel share it
    // and whichever comes first can cut it off.
    let mut owner = ChildOwner::adopt(child, what)?;
    if !wait_with_timeout(&mut owner, budget) {
        let cancelled = owner.is_cancelled();
        owner.kill_and_reap_now();
        return Err(if cancelled {
            cancelled_error(what)
        } else {
            PortDiscoveryError::new(
                PortDiscoveryFailureKind::TimedOut,
                format!("{what}: killed after not finishing within {budget:?}"),
            )
        });
    }
    let child = owner.finish(what)?;
    let output = child.wait_with_output().map_err(|e| {
        PortDiscoveryError::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            format!("{what}: collecting the output failed: {e}"),
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code();
        let elapsed = started.elapsed();
        let mut error = PortDiscoveryError::new(
            refine_with_elapsed(classify_by_exit_code(code), elapsed),
            format!(
                "{what}: code={code:?} elapsed={elapsed:?}: {}",
                stderr.trim()
            ),
        );
        error.exit_code = code;
        return Err(error);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One discovery call: launcher, target, names, and the shared deadline.
struct Probe<'a> {
    launcher: &'a SshLauncher,
    target: &'a SshTarget,
    config: &'a PortDiscoveryConfig,
    deadline: Deadline,
}

impl Probe<'_> {
    /// Runs one remote command and decodes its stdout. The decoder's error goes into the
    /// detail; the stdout does not.
    fn run<T, E: fmt::Display>(
        &self,
        remote_argv: &[&str],
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let cmd = self.launcher.command(self.target, &[], remote_argv);
        // `what` goes only into the detail (logs). `Display` does not show it.
        let what = format!("ssh({})", self.launcher.program().display());
        let out = run_capture_with_budget(
            cmd,
            self.deadline.step_budget(),
            &what,
            self.launcher.spawn_hook(),
        )?;
        decode(&out).map_err(|e| {
            PortDiscoveryError::new(
                PortDiscoveryFailureKind::PortParseFailed,
                format!("{what}: the output was not accepted: {e}"),
            )
        })
    }

    /// The subcommand mode.
    fn subcommand<T, E: fmt::Display>(
        &self,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let mut argv: Vec<&str> = vec![&self.config.remote_program];
        argv.extend(self.config.remote_args.iter().map(String::as_str));
        self.run(&argv, decode).map_err(refine_port_command_failure)
    }

    /// The file modes at the conventional place.
    fn file<T, E: fmt::Display>(
        &self,
        windows: bool,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        self.run(&[&self.config.file_command(windows)], decode)
    }

    /// The explicit path: `cat <path>` (Unix, PowerShell, Git Bash), then `type <path>` (cmd).
    fn explicit_file<T, E: fmt::Display>(
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
    fn single_mode<T, E: fmt::Display>(
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
    fn auto<T, E: fmt::Display>(
        &self,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let mut errors = Vec::with_capacity(AUTO_FALLBACK_CHAIN.len());
        for m in AUTO_FALLBACK_CHAIN {
            match self.single_mode(m, decode) {
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
    [format!("cat {port_file}"), format!("type {port_file}")]
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
/// most [`PORT_DISCOVERY_STEP_TIMEOUT`]. Spawning, reaping, and output collection have no
/// bound of their own. It blocks; call it from a worker. A [`SshCancel`] installed on the
/// calling thread with [`SshCancel::scope`] stops it.
pub fn discover_remote_port<T, E: fmt::Display>(
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
pub fn detect_port_mode<T, E: fmt::Display>(
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

    /// A command that fails if it is ever started. Tells an early return from a spawn.
    fn unspawnable_command() -> Command {
        Command::new("/tasty-net-ssh-test/this-path-must-not-exist")
    }

    fn never_returns_command() -> Command {
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd");
            c.args(["/c", "ping -n 60 127.0.0.1 > nul"]);
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = Command::new("sleep");
            c.arg("60");
            c
        }
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
        #[cfg(windows)]
        let cmd = {
            let mut c = Command::new("cmd");
            c.args(["/c", "echo 45123"]);
            c
        };
        #[cfg(not(windows))]
        let cmd = {
            let mut c = Command::new("echo");
            c.arg("45123");
            c
        };
        let out = capture(cmd, Duration::from_secs(10), "echo").unwrap();
        assert_eq!(decode_port(&out).unwrap(), 45123);
    }

    #[test]
    fn a_failing_command_keeps_its_exit_code() {
        #[cfg(windows)]
        let cmd = {
            let mut c = Command::new("cmd");
            c.args(["/c", "exit 3"]);
            c
        };
        #[cfg(not(windows))]
        let cmd = {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 3"]);
            c
        };
        let err = capture(cmd, Duration::from_secs(10), "exit 3").expect_err("exit 3");
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
        assert_eq!(decode_port("not-a-port"), Err(PortDecodeError));
        assert_eq!(decode_port(""), Err(PortDecodeError));
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
        assert_eq!(cmds[1], "type /data/x/net.port");
    }

    #[test]
    fn file_commands_use_the_configured_names() {
        let c = config();
        assert_eq!(c.file_command(false), "cat ~/.netprog-data/net.port");
        assert_eq!(
            c.file_command(true),
            "type %USERPROFILE%\\.netprog-data\\net.port"
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
    #[cfg(unix)]
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

    /// A child killed by cancellation is not reported as a timeout.
    #[cfg(unix)]
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
        assert!(wait_with_timeout(&mut owner, Duration::from_secs(5)));
        let err = owner
            .finish("cancel vs timeout")
            .expect_err("cancelled child");
        assert_eq!(err.kind(), PortDiscoveryFailureKind::Cancelled);
    }

    /// Tests with a fake `ssh`: a shell script that logs the remote command (its last argument)
    /// and acts by it.
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
            /// `on_port` runs for the subcommand (last argument `port`), `on_file` for any
            /// other remote command.
            fn new(name: &str, on_port: &str, on_file: &str) -> Self {
                static SEQ: AtomicUsize = AtomicUsize::new(0);
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir().join(format!(
                    "tasty-net-ssh-fake-{}-{seq}-{name}",
                    std::process::id()
                ));
                let log = base.with_extension("log");
                let script = format!(
                    "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n\
                     printf '%s\\n' \"$last\" >> '{}'\n\
                     if [ \"$last\" = port ]; then {on_port}; fi\n{on_file}\n",
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
            assert_eq!(exited.calls(), ["port"]);

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
            assert_eq!(old.calls(), ["port", "cat ~/.netprog-data/net.port"]);
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
            assert_eq!(slow.calls(), ["port"], "no file mode after the cancel");
        }

        /// Detection counts the mode whose subcommand reports an exited instance as reached.
        #[test]
        fn detect_port_mode_returns_the_mode_that_reported_an_exited_instance() {
            let exited = FakeSsh::new("detect-exited", "exit 3", "echo 45123");
            let mode = detect_port_mode(&exited.launcher(), &target(), &config(), decode_port)
                .expect("reached");
            assert_eq!(mode, PortMode::Subcommand);
            assert_eq!(exited.calls(), ["port"]);

            let windows_only = FakeSsh::new(
                "detect-windows",
                "exit 1",
                "case \"$last\" in type*) echo 45123;; *) exit 1;; esac",
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
                "case \"$last\" in type*) echo 45124;; *) exit 1;; esac",
            );
            let mut c = config();
            c.explicit_port_file = Some("/srv/rec/net.port".into());
            let port =
                discover_remote_port(&fake.launcher(), &target(), PortMode::Auto, &c, decode_port)
                    .expect("type works");
            assert_eq!(port, 45124);
            assert_eq!(
                fake.calls(),
                ["cat /srv/rec/net.port", "type /srv/rec/net.port"]
            );
        }

        /// The subcommand runs the configured program and arguments.
        #[test]
        fn the_subcommand_uses_the_configured_program_and_arguments() {
            let fake = FakeSsh::new("argv", "echo 45125", "exit 1");
            let launcher = fake.launcher();
            let cmd = launcher.command(&target(), &[], &["netprog", "port"]);
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(&args[args.len() - 3..], ["fake-host", "netprog", "port"]);
            let port = discover_remote_port(
                &launcher,
                &target(),
                PortMode::Subcommand,
                &config(),
                decode_port,
            )
            .expect("subcommand");
            assert_eq!(port, 45125);
        }

        /// The output the decoder refuses is not copied into the error.
        #[test]
        fn a_refused_record_stays_out_of_the_error() {
            let fake = FakeSsh::new("secret", "echo secret-token-0123", "exit 1");
            let err = discover_remote_port(
                &fake.launcher(),
                &target(),
                PortMode::Subcommand,
                &config(),
                |_out: &str| Err::<u16, _>("token: not 64 lowercase hex"),
            )
            .expect_err("refused");
            assert_eq!(err.kind(), PortDiscoveryFailureKind::PortParseFailed);
            assert!(err.detail().contains("token: not 64 lowercase hex"));
            assert!(!err.detail().contains("secret-token"), "{}", err.detail());
            assert!(!err.to_string().contains("secret-token"));
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
