// `deny`, not `forbid`: the Android module that reads the local network block reason through
// the NDK is the one place allowed to (docs/design/crates.md "unsafe 규칙", director 2937). A
// test checks that no other file allows it.
#![deny(unsafe_code)]

//! The SSH connections of the Tasty network library, over one of two backends
//! (docs/design/ssh-backends.md).
//!
//! - The system backend (`system-ssh`) runs the system `ssh` client as a child process.
//! - The embedded backend (`embedded-ssh`) is an in-process client: russh speaks the protocol
//!   and the OpenSSH port `tasty-net-ssh-config` reads the configuration and known_hosts files
//!   the way `ssh` does.
//!
//! `ssh.backend` chooses per attempt ([`SshConnector::select`]); `auto` falls back to the
//! embedded backend only when the system program file is not found.
//!
//! # Public contract
//!
//! | Item | Consumer |
//! |------|----------|
//! | [`SshTarget`] · [`PortMode`] · [`Backoff`] | consumers that connect over SSH |
//! | [`SshLauncher`] · [`resolve_ssh_path`] | consumers that start the system `ssh` (program, host key checking, hook) |
//! | [`SpawnHook`] · [`SpawnFn`] | hosts that bind `ssh` children to their own lifetime |
//! | [`SshTunnel`] · [`TunnelError`] | consumers that open `-L` tunnels (shared through an `Arc`; the last reference reaps the child) |
//! | [`SshCancel`] ([`SshCancelScope`] is its return type) | consumers that stop a port discovery from another thread |
//! | [`discover_remote_port`] · [`PortDiscoveryConfig`] · [`decode_port`] · [`PortDecodeError`] | consumers that read the remote record over SSH (the record decoder is passed in) |
//! | [`detect_port_mode`] | consumers that find which mode reaches a remote host (shell detection) |
//! | [`SshSettings`] ([`settings`]) · [`SshLauncher::with_settings`] · [`SshLauncher::settings`] | consumers that set or show the times, counts and limits of this crate |
//! | [`SshLauncher::applied_options`] · [`AppliedOption`] · [`OptionSource`] | consumers that show which ssh option value a target runs with and where it comes from |
//! | [`SSH_CONNECT_TIMEOUT`] · [`PORT_DISCOVERY_STEP_TIMEOUT`] · [`PORT_DISCOVERY_TOTAL_TIMEOUT`] | the defaults of `ssh.connect_timeout`, `discovery.step_timeout` and `discovery.total_timeout`; the values in effect are in [`SshLauncher::settings`] |
//! | [`PortDiscoveryError`] · [`PortDiscoveryFailureKind`] · [`PortModeParseError`] | consumers that branch on the failure cause (messages come from the kind, `Display` is fixed English) |
//! | [`PORT_COMMAND_EXITED_INSTANCE_CODE`] | the remote subcommand and port discovery, which share this exit code |
//! | [`tunnel_drop_totals`] | hosts that measure the time spent releasing tunnels |
//! | [`ListenerVerdict`] ([`SshTunnel::listener_verdict`]) · [`RemoteListenerAbsent`] · [`connection_closed_error`] · [`is_dropped_connection`] | consumers that classify a channel's failure behind a tunnel (one verdict per channel, taken before opening it) |
//! | [`SshConnector`] · [`Discovered`] | consumers that let `ssh.backend` choose the backend of each attempt and open port discovery and tunnels over it |
//! | [`SelectedBackend`] · [`SshBackendKind`] · [`BackendReason`] · [`SshBackendError`] · [`find_program`] · [`BackendChoice`] | consumers that show which backend an attempt used and why |
//! | [`EffectiveTarget`] · [`TunnelError::TargetMismatch`] | the check that port discovery and the tunnel reach the same target |
//! | [`SshInspection`] ([`SshConnector::inspect`]) · [`SshPurpose`] · [`InspectedSetting`] · [`ResolverState`] · [`ConfigNote`] · [`ResolvedTarget`] · [`TargetInspection`] and their parts | the settings query |
//! | [`SshConnector::resolve`] · [`SshTunnel::resolved`] · [`SshTunnel::config_notes`] | the connection info of the embedded backend (the resolution and the directives it did not apply) |
//! | [`SshPrompt`] · [`HostKeyQuestion`] · [`KeyboardInteractiveRequest`] ([`SshConnector::with_prompt`]) | applications that answer passphrase, password, keyboard-interactive and host key questions |
//! | [`HostKeyRefusal`] · [`HostKeyReason`] | consumers that show why a host key was not accepted |
//! | `SessionHook` · `SessionControl` (`embedded-ssh`) | tests that end an embedded session from outside, like [`SpawnHook`] sees `ssh` children |
//!
//! Profile handling (building an [`SshTarget`] from a stored profile, saving detection results)
//! is not here; the consumer does it with these value types. Host key checking is
//! [`SshLauncher::with_verify`]. The environment the crate reads: `PATH` when the backend
//! choice looks for the ssh program, and the OS defaults of the `embedded.*` text settings
//! (`HOME`/`USERPROFILE`, `USER`/`USERNAME`, `PROGRAMDATA`) when they are queried.

