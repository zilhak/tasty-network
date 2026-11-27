//! The system ssh backend: everything that starts the system `ssh` client as a child process.
//!
//! The program to run ([`resolve_ssh_path`], [`spawn`]), registering and reaping the children
//! ([`cancel`]), port discovery ([`discovery`]) and the `-L` tunnel ([`tunnel`]) live here, so
//! the code that runs another program stays in this folder
//! (docs/design/mobile.md "다른 프로그램 실행 금지").

pub(crate) mod cancel;
pub(crate) mod discovery;
pub(crate) mod spawn;
pub(crate) mod tunnel;

use std::path::PathBuf;

/// The tracing target of this module's events: its module path before it moved into
/// `system`, so log filters on the old path keep selecting them.
#[cfg(windows)]
const LOG_TARGET: &str = "tasty_net_ssh";

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
            target: LOG_TARGET,
            "system OpenSSH (%WINDIR%\\System32\\OpenSSH\\ssh.exe) not found; falling back to \
             ssh.exe from PATH. A Git-bundled ssh does not see the Windows agent, so \
             passwordless authentication may fail.",
        );
        PathBuf::from("ssh.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("ssh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ssh_path_nonempty() {
        let p = resolve_ssh_path();
        assert!(!p.as_os_str().is_empty());
        #[cfg(not(windows))]
        assert_eq!(p, PathBuf::from("ssh"));
    }

    /// Every tracing event of the system backend names its target, so moving the modules into
    /// `system` did not change the targets log filters select (the default would be the new
    /// module path).
    #[test]
    fn every_system_event_names_its_target() {
        let sources = [
            ("mod.rs", include_str!("mod.rs")),
            ("cancel.rs", include_str!("cancel.rs")),
            ("discovery.rs", include_str!("discovery.rs")),
            ("spawn.rs", include_str!("spawn.rs")),
            ("tunnel.rs", include_str!("tunnel.rs")),
        ];
        let levels = ["trace", "debug", "info", "warn", "error"];
        let mut events = 0;
        for (name, text) in sources {
            for level in levels {
                let call = format!("tracing::{level}!(");
                for (at, _) in text.match_indices(&call) {
                    let args = text[at + call.len()..].trim_start();
                    assert!(
                        args.starts_with("target:"),
                        "{name}: a tracing::{level}! event without a target"
                    );
                    events += 1;
                }
            }
        }
        // The scan sees the events: the backend has twenty (one in this file, three in
        // cancel.rs, ten in discovery.rs, six in tunnel.rs).
        assert!(events >= 20, "{events} events found");
    }
}
