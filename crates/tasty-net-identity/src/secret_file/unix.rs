//! Unix: directory 0700, file 0600, owner = effective user.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    thread,
};

use super::{CreateFailure, DirFailure, OpenFailure};
use crate::error::UnsafeReason;
use crate::settings::IdentitySettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

/// Create the missing elements of `dir`. The deepest existing ancestor is found first (so `/`
/// is never passed to mkdir, which macOS answers with `EISDIR`), then the elements below it are
/// created from the top, one at a time. Each directory this call creates is set to 0700 before
/// the next one is made, because the umask can take owner bits away (0500 under 0277 would keep
/// the next element from being created). Existing elements keep their mode.
pub(super) fn create_dir(dir: &Path, settings: &IdentitySettings) -> Result<(), DirFailure> {
    let mut missing = Vec::new();
    for element in dir.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(element) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(element),
            Err(e) => return Err(DirFailure::Io(element.to_owned(), e)),
        }
    }
    for element in missing.into_iter().rev() {
        create_one_dir(element, settings)?;
    }
    Ok(())
}

/// Create one directory element. An element that appeared meanwhile must be a directory, not a
/// link or a file. Another thread of the same creation may have made the parent and not yet set
/// it to 0700; while the parent is a directory of the current user, `EACCES` is retried
/// `new_parent_retries` times, `new_parent_wait` apart.
fn create_one_dir(element: &Path, settings: &IdentitySettings) -> Result<(), DirFailure> {
    let io_failure = |e| DirFailure::Io(element.to_owned(), e);
    let mut retries = 0;
    loop {
        let error = match DirBuilder::new().mode(0o700).create(element) {
            Ok(()) => {
                return fs::set_permissions(element, fs::Permissions::from_mode(0o700))
                    .map_err(io_failure);
            }
            Err(e) => e,
        };
        if error.kind() == io::ErrorKind::AlreadyExists {
            let meta = fs::symlink_metadata(element).map_err(io_failure)?;
            if meta.file_type().is_symlink() {
                return Err(DirFailure::Unsafe(
                    element.to_owned(),
                    UnsafeReason::Symlink,
                ));
            }
            if !meta.is_dir() {
                return Err(DirFailure::Unsafe(
                    element.to_owned(),
                    UnsafeReason::NotRegular,
                ));
            }
            return Ok(());
        }
        // Like std's create_dir_all: an element that is a directory by now is fine.
        if fs::symlink_metadata(element).is_ok_and(|meta| meta.is_dir()) {
            return Ok(());
        }
        let parent_is_ours = || {
            element
                .parent()
                .and_then(|parent| fs::symlink_metadata(parent).ok())
                .is_some_and(|meta| meta.is_dir() && meta.uid() == effective_uid())
        };
        if error.kind() == io::ErrorKind::PermissionDenied
            && retries < settings.new_parent_retries()
            && parent_is_ours()
        {
            retries += 1;
            thread::sleep(settings.new_parent_wait());
            continue;
        }
        return Err(io_failure(error));
    }
}

pub(super) fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// Set a directory this crate just created to 0700 and read the mode back.
pub(super) fn set_new_dir_private(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mode = fs::symlink_metadata(dir)?.mode() & 0o7777;
    if mode != 0o700 {
        return Err(io::Error::other(format!(
            "new secret directory has mode {mode:o}, not 700"
        )));
    }
    Ok(())
}

pub(super) fn narrow_dir(dir: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 == 0 {
        return Ok(());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700))
}

pub(super) fn create_temp(path: &Path) -> Result<File, CreateFailure> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(CreateFailure::Create)
}

/// Set 0600 explicitly (the creation mode is reduced by the umask only, never widened, but an
/// inherited default ACL can still add group bits) and verify the result on the handle.
pub(super) fn protect(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let meta = file.metadata()?;
    check(&meta).map_err(|reason| {
        io::Error::other(format!("new secret file is still not private: {reason}"))
    })
}

