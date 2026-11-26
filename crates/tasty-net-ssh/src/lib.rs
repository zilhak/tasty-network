#![forbid(unsafe_code)]

//! System `ssh` delegation of the Tasty network library.
//!
//! The crate runs the system `ssh` client. It does not speak the SSH protocol itself.
//!
//! # Public contract
//!
//! | Item | Consumer |
//! |------|----------|
//! | [`SshTarget`] | consumers that connect over SSH |
//! | [`SpawnHook`] · [`SpawnFn`] | hosts that bind `ssh` children to their own lifetime |
//! | [`resolve_ssh_path`] | consumers that start the system `ssh` |

mod spawn;

pub use spawn::{SpawnFn, SpawnHook};

use std::path::PathBuf;

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

    #[test]
    fn resolve_ssh_path_nonempty() {
        let p = resolve_ssh_path();
        assert!(!p.as_os_str().is_empty());
        #[cfg(not(windows))]
        assert_eq!(p, PathBuf::from("ssh"));
    }
}
