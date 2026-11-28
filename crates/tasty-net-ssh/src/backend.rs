//! Which SSH backend a connection attempt uses and why (docs/design/ssh-backends.md "선택 규칙",
//! "연결 정보").
//!
//! The choice is the setting `ssh.backend`. With `auto` the system backend is used when its
//! program file is found; the search only looks at files and runs no program.

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::inspect::{ConfigNote, RefusalNote};

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

    #[cfg(feature = "system-ssh")]
    pub(crate) fn system(reason: BackendReason, program: PathBuf) -> Self {
        Self {
            backend: Some(SshBackendKind::System),
            reason,
            selected_at: SystemTime::now(),
            program: Some(program),
        }
    }

    #[cfg(feature = "embedded-ssh")]
    pub(crate) fn embedded(reason: BackendReason) -> Self {
        Self {
            backend: Some(SshBackendKind::Embedded),
            reason,
            selected_at: SystemTime::now(),
            program: None,
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
    /// The embedded backend's configuration resolution was refused (an unknown keyword, a
    /// security directive it does not implement, a configuration error, `RefuseConnection`,
    /// or a value it cannot judge). Nothing reached the network.
    ConfigRefused {
        /// Why, with the keyword and where it is written.
        refusal: Box<RefusalNote>,
        /// The notes of the resolution up to the refusal.
        notes: Vec<ConfigNote>,
    },
    /// Neither the target, the configuration nor `embedded.local_user` gives the remote user.
    UserRequired {
        /// The host.
        host: String,
    },
    /// The TCP connection to a host could not be made (name lookup, connect, or the SSH
    /// handshake before the host key check).
    Connect {
        /// The host.
        host: String,
        /// The port.
        port: u16,
        /// What failed.
        detail: String,
    },
    /// The connection did not get through within `ConnectTimeout` (time spent waiting for an
    /// [`crate::SshPrompt`] answer does not count).
    TimedOut {
        /// The host.
        host: String,
        /// How long it waited.
        waited: Duration,
    },
    /// The host key was not accepted.
    HostKey(Box<HostKeyRefusal>),
    /// No authentication method the server allows succeeded.
    AuthFailed {
        /// The remote user.
        user: String,
        /// The host.
        host: String,
        /// The methods tried, in order (`none`, `publickey`, `keyboard-interactive`,
        /// `password`).
        tried: Vec<String>,
        /// The notes of the session up to the failure: the resolution's, and the ones the
        /// authentication left, such as a key file not used for its permissions
        /// (`bad_key_permissions`), so the caller sees why a key was not offered.
        notes: Vec<ConfigNote>,
    },
    /// The SSH session failed after it was set up, or the embedded runtime could not run it.
    Session {
        /// What failed.
        detail: String,
    },
}

/// Why a host key was not accepted (docs/design/ssh-backends.md "호스트 키 판정").
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyReason {
    /// A known_hosts line names the host with another key (`HOST_CHANGED`).
    Changed {
        /// The file of that line.
        file: PathBuf,
        /// The line.
        line: usize,
    },
    /// An `@revoked` line or a `RevokedHostKeys` file has the key.
    Revoked {
        /// The file.
        file: PathBuf,
        /// The line, when a known_hosts line is the cause.
        line: Option<usize>,
    },
    /// The host is not known and `StrictHostKeyChecking yes` (or `ask` without an answer, or
    /// under `BatchMode yes`).
    Unknown,
    /// A `RevokedHostKeys` file is missing or cannot be read.
    RevokedListUnreadable {
        /// The file.
        file: PathBuf,
    },
    /// An RSA host key smaller than `RequiredRSASize`.
    RsaTooSmall {
        /// The key's size.
        bits: u32,
        /// `RequiredRSASize`.
        required: u32,
    },
}

/// A host key that was not accepted, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyRefusal {
    /// The name the key was looked up under.
    pub host: String,
    /// The key type the server offered.
    pub key_type: String,
    /// Why.
    pub reason: HostKeyReason,
}