// An SSH crate without a backend is not built (docs/design/ssh-backends.md "feature 계약").
#[cfg(not(any(feature = "system-ssh", feature = "embedded-ssh")))]
compile_error!("tasty-net-ssh needs the system-ssh or embedded-ssh feature");

mod backend;
mod backoff;
mod cancel;
mod connector;
mod discovery;
#[cfg(feature = "embedded-ssh")]
mod embedded;
mod inspect;
mod listener;
mod prompt;
pub mod settings;
#[cfg(feature = "system-ssh")]
mod system;
mod tunnel;

pub use backend::{
    BackendReason, EffectiveTarget, HostKeyReason, HostKeyRefusal, SelectedBackend,
    SshBackendError, SshBackendKind, find_program,
};
pub use backoff::Backoff;
pub use cancel::{SshCancel, SshCancelScope};
pub use connector::{Discovered, SshConnector};
pub use discovery::{
    PORT_COMMAND_EXITED_INSTANCE_CODE, PortDecodeError, PortDiscoveryConfig, PortDiscoveryError,
    PortDiscoveryFailureKind, PortMode, PortModeParseError, decode_port,
};
#[cfg(feature = "embedded-ssh")]
pub use embedded::{SessionControl, SessionHook};
pub use inspect::{
    ConfigFileReport, ConfigFileState, ConfigNote, InspectedSetting, JumpHop, NoteAction,
    NoteClass, RefusalNote, ResolvedTarget, ResolverState, SshInspection, SshPurpose,
    TargetInspection, ValueSource,
};
pub use listener::{
    ChannelOpenRefused, ListenerVerdict, RemoteListenerAbsent, connection_closed_error,
    is_dropped_connection,
};
pub use prompt::{HostKeyQuestion, KeyboardInteractiveRequest, SshPrompt};
pub use settings::{
    BackendChoice, PORT_DISCOVERY_STEP_TIMEOUT, PORT_DISCOVERY_TOTAL_TIMEOUT, SSH_CONNECT_TIMEOUT,
    SshSettings,
};
#[cfg(feature = "system-ssh")]
pub use system::discovery::{detect_port_mode, discover_remote_port};
#[cfg(feature = "system-ssh")]
pub use system::resolve_ssh_path;
#[cfg(feature = "system-ssh")]
pub use system::spawn::{SpawnFn, SpawnHook, SshLauncher};
#[cfg(feature = "system-ssh")]
pub use system::tunnel::tunnel_drop_totals;
pub use tunnel::{SshTunnel, TunnelError};

use std::fmt;
use std::time::Duration;

use settings::Source;

/// Connection target and its ssh options. `ssh` resolves the destination itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshTarget {
    /// Destination passed to `ssh` as is (`user@host`, `host`, or a config alias).
    pub destination: String,
    /// ssh port given by the user. `None` leaves it to the ssh config (or 22).
    pub ssh_port: Option<u16>,
    /// Identity file (`-i`). A leading `~` is expanded to the home directory.
    pub identity_file: Option<String>,
    /// Extra ssh options. `"Key=Value"` becomes `-o Key=Value`.
    pub extra_options: Vec<String>,
}

impl SshTarget {
    /// A target with only a destination (`user@host`, `host`, or a config alias). The ssh port
    /// is left to `~/.ssh/config`.
    pub fn parse(dest: &str) -> Self {
        Self {
            destination: dest.to_string(),
            ssh_port: None,
            identity_file: None,
            extra_options: Vec::new(),
        }
    }
}

