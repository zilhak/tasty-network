//! Remote port discovery: run a remote command over SSH and decode what it prints.
//!
//! The discovery loop does not know the record format. The caller passes a decoder for the
//! command's stdout ([`decode_port`] reads a bare port number). Program, directory, and file
//! names of the remote side come from [`PortDiscoveryConfig`]; this crate has no defaults.
//!
//! This module is the part both backends share: the modes and their remote commands, the
//! failure classes, the budget, and the Auto chain. How one remote command runs is the
//! backend's ([`RemoteRun`]): the system backend starts an `ssh` child (`system::discovery`).

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::backend::SshBackendError;
use crate::settings::SshSettings;
use crate::{SshTarget, connect_timeout_for};

/// The tracing target of the discovery events of both backends: the module path discovery
/// had before the backends were split, so log filters on that path keep selecting them.
pub(crate) const LOG_TARGET: &str = "tasty_net_ssh::discovery";

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
    /// The subcommand's words for the remote shell. ssh joins them with spaces into one command
    /// line. The program word stays as is when [`program_plain`] allows it, so sh, PowerShell,
    /// and cmd all run it from the command position; otherwise it is quoted for sh (PowerShell
    /// then reads the quoted word as a string and does not run it). Arguments are quoted with
    /// [`sh_quote`].
    pub(crate) fn subcommand_words(&self) -> Vec<String> {
        let program = if program_plain(&self.remote_program) {
            self.remote_program.clone()
        } else {
            sh_single_quote(&self.remote_program)
        };
        std::iter::once(program)
            .chain(self.remote_args.iter().map(|w| sh_quote(w)))
            .collect()
    }

    /// The remote command that reads the record file at the conventional place, quoted for
    /// the shell that runs it ([`sh_path`] for `cat`, [`cmd_quote`] for `type`).
    pub(crate) fn file_command(&self, windows: bool) -> String {
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
pub(crate) fn sh_plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:%+".contains(c))
}

/// Whether the subcommand's program word goes unquoted: ASCII letters, digits, and `_-./:\`.
/// A Windows path such as `C:\Tools\netprog.exe` then reaches PowerShell and cmd as tasty sent
/// it. sh drops an unquoted `\`, so a Unix program path with a backslash does not run as
/// written; such paths are rare.
pub(crate) fn program_plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:\\".contains(c))
}

/// Quotes `s` for a POSIX shell: single quotes, with `'` written as `'\''`. A plain value
/// ([`sh_plain`]) is left as is. PowerShell reads single quotes too, but writes `'` inside them
/// as `''`; a value with `'` is quoted for sh only.
pub(crate) fn sh_quote(s: &str) -> String {
    if sh_plain(s) {
        s.to_string()
    } else {
        sh_single_quote(s)
    }
}

/// `s` in sh single quotes, always: `'` is written as `'\''`.
pub(crate) fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// [`sh_quote`] for a record path, keeping a leading `~/` outside the quotes so the remote shell
/// still expands it. Only a path's leading `~/` expands; a `~` anywhere else is quoted.
pub(crate) fn sh_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", sh_quote(rest)),
        None => sh_quote(path),
    }
}

/// Quotes `s` for cmd, always: double quotes keep one word through spaces, `,`, `=`, and `;`
/// (cmd's built-ins split on all of them), and cmd still expands `%VAR%` inside them, whose
/// value can itself hold a space (`%USERPROFILE%` of `C:\Users\Jane Doe`). cmd has no escape
/// for `"` inside double quotes, so such a value cannot be read.
pub(crate) fn cmd_quote(s: &str) -> String {
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
    /// The caller stopped the lookup with [`SshCancel::cancel`](crate::SshCancel::cancel). It wins over a timeout.
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
    pub(crate) kind: PortDiscoveryFailureKind,
    pub(crate) detail: String,
    /// Exit code of the remote command. `None` if it did not finish or ended by a signal.
    pub(crate) exit_code: Option<i32>,
    /// The embedded backend's resolution for port discovery refused the target
    /// ([`SshBackendError::ConfigRefused`]) before anything reached the network.
    pub(crate) refusal: Option<Box<SshBackendError>>,
}

impl PortDiscoveryError {
    pub(crate) fn new(kind: PortDiscoveryFailureKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        tracing::debug!(target: LOG_TARGET, ?kind, %detail, "remote port discovery failed");
        Self {
            kind,
            detail,
            exit_code: None,
            refusal: None,
        }
    }

