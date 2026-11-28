//! Unix: directory 0700, file 0600, owner = effective user.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    thread,
};

use super::{CreateFailure, DirCheck, DirFailure, OpenFailure, Synced};
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
/// the next element from being created). The exception is a new element that cannot be opened
/// and is left as it is ([`create_one_dir`]: a private one without the owner read bit, such as
/// 0300 for a user without privileges); the next element is still created below it, and a last
/// element left so fails. Existing elements keep their mode. Then the parent of
/// every element that was missing is synced, the deepest first, so that the new entries survive
/// a power loss; the walk ends at the existing ancestor (docs/design/identity.md "폴더 동기화").
pub(super) fn create_dir(dir: &Path, settings: &IdentitySettings) -> Result<(), DirFailure> {
    let mut missing = Vec::new();
    for element in dir.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(element) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(element),
            Err(e) => return Err(DirFailure::Io(element.to_owned(), e)),
        }
    }
    for element in missing.iter().rev() {
        create_one_dir(element, settings)?;
    }
    // An element another creator made meanwhile is synced too: its entry may not be durable yet
    // when this call returns. The parent of the topmost missing element is the existing
    // ancestor; every other parent is a missing element.
    let topmost = missing.last().copied();
    for element in missing {
        let parent = element
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let check = if Some(element) == topmost {
            DirCheck::Ancestor
        } else {
            DirCheck::Created
        };
        super::sync_dir(parent, check);
    }
    Ok(())
}

/// The ID of a directory from its metadata (not following a link), for [`DirCheck::Same`].
pub(crate) type DirId = FileId;