/// Adds the common ssh `-o` options to `args`.
///
/// - `BatchMode=no`: allows the host key and passphrase prompts of a first connection.
/// - `ServerAliveInterval`, `ServerAliveCountMax` (`ssh.server_alive_interval`,
///   `ssh.server_alive_count_max`; by default 15 s and 3): ssh ends itself within about 45 s
///   after the network drops.
/// - `StrictHostKeyChecking=accept-new` only with `verify`. Without it no host key option is
///   passed, so the user's options, the ssh config, or the ssh default (`ask`) decide.
/// - `forced`: options the caller requires (`Key=Value`, each after `-o`), such as the tunnel's
///   `ExitOnForwardFailure=yes`.
/// - The target's port (`-p`), identity file (`-i`) and `extra_options`.
/// - `ssh.overrides` (`Key=Value`, each after `-o`).
/// - `ConnectTimeout` (`ssh.connect_timeout`, by default [`SSH_CONNECT_TIMEOUT`]).
///
/// ssh uses the **first** value of an option. The options above `extra_options` therefore win
/// over them, the target's options win over `ssh.overrides`, and `ConnectTimeout` goes last so
/// that a user `ConnectTimeout` wins over the setting. Command line `-o` options also win over
/// the ssh config file. [`SshLauncher::applied_options`] reports the result
/// (docs/design/ssh-backends.md "옵션 적용 순서").
#[cfg(all(test, feature = "system-ssh"))]
pub(crate) fn push_common_opts(
    args: &mut Vec<String>,
    target: &SshTarget,
    verify: bool,
    forced: &[&str],
    settings: &SshSettings,
) {
    push_opts(args, target, verify, forced, &[], settings);
}

/// `push_common_opts` with the purpose's session flags (`-N`, `-L …`) right after the forced
/// options, before the target's options: OpenSSH fails when `-N` meets a session type that is
/// already set, and the first `-o SessionType` wins (docs/design/ssh-backends.md "옵션 적용
/// 순서" 4).
#[cfg(feature = "system-ssh")]
pub(crate) fn push_opts(
    args: &mut Vec<String>,
    target: &SshTarget,
    verify: bool,
    forced: &[&str],
    session: &[String],
    settings: &SshSettings,
) {
    args.push("-o".into());
    args.push("BatchMode=no".into());
    args.push("-o".into());
    args.push(format!(
        "{SERVER_ALIVE_INTERVAL_OPTION}={}",
        settings.server_alive_interval().as_secs()
    ));
    args.push("-o".into());
    args.push(format!(
        "{SERVER_ALIVE_COUNT_MAX_OPTION}={}",
        settings.server_alive_count_max()
    ));
    if verify {
        args.push("-o".into());
        args.push("StrictHostKeyChecking=accept-new".into());
    }
    for opt in forced {
        args.push("-o".into());
        args.push((*opt).to_string());
    }
    args.extend(session.iter().cloned());
    if let Some(p) = target.ssh_port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    if let Some(identity) = &target.identity_file {
        args.push("-i".into());
        args.push(expand_tilde(identity));
    }
    for opt in &target.extra_options {
        args.push("-o".into());
        args.push(opt.clone());
    }
    for opt in settings.overrides() {
        args.push("-o".into());
        args.push(opt.clone());
    }
    // Last, so that the user's value wins (see above).
    args.push("-o".into());
    args.push(format!(
        "{CONNECT_TIMEOUT_OPTION}={}",
        settings.connect_timeout().as_secs()
    ));
}

const CONNECT_TIMEOUT_OPTION: &str = "ConnectTimeout";
const SERVER_ALIVE_INTERVAL_OPTION: &str = "ServerAliveInterval";
const SERVER_ALIVE_COUNT_MAX_OPTION: &str = "ServerAliveCountMax";

/// The values of the options `name` in the target's extra options, in order (`Key=Value` or
/// `Key Value`; ssh compares option names without case). The value is the text after the name
/// and one `=`, as written; [`openssh_argument`] reads it the way ssh does. A name with no value
/// (`Key` alone or `Key=`) gives an empty value, which ssh refuses as a missing argument.
fn target_options<'a>(target: &'a SshTarget, name: &'a str) -> impl Iterator<Item = &'a str> {
    target.extra_options.iter().filter_map(move |opt| {
        let opt = opt.trim();
        let split = opt
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(opt.len());
        let (key, rest) = opt.split_at(split);
        let rest = rest.trim_start();
        let value = rest.strip_prefix('=').unwrap_or(rest).trim();
        key.eq_ignore_ascii_case(name).then_some(value)
    })
}