    /// The error of a lookup whose configuration resolution refused the target: `refusal` is
    /// the [`SshBackendError::ConfigRefused`]. Its kind is a failed connection, and no other
    /// step or mode runs after it (the same configuration refuses them too).
    #[cfg(feature = "embedded-ssh")]
    pub(crate) fn refused(what: &str, refusal: SshBackendError) -> Self {
        let mut error = Self::new(
            PortDiscoveryFailureKind::SshConnectionFailed,
            format!("{what}: {refusal}"),
        );
        error.refusal = Some(Box::new(refusal));
        error
    }

    /// Whether the lookup stops here: a cancel, or a refused configuration.
    fn stops(&self) -> bool {
        self.kind == PortDiscoveryFailureKind::Cancelled || self.refusal.is_some()
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

    /// The configuration refusal that ended the lookup before it reached the network: the
    /// embedded backend's resolution for port discovery (`Match command`, `Match sessiontype`
    /// included) refused the target ([`SshBackendError::ConfigRefused`], with the refused
    /// directive and the notes). The same configuration refuses again, so a caller does not
    /// retry it.
    pub fn config_refused(&self) -> Option<&SshBackendError> {
        self.refusal.as_deref()
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
pub(crate) fn classify_by_exit_code(code: Option<i32>) -> PortDiscoveryFailureKind {
    match code {
        Some(255) | None => PortDiscoveryFailureKind::SshConnectionFailed,
        Some(_) => PortDiscoveryFailureKind::RemoteInstanceNotRunning,
    }
}

/// A connection failure that took at least the connect timeout in effect is reported as a
/// timeout. This is an estimate from exit code and time.
pub(crate) fn refine_with_elapsed(
    kind: PortDiscoveryFailureKind,
    elapsed: Duration,
    connect_timeout: Duration,
) -> PortDiscoveryFailureKind {
    if kind == PortDiscoveryFailureKind::SshConnectionFailed && elapsed >= connect_timeout {
        return PortDiscoveryFailureKind::TimedOut;
    }
    kind
}

/// Expiry of the total budget of one discovery call. It hands each step what is left, so the
/// total wait does not grow with the number of steps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    pub(crate) at: Instant,
}

impl Deadline {
    pub(crate) fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    pub(crate) fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// The bound for this step: the smaller of the step bound `step` and the rest of the
    /// budget. `Duration::ZERO` means the budget is used up (no ssh is started).
    pub(crate) fn step_budget(self, step: Duration) -> Duration {
        step.min(self.remaining())
    }
}

/// The values one discovery call runs with: the launcher's settings, and the connect timeout
/// ssh uses for the target ([`connect_timeout_for`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) step_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) output_limit: usize,
    pub(crate) stderr_limit: usize,
    pub(crate) stderr_detail_limit: usize,
    #[cfg(feature = "system-ssh")]
    pub(crate) overflow_exit_grace: Duration,
    #[cfg(feature = "system-ssh")]
    pub(crate) stderr_grace: Duration,
    #[cfg(feature = "system-ssh")]
    pub(crate) poll_min: Duration,
    pub(crate) poll_max: Duration,
}

impl Limits {
    pub(crate) fn new(settings: &SshSettings, target: &SshTarget) -> Self {
        Self {
            step_timeout: settings.step_timeout(),
            connect_timeout: connect_timeout_for(settings, target),
            output_limit: settings.output_limit(),
            stderr_limit: settings.stderr_limit(),
            stderr_detail_limit: settings.stderr_detail_limit(),
            #[cfg(feature = "system-ssh")]
            overflow_exit_grace: settings.overflow_exit_grace(),
            #[cfg(feature = "system-ssh")]
            stderr_grace: settings.stderr_grace(),
            #[cfg(feature = "system-ssh")]
            poll_min: settings.poll_min(),
            poll_max: settings.poll_max(),
        }
    }
}

/// The error of an attempt stopped by cancellation. `what` goes only into the detail.
pub(crate) fn cancelled_error(what: &str) -> PortDiscoveryError {
    PortDiscoveryError::new(
        PortDiscoveryFailureKind::Cancelled,
        format!("{what}: stopped by cancellation"),
    )
}

/// Decodes the stdout of a successful command. A refusal is
/// [`PortDiscoveryFailureKind::PortParseFailed`]. The detail keeps only the output length: neither
/// the stdout nor the decoder's error goes into it, because both can quote a record with a token.
pub(crate) fn decode_output<T, E>(
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

/// Runs of at least this many token-like characters are removed from stderr. It is the redaction
/// rule tied to the token format (64 hex digits), not a setting: half a token is still removed,
/// and ordinary words and paths are shorter (docs/design/transport.md "SSH 크레이트 설정값").
pub(crate) const SECRET_RUN: usize = 32;

/// The stderr text a detail may keep: trimmed, with every run of [`SECRET_RUN`] or more ASCII
/// letters, digits, `+`, `=`, `_`, or `-` replaced by `<redacted N chars>`, and cut to
/// `detail_limit` characters (`discovery.stderr_detail_limit`), the `...` of a cut included. A
/// failing remote command could print a record (and its token) to stderr; ordinary messages
/// (paths, "Permission denied") have no such runs.
///
/// `cut` says the text is the start of a longer stderr (`discovery.stderr_limit`). A run at its
/// end may then be the first part of a token, so it is redacted whatever its length.
pub(crate) fn sanitize_stderr(stderr: &str, cut: bool, detail_limit: usize) -> String {
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
    if out.chars().count() <= detail_limit {
        return out;
    }
    let keep = detail_limit.saturating_sub(ELLIPSIS.len());
    match out.char_indices().nth(keep) {
        Some((at, _)) => format!("{}{ELLIPSIS}", &out[..at]),
        None => out,
    }
}

/// How one remote command of a discovery runs: the backend's part of [`Probe`].
pub(crate) trait RemoteRun {
    /// The label of the runs in failure details (logs). `Display` does not show it.
    fn what(&self) -> String;

    /// Runs the command line `remote_argv` (the words are joined with spaces, as OpenSSH sends
    /// them) within `budget` and returns the stdout of a successful command. A failure carries
    /// the class and exit code of [`PortDiscoveryError`].
    fn run(
        &self,
        remote_argv: &[&str],
        budget: Duration,
        what: &str,
        limits: Limits,
    ) -> Result<String, PortDiscoveryError>;
}

/// One discovery call: the backend's runner, names, the shared deadline, and the values in
/// effect.
/// A [`RemoteRun`] that remembers the last command that ran to the end, so the effective
/// target of the command the discovery used can be read afterwards.
pub(crate) struct Recording<'a> {
    pub(crate) inner: &'a dyn RemoteRun,
    pub(crate) last: std::sync::Mutex<Option<Vec<String>>>,
}

impl RemoteRun for Recording<'_> {
    fn what(&self) -> String {
        self.inner.what()
    }

