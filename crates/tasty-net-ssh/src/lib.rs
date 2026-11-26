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
//! | [`SSH_CONNECT_TIMEOUT`] · [`PORT_DISCOVERY_STEP_TIMEOUT`] · [`PORT_DISCOVERY_TOTAL_TIMEOUT`] | consumers that match progress display and messages to the same values |
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
mod spawn;
mod tunnel;

pub use cancel::{SshCancel, SshCancelScope};
pub use discovery::{
    PORT_COMMAND_EXITED_INSTANCE_CODE, PORT_DISCOVERY_STEP_TIMEOUT, PORT_DISCOVERY_TOTAL_TIMEOUT,
    PortDecodeError, PortDiscoveryConfig, PortDiscoveryError, PortDiscoveryFailureKind, PortMode,
    PortModeParseError, decode_port, detect_port_mode, discover_remote_port,
};
pub use listener::{
    ListenerVerdict, RemoteListenerAbsent, connection_closed_error, is_dropped_connection,
};
pub use spawn::{SpawnFn, SpawnHook, SshLauncher};
pub use tunnel::{Backoff, SshTunnel, TunnelError, tunnel_drop_totals};

use std::path::PathBuf;
use std::time::Duration;

/// Default ssh `ConnectTimeout`. It bounds connection setup, not keepalive after it. A
/// `ConnectTimeout` in the target's extra options comes first and wins.
pub const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
/// - `ServerAliveInterval=15`, `ServerAliveCountMax=3`: ssh ends itself within about 45 s
///   after the network drops.
/// - `StrictHostKeyChecking=accept-new` only with `verify`. Without it no host key option is
///   passed, so the user's options, the ssh config, or the ssh default (`ask`) decide.
/// - `forced`: options the caller requires (`Key=Value`, each after `-o`), such as the tunnel's
///   `ExitOnForwardFailure=yes`.
/// - `ConnectTimeout` ([`SSH_CONNECT_TIMEOUT`]).
///
/// ssh uses the **first** value of an option. The options above `extra_options` therefore win
/// over them, and `ConnectTimeout` goes last so that a user `ConnectTimeout` in
/// `extra_options` wins over the default. Command line `-o` options also win over the ssh
/// config file.
pub(crate) fn push_common_opts(
    args: &mut Vec<String>,
    target: &SshTarget,
    verify: bool,
    forced: &[&str],
) {
    args.push("-o".into());
    args.push("BatchMode=no".into());
    args.push("-o".into());
    args.push("ServerAliveInterval=15".into());
    args.push("-o".into());
    args.push("ServerAliveCountMax=3".into());
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
    args.push(format!("ConnectTimeout={}", SSH_CONNECT_TIMEOUT.as_secs()));
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
    /// - `flood:<bytes>`: writes `<bytes>` bytes to stdout and exits 0. `flood:forever` writes
    ///   until the pipe closes.
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
                let n: usize = if arg == "forever" {
                    usize::MAX
                } else {
                    arg.parse().expect("byte count")
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
                std::process::exit(0);
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
        #[cfg(unix)]
        pub(crate) fn pid_file(&self) -> &std::path::Path {
            &self.pid_file
        }

        /// The helper command: a grandchild sleeping `secs` seconds, the child exiting `code`.
        pub(crate) fn command(&self, secs: u64, code: i32) -> Command {
            helper_command(&format!("orphan:{secs}:{code}:{}", self.pid_file.display()))
        }

        /// Kills the grandchild and returns whether it is gone. The grandchild is not our child,
        /// so it cannot be waited for; this checks with the OS until it no longer runs.
        pub(crate) fn end(&mut self) -> bool {
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
                if !pid_runs(&pid) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }
    }

    impl Drop for Orphan {
        fn drop(&mut self) {
            if !self.ended {
                self.end();
            }
        }
    }

    #[cfg(unix)]
    fn kill_pid(pid: &str) {
        let _status = Command::new("kill").args(["-KILL", pid]).status();
    }

    /// Whether `pid` still runs. A zombie waiting for its new parent to reap it does not run.
    #[cfg(unix)]
    fn pid_runs(pid: &str) -> bool {
        let Ok(out) = Command::new("ps").args(["-o", "stat=", "-p", pid]).output() else {
            return true;
        };
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
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

    #[cfg(windows)]
    fn pid_runs(pid: &str) -> bool {
        let filter = format!("PID eq {pid}");
        let Ok(out) = Command::new("tasklist")
            .args(["/FI", &filter, "/NH", "/FO", "CSV"])
            .output()
        else {
            return true;
        };
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        push_common_opts(&mut args, target, verify, &[]);
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