/// The single argument of an option value, split the way OpenSSH splits it (`argv_split`):
/// spaces and tabs end it, single or double quotes group text and are removed, and a backslash
/// escapes a quote, a backslash or (outside quotes) a space. A `#` that starts an argument
/// outside quotes starts a comment to the end (`terminate_on_comment`, as ssh reads options).
/// `None` when ssh would refuse it: no argument, an unclosed quote, or more than one argument.
fn openssh_argument(value: &str) -> Option<String> {
    let mut args: Vec<String> = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        if chars.peek().is_none_or(|c| *c == '#') {
            break;
        }
        let mut arg = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = chars.next() {
            match (c, quote) {
                ('\\', _) => match chars.peek() {
                    Some(&n)
                        if n == '\'' || n == '"' || n == '\\' || (n == ' ' && quote.is_none()) =>
                    {
                        arg.push(n);
                        chars.next();
                    }
                    _ => arg.push(c),
                },
                (' ' | '\t', None) => break,
                ('"' | '\'', None) => quote = Some(c),
                (q, Some(open)) if q == open => quote = None,
                _ => arg.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        args.push(arg);
    }
    let mut args = args.into_iter();
    match (args.next(), args.next()) {
        (Some(arg), None) => Some(arg),
        _ => None,
    }
}

/// The `none` of an OpenSSH time option: it leaves the option unset, so a later value sets it.
const OPENSSH_UNSET: &str = "none";
const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
const SECS_PER_WEEK: u64 = 7 * SECS_PER_DAY;
/// The units of the OpenSSH time format, with their seconds. A number with no unit is seconds.
const TIME_UNITS: [(char, u64); 5] = [
    ('s', 1),
    ('m', SECS_PER_MINUTE),
    ('h', SECS_PER_HOUR),
    ('d', SECS_PER_DAY),
    ('w', SECS_PER_WEEK),
];

/// A time in the integer OpenSSH time format (`convtime`): whole numbers, each followed by a
/// unit `s`, `m`, `h`, `d` or `w` (either case) or by nothing (seconds), written one after
/// another (`1m30s`, `90`), each unit at most once. A sign (`+5`), a unit written twice (`1s2s`,
/// also `1s2`) and a fraction (`1.5`, which OpenSSH 10.3 and later read) are not read. OpenSSH refuses a
/// total above `i32::MAX` seconds; so does this.
pub(crate) fn parse_openssh_time(text: &str) -> Option<Duration> {
    let mut total: u64 = 0;
    let mut used = [false; TIME_UNITS.len()];
    let mut digits = String::new();
    let mut add = |digits: &mut String, unit: char| -> Option<()> {
        let at = TIME_UNITS.iter().position(|(u, _)| *u == unit)?;
        if std::mem::replace(&mut used[at], true) {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        digits.clear();
        total = total.checked_add(n.checked_mul(TIME_UNITS[at].1)?)?;
        Some(())
    };
    if text.is_empty() {
        return None;
    }
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            add(&mut digits, c.to_ascii_lowercase())?;
        }
    }
    if !digits.is_empty() {
        add(&mut digits, 's')?;
    }
    (total <= u64::from(i32::MAX.unsigned_abs())).then(|| Duration::from_secs(total))
}

/// The target's own `ConnectTimeout` as this crate picks it for the query and the discovery
/// classification.
///
/// Like OpenSSH (`readconf.c`, `parse_time`) it takes the first value, skipping `none`, which
/// leaves the option unset so a later value still sets it. The first other value is the one
/// picked even when this crate does not read it. ssh itself also checks every later value and
/// exits before connecting if one is invalid; this crate does not repeat that check.
enum TargetTimeout<'a> {
    /// The target leaves it to this crate's setting.
    Unset,
    /// A value in the integer OpenSSH time format, as written.
    Read(&'a str, Duration),
    /// A value this crate does not read; it is shown as written and does not classify.
    Unreadable(&'a str),
}

fn target_connect_timeout(target: &SshTarget) -> TargetTimeout<'_> {
    for raw in target_options(target, CONNECT_TIMEOUT_OPTION) {
        let Some(arg) = openssh_argument(raw) else {
            return TargetTimeout::Unreadable(raw);
        };
        if arg == OPENSSH_UNSET {
            continue;
        }
        return match parse_openssh_time(&arg) {
            Some(d) => TargetTimeout::Read(raw, d),
            None => TargetTimeout::Unreadable(raw),
        };
    }
    TargetTimeout::Unset
}

/// The connect timeout ssh runs with for `target`: the target's own `ConnectTimeout` if it sets
/// one that can be read, otherwise `ssh.connect_timeout`. Port discovery classifies a slow
/// connection failure with it. A `ConnectTimeout` of 0 is no timeout for OpenSSH, so it gives
/// `Duration::MAX` (no failure is slow enough to be a timeout).
pub(crate) fn connect_timeout_for(settings: &SshSettings, target: &SshTarget) -> Duration {
    match target_connect_timeout(target) {
        TargetTimeout::Read(_, d) if d.is_zero() => Duration::MAX,
        TargetTimeout::Read(_, d) => d,
        TargetTimeout::Unset | TargetTimeout::Unreadable(_) => settings.connect_timeout(),
    }
}

/// Where the value of an ssh option this crate passes comes from, for one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionSource {
    /// The setting of the launcher ([`SshSettings`]), with its own source.
    Setting(Source),
    /// The target's `extra_options`, which ssh reads first.
    Target,
}

