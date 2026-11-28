//! Which SSH backend a connection attempt uses and why (docs/design/ssh-backends.md "선택 규칙",
//! "연결 정보").
//!
//! The choice is the setting `ssh.backend`. With `auto` the system backend is used when its
//! program file is found; the search only looks at files and runs no program.

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// An SSH backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SshBackendKind {
    /// The system `ssh` client, run as a child process (`system-ssh`).
    System,
    /// The in-process SSH client (`embedded-ssh`).
    Embedded,
}

impl SshBackendKind {
    /// `system` or `embedded`.
    pub fn name(self) -> &'static str {
        match self {
            SshBackendKind::System => "system",
            SshBackendKind::Embedded => "embedded",
        }
    }
}

impl fmt::Display for SshBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a backend was chosen (or none was).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendReason {
    /// `auto` found the system program file at `path`.
    Found {
        /// The file the system backend runs.
        path: PathBuf,
    },
    /// The system program file was not found at any of `searched`.
    NotFound {
        /// The files looked at, in order.
        searched: Vec<PathBuf>,
    },
    /// `auto` in a build without the system backend.
    SystemNotBuilt,
    /// `ssh.backend` names the backend.
    Configured,
}

impl BackendReason {
    /// `found`, `not_found`, `system_not_built` or `configured`.
    pub fn name(&self) -> &'static str {
        match self {
            BackendReason::Found { .. } => "found",
            BackendReason::NotFound { .. } => "not_found",
            BackendReason::SystemNotBuilt => "system_not_built",
            BackendReason::Configured => "configured",
        }
    }
}

impl fmt::Display for BackendReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendReason::Found { path } => write!(f, "found {}", path.display()),
            BackendReason::NotFound { searched } => {
                write!(f, "not_found ({} files looked at)", searched.len())
            }
            other => f.write_str(other.name()),
        }
    }
}

/// The backend of one connection attempt, with the reason and when it was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedBackend {
    /// The backend; `None` when none could be chosen ([`BackendReason::NotFound`]).
    pub backend: Option<SshBackendKind>,
    /// Why.
    pub reason: BackendReason,
    /// When the choice was made.
    pub selected_at: SystemTime,
    /// The file the system backend runs (also for [`BackendReason::Configured`]).
    pub(crate) program: Option<PathBuf>,
}

impl SelectedBackend {
    /// The record of a choice that found no backend: the system program file was looked for at
    /// `searched` and not found.
    pub fn not_found(searched: Vec<PathBuf>) -> Self {
        Self {
            backend: None,
            reason: BackendReason::NotFound { searched },
            selected_at: SystemTime::now(),
            program: None,
        }
    }

    pub(crate) fn system(reason: BackendReason, program: PathBuf) -> Self {
        Self {
            backend: Some(SshBackendKind::System),
            reason,
            selected_at: SystemTime::now(),
            program: Some(program),
        }
    }

    /// The file the system backend runs, when it is the chosen backend.
    pub fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }
}

/// Why an SSH operation did not reach the network through a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshBackendError {
    /// No backend: the system program file was not found (and the embedded backend is not
    /// built or not allowed by `ssh.backend`). The attempt does not fall back silently.
    NotFound {
        /// The files looked at, in order.
        searched: Vec<PathBuf>,
    },
    /// The effective target of one purpose could not be read (the system backend's `ssh -G`
    /// failed), so the port discovery and the tunnel cannot be compared. The attempt stops
    /// before the record's token is sent.
    TargetUnresolved {
        /// `tunnel` or `discovery`.
        purpose: &'static str,
        /// What failed.
        detail: String,
    },
}

impl SshBackendError {
    /// The choice this error records ([`SelectedBackend::backend`] is `None` for
    /// [`SshBackendError::NotFound`]).
    pub fn selection(&self) -> Option<SelectedBackend> {
        match self {
            SshBackendError::NotFound { searched } => {
                Some(SelectedBackend::not_found(searched.clone()))
            }
            SshBackendError::TargetUnresolved { .. } => None,
        }
    }
}

impl fmt::Display for SshBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshBackendError::NotFound { searched } => {
                f.write_str("No SSH backend: the ssh program was not found")?;
                match searched.as_slice() {
                    [] => {}
                    [only] => write!(f, " (looked at {})", only.display())?,
                    [first, rest @ ..] => write!(
                        f,
                        " (looked at {} and {} more)",
                        first.display(),
                        rest.len()
                    )?,
                }
                f.write_str(".")
            }
            SshBackendError::TargetUnresolved { purpose, detail } => write!(
                f,
                "Could not read the effective SSH target of the {purpose}: {detail}"
            ),
        }
    }
}

impl Error for SshBackendError {}

/// The effective destination of one purpose: host, port, user and the jump hosts, as the
/// backend resolved them. Port discovery and the tunnel of one attempt must have the same one
/// (docs/design/ssh-backends.md "포트 발견과 터널의 대상 일치").
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EffectiveTarget {
    /// The host connected to (`hostname`).
    pub hostname: String,
    /// The port.
    pub port: u16,
    /// The remote user.
    pub user: String,
    /// The jump hosts: the system backend's `proxyjump` value as `ssh -G` prints it (empty
    /// when there is none), the embedded backend's resolved hops (`user@host:port` joined by
    /// `,`).
    pub jump: String,
}

