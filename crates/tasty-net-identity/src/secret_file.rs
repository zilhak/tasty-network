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
use crate::settings::IdentitySettings;

#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "reads the effective user ID and renames without replacing through libc"
)]
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

pub(crate) use os::{DirId, FileId};

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
    /// At most the `max_len + 1` bytes given to [`open_private`], so an oversized file is
    /// visible as such.
    pub(crate) contents: Vec<u8>,
}

/// Make sure `dir` is a private directory (Unix 0700) and return it without trailing
/// separators. The last path element is checked before anything is created: a link (also a
/// dangling one) or a non-directory there is [`IdentityError::UnsafeFile`]. A missing directory
/// is created (with the retries of `settings`); a wider existing one is narrowed.
pub(crate) fn prepare_dir(
    dir: &Path,
    settings: &IdentitySettings,
) -> Result<PathBuf, IdentityError> {
    prepare_dir_with_id(dir, settings).map(|(dir, _)| dir)
}

/// [`prepare_dir`], and the ID of the directory it checked, for [`DirCheck::Same`].
pub(crate) fn prepare_dir_with_id(
    dir: &Path,
    settings: &IdentitySettings,
) -> Result<(PathBuf, DirId), IdentityError> {
    // `a/link/` would let lstat follow the link; the components drop the trailing separator.
    let dir: PathBuf = match dir.components().collect::<PathBuf>() {
        p if p.as_os_str().is_empty() => PathBuf::from("."),
        p => p,
    };
    let lstat = || std::fs::symlink_metadata(&dir);
    let (meta, created) = match lstat() {
        Ok(meta) => (meta, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            os::create_dir(&dir, settings).map_err(|failure| match failure {
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
    let id = os::dir_id(&meta);
    Ok((dir, id))
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

/// How [`publish_new`] gave the file its new name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Published {
    /// Renamed: the temporary name is gone.
    Renamed,
    /// Hard-linked: the temporary name still exists and is the caller's to remove.
    Linked,
}

/// Give the complete file `temp` the new name `target` without ever replacing a file that is
/// at `target` (docs/design/identity.md "동시 첫 생성"). A rename that refuses to replace is
/// tried first (Linux and Android `renameat2` with `RENAME_NOREPLACE`, macOS and iOS
/// `renamex_np` with `RENAME_EXCL`); where the OS or the file system does not offer it, a hard
/// link, which never replaces either. `AlreadyExists` means a file is at `target`, and `temp`
/// is then still there. Any other error is returned as it is, except that a hard link which
/// fails after the OS refused the rename as not offered keeps both errors ([`FallbackError`]).
pub(crate) fn publish_new(temp: &Path, target: &Path) -> io::Result<Published> {
    #[cfg(test)]
    fault::publish_competitor(target);
    match rename_noreplace(temp, target) {
        Ok(()) => Ok(Published::Renamed),
        Err(rename) if os::no_replace_unsupported(&rename) => match hard_link(temp, target) {
            Ok(()) => Ok(Published::Linked),
            // Lost to another creator, or no rename was tried at all (Windows and other Unix
            // systems, whose `rename` error carries no OS error code): the link error as it is.
            Err(link) if link.kind() == io::ErrorKind::AlreadyExists => Err(link),
            Err(link) if rename.raw_os_error().is_none() => Err(link),
            Err(link) => Err(io::Error::new(link.kind(), FallbackError { rename, link })),
        },
        Err(error) => Err(error),
    }
}

/// A hard link that failed after the rename that refuses to replace was refused as not
/// offered. Both OS errors are in the message; the kind of the outer `io::Error` is the one of
/// the link, and the link error is the source.
#[derive(Debug)]
struct FallbackError {
    rename: io::Error,
    link: io::Error,
}

impl std::fmt::Display for FallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hard link failed ({}) after the rename that refuses to replace was not offered ({})",
            self.link, self.rename
        )
    }
}

impl std::error::Error for FallbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.link)
    }
}

fn rename_noreplace(temp: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(code) = fault::rename_error() {
        return Err(io::Error::from_raw_os_error(code));
    }
    os::rename_noreplace(temp, target)
}

