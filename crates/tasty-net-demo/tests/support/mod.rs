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
    pub fn new(what: &str) -> Self {
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
            .unwrap_or_else(|e| panic!("creating {} failed: {e}", path.display()));
        Self(path)
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