impl fmt::Display for HostKeyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (host, key) = (&self.host, &self.key_type);
        match &self.reason {
            HostKeyReason::Changed { file, line } => write!(
                f,
                "The {key} host key of {host} is not the one known at {}:{line}; the host key may have changed or the connection may be intercepted.",
                file.display()
            ),
            HostKeyReason::Revoked { file, line } => {
                write!(
                    f,
                    "The {key} host key of {host} is revoked in {}",
                    file.display()
                )?;
                match line {
                    Some(line) => write!(f, ":{line}."),
                    None => f.write_str("."),
                }
            }
            HostKeyReason::Unknown => write!(
                f,
                "The {key} host key of {host} is not known and was not accepted."
            ),
            HostKeyReason::RevokedListUnreadable { file } => write!(
                f,
                "The revoked host key list {} cannot be read, so the host key of {host} cannot be checked.",
                file.display()
            ),
            HostKeyReason::RsaTooSmall { bits, required } => write!(
                f,
                "The RSA host key of {host} has {bits} bits; at least {required} are required."
            ),
        }
    }
}

impl SshBackendError {
    /// The choice this error records ([`SelectedBackend::backend`] is `None` for
    /// [`SshBackendError::NotFound`]).
    pub fn selection(&self) -> Option<SelectedBackend> {
        match self {
            SshBackendError::NotFound { searched } => {
                Some(SelectedBackend::not_found(searched.clone()))
            }
            _ => None,
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
            SshBackendError::ConfigRefused { refusal, .. } => {
                write!(f, "The SSH configuration was refused: {refusal}")
            }
            SshBackendError::UserRequired { host } => write!(
                f,
                "No remote user for {host}: give one in the target, the SSH configuration or embedded.local_user."
            ),
            SshBackendError::Connect { host, port, detail } => {
                write!(f, "Could not connect to {host} port {port}: {detail}")
            }
            SshBackendError::TimedOut { host, waited } => {
                write!(
                    f,
                    "The SSH connection to {host} timed out after {waited:?}."
                )
            }
            SshBackendError::HostKey(refusal) => write!(f, "{refusal}"),
            SshBackendError::AuthFailed {
                user,
                host,
                tried,
                notes,
            } => {
                write!(
                    f,
                    "SSH authentication of {user} on {host} failed (tried: {}).",
                    tried.join(", ")
                )?;
                for note in notes {
                    write!(
                        f,
                        " {} at {}: {}.",
                        note.keyword,
                        note.origin,
                        note.class.name()
                    )?;
                }
                Ok(())
            }
            SshBackendError::Session { detail } => write!(f, "The SSH session failed: {detail}"),
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
/// `<name>.exe`. It returns the file found, or every file looked at. Each file is made
/// absolute against the working directory first (a relative `PATH` entry such as `.`), so the
/// file found is the one the system backend runs, without another search.
pub fn find_program(
    program: &Path,
    path: Option<&OsStr>,
    windows: bool,
) -> Result<PathBuf, Vec<PathBuf>> {
    let text = program.as_os_str().to_string_lossy();
    let has_separator = text.contains('/') || (windows && text.contains('\\'));
    if has_separator {
        let file = absolute(program);
        return if runnable(&file, windows) {
            Ok(file)
        } else {
            Err(vec![file])
        };
    }
    let mut searched = Vec::new();
    if program.as_os_str().is_empty() {
        return Err(searched);
    }
    let with_exe = windows && program.extension().is_none();
    for dir in split_path(path, windows) {
        let dir = absolute(&dir);
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

/// `path` against the working directory when it is relative; as given when that fails.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The directories of a `PATH` value: `;`-separated on Windows, `:`-separated elsewhere. Empty
/// entries are skipped (they would name the working directory). The entries keep their bytes
/// (a Unix folder name that is not UTF-8 is still found).
fn split_path(path: Option<&OsStr>, windows: bool) -> Vec<PathBuf> {
    let Some(path) = path else {
        return Vec::new();
    };
    let separator = if windows { b';' } else { b':' };
    split_os(path, separator)
        .into_iter()
        .filter(|d| !d.as_os_str().is_empty())
        .collect()
}

#[cfg(unix)]
fn split_os(value: &OsStr, separator: u8) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    value
        .as_bytes()
        .split(|b| *b == separator)
        .map(|part| PathBuf::from(OsStr::from_bytes(part)))
        .collect()
}

#[cfg(windows)]
fn split_os(value: &OsStr, separator: u8) -> Vec<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = value.encode_wide().collect();
    wide.split(|c| *c == u16::from(separator))
        .map(|part| PathBuf::from(std::ffi::OsString::from_wide(part)))
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn split_os(value: &OsStr, separator: u8) -> Vec<PathBuf> {
    value
        .to_string_lossy()
        .split(char::from(separator))
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

    /// Whether the tests judge paths the way this OS does (its `PATH` separator and its path
    /// separators). The Windows rules on another OS are tested on their own below.
    const HOST_WINDOWS: bool = cfg!(windows);

    fn path_of(dirs: &[&Path], windows: bool) -> OsString {
        let sep = if windows { ";" } else { ":" };
        OsString::from(
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(sep),
        )
    }

    /// The files a search for `ssh` looks at in `dirs` on this OS: `ssh`, and on Windows
    /// `ssh.exe` after it.
    fn looked_at(dirs: &[&Path]) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir in dirs {
            files.push(dir.join("ssh"));
            if HOST_WINDOWS {
                files.push(dir.join("ssh.exe"));
            }
        }
        files
    }

    #[test]
    fn a_bare_name_is_found_in_the_first_directory_that_has_it() {
        let empty = Dir::new("empty");
        let first = Dir::new("first");
        let second = Dir::new("second");
        let want = first.file("ssh", true);
        second.file("ssh", true);
        let path = path_of(&[&empty.0, &first.0, &second.0], HOST_WINDOWS);
        // Outside Unix there is no execute bit to test; the file alone is enough.
        assert_eq!(
            find_program(Path::new("ssh"), Some(&path), HOST_WINDOWS),
            Ok(want)
        );
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
        let path = path_of(&[&a.0, &b.0], HOST_WINDOWS);
        assert_eq!(
            find_program(Path::new("ssh"), Some(&path), HOST_WINDOWS),
            Err(looked_at(&[&a.0, &b.0]))
        );
        // No PATH at all: nothing is looked at, and nothing is found.
        assert_eq!(
            find_program(Path::new("ssh"), None, HOST_WINDOWS),
            Err(Vec::new())
        );
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
        let path = path_of(&[&elsewhere.0], HOST_WINDOWS);
        assert_eq!(
            find_program(&file, Some(&path), HOST_WINDOWS),
            Ok(file.clone())
        );
        let missing = dir.0.join("missing");
        assert_eq!(
            find_program(&missing, Some(&path), HOST_WINDOWS),
            Err(vec![missing.clone()])
        );
    }

    #[test]
    fn a_relative_entry_is_made_absolute() {
        let dir = Dir::new("relative");
        let file = dir.file("ssh", true);
        let cwd = std::env::current_dir().expect("cwd");
        // The same folder named relative to the working directory, as `.` or `bin` would be.
        let Some(relative) = pathdiff(&dir.0, &cwd) else {
            eprintln!("the temporary folder is on another drive than the working directory");
            return;
        };
        let path = path_of(&[&relative], HOST_WINDOWS);
        let found = find_program(Path::new("ssh"), Some(&path), HOST_WINDOWS).expect("found");
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(
            std::fs::canonicalize(&found).expect("found"),
            std::fs::canonicalize(&file).expect("file")
        );
    }

    /// `to` relative to `from` with `..` steps (both absolute), or `None` when they share no
    /// root (two Windows drives).
    fn pathdiff(to: &Path, from: &Path) -> Option<PathBuf> {
        let to: Vec<_> = to.components().collect();
        let from: Vec<_> = from.components().collect();
        let common = to.iter().zip(&from).take_while(|(a, b)| a == b).count();
        if common == 0 {
            return None;
        }
        let mut out = PathBuf::new();
        for _ in common..from.len() {
            out.push("..");
        }
        for c in &to[common..] {
            out.push(c.as_os_str());
        }
        Some(out)
    }

    // Apple file systems refuse a name that is not UTF-8 (EILSEQ), so such a folder cannot be
    // made there and the PATH entry cannot name one.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn a_folder_name_that_is_not_utf8_is_searched_as_it_is() {
        use std::os::unix::ffi::OsStrExt;
        let parent = Dir::new("bytes");
        let odd = parent.0.join(OsStr::from_bytes(b"bin-\xff"));
        std::fs::create_dir_all(&odd).expect("odd folder");
        let file = odd.join("ssh");
        std::fs::write(&file, b"#!/bin/sh\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut path = OsString::from("/nonexistent:");
        path.push(odd.as_os_str());
        assert_eq!(find_program(Path::new("ssh"), Some(&path), false), Ok(file));
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
