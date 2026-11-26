//! Unix: directory 0700, file 0600, owner = effective user.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use super::{CreateFailure, OpenFailure};
use crate::error::UnsafeReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

/// Create the missing elements of `dir` from the top, one at a time. Each directory this call
/// creates is set to 0700 before the next one is made, because the umask can take owner bits
/// away (0500 under 0277 would keep the next element from being created). Elements that already
/// exist, also when another process made them meanwhile, keep their mode.
pub(super) fn create_dir(dir: &Path) -> io::Result<()> {
    let mut missing: Vec<&Path> = dir
        .ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    missing.reverse();
    for element in missing {
        match DirBuilder::new().mode(0o700).create(element) {
            Ok(()) => fs::set_permissions(element, fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
            .custom_flags(libc::O_NOFOLLOW);
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