fn hard_link(temp: &Path, target: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(code) = fault::link_error() {
        return Err(io::Error::from_raw_os_error(code));
    }
    std::fs::hard_link(temp, target)
}

/// What a directory sync expects to find at its path (docs/design/identity.md "폴더 동기화").
/// Every kind is opened as a directory only and without blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirCheck {
    /// The directory [`prepare_dir_with_id`] checked: not a link at the last element, and the
    /// opened directory has this ID.
    Same(DirId),
    /// A directory this call created: not a link at the last element.
    #[cfg_attr(windows, allow(dead_code, reason = "Windows uses create_dir_all"))]
    Created,
    /// The existing ancestor below which this call created directories. It may be a link the
    /// consumer's path goes through, so the link is followed.
    #[cfg_attr(windows, allow(dead_code, reason = "Windows uses create_dir_all"))]
    Ancestor,
}

/// Make the entries of the directory `dir` durable: a name this crate just published or removed
/// in it, or a directory it just created in it (docs/design/identity.md "폴더 동기화"). Unix
/// opens the directory as `check` says and syncs it like the key file (`fsync`, on macOS and
/// iOS `F_FULLFSYNC`); Windows has no directory sync. The name is in place already, so a
/// failure, also a directory that is not the expected one, is logged and not returned.
pub(crate) fn sync_dir(dir: &Path, check: DirCheck) {
    if let Err(error) = try_sync_dir(dir, check) {
        tracing::warn!(
            dir = %dir.display(),
            %error,
            "could not sync the secret directory; a power loss may lose its new entries"
        );
    }
}

