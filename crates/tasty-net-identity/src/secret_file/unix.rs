//! Unix: directory 0700, file 0600, owner = effective user.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use super::OpenFailure;
use crate::error::UnsafeReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

pub(super) fn create_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

pub(super) fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

pub(super) fn narrow_dir(dir: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 == 0 {
        return Ok(());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700))
}

pub(super) fn create_temp(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
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
        Err(e) => return Err(OpenFailure::Io(e)),
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
        Err(e) => return Err(OpenFailure::Io(e)),
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

pub(super) fn file_id(file: &File) -> io::Result<FileId> {
    let meta = file.metadata()?;
    Ok(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

pub(super) fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