impl OptionSource {
    /// `default`, `configured` or `target`.
    pub fn name(self) -> &'static str {
        match self {
            OptionSource::Setting(s) => s.name(),
            OptionSource::Target => "target",
        }
    }
}

impl fmt::Display for OptionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One ssh option as ssh uses it for a target ([`SshLauncher::applied_options`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedOption {
    /// The setting the option comes from (`ssh.connect_timeout` and so on).
    pub key: &'static str,
    /// The ssh option name (`ConnectTimeout` and so on).
    pub option: &'static str,
    /// The value ssh uses, as its command line or the target's option writes it (seconds or a
    /// count).
    pub value: String,
    /// Where that value comes from.
    pub source: OptionSource,
}

impl fmt::Display for AppliedOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}) = {} ({})",
            self.key, self.option, self.value, self.source
        )
    }
}

/// The ssh options of [`push_common_opts`] that come from settings, as ssh uses them for
/// `target`. `ServerAliveInterval` and `ServerAliveCountMax` go before the target's options and
/// win over them; the target's own `ConnectTimeout` goes before the setting and wins over it.
pub(crate) fn applied_options(settings: &SshSettings, target: &SshTarget) -> Vec<AppliedOption> {
    let from_setting = |key, option, value: String| AppliedOption {
        key,
        option,
        value,
        source: OptionSource::Setting(settings.source(key)),
    };
    let connect = match target_connect_timeout(target) {
        TargetTimeout::Read(value, _) | TargetTimeout::Unreadable(value) => AppliedOption {
            key: settings::CONNECT_TIMEOUT,
            option: CONNECT_TIMEOUT_OPTION,
            value: value.to_owned(),
            source: OptionSource::Target,
        },
        TargetTimeout::Unset => from_setting(
            settings::CONNECT_TIMEOUT,
            CONNECT_TIMEOUT_OPTION,
            settings.connect_timeout().as_secs().to_string(),
        ),
    };
    vec![
        from_setting(
            settings::SERVER_ALIVE_INTERVAL,
            SERVER_ALIVE_INTERVAL_OPTION,
            settings.server_alive_interval().as_secs().to_string(),
        ),
        from_setting(
            settings::SERVER_ALIVE_COUNT_MAX,
            SERVER_ALIVE_COUNT_MAX_OPTION,
            settings.server_alive_count_max().to_string(),
        ),
        connect,
    ]
}

/// Expands a leading `~` or `~/` to the home directory. ssh is started without a shell, so
/// nothing else expands `-i ~/.ssh/id_ed25519`.
#[cfg(feature = "system-ssh")]
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok();
        if let Some(home) = home {
            let rest = &path[1..];
            return format!("{home}{rest}");
        }
    }
    path.to_string()
}