/// Looks for the program file of the system backend without running anything.
///
/// A `program` with a path separator is that one file. A bare name is looked for in each
/// directory of `path` (the value of `PATH`) in order: on Unix a regular file with an execute
/// bit, on Windows (`windows`) `<name>` and then, for a name without an extension,
/// `<name>.exe`. It returns the file found, or every file looked at.
pub fn find_program(
    program: &Path,
    path: Option<&OsStr>,
    windows: bool,
) -> Result<PathBuf, Vec<PathBuf>> {
    let text = program.as_os_str().to_string_lossy();
    let has_separator = text.contains('/') || (windows && text.contains('\\'));
    if has_separator {
        return if runnable(program, windows) {
            Ok(program.to_path_buf())
        } else {
            Err(vec![program.to_path_buf()])
        };
    }
    let mut searched = Vec::new();
    if program.as_os_str().is_empty() {
        return Err(searched);
    }
    let with_exe = windows && program.extension().is_none();
    for dir in split_path(path, windows) {
        let mut candidates = vec![dir.join(program)];
        if with_exe {
            let mut name = program.as_os_str().to_owned();
            name.push(".exe");
            candidates.push(dir.join(name));
        }
        for candidate in candidates {
            if runnable(&candidate, windows) {
                return Ok(candidate);
            }
            searched.push(candidate);
        }
    }
    Err(searched)
}

/// The directories of a `PATH` value: `;`-separated on Windows, `:`-separated elsewhere. Empty
/// entries are skipped (they would name the working directory).
fn split_path(path: Option<&OsStr>, windows: bool) -> Vec<PathBuf> {
    let Some(path) = path else {
        return Vec::new();
    };
    let separator = if windows { ';' } else { ':' };
    path.to_string_lossy()
        .split(separator)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// A regular file; on Unix (when not judging a Windows path) also with an execute bit.
fn runnable(path: &Path, windows: bool) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if windows {
        return true;
    }
    executable(&meta)
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const ANY_EXECUTE_BIT: u32 = 0o111;
    meta.permissions().mode() & ANY_EXECUTE_BIT != 0
}

#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tasty-net-ssh-find-{}-{}-{name}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn file(&self, name: &str, executable: bool) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("chmod");
            }
            #[cfg(not(unix))]
            let _ = executable;
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                eprintln!("could not remove the test folder: {e}");
            }
        }
    }

    fn path_of(dirs: &[&Path], windows: bool) -> OsString {
        let sep = if windows { ";" } else { ":" };
        OsString::from(
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(sep),
        )
    }

    #[test]
    fn a_bare_name_is_found_in_the_first_directory_that_has_it() {
        let empty = Dir::new("empty");
        let first = Dir::new("first");
        let second = Dir::new("second");
        let want = first.file("ssh", true);
        second.file("ssh", true);
        let path = path_of(&[&empty.0, &first.0, &second.0], false);
        // Outside Unix there is no execute bit to test; the file alone is enough.
        assert_eq!(find_program(Path::new("ssh"), Some(&path), false), Ok(want));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_an_execute_bit_is_not_the_program() {
        let dir = Dir::new("noexec");
        let file = dir.file("ssh", false);
        let path = path_of(&[&dir.0], false);
        assert_eq!(
            find_program(Path::new("ssh"), Some(&path), false),
            Err(vec![file])
        );
    }

    #[test]
    fn not_found_lists_every_file_looked_at() {
        let a = Dir::new("a");
        let b = Dir::new("b");
        let path = path_of(&[&a.0, &b.0], false);
        assert_eq!(
            find_program(Path::new("ssh"), Some(&path), false),
            Err(vec![a.0.join("ssh"), b.0.join("ssh")])
        );
        // No PATH at all: nothing is looked at, and nothing is found.
        assert_eq!(find_program(Path::new("ssh"), None, false), Err(Vec::new()));
    }

    #[test]
    fn windows_tries_the_exe_name_after_the_bare_name() {
        let a = Dir::new("win-a");
        let b = Dir::new("win-b");
        let exe = b.file("ssh.exe", true);
        let path = path_of(&[&a.0, &b.0], true);
        assert_eq!(find_program(Path::new("ssh"), Some(&path), true), Ok(exe));
        // A name with an extension is looked for as written only.
        let path = path_of(&[&a.0], true);
        assert_eq!(
            find_program(Path::new("ssh.exe"), Some(&path), true),
            Err(vec![a.0.join("ssh.exe")])
        );
        // The bare name and then the .exe name, directory by directory.
        assert_eq!(
            find_program(Path::new("ssh"), Some(&path), true),
            Err(vec![a.0.join("ssh"), a.0.join("ssh.exe")])
        );
    }

    #[test]
    fn a_program_with_a_separator_is_that_file_only() {
        let dir = Dir::new("sep");
        let file = dir.file("my-ssh", true);
        let elsewhere = Dir::new("elsewhere");
        elsewhere.file("my-ssh", true);
        let path = path_of(&[&elsewhere.0], false);
        assert_eq!(find_program(&file, Some(&path), false), Ok(file.clone()));
        let missing = dir.0.join("missing");
        assert_eq!(
            find_program(&missing, Some(&path), false),
            Err(vec![missing.clone()])
        );
    }

    #[test]
    fn the_reasons_have_the_names_of_the_design() {
        assert_eq!(
            BackendReason::Found {
                path: PathBuf::from("/usr/bin/ssh")
            }
            .name(),
            "found"
        );
        assert_eq!(
            BackendReason::NotFound { searched: vec![] }.name(),
            "not_found"
        );
        assert_eq!(BackendReason::SystemNotBuilt.name(), "system_not_built");
        assert_eq!(BackendReason::Configured.name(), "configured");
        let error = SshBackendError::NotFound {
            searched: vec![PathBuf::from("/a/ssh"), PathBuf::from("/b/ssh")],
        };
        let selection = error.selection().expect("a NotFound records the choice");
        assert_eq!(selection.backend, None);
        assert_eq!(selection.reason.name(), "not_found");
        assert!(error.to_string().contains("/a/ssh and 1 more"), "{error}");
    }
}