fn try_sync_dir(dir: &Path, check: DirCheck) -> io::Result<()> {
    #[cfg(test)]
    fault::record_sync(dir, check)?;
    os::sync_dir(dir, check)
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

/// Open an existing secret file with the privacy checks and read at most `max_len + 1` bytes
/// of it. `Ok(None)` means the file does not exist.
pub(crate) fn open_private(
    path: &Path,
    max_len: usize,
) -> Result<Option<PrivateFile>, IdentityError> {
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
        // One byte more than the limit, so that a longer file shows as longer.
        .take(u64::try_from(max_len).unwrap_or(u64::MAX).saturating_add(1))
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
    // Judged on the opened handle: a FIFO, device or directory is not a lock file.
    let meta = file
        .metadata()
        .map_err(|e| IdentityError::io(FileOp::Lock, &path, e))?;
    if !meta.is_file() {
        return Err(IdentityError::unsafe_file(&path, UnsafeReason::NotRegular));
    }
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

    /// The real directory sync works on this OS (macOS and iOS: `F_FULLFSYNC` on a directory).
    #[cfg(unix)]
    #[test]
    fn a_directory_can_be_synced() {
        let dir = tempfile::tempdir().unwrap();
        let id = os::dir_id(&std::fs::symlink_metadata(dir.path()).unwrap());
        for check in [DirCheck::Same(id), DirCheck::Created, DirCheck::Ancestor] {
            os::sync_dir(dir.path(), check).unwrap();
            assert!(os::sync_dir(&dir.path().join("missing"), check).is_err());
        }
    }

    /// What took the place of the checked directory is not synced as if it were that directory.
    #[cfg(unix)]
    #[test]
    fn a_directory_sync_refuses_what_replaced_the_directory() {
        use std::fs;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("keys");
        let old = root.path().join("old");
        fs::create_dir(&dir).unwrap();
        let id = os::dir_id(&fs::symlink_metadata(&dir).unwrap());

        // Another directory renamed into its place: another ID.
        fs::rename(&dir, &old).unwrap();
        fs::create_dir(&dir).unwrap();
        assert!(os::sync_dir(&dir, DirCheck::Same(id)).is_err());
        os::sync_dir(&dir, DirCheck::Created).unwrap();

        // A link to the checked directory: refused at the last element, even with its ID,
        // except for the existing ancestor, which may be a link.
        fs::remove_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&old, &dir).unwrap();
        assert!(os::sync_dir(&dir, DirCheck::Same(id)).is_err());
        assert!(os::sync_dir(&dir, DirCheck::Created).is_err());
        os::sync_dir(&dir, DirCheck::Ancestor).unwrap();

        // A FIFO: refused, and the open does not wait for a writer.
        fs::remove_file(&dir).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        for check in [DirCheck::Same(id), DirCheck::Created, DirCheck::Ancestor] {
            assert!(os::sync_dir(&dir, check).is_err(), "{check:?}");
        }
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

    thread_local! {
        static RENAME_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
        static LINK_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
        static COMPETITOR: Cell<Option<&'static [u8]>> = const { Cell::new(None) };
    }

    /// The injection guards are used only by the tests of the rename that refuses to replace,
    /// which run where that rename exists.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(crate) struct ErrorGuard(&'static LocalKey<Cell<Option<i32>>>);

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    impl Drop for ErrorGuard {
        fn drop(&mut self) {
            self.0.with(|f| f.set(None));
        }
    }

    pub(crate) fn rename_error() -> Option<i32> {
        RENAME_ERROR.with(Cell::get)
    }

    pub(crate) fn link_error() -> Option<i32> {
        LINK_ERROR.with(Cell::get)
    }

    /// Make the rename that refuses to replace fail with the OS error `code` on this thread
    /// while the guard lives.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(crate) fn fail_rename(code: i32) -> ErrorGuard {
        RENAME_ERROR.with(|f| f.set(Some(code)));
        ErrorGuard(&RENAME_ERROR)
    }

    /// Make hard links fail with the OS error `code` on this thread while the guard lives (the
    /// Android app sandbox refuses them with `EACCES`).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(crate) fn fail_link(code: i32) -> ErrorGuard {
        LINK_ERROR.with(|f| f.set(Some(code)));
        ErrorGuard(&LINK_ERROR)
    }

    /// Make the next publication on this thread find a private file with `contents` already at
    /// its target, as if another creator had published it just before.
    pub(crate) fn publish_first(contents: &'static [u8]) {
        COMPETITOR.with(|f| f.set(Some(contents)));
    }

    pub(super) fn publish_competitor(target: &std::path::Path) {
        if let Some(contents) = COMPETITOR.with(Cell::take) {
            let dir = target.parent().unwrap();
            let other = super::write_temp(dir, target.file_name().unwrap(), contents).unwrap();
            std::fs::rename(&other.path, target).unwrap();
        }
    }

    /// One directory sync seen by the test hook.
    #[cfg_attr(
        windows,
        allow(dead_code, reason = "only the Unix tests read the notes")
    )]
    pub(crate) struct SyncNote {
        pub(crate) dir: std::path::PathBuf,
        pub(crate) check: super::DirCheck,
        /// The temporary file names in `dir` when the sync was called.
        pub(crate) temps: Vec<std::ffi::OsString>,
    }

    thread_local! {
        static SYNCED: std::cell::RefCell<Vec<SyncNote>> =
            const { std::cell::RefCell::new(Vec::new()) };
        static SYNC_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    /// Note a directory sync on this thread; the injected failure comes after the note.
    pub(super) fn record_sync(
        dir: &std::path::Path,
        check: super::DirCheck,
    ) -> std::io::Result<()> {
        let temps = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        let note = SyncNote {
            dir: dir.to_path_buf(),
            check,
            temps,
        };
        SYNCED.with(|synced| synced.borrow_mut().push(note));
        if SYNC_FAILS.with(Cell::get) {
            return Err(std::io::Error::other("injected directory sync failure"));
        }
        Ok(())
    }

    /// The directory syncs on this thread since the last call, in order (Unix tests).
    #[cfg(unix)]
    pub(crate) fn take_sync_notes() -> Vec<SyncNote> {
        SYNCED.with(std::cell::RefCell::take)
    }

    /// The directories synced on this thread since the last call, in order (Unix tests).
    #[cfg(unix)]
    pub(crate) fn take_synced() -> Vec<std::path::PathBuf> {
        take_sync_notes().into_iter().map(|note| note.dir).collect()
    }

    /// Make directory syncs fail on this thread while the guard lives (Unix tests).
    #[cfg(unix)]
    pub(crate) fn fail_sync() -> Guard {
        set(&SYNC_FAILS)
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
