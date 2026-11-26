//! Private secret files (docs/design/identity.md "파일 권한").
//!
//! A secret is written only into a new temporary file that was first restricted to the current
//! user (Unix 0600, Windows a protected DACL with one entry for the current user) and checked
//! again. Every existing secret file is opened through [`open_private`], which refuses links,
//! non-regular files and files that are not private, judged on the opened handle.

use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::error::{FileOp, IdentityError, UnsafeReason};

#[cfg(unix)]
#[allow(unsafe_code, reason = "reads the effective user ID from libc")]
mod unix;
#[cfg(unix)]
use unix as os;

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Windows security and file ID APIs have no safe std equivalent"
)]
pub(crate) mod windows;
#[cfg(windows)]
use windows as os;

pub(crate) use os::FileId;

/// Larger than any secret file this crate writes; a bigger file is not read whole.
pub(crate) const MAX_SECRET_LEN: usize = 4096;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The result of an OS-level open with the privacy checks. An existing file that cannot be
/// opened or examined is `Unsafe(CheckFailed)`: it could not be shown to be private.
pub(crate) enum OpenFailure {
    NotFound,
    Unsafe(UnsafeReason, Option<io::Error>),
}

/// Why a secret directory could not be created: an I/O failure at `path`, or an element that
/// appeared meanwhile and is not a directory.
pub(crate) enum DirFailure {
    Io(PathBuf, io::Error),
    #[cfg_attr(windows, allow(dead_code, reason = "Windows uses create_dir_all"))]
    Unsafe(PathBuf, UnsafeReason),
}

/// Why a new temporary file could not be created. `Protect` is a failure to prepare its access
/// restriction (Windows creates the file with it); `Create` is the creation itself.
#[derive(Debug)]
pub(crate) enum CreateFailure {
    #[cfg_attr(unix, allow(dead_code, reason = "Unix restricts after creation"))]
    Protect(io::Error),
    Create(io::Error),
}

/// Windows error codes with which `CreateFileW` refuses the owner or group of the security
/// descriptor the file is created with: the access restriction failed, not the creation.
const CREATE_PROTECT_ERRORS: [i32; 3] = [
    1307, // ERROR_INVALID_OWNER
    1308, // ERROR_INVALID_PRIMARY_GROUP
    1314, // ERROR_PRIVILEGE_NOT_HELD
];

/// Classify a failed Windows `CreateFileW` of a new temporary file. Kept OS independent so the
/// mapping is tested everywhere.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "only Windows creates the file with its security")
)]
pub(crate) fn classify_create_error(error: io::Error) -> CreateFailure {
    match error.raw_os_error() {
        Some(code) if CREATE_PROTECT_ERRORS.contains(&code) => CreateFailure::Protect(error),
        _ => CreateFailure::Create(error),
    }
}

/// An existing secret file opened after the privacy checks, and its contents.
pub(crate) struct PrivateFile {
    pub(crate) file: File,
    /// At most `MAX_SECRET_LEN + 1` bytes, so an oversized file is visible as such.
    pub(crate) contents: Vec<u8>,
}

/// Make sure `dir` is a private directory (Unix 0700) and return it without trailing
/// separators. The last path element is checked before anything is created: a link (also a
/// dangling one) or a non-directory there is [`IdentityError::UnsafeFile`]. A missing directory
/// is created; a wider existing one is narrowed.
pub(crate) fn prepare_dir(dir: &Path) -> Result<PathBuf, IdentityError> {
    // `a/link/` would let lstat follow the link; the components drop the trailing separator.
    let dir: PathBuf = match dir.components().collect::<PathBuf>() {
        p if p.as_os_str().is_empty() => PathBuf::from("."),
        p => p,
    };
    let lstat = || std::fs::symlink_metadata(&dir);
    let (meta, created) = match lstat() {
        Ok(meta) => (meta, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            os::create_dir(&dir).map_err(|failure| match failure {
                DirFailure::Io(path, e) => IdentityError::io(FileOp::CreateDir, &path, e),
                DirFailure::Unsafe(path, reason) => IdentityError::unsafe_file(&path, reason),
            })?;
            let meta = lstat().map_err(|e| IdentityError::io(FileOp::CreateDir, &dir, e))?;
            (meta, true)
        }
        Err(e) => return Err(IdentityError::io(FileOp::CreateDir, &dir, e)),
    };
    if os::is_link(&meta) {
        return Err(IdentityError::unsafe_file(&dir, UnsafeReason::Symlink));
    }
    if !meta.is_dir() {
        return Err(IdentityError::unsafe_file(&dir, UnsafeReason::NotRegular));
    }
    // A new directory is set to exactly 0700: the umask can also take owner bits away.
    let fixed = if created {
        os::set_new_dir_private(&dir)
    } else {
        os::narrow_dir(&dir, &meta)
    };
    fixed.map_err(|e| IdentityError::io(FileOp::CreateDir, &dir, e))?;
    Ok(dir)
}