    fn run(
        &self,
        remote_argv: &[&str],
        budget: Duration,
        what: &str,
        limits: Limits,
    ) -> Result<String, PortDiscoveryError> {
        let out = self.inner.run(remote_argv, budget, what, limits)?;
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *last = Some(remote_argv.iter().map(|a| (*a).to_owned()).collect());
        Ok(out)
    }
}

pub(crate) struct Probe<'a> {
    runner: &'a dyn RemoteRun,
    config: &'a PortDiscoveryConfig,
    deadline: Deadline,
    limits: Limits,
}

impl<'a> Probe<'a> {
    /// A probe with `settings`: the budget `discovery.total_timeout` starts now.
    pub(crate) fn new(
        runner: &'a dyn RemoteRun,
        settings: &SshSettings,
        target: &SshTarget,
        config: &'a PortDiscoveryConfig,
    ) -> Self {
        Self {
            runner,
            config,
            deadline: Deadline::after(settings.total_timeout()),
            limits: Limits::new(settings, target),
        }
    }
}

impl Probe<'_> {
    /// Runs one remote command and decodes its stdout ([`decode_output`]).
    fn run<T, E>(
        &self,
        remote_argv: &[&str],
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        // `what` goes only into the detail (logs). `Display` does not show it.
        let what = self.runner.what();
        let out = self.runner.run(
            remote_argv,
            self.deadline.step_budget(self.limits.step_timeout),
            &what,
            self.limits,
        )?;
        decode_output(&what, &out, decode)
    }

    /// The subcommand mode.
    fn subcommand<T, E>(
        &self,
        decode: &impl Fn(&str) -> Result<T, E>,
    ) -> Result<T, PortDiscoveryError> {
        let words = self.config.subcommand_words();
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
            Err(e) if e.stops() => return Err(e),
            Err(e) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    "port file `{}` failed ({e}); trying the next",
                    cmds[0],
                )
            }
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
pub(crate) fn auto_chain<T, F>(mut try_mode: F) -> Result<T, PortDiscoveryError>
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
            Err(e) if stops_auto_chain(e.kind) || e.stops() => return Err(e),
            Err(e) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    "{m:?} port discovery failed ({e}); falling back",
                );
                errors.push(e);
            }
        }
    }
    Err(pick_most_informative(errors))
}

