//! The questions an SSH backend asks the application: key passphrases, passwords,
//! keyboard-interactive prompts and unknown host keys (docs/design/ssh-backends.md "진입점",
//! android 요구 #1).
//!
//! The system backend's `ssh` asks on its own terminal and does not call these. The embedded
//! backend calls them on a blocking worker of its runtime, waits up to
//! `embedded.prompt_timeout` for the answer, and never calls them under `BatchMode yes`.

use std::path::Path;

/// A host key the known_hosts files do not know, asked under `StrictHostKeyChecking ask`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostKeyQuestion {
    /// The name the key is looked up and recorded under (`HostKeyAlias`, or `[host]:port`
    /// when the port is not 22).
    pub host: String,
    /// The key type (`ssh-ed25519`, ...).
    pub key_type: String,
    /// The SHA-256 fingerprint as OpenSSH prints it (`SHA256:...`).
    pub fingerprint: String,
}

/// One keyboard-interactive round (RFC 4256 `SSH_MSG_USERAUTH_INFO_REQUEST`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyboardInteractiveRequest {
    /// The server's name of the round (can be empty).
    pub name: String,
    /// The server's instructions (can be empty).
    pub instructions: String,
    /// The prompts in order, each with whether the answer may be shown as it is typed.
    pub prompts: Vec<(String, bool)>,
}

/// Answers the questions of an SSH connection. Every method has a default that gives no
/// answer, so an implementation overrides only what it can ask.
///
/// A method returns `None` (or `false`) to decline; the backend then goes on as `ssh` does
/// when the user gives no answer (the next key, the next method, or a refused host key).
pub trait SshPrompt: Send + Sync {
    /// The passphrase of the private key file `key_file`.
    fn passphrase(&self, key_file: &Path) -> Option<String> {
        let _ = key_file;
        None
    }

    /// The password of `user` on `host`.
    fn password(&self, user: &str, host: &str) -> Option<String> {
        let _ = (user, host);
        None
    }

    /// The answers to one keyboard-interactive round, one per prompt.
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<String>> {
        let _ = request;
        None
    }

    /// Whether to trust and record an unknown host key.
    fn confirm_host_key(&self, question: &HostKeyQuestion) -> bool {
        let _ = question;
        false
    }
}