/// A written private temporary file and the file ID read from its handle before it was closed.
pub(crate) struct WrittenTemp {
    pub(crate) path: PathBuf,
    pub(crate) id: io::Result<FileId>,
}

/// Write `bytes` into a new private temporary file next to `name` in `dir` and return its path.
/// The access restriction is set and verified before the first byte is written; when that
/// fails, the temporary file is removed and nothing was written. When a later step fails, the
/// temporary file is removed too; if that removal fails, the error is
/// [`IdentityError::Leftover`].
pub(crate) fn write_temp(
    dir: &Path,
    name: &OsStr,
    bytes: &[u8],
) -> Result<WrittenTemp, IdentityError> {
    let (temp, mut file) = create_temp(dir, name)?;
    let result = (|| {
        protect(&file).map_err(|source| IdentityError::Protect {
            path: temp.clone(),
            source,
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| IdentityError::io(FileOp::Write, &temp, e))
    })();
    let id = file_id(&file);
    drop(file);
    match result {
        Ok(()) => Ok(WrittenTemp { path: temp, id }),
        Err(error) => Err(fail_with_cleanup(&temp, error)),
    }
}

fn create_temp(dir: &Path, name: &OsStr) -> Result<(PathBuf, File), IdentityError> {
    loop {
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".tmp-{}-{n}", std::process::id()));
        let temp = dir.join(temp_name);
        match os::create_temp(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(CreateFailure::Create(e)) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(CreateFailure::Create(e)) => {
                return Err(IdentityError::io(FileOp::CreateTemp, temp, e));
            }
            // No file was created, so there is nothing to remove.
            Err(CreateFailure::Protect(source)) => {
                return Err(IdentityError::Protect { path: temp, source });
            }
        }
    }
}

fn protect(file: &File) -> io::Result<()> {
    #[cfg(test)]
    if fault::protect_fails() {
        return Err(io::Error::other("injected protection failure"));
    }
    os::protect(file)
}

