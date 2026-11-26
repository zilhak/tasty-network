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

/// The result of an OS-level open with the privacy checks.
pub(crate) enum OpenFailure {
    NotFound,
    Unsafe(UnsafeReason, Option<io::Error>),
    Io(io::Error),
}

/// An existing secret file opened after the privacy checks, and its contents.
pub(crate) struct PrivateFile {
    pub(crate) file: File,
    /// At most `MAX_SECRET_LEN + 1` bytes, so an oversized file is visible as such.
    pub(crate) contents: Vec<u8>,
}

/// Create `dir` if needed and make sure it is a private directory (Unix 0700). A wider
/// existing directory is narrowed. A link in place of the directory is refused.
pub(crate) fn prepare_dir(dir: &Path) -> Result<(), IdentityError> {
    os::create_dir(dir).map_err(|e| IdentityError::io(FileOp::CreateDir, dir, e))?;
    let meta =
        std::fs::symlink_metadata(dir).map_err(|e| IdentityError::io(FileOp::CreateDir, dir, e))?;
    if meta.file_type().is_symlink() {
        return Err(IdentityError::unsafe_file(dir, UnsafeReason::Symlink));
    }
    if !meta.is_dir() {
        return Err(IdentityError::unsafe_file(dir, UnsafeReason::NotRegular));
    }
    os::narrow_dir(dir, &meta).map_err(|e| IdentityError::io(FileOp::CreateDir, dir, e))
}

/// Write `bytes` into a new private temporary file next to `name` in `dir` and return its path.
/// The access restriction is set and verified before the first byte is written; when that
/// fails, the temporary file is removed and nothing was written.
pub(crate) fn write_temp(dir: &Path, name: &OsStr, bytes: &[u8]) -> Result<PathBuf, IdentityError> {
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
    drop(file);
    match result {
        Ok(()) => Ok(temp),
        Err(error) => {
            remove_temp(&temp);
            Err(error)
        }
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
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(IdentityError::io(FileOp::CreateTemp, temp, e)),
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

/// Remove a temporary file this crate created. A failure is logged, not returned: the caller's
/// own result decides the outcome.
pub(crate) fn remove_temp(temp: &Path) {
    if let Err(error) = std::fs::remove_file(temp)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(temp = %temp.display(), %error, "could not remove a temporary secret file");
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
        Err(OpenFailure::Io(e)) => return Err(IdentityError::io(FileOp::Read, path, e)),
    };
    let mut contents = Vec::new();
    (&file)
        .take(MAX_SECRET_LEN as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|e| IdentityError::io(FileOp::Read, path, e))?;
    Ok(Some(PrivateFile { file, contents }))
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
pub(crate) mod fault {
    use std::cell::Cell;

    thread_local! {
        static PROTECT_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn protect_fails() -> bool {
        PROTECT_FAILS.with(Cell::get)
    }

    /// Make the access restriction of new secret files fail on this thread while the guard lives.
    pub(crate) fn fail_protect() -> impl Drop {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                PROTECT_FAILS.with(|f| f.set(false));
            }
        }
        PROTECT_FAILS.with(|f| f.set(true));
        Guard
    }
}
