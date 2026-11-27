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
}
