#![forbid(unsafe_code)]

//! System `ssh` delegation of the Tasty network library.
//!
//! The crate runs the system `ssh` client. It does not speak the SSH protocol itself.
//!
//! # Public contract
//!
//! | Item | Consumer |
//! |------|----------|
//! | [`SshTarget`] · [`PortMode`] | consumers that connect over SSH |
//! | [`SshLauncher`] · [`resolve_ssh_path`] | consumers that start the system `ssh` (program, host key checking, hook) |
//! | [`SpawnHook`] · [`SpawnFn`] | hosts that bind `ssh` children to their own lifetime |
//! | [`SshCancel`] ([`SshCancelScope`] is its return type) | consumers that stop a port discovery from another thread |
//! | [`discover_remote_port`] · [`PortDiscoveryConfig`] · [`decode_port`] · [`PortDecodeError`] | consumers that read the remote record over SSH (the record decoder is passed in) |
//! | [`detect_port_mode`] | consumers that find which mode reaches a remote host (shell detection) |
//! | [`SSH_CONNECT_TIMEOUT`] · [`PORT_DISCOVERY_STEP_TIMEOUT`] · [`PORT_DISCOVERY_TOTAL_TIMEOUT`] | consumers that match progress display and messages to the same values |
//! | [`PortDiscoveryError`] · [`PortDiscoveryFailureKind`] · [`PortModeParseError`] | consumers that branch on the failure cause (messages come from the kind, `Display` is fixed English) |
//! | [`PORT_COMMAND_EXITED_INSTANCE_CODE`] | the remote subcommand and port discovery, which share this exit code |

mod cancel;
mod discovery;
mod spawn;

pub use cancel::{SshCancel, SshCancelScope};
pub use discovery::{
    PORT_COMMAND_EXITED_INSTANCE_CODE, PORT_DISCOVERY_STEP_TIMEOUT, PORT_DISCOVERY_TOTAL_TIMEOUT,
    PortDecodeError, PortDiscoveryConfig, PortDiscoveryError, PortDiscoveryFailureKind, PortMode,
    PortModeParseError, decode_port, detect_port_mode, discover_remote_port,
};
pub use spawn::{SpawnFn, SpawnHook, SshLauncher};

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
/// - `ConnectTimeout` ([`SSH_CONNECT_TIMEOUT`]).
///
/// ssh uses the **first** value of an option. The options above `extra_options` therefore win
/// over them, and `ConnectTimeout` goes last so that a user `ConnectTimeout` in
/// `extra_options` wins over the default. Command line `-o` options also win over the ssh
/// config file.
pub(crate) fn push_common_opts(args: &mut Vec<String>, target: &SshTarget, verify: bool) {
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
        push_common_opts(&mut args, target, verify);
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
