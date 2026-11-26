//! Test support of the two-process SSH tests: child processes, the test SSH server
//! (docs/design/testing.md "SSH 통합 시험의 sshd") and temporary directories.

pub mod process;
mod russh_server;
pub mod sshd;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A directory under the system temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// A new directory; panics when it cannot be made.
    pub fn new(what: &str) -> Self {
        Self::try_new(what).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A new directory, or why it could not be made.
    pub fn try_new(what: &str) -> Result<Self, String> {
        static N: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "tasty-net-demo-{what}-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("creating {} failed: {e}", path.display()))?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Removes the directory now and checks that it is gone.
    pub fn remove(self) {
        let path = self.0.clone();
        drop(self);
        assert!(!path.exists(), "{} was not removed", path.display());
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            process::log(&format!("removing {} failed: {error}", self.0.display()));
        }
    }
}

/// `path` as ssh and sshd options take it: forward slashes on Windows, where a backslash in
/// an option value can be read as an escape.
pub fn ssh_path(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

/// `path` as the value of an ssh `-o Key=Value` option. OpenSSH splits option values into
/// words again (and reads the known hosts options as a list of files), so the path goes in
/// double quotes, with `\` and `"` escaped, and stays one word even with spaces in it.
pub fn ssh_option_path(path: &Path) -> String {
    let text = ssh_path(path).replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{text}\"")
}

#[test]
fn option_paths_with_spaces_stay_one_word() {
    let path = Path::new("/tmp/with space/known hosts");
    let option = format!("UserKnownHostsFile={}", ssh_option_path(path));
    assert_eq!(option, "UserKnownHostsFile=\"/tmp/with space/known hosts\"");
    assert_eq!(
        ssh_option_path(Path::new("/a\"b")),
        "\"/a\\\"b\"",
        "a quote inside is escaped"
    );
}