/// Writes an executable test script. A write fd open in this process could be inherited by a
/// child another test thread forks, and running the script would then fail with ETXTBSY until
/// that child execs. A separate `sh` writes the file, so this process never holds a write fd.
#[cfg(all(test, unix, feature = "system-ssh"))]
pub(crate) fn write_test_script(path: &std::path::Path, script: &str) {
    let status = std::process::Command::new("sh")
        .args([
            "-c",
            "printf '%s' \"$1\" > \"$2\" && chmod 700 \"$2\"",
            "sh",
        ])
        .arg(script)
        .arg(path)
        .status()
        .expect("sh");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

/// A test helper: this test binary run again in one mode, so the fakes behave the same on every
/// OS. Only the `orphan` mode leaves a descendant, and its test ends it ([`test_helper::Orphan`]).
#[cfg(all(test, feature = "system-ssh"))]
pub(crate) mod test_helper {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const ENV: &str = "TASTY_NET_SSH_TEST_HELPER";
    const ENTRY: &str = "test_helper::helper_entry";

    /// The helper in `mode`:
    /// - `sleep` or `sleep:<secs>`: runs for 60 s (or `<secs>`).
    /// - `orphan:<secs>:<code>:<pid file>`: starts `sleep:<secs>` sharing its stdout and stderr,
    ///   writes the grandchild's PID to `<pid file>`, and exits with `<code>` at once (0 prints a
    ///   port line first), so both pipes stay open after it exits. Build it with [`Orphan`].
    /// - `echo:<text>`: prints `<text>` on its own line and exits 0. The test harness prints its
    ///   own lines before it, so read the last line.
    /// - `fail:<code>:<text>`: writes `<text>` to stderr and exits with `<code>`.
    /// - `stderr:<code>:<n>:<text>`: writes `n` times `x`, a space, and `<text>` to stderr and
    ///   exits with `<code>`.
    /// - `flood:<bytes>[:<code>]`: writes `<bytes>` bytes to stdout (stopping at a write error)
    ///   and exits with `<code>` (0 by default). `flood:forever` writes until the pipe closes.
    ///   `flood:hold` writes until the pipe closes and then stays alive for 60 s, ignoring the
    ///   write error, so only a kill ends it.
    pub(crate) fn helper_command(mode: &str) -> Command {
        let mut c = Command::new(std::env::current_exe().expect("test binary path"));
        c.args([ENTRY, "--exact", "--nocapture", "--test-threads=1"])
            .env(ENV, mode);
        c
    }

    /// In helper mode (the env var is set) this acts as the mode and exits. In a normal test
    /// run it returns at once.
    #[test]
    fn helper_entry() {
        let Ok(mode) = std::env::var(ENV) else {
            return;
        };
        let (name, arg) = mode.split_once(':').unwrap_or((mode.as_str(), ""));
        match name {
            "sleep" => {
                let secs = arg.parse().unwrap_or(60);
                std::thread::sleep(Duration::from_secs(secs));
                std::process::exit(0);
            }
            "orphan" => {
                let mut parts = arg.splitn(3, ':');
                let secs = parts.next().expect("secs");
                let code: i32 = parts.next().expect("code").parse().expect("exit code");
                let pid_file = PathBuf::from(parts.next().expect("pid file"));
                let grandchild = helper_command(&format!("sleep:{secs}"))
                    .stdin(Stdio::null())
                    .spawn()
                    .expect("grandchild");
                // Write then rename, so the test never reads a partial PID.
                let partial = pid_file.with_extension("partial");
                std::fs::write(&partial, grandchild.id().to_string()).expect("pid file");
                std::fs::rename(&partial, &pid_file).expect("pid file");
                if code == 0 {
                    println!("\n45123");
                } else {
                    eprint!("the instance has exited");
                }
                std::process::exit(code);
            }
            "echo" => {
                println!("\n{arg}");
                std::process::exit(0);
            }
            "fail" => {
                let (code, text) = arg.split_once(':').unwrap_or((arg, ""));
                eprint!("{text}");
                std::process::exit(code.parse().expect("exit code"));
            }
            "stderr" => {
                let mut parts = arg.splitn(3, ':');
                let code: i32 = parts.next().expect("code").parse().expect("exit code");
                let n: usize = parts.next().expect("count").parse().expect("count");
                let text = parts.next().unwrap_or("");
                let mut err = std::io::stderr().lock();
                let _written = write!(err, "{} {text}", "x".repeat(n));
                let _flushed = err.flush();
                std::process::exit(code);
            }
            "flood" => {
                let (count, code) = arg.split_once(':').unwrap_or((arg, "0"));
                let code: i32 = code.parse().expect("exit code");
                let hold = count == "hold";
                let n: usize = if count == "forever" || hold {
                    usize::MAX
                } else {
                    count.parse().expect("byte count")
                };
                let chunk = [b'x'; 4096];
                let mut out = std::io::stdout().lock();
                let mut left = n;
                while left > 0 {
                    let k = left.min(chunk.len());
                    if out.write_all(&chunk[..k]).is_err() {
                        break;
                    }
                    left -= k;
                }
                let _flushed = out.flush();
                if hold {
                    std::thread::sleep(Duration::from_secs(60));
                }
                std::process::exit(code);
            }
            other => panic!("unknown helper mode {other}"),
        }
    }

    /// The grandchild an `orphan` helper leaves behind. [`Orphan::end`] (or drop) kills it by the
    /// PID the helper wrote and waits until it is gone, so no test leaves a process behind.
    pub(crate) struct Orphan {
        pid_file: PathBuf,
        ended: bool,
    }

    impl Orphan {
        pub(crate) fn new(name: &str) -> Self {
            let pid_file = std::env::temp_dir().join(format!(
                "tasty-net-ssh-orphan-{}-{name}.pid",
                std::process::id()
            ));
            let _stale = std::fs::remove_file(&pid_file);
            Self {
                pid_file,
                ended: false,
            }
        }

        /// Where the PID goes. A shell fake can write a background job's PID here too.
        pub(crate) fn pid_file(&self) -> &std::path::Path {
            &self.pid_file
        }

        /// The helper command: a grandchild sleeping `secs` seconds, the child exiting `code`.
        pub(crate) fn command(&self, secs: u64, code: i32) -> Command {
            helper_command(&format!("orphan:{secs}:{code}:{}", self.pid_file.display()))
        }

        /// Kills the grandchild and returns whether it is gone. The grandchild is not our child,
        /// so it cannot be waited for; this checks with the OS until it no longer runs. A failed
        /// process lookup panics (the test fails).
        pub(crate) fn end(&mut self) -> bool {
            self.end_checked(false)
        }

        /// [`Orphan::end`]. With `lenient`, a failed lookup ends the check instead of panicking:
        /// the kill is still sent, and `false` says the end was not confirmed.
        fn end_checked(&mut self, lenient: bool) -> bool {
            self.ended = true;
            let limit = Instant::now() + Duration::from_secs(5);
            let pid = loop {
                if let Ok(pid) = std::fs::read_to_string(&self.pid_file) {
                    break pid.trim().to_string();
                }
                if Instant::now() > limit {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let _removed = std::fs::remove_file(&self.pid_file);
            kill_pid(&pid);
            while Instant::now() < limit {
                match lookup(&pid) {
                    Ok(stat) if !runs_in(&stat) => return true,
                    Ok(_) => {}
                    Err(_) if lenient => return false,
                    Err(e) => panic!("{e}"),
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }
    }

    impl Drop for Orphan {
        /// Ends the grandchild if the test did not. While a test is already panicking, a failed
        /// lookup does not panic again (that would abort); the kill is still sent.
        fn drop(&mut self) {
            if !self.ended {
                self.end_checked(std::thread::panicking());
            }
        }
    }

    /// The process state of `pid` ([`lookup`]); a failed lookup panics.
    fn pid_stat(pid: &str) -> String {
        lookup(pid).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Whether `pid` still runs. A zombie waiting for its new parent to reap it does not run.
    fn pid_runs(pid: &str) -> bool {
        runs_in(&pid_stat(pid))
    }

    /// Whether process `pid` runs (a zombie does not).
    pub(crate) fn process_runs(pid: u32) -> bool {
        pid_runs(&pid.to_string())
    }

    /// Whether process `pid` exists at all, a zombie included. A reaped child does not. A lookup
    /// that cannot run or fails panics, so a test never reads "gone" from a failed query.
    pub(crate) fn process_exists(pid: u32) -> bool {
        !pid_stat(&pid.to_string()).is_empty()
    }

    #[cfg(unix)]
    fn kill_pid(pid: &str) {
        let _status = Command::new("kill").args(["-KILL", pid]).status();
    }

    /// The `ps` state of `pid`, empty when there is no such process. `ps -p` exits 1 with no
    /// output for a PID it does not find. Any other result is a failed lookup: `ps` cannot run,
    /// exits otherwise, or writes to stderr even with exit 0 (macOS `ps` can when sysctl fails).
    #[cfg(unix)]
    fn lookup(pid: &str) -> Result<String, String> {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .map_err(|e| format!("ps could not run: {e}"))?;
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let quiet = out.stderr.is_empty();
        let no_such_process = out.status.code() == Some(1) && stat.is_empty() && quiet;
        if (out.status.success() && quiet) || no_such_process {
            return Ok(stat);
        }
        Err(format!(
            "ps -p {pid} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }

    /// Whether a `ps` state means the process runs (a zombie, `Z`, does not).
    #[cfg(unix)]
    fn runs_in(stat: &str) -> bool {
        !stat.is_empty() && !stat.starts_with('Z')
    }

    /// `taskkill /F` ends the process with TerminateProcess.
    #[cfg(windows)]
    fn kill_pid(pid: &str) {
        let _status = Command::new("taskkill")
            .args(["/F", "/PID", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// The `tasklist` row of `pid`, empty when there is no such process. Windows has no zombies;
    /// an exited process whose handle is still open is not listed. `tasklist` exits 0 whether or
    /// not it finds the PID, so a failure to run it or another exit is a failed lookup.
    #[cfg(windows)]
    fn lookup(pid: &str) -> Result<String, String> {
        let filter = format!("PID eq {pid}");
        let out = Command::new("tasklist")
            .args(["/FI", &filter, "/NH", "/FO", "CSV"])
            .output()
            .map_err(|e| format!("tasklist could not run: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "tasklist for {pid} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let rows = String::from_utf8_lossy(&out.stdout).into_owned();
        Ok(if rows.contains(&format!("\"{pid}\"")) {
            rows
        } else {
            String::new()
        })
    }

    #[cfg(windows)]
    fn runs_in(row: &str) -> bool {
        !row.is_empty()
    }
}

// These tests drive the system backend's helpers; the default build runs them.
#[cfg(all(test, feature = "system-ssh"))]
mod tests {
    use super::*;

    /// The process lookup tells a running process from a missing one. A failed lookup would
    /// panic rather than read as "gone".
    #[test]
    fn the_process_lookup_tells_running_from_gone() {
        use test_helper::{process_exists, process_runs};
        assert!(process_exists(std::process::id()));
        assert!(process_runs(std::process::id()));
        /// Kills and reaps the 60 s child even if a lookup below panics.
        struct Reaped(std::process::Child);
        impl Drop for Reaped {
            fn drop(&mut self) {
                let _killed = self.0.kill();
                let _reaped = self.0.wait();
            }
        }
        let mut child = Reaped(
            test_helper::helper_command("sleep")
                .stdin(std::process::Stdio::null())
                .spawn()
                .expect("spawn"),
        );
        let pid = child.0.id();
        assert!(process_runs(pid));
        child.0.kill().expect("kill");
        child.0.wait().expect("reap");
        assert!(!process_exists(pid), "a reaped child is gone");
    }

    #[test]
    fn ssh_target_parse_keeps_destination() {
        assert_eq!(SshTarget::parse("user@host").destination, "user@host");
        assert_eq!(SshTarget::parse("host").destination, "host");
        assert_eq!(SshTarget::parse("devbox").destination, "devbox");
        assert!(SshTarget::parse("user@host").ssh_port.is_none());
        assert!(SshTarget::parse("host").identity_file.is_none());
        assert!(SshTarget::parse("host").extra_options.is_empty());
    }

    fn opts(target: &SshTarget, verify: bool) -> Vec<String> {
        let mut args = Vec::new();
        push_common_opts(&mut args, target, verify, &[], &SshSettings::default());
        args
    }

    fn position(args: &[String], value: &str) -> usize {
        args.iter()
            .position(|a| a == value)
            .unwrap_or_else(|| panic!("{value} missing from {args:?}"))
    }

    #[test]
    fn push_common_opts_emits_port_identity_and_extra_options() {
        let mut t = SshTarget::parse("host");
        t.ssh_port = Some(2200);
        t.identity_file = Some("/home/me/.ssh/key".into());
        t.extra_options = vec!["ProxyJump=bastion".into()];
        let args = opts(&t, false);
        assert_eq!(args[position(&args, "-p") + 1], "2200");
        assert_eq!(args[position(&args, "-i") + 1], "/home/me/.ssh/key");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ProxyJump=bastion")
        );
        // Options and values alternate with -o, -p, -i.
        for pair in args.chunks(2) {
            assert!(["-o", "-p", "-i"].contains(&pair[0].as_str()), "{args:?}");
        }
    }

    #[test]
    fn push_common_opts_appends_default_connect_timeout() {
        let args = opts(&SshTarget::parse("host"), false);
        let expected = format!("ConnectTimeout={}", SSH_CONNECT_TIMEOUT.as_secs());
        assert_eq!(args.last(), Some(&expected));
        assert_eq!(args[args.len() - 2], "-o");
    }

    #[test]
    fn user_connect_timeout_wins_over_default() {
        let mut t = SshTarget::parse("host");
        t.extra_options = vec!["ConnectTimeout=60".into()];
        let args = opts(&t, false);
        let user = position(&args, "ConnectTimeout=60");
        let default = position(
            &args,
            &format!("ConnectTimeout={}", SSH_CONNECT_TIMEOUT.as_secs()),
        );
        assert!(user < default, "the user value must come first: {args:?}");
    }

    #[test]
    fn fixed_options_come_before_user_options() {
        let mut t = SshTarget::parse("host");
        t.extra_options = vec![
            "BatchMode=yes".into(),
            "ServerAliveInterval=99".into(),
            "StrictHostKeyChecking=no".into(),
        ];
        let args = opts(&t, true);
        for fixed in [
            "BatchMode=no",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "StrictHostKeyChecking=accept-new",
        ] {
            assert!(
                position(&args, fixed) < position(&args, "BatchMode=yes"),
                "{fixed} must win: {args:?}"
            );
        }
    }

    #[test]
    fn without_verify_no_host_key_option_is_added() {
        let args = opts(&SshTarget::parse("host"), false);
        assert!(
            !args.iter().any(|a| a.starts_with("StrictHostKeyChecking")),
            "{args:?}"
        );
        let verified = opts(&SshTarget::parse("host"), true);
        assert!(
            verified
                .iter()
                .any(|a| a == "StrictHostKeyChecking=accept-new")
        );
    }

    #[test]
    fn expand_tilde_only_touches_leading_tilde() {
        assert_eq!(expand_tilde("/etc/x"), "/etc/x");
        assert_eq!(expand_tilde("/a/~/b"), "/a/~/b");
        assert_eq!(expand_tilde("~user/x"), "~user/x");
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            assert_eq!(expand_tilde("~/.ssh/k"), format!("{home}/.ssh/k"));
        }
    }
}