/// The remote subcommand's exit code [`PORT_COMMAND_EXITED_INSTANCE_CODE`] settles on an
/// exited instance. Older remotes without that finding exit with other codes and keep the
/// estimated class.
pub(crate) fn refine_port_command_failure(mut error: PortDiscoveryError) -> PortDiscoveryError {
    if error.kind == PortDiscoveryFailureKind::RemoteInstanceNotRunning
        && error.exit_code == Some(PORT_COMMAND_EXITED_INSTANCE_CODE)
    {
        error.kind = PortDiscoveryFailureKind::RemoteInstanceExited;
    }
    error
}

/// Remote commands for an explicit record path, in the order they are tried.
pub(crate) fn explicit_file_commands(port_file: &str) -> [String; 2] {
    [
        format!("cat {}", sh_path(port_file)),
        format!("type {}", cmd_quote(port_file)),
    ]
}

/// Auto tries the subcommand, then the Unix file command, then the Windows file command. An
/// earlier way can fail depending on the shell and the program, hence several ways.
pub(crate) const AUTO_FALLBACK_CHAIN: [PortMode; 3] = [
    PortMode::Subcommand,
    PortMode::FileUnix,
    PortMode::FileWindows,
];

/// Failures after which Auto does not try the next mode.
pub(crate) fn stops_auto_chain(kind: PortDiscoveryFailureKind) -> bool {
    matches!(
        kind,
        PortDiscoveryFailureKind::Cancelled | PortDiscoveryFailureKind::RemoteInstanceExited
    )
}

/// Picks the representative of several failures: cancelled, exited instance, timed out,
/// connection failed, no instance (estimate), parse failed. Within one class the earliest step
/// wins.
pub(crate) fn pick_most_informative(mut errors: Vec<PortDiscoveryError>) -> PortDiscoveryError {
    const PRIORITY: [PortDiscoveryFailureKind; 6] = [
        PortDiscoveryFailureKind::Cancelled,
        PortDiscoveryFailureKind::RemoteInstanceExited,
        PortDiscoveryFailureKind::TimedOut,
        PortDiscoveryFailureKind::SshConnectionFailed,
        PortDiscoveryFailureKind::RemoteInstanceNotRunning,
        PortDiscoveryFailureKind::PortParseFailed,
    ];
    // A refused configuration first: it is the reason, not a guess.
    if let Some(pos) = errors.iter().position(|e| e.refusal.is_some()) {
        return errors.remove(pos);
    }
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

/// Reads the remote record with `probe` and decodes it with `decode`: the explicit path of the
/// probe's names first and alone, otherwise `mode` (see `discover_remote_port`).
pub(crate) fn discover<T, E>(
    probe: &Probe<'_>,
    mode: PortMode,
    decode: &impl Fn(&str) -> Result<T, E>,
) -> Result<T, PortDiscoveryError> {
    if let Some(path) = &probe.config.explicit_port_file {
        return probe.explicit_file(path, decode);
    }
    probe.single_mode(mode, decode)
}

/// Returns the first mode of the probe that reached the remote side. It does not find out the
/// shell itself.
pub(crate) fn detect_first_success<F>(mut try_mode: F) -> Result<PortMode, PortDiscoveryError>
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
            Err(e) if e.stops() => return Err(e),
            Err(e) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    "{m:?} detection probe failed ({e}); trying the next mode",
                );
                errors.push(e);
            }
        }
    }
    Err(pick_most_informative(errors))
}

/// The first mode of the Auto chain that reaches the remote side with `probe` (see
/// `detect_port_mode`).
pub(crate) fn detect<T, E>(
    probe: &Probe<'_>,
    decode: &impl Fn(&str) -> Result<T, E>,
) -> Result<PortMode, PortDiscoveryError> {
    detect_first_success(|m| probe.single_mode(m, decode).map(|_| ()))
}