/// Remove a file this crate wrote. A file that is already gone counts as removed.
pub(crate) fn discard(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if fault::discard_fails() {
        return Err(io::Error::other("injected removal failure"));
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Remove `path` after `cause`; the result is `cause`, or [`IdentityError::Leftover`] holding
/// `cause` when the removal failed.
pub(crate) fn fail_with_cleanup(path: &Path, cause: IdentityError) -> IdentityError {
    match discard(path) {
        Ok(()) => cause,
        Err(cleanup) => IdentityError::Leftover {
            leftover: path.to_path_buf(),
            cause: Some(Box::new(cause)),
            cleanup,
        },
    }
}

/// Open an existing secret file with the privacy checks and read it. `Ok(None)` means the
/// file does not exist.
pub(crate) fn open_private(path: &Path) -> Result<Option<PrivateFile>, IdentityError> {
    let file = match os::open_checked(path) {
        Ok(file) => file,
        Err(OpenFailure::NotFound) => return Ok(None),
        Err(OpenFailure::Unsafe(reason, source)) => {
            return Err(IdentityError::UnsafeFile {
                path: path.to_path_buf(),
                reason,
                source,
            });
        }
    };
    let mut contents = Vec::new();
    (&file)
        .take(MAX_SECRET_LEN as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|e| IdentityError::io(FileOp::Read, path, e))?;
    Ok(Some(PrivateFile { file, contents }))
}

/// Whether `error`, from reopening the path of the still open `pinned` file, only means that
/// the file was deleted and waits for its last handle to close (Windows keeps the name until
/// then and refuses to open it). Always false on Unix, where a deleted name is gone at once.
pub(crate) fn is_delete_pending(pinned: &File, error: &io::Error) -> bool {
    os::is_delete_pending(pinned, error)
}

/// Whether the file at `path` (not following a link) has the file ID `id`. Any failure to tell
/// is false.
pub(crate) fn has_id(path: &Path, id: FileId) -> bool {
    os::open_for_id(path)
        .and_then(|file| file_id(&file))
        .is_ok_and(|found| found == id)
}

/// The identity of an open file: Unix device and inode, Windows volume and file ID.
pub(crate) fn file_id(file: &File) -> io::Result<FileId> {
    os::file_id(file)
}

/// The publication lock of `target`: an exclusive lock on `<target>.lock`. The lock file is a
/// fixed name, so it stays the same lock across replacements of the target.
pub(crate) fn lock(target: &Path) -> Result<File, IdentityError> {
    let mut name = target.as_os_str().to_owned();
    name.push(".lock");
    let path = PathBuf::from(name);
    let file = os::open_lock(&path).map_err(|e| IdentityError::io(FileOp::Lock, &path, e))?;
    file.lock()
        .map_err(|e| IdentityError::io(FileOp::Lock, &path, e))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_and_privilege_refusals_at_creation_are_protect_failures() {
        let classify = |code| classify_create_error(io::Error::from_raw_os_error(code));
        for code in [1307, 1308, 1314] {
            assert!(
                matches!(classify(code), CreateFailure::Protect(e) if e.raw_os_error() == Some(code)),
                "{code}"
            );
        }
        // ERROR_ACCESS_DENIED, ERROR_FILE_EXISTS, ERROR_PATH_NOT_FOUND stay creation failures.
        for code in [5, 80, 3] {
            assert!(
                matches!(classify(code), CreateFailure::Create(e) if e.raw_os_error() == Some(code)),
                "{code}"
            );
        }
        assert!(matches!(
            classify_create_error(io::Error::other("no code")),
            CreateFailure::Create(_)
        ));
    }
}

#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    use std::thread::LocalKey;

    thread_local! {
        static PROTECT_FAILS: Cell<bool> = const { Cell::new(false) };
        static DISCARD_FAILS: Cell<bool> = const { Cell::new(false) };
        static AFTER_REPLACE_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) struct Guard(&'static LocalKey<Cell<bool>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.with(|f| f.set(false));
        }
    }

    fn set(key: &'static LocalKey<Cell<bool>>) -> Guard {
        key.with(|f| f.set(true));
        Guard(key)
    }

    pub(crate) fn protect_fails() -> bool {
        PROTECT_FAILS.with(Cell::get)
    }

    pub(crate) fn discard_fails() -> bool {
        DISCARD_FAILS.with(Cell::get)
    }

    pub(crate) fn after_replace_fails() -> bool {
        AFTER_REPLACE_FAILS.with(Cell::get)
    }

    /// Make the step after a record replacement fail on this thread while the guard lives.
    pub(crate) fn fail_after_replace() -> Guard {
        set(&AFTER_REPLACE_FAILS)
    }

    thread_local! {
        static FOREIGN_REPLACE: Cell<bool> = const { Cell::new(false) };
    }

    /// Make a record publication on this thread see its target replaced by another private file
    /// right after its own rename, while the guard lives.
    pub(crate) fn foreign_replace() -> Guard {
        set(&FOREIGN_REPLACE)
    }

    /// The injected replacement: a private file with other contents and another file ID.
    pub(crate) fn replace_foreign(
        dir: &std::path::Path,
        name: &std::ffi::OsStr,
        target: &std::path::Path,
    ) {
        if FOREIGN_REPLACE.with(Cell::get) {
            let foreign = super::write_temp(dir, name, b"foreign\n").unwrap();
            std::fs::rename(&foreign.path, target).unwrap();
        }
    }

    /// Make the access restriction of new secret files fail on this thread while the guard lives.
    pub(crate) fn fail_protect() -> Guard {
        set(&PROTECT_FAILS)
    }

    /// Make the removal of files this crate wrote fail on this thread while the guard lives.
    pub(crate) fn fail_discard() -> Guard {
        set(&DISCARD_FAILS)
    }
}