pub(super) fn open_checked(path: &Path) -> Result<File, OpenFailure> {
    // A cheap prefilter so that device files are never opened.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(OpenFailure::Unsafe(UnsafeReason::Symlink, None));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(OpenFailure::Unsafe(UnsafeReason::NotRegular, None));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenFailure::NotFound),
        Err(e) => return Err(OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e))),
    }
    // O_NOFOLLOW refuses a link swapped in after the prefilter; O_NONBLOCK keeps a FIFO swapped
    // in from blocking the open. The decision below uses the opened handle only.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenFailure::NotFound),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(OpenFailure::Unsafe(UnsafeReason::Symlink, None));
        }
        Err(e) => return Err(OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e))),
    };
    let meta = file
        .metadata()
        .map_err(|e| OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e)))?;
    check(&meta).map_err(|reason| OpenFailure::Unsafe(reason, None))?;
    Ok(file)
}

fn check(meta: &fs::Metadata) -> Result<(), UnsafeReason> {
    if !meta.is_file() {
        return Err(UnsafeReason::NotRegular);
    }
    if meta.uid() != effective_uid() {
        return Err(UnsafeReason::ForeignOwner);
    }
    let mode = meta.mode() & 0o7777;
    if mode & !0o600 != 0 {
        return Err(UnsafeReason::WideMode { mode });
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no caller memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Rename `from` to `to` unless something is at `to` (`EEXIST`). Linux and Android: the
/// `renameat2` syscall with `RENAME_NOREPLACE`, called directly because bionic has its wrapper
/// only from API level 30.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both pointers are NUL-terminated strings owned by `from` and `to`, which live
    // until the call returns; the kernel only reads them. The arguments match the kernel
    // signature renameat2(int, const char *, int, const char *, unsigned int).
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result.is_negative() {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether a failed [`rename_noreplace`] only means it is not offered here: `ENOSYS` (a kernel
/// before 3.15) or `EINVAL` (a file system without `RENAME_NOREPLACE`).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn no_replace_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL))
}

/// Rename `from` to `to` unless something is at `to` (`EEXIST`). macOS and iOS: `renamex_np`
/// with `RENAME_EXCL` (macOS 10.12, iOS 10).
#[cfg(target_vendor = "apple")]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both pointers are NUL-terminated strings owned by `from` and `to`, which live
    // until the call returns; renamex_np only reads them.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result.is_negative() {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether a failed [`rename_noreplace`] only means it is not offered here: `ENOTSUP` (a file
/// system without `RENAME_EXCL`) or `EINVAL` (the flag is not known).
#[cfg(target_vendor = "apple")]
pub(super) fn no_replace_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL))
}

/// Other Unix systems have no rename that refuses to replace here; the caller links instead.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub(super) fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub(super) fn no_replace_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

pub(super) fn is_delete_pending(_pinned: &File, _error: &io::Error) -> bool {
    false
}

pub(super) fn file_id(file: &File) -> io::Result<FileId> {
    let meta = file.metadata()?;
    Ok(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

pub(super) fn open_for_id(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Open the lock file, creating it when missing. A lock file this call creates is set to 0600
/// explicitly, because the creation mode is reduced by the umask (0400 under 0277 would keep a
/// later call from reopening it for writing). An existing lock file keeps its mode; when it
/// cannot be opened for writing (a lock file another process created 0400 before setting its
/// mode, or one left from an older version), it is opened read-only. The lock is `flock`
/// (`File::lock` on Linux and macOS), which a read-only descriptor can hold.
pub(super) fn open_lock(path: &Path) -> io::Result<File> {
    let options = |write: bool, create_new: bool| {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(write)
            .create_new(create_new)
            .mode(0o600)
            // O_NONBLOCK: a FIFO at the lock path must not block the open (it is refused by
            // the regular-file check in `secret_file::lock`); flock still waits.
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        options
    };
    loop {
        match options(true, true).open(path) {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let existing = match options(true, false).open(path) {
            Err(e) if e.raw_os_error() == Some(libc::EACCES) => options(false, false).open(path),
            other => other,
        };
        match existing {
            Ok(file) => return Ok(file),
            // Removed between the opens: try to create it again.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
}
