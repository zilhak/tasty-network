#![forbid(unsafe_code)]

//! System `ssh` delegation of the Tasty network library.
//!
//! The crate runs the system `ssh` client. It does not speak the SSH protocol itself.
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
//!
//! Profile handling (building an [`SshTarget`] from a stored profile, saving detection results)
//! is not here; the consumer does it with these value types. The crate reads no environment
//! variable: host key checking is [`SshLauncher::with_verify`].

mod cancel;
mod discovery;
mod listener;
pub mod settings;
mod spawn;
mod tunnel;

pub use cancel::{SshCancel, SshCancelScope};
pub use discovery::{
    PORT_COMMAND_EXITED_INSTANCE_CODE, PortDecodeError, PortDiscoveryConfig, PortDiscoveryError,
    PortDiscoveryFailureKind, PortMode, PortModeParseError, decode_port, detect_port_mode,
    discover_remote_port,
};
pub use listener::{
    ListenerVerdict, RemoteListenerAbsent, connection_closed_error, is_dropped_connection,
};
pub use settings::{
    PORT_DISCOVERY_STEP_TIMEOUT, PORT_DISCOVERY_TOTAL_TIMEOUT, SSH_CONNECT_TIMEOUT, SshSettings,
};
pub use spawn::{SpawnFn, SpawnHook, SshLauncher};
pub use tunnel::{Backoff, SshTunnel, TunnelError, tunnel_drop_totals};

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use settings::Source;

/// Path of the system `ssh` binary.
///
/// Windows prefers the system OpenSSH so that the Windows ssh-agent is used. When it is
/// missing, a warning is logged and `ssh.exe` from `PATH` is used. Other systems use `ssh`
/// from `PATH`.
pub fn resolve_ssh_path() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(windir) = std::env::var("WINDIR") {
            let full = PathBuf::from(windir).join(r"System32\OpenSSH\ssh.exe");
            if full.exists() {
                return full;
            }
        }
        tracing::warn!(
            "system OpenSSH (%WINDIR%\\System32\\OpenSSH\\ssh.exe) not found; falling back to \
             ssh.exe from PATH. A Git-bundled ssh does not see the Windows agent, so \
             passwordless authentication may fail."
        );
        PathBuf::from("ssh.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("ssh")
    }
}

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
/// - `ConnectTimeout` (`ssh.connect_timeout`, by default [`SSH_CONNECT_TIMEOUT`]).
///
/// ssh uses the **first** value of an option. The options above `extra_options` therefore win
/// over them, and `ConnectTimeout` goes last so that a user `ConnectTimeout` in
/// `extra_options` wins over the setting. Command line `-o` options also win over the ssh
/// config file. [`SshLauncher::applied_options`] reports the result.
pub(crate) fn push_common_opts(
    args: &mut Vec<String>,
    target: &SshTarget,
    verify: bool,
    forced: &[&str],
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

/// The value of the first option `name` in the target's extra options (`Key=Value` or
/// `Key Value`; ssh compares option names without case), as ssh would read it.
fn target_option<'a>(target: &'a SshTarget, name: &str) -> Option<&'a str> {
    target.extra_options.iter().find_map(|opt| {
        let opt = opt.trim();
        let split = opt.find(|c: char| c == '=' || c.is_whitespace())?;
        let (key, rest) = opt.split_at(split);
        let value = rest.trim_start().trim_start_matches('=').trim();
        key.eq_ignore_ascii_case(name).then_some(value)
    })
}

/// The connect timeout ssh runs with for `target`: the target's own whole-second
/// `ConnectTimeout` if it has one, otherwise `ssh.connect_timeout`. Port discovery classifies a
/// slow connection failure with it.
pub(crate) fn connect_timeout_for(settings: &SshSettings, target: &SshTarget) -> Duration {
    target_option(target, CONNECT_TIMEOUT_OPTION)
        .and_then(|v| v.parse::<u64>().ok())
        .map_or_else(|| settings.connect_timeout(), Duration::from_secs)
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
    let connect = match target_option(target, CONNECT_TIMEOUT_OPTION) {
        Some(value) => AppliedOption {
            key: settings::CONNECT_TIMEOUT,
            option: CONNECT_TIMEOUT_OPTION,
            value: value.to_owned(),
            source: OptionSource::Target,
        },
        None => from_setting(
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
#[cfg(all(test, unix))]
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
#[cfg(test)]
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

#[cfg(test)]
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

    #[test]
    fn resolve_ssh_path_nonempty() {
        let p = resolve_ssh_path();
        assert!(!p.as_os_str().is_empty());
        #[cfg(not(windows))]
        assert_eq!(p, PathBuf::from("ssh"));
    }
}