pub(super) fn dir_id(meta: &fs::Metadata) -> DirId {
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// Sync the directory `dir` through a handle opened on it. `O_DIRECTORY` refuses anything else
/// that took its place, and `O_NONBLOCK` keeps such an open from waiting (a FIFO); `O_NOFOLLOW`
/// refuses a link at the last element unless `check` is the existing ancestor. A directory
/// with another ID than the checked one is still synced, because the publication went by the
/// path to that directory, and is reported as [`Synced::Replaced`]. `File::sync_all` is
/// `fsync`, and on macOS and iOS `fcntl(F_FULLFSYNC)`, the same as for the key file.
pub(super) fn sync_dir(dir: &Path, check: DirCheck) -> io::Result<Synced> {
    let flags = match check {
        DirCheck::Ancestor => libc::O_DIRECTORY | libc::O_NONBLOCK,
        DirCheck::Same(_) | DirCheck::Created => {
            libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_NOFOLLOW
        }
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(dir)?;
    file.sync_all()?;
    match check {
        DirCheck::Same(id) if file_id(&file)? != id => Ok(Synced::Replaced),
        _ => Ok(Synced::AsChecked),
    }
}

/// Create one directory element. An element that appeared meanwhile must be a directory, not a
/// link or a file. Another thread of the same creation may have made the parent and not yet set
/// it to 0700; while the parent is a directory of the current user, `EACCES` is retried
/// `new_parent_retries` times, `new_parent_wait` apart. The new element is set to 0700 through
/// a handle ([`set_private`]); one that cannot be opened and is left as it is does not stop
/// the creation of the next element.
fn create_one_dir(element: &Path, settings: &IdentitySettings) -> Result<(), DirFailure> {
    let io_failure = |e| DirFailure::Io(element.to_owned(), e);
    let mut retries = 0;
    loop {
        let error = match DirBuilder::new().mode(0o700).create(element) {
            Ok(()) => {
                #[cfg(test)]
                super::fault::run_after_mkdir(element);
                return set_private(element).map(|_left_or_private| ());
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

/// Set a directory this crate just created to 0700 and read the mode back, both on a handle
/// ([`set_private`]). A new directory left as it was (a private one without the owner read bit,
/// so it cannot be opened) is a failure here.
pub(super) fn set_new_dir_private(dir: &Path) -> Result<(), DirFailure> {
    match set_private(dir)? {
        NewDirMode::Private => Ok(()),
        NewDirMode::LeftAsIs(mode) => Err(not_private(dir, mode)),
    }
}

/// The failure of a new secret directory that does not have mode 0700 afterwards.
fn not_private(dir: &Path, mode: u32) -> DirFailure {
    DirFailure::Io(
        dir.to_owned(),
        io::Error::other(format!("new secret directory has mode {mode:o}, not 700")),
    )
}

/// What [`set_private`] did with a new directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NewDirMode {
    /// Set to 0700 through a handle, and 0700 read back from that handle.
    Private,
    /// Not opened because of a permission error, so left as it is with this mode (a private
    /// directory of the current user without the owner read bit).
    LeftAsIs(u32),
}

/// Set a directory this crate just created to 0700 through a handle from
/// [`open_dir_for_change`]. The handle must be a directory of the current user; another owner is
/// [`DirFailure::Unsafe`]. A mode other than 0700 read back from the handle after the change is
/// [`DirFailure::Io`]. A directory that cannot be opened is judged by [`new_dir_unopened`]:
/// only a private one of the current user that the owner cannot read (no owner read bit) is
/// left as it is.
fn set_private(dir: &Path) -> Result<NewDirMode, DirFailure> {
    let io_failure = |e| DirFailure::Io(dir.to_owned(), e);
    let handle = match open_dir_for_change(dir)? {
        OpenedDir::Handle(handle, meta) => {
            if meta.uid() != effective_uid() {
                return Err(DirFailure::Unsafe(
                    dir.to_owned(),
                    UnsafeReason::ForeignOwner,
                ));
            }
            handle
        }
        OpenedDir::Unopened(error, kind, mode) => {
            if kind == Unopened::Private {
                tracing::debug!(
                    dir = %dir.display(),
                    %error,
                    "could not open the new secret directory to set it to 0700"
                );
            }
            return new_dir_unopened(dir, error, kind, mode);
        }
    };
    handle
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(io_failure)?;
    #[cfg(test)]
    super::fault::run_after_new_dir_chmod(dir);
    let mode = handle.metadata().map_err(io_failure)?.mode() & 0o7777;
    if mode != 0o700 {
        return Err(not_private(dir, mode));
    }
    Ok(NewDirMode::Private)
}

/// A new directory that could not be opened to set 0700: another owner is
/// [`DirFailure::Unsafe`], the same as with a handle, whatever the open error. A private one of
/// the current user is left as it is only when the owner cannot read it: the open failed with a
/// permission error (`EACCES`) and the mode has no owner read bit. Any other open error (such as
/// `EMFILE`), or a permission error on one the owner can read (a read restriction such as an
/// LSM), is that error as [`DirFailure::Io`], so the cause is kept and an intermediate element
/// does not pass. A
/// directory with group or other bits is the open error as [`DirFailure::Io`]. [`narrow_dir`]
/// keeps its own rule for an existing directory.
pub(super) fn new_dir_unopened(
    dir: &Path,
    error: io::Error,
    kind: Unopened,
    mode: u32,
) -> Result<NewDirMode, DirFailure> {
    match kind {
        Unopened::Private if error.raw_os_error() == Some(libc::EACCES) && mode & 0o400 == 0 => {
            Ok(NewDirMode::LeftAsIs(mode))
        }
        Unopened::Private => Err(DirFailure::Io(dir.to_owned(), error)),
        Unopened::ForeignOwner => Err(DirFailure::Unsafe(
            dir.to_owned(),
            UnsafeReason::ForeignOwner,
        )),
        Unopened::NotPrivate => Err(DirFailure::Io(dir.to_owned(), error)),
    }
}

/// A secret directory opened by [`open_dir_for_change`].
enum OpenedDir {
    /// The handle and its metadata, a directory.
    Handle(File, fs::Metadata),
    /// Not opened, and a directory when looked at again: the open error, what that look found
    /// and the mode it read.
    Unopened(io::Error, Unopened, u32),
}

/// What a directory that could not be opened for a mode change is when looked at again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unopened {
    /// A directory of the current user without group and other bits. This says nothing about
    /// why the open failed or whether the owner can read it; a new directory is left as it is
    /// only when, besides, the open error is `EACCES` and the mode has no owner read bit
    /// ([`new_dir_unopened`]).
    Private,
    /// A directory of another user.
    ForeignOwner,
    /// A directory of the current user with group or other bits.
    NotPrivate,
}

/// What the element that could not be opened is, from its metadata not following a link: a
/// link or a non-directory is [`DirFailure::Unsafe`]; otherwise its owner and mode tell the
/// [`Unopened`] kind. `euid` is the effective user.
pub(super) fn judge_unopened(
    dir: &Path,
    link: bool,
    is_dir: bool,
    mode: u32,
    uid: u32,
    euid: u32,
) -> Result<Unopened, DirFailure> {
    if link {
        return Err(DirFailure::Unsafe(dir.to_owned(), UnsafeReason::Symlink));
    }
    if !is_dir {
        return Err(DirFailure::Unsafe(dir.to_owned(), UnsafeReason::NotRegular));
    }
    Ok(if uid != euid {
        Unopened::ForeignOwner
    } else if mode & 0o077 == 0 {
        Unopened::Private
    } else {
        Unopened::NotPrivate
    })
}

/// Open the directory `dir` for a mode change without following a link at the last element
/// (`O_NOFOLLOW`; `O_DIRECTORY` refuses anything else, and `O_NONBLOCK` keeps such an open from
/// waiting). Its type, owner and mode are then read from the handle and the mode is set through
/// it (`fchmod`), so the object changed is the one judged. What this guards against is a link
/// or a non-directory at the last element, which is [`DirFailure::Unsafe`]; a missing element is
/// [`DirFailure::Io`]. The handle is not compared with the object of an earlier check or
/// `mkdir` (no file ID comparison): another directory of the same user put in its place before
/// this open is changed. When the open fails otherwise, the element is looked at again without
/// following a link, because its mode may have changed since the earlier check
/// ([`judge_unopened`]), and the caller decides on the [`OpenedDir::Unopened`] result.
fn open_dir_for_change(dir: &Path) -> Result<OpenedDir, DirFailure> {
    let io_failure = |e| DirFailure::Io(dir.to_owned(), e);
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir);
    let handle = match handle {
        Ok(handle) => handle,
        Err(e) => match e.raw_os_error() {
            // A link at the last element is ELOOP or, because of O_DIRECTORY, ENOTDIR
            // (Linux); the element itself tells a link from another non-directory.
            Some(libc::ELOOP | libc::ENOTDIR) => {
                let link = e.raw_os_error() == Some(libc::ELOOP)
                    || fs::symlink_metadata(dir).is_ok_and(|meta| is_link(&meta));
                let reason = if link {
                    UnsafeReason::Symlink
                } else {
                    UnsafeReason::NotRegular
                };
                return Err(DirFailure::Unsafe(dir.to_owned(), reason));
            }
            _ if e.kind() == io::ErrorKind::NotFound => return Err(io_failure(e)),
            // Not opened, for any other reason (`EACCES`, `EMFILE` and so on). Judged on the
            // element as it is now, not on the check before, because its mode may have changed
            // since: owner and group and other bits give the [`Unopened`] kind. Whether the
            // open error and the owner read bit allow leaving it as it is, the caller decides.
            _ => {
                let now = match fs::symlink_metadata(dir) {
                    Ok(now) => now,
                    Err(_) => return Err(io_failure(e)),
                };
                let mode = now.mode() & 0o7777;
                let kind = judge_unopened(
                    dir,
                    is_link(&now),
                    now.is_dir(),
                    mode,
                    now.uid(),
                    effective_uid(),
                )?;
                return Ok(OpenedDir::Unopened(e, kind, mode));
            }
        },
    };
    let meta = handle.metadata().map_err(io_failure)?;
    if !meta.is_dir() {
        return Err(DirFailure::Unsafe(dir.to_owned(), UnsafeReason::NotRegular));
    }
    Ok(OpenedDir::Handle(handle, meta))
}

/// Narrow an existing directory with group or other bits to its owner bits. A directory of the
/// current user without group and other bits but without all owner bits is what `mkdir` gives
/// under a umask that removes owner bits (0500 under 0277): another creator of the same path may
/// have made it and not set it to 0700 yet, so the missing owner bits are added here instead of
/// failing on the directory. That is best effort: when it fails (a read-only file system, or a
/// directory the owner cannot read and so cannot open), the directory is used as it is.
///
/// `meta` (from the check before, not following a link) only tells whether anything is to be
/// changed. The change is judged and made on a handle from [`open_dir_for_change`]: a link or a
/// non-directory swapped in after the check is refused, and once the handle is open the change
/// goes to the object it holds. Another directory of the same user swapped in before that open
/// is not told apart (see [`open_dir_for_change`]). A directory that cannot be opened is used as
/// it is only when it is a private one of the current user; otherwise (another owner, or group
/// or other bits) the open error is [`DirFailure::Io`].
pub(super) fn narrow_dir(dir: &Path, meta: &fs::Metadata) -> Result<(), DirFailure> {
    if !needs_change(meta) {
        return Ok(());
    }
    #[cfg(test)]
    super::fault::run_before_narrow();
    let io_failure = |e| DirFailure::Io(dir.to_owned(), e);
    let (handle, meta) = match open_dir_for_change(dir)? {
        OpenedDir::Handle(handle, meta) => (handle, meta),
        OpenedDir::Unopened(error, Unopened::Private, _) => {
            tracing::debug!(
                dir = %dir.display(),
                %error,
                "could not open the secret directory to add the missing owner bits"
            );
            return Ok(());
        }
        OpenedDir::Unopened(error, Unopened::ForeignOwner | Unopened::NotPrivate, _) => {
            return Err(io_failure(error));
        }
    };
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 == 0 {
        if mode & 0o700 != 0o700
            && meta.uid() == effective_uid()
            && let Err(error) = handle.set_permissions(fs::Permissions::from_mode(mode | 0o700))
        {
            tracing::debug!(
                dir = %dir.display(),
                %error,
                "could not add the missing owner bits to the secret directory"
            );
        }
        return Ok(());
    }
    handle
        .set_permissions(fs::Permissions::from_mode(mode & 0o700))
        .map_err(io_failure)
}

/// Whether [`narrow_dir`] has anything to change in a directory with `meta`: group or other bits
/// to take away, or owner bits to add to a directory of the current user.
fn needs_change(meta: &fs::Metadata) -> bool {
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 == 0 {
        mode & 0o700 != 0o700 && meta.uid() == effective_uid()
    } else {
        true
    }
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
