//! Windows: owner = current user SID, protected DACL with one allow entry (FullControl) for the
//! current user SID. Set on the empty temporary file and read back before the secret is
//! written; checked on the opened handle for every existing secret file.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, RawHandle},
    },
    path::Path,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING, ERROR_SUCCESS, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAce,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
        CopySid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid,
        GetSecurityDescriptorControl, GetTokenInformation, INHERITED_ACE, InitializeAcl,
        InitializeSecurityDescriptor, IsValidSid, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PRESENT,
        SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_SHARE_NONE, FILE_STANDARD_INFO, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
    System::{
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

use super::{CreateFailure, OpenFailure};
use crate::error::UnsafeReason;

/// Volume serial plus the complete 128-bit file ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    volume: u64,
    file_id: [u8; 16],
}

pub(super) fn create_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// A symbolic link, junction or any other reparse point.
pub(super) fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Windows has no directory mode bits; the contract is on the files.
pub(super) fn set_new_dir_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Windows has no directory mode bits; the contract is on the files.
pub(super) fn narrow_dir(_dir: &Path, _meta: &std::fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// Create the temporary file already private: the security descriptor given to CreateFileW
/// has the current user as owner and the protected one-entry DACL, so the file never carries
/// the directory's inherited entries. The share mode is 0, so no other handle can open its data
/// until this one is closed after the write. `protect` sets and verifies the same security
/// again before the secret is written.
pub(super) fn create_temp(path: &Path) -> Result<File, CreateFailure> {
    let security = PrivateSecurity::new().map_err(CreateFailure::Protect)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&raw const security.descriptor).cast_mut().cast(),
        bInheritHandle: 0,
    };
    let name = wide_path(path).map_err(CreateFailure::Create)?;
    // SAFETY: name is a NUL-terminated wide string; attributes and the descriptor, SID and DACL
    // it points to live until the call returns; no template handle is passed. CREATE_NEW does
    // not follow an existing link, and FILE_FLAG_OPEN_REPARSE_POINT keeps it that way.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_NONE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(CreateFailure::Create(io::Error::last_os_error()));
    }
    // SAFETY: CreateFileW returned a new open handle that nothing else owns; File takes it over.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// The security descriptor of a new secret file: owner = current user SID, protected DACL with
/// one FullControl entry for that SID. The descriptor points into the SID and DACL buffers, which
/// are heap allocations owned by this value, so moving the value keeps them valid.
struct PrivateSecurity {
    _sid: Sid,
    _dacl: Vec<u32>,
    descriptor: SECURITY_DESCRIPTOR,
}

impl PrivateSecurity {
    fn new() -> io::Result<Self> {
        #[cfg(test)]
        if fault::descriptor_fails() {
            return Err(io::Error::other("injected security descriptor failure"));
        }
        let sid = Sid::current_user()?;
        let dacl = private_dacl(&sid)?;
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let sd: PSECURITY_DESCRIPTOR = (&raw mut descriptor).cast();
        // SAFETY: sd points to writable SECURITY_DESCRIPTOR storage in this frame.
        if unsafe { InitializeSecurityDescriptor(sd, SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sd was initialized above; the SID is a heap buffer kept in the returned value.
        if unsafe { SetSecurityDescriptorOwner(sd, sid.as_psid(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sd was initialized above; dacl is an initialized ACL in a heap buffer kept in
        // the returned value.
        if unsafe { SetSecurityDescriptorDacl(sd, 1, dacl.as_ptr().cast::<ACL>(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sd was initialized above; only the DACL protection bit is changed.
        if unsafe { SetSecurityDescriptorControl(sd, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _sid: sid,
            _dacl: dacl,
            descriptor,
        })
    }
}

#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    thread_local! {
        static DESCRIPTOR_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn descriptor_fails() -> bool {
        DESCRIPTOR_FAILS.with(Cell::get)
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            DESCRIPTOR_FAILS.with(|f| f.set(false));
        }
    }

    /// Make building the security descriptor of new secret files fail on this thread while the
    /// guard lives.
    pub(crate) fn fail_descriptor() -> Guard {
        DESCRIPTOR_FAILS.with(|f| f.set(true));
        Guard
    }
}

/// Paths from this length on get the verbatim prefix, like std's own conversion (a directory
/// path must leave room for an 8.3 file name below MAX_PATH).
const LEGACY_MAX_PATH: usize = 248;

/// The NUL-terminated wide path for CreateFileW. std's `OpenOptions::open` makes long paths
/// absolute and verbatim; the direct call has to do the same, or a key directory deeper than
/// MAX_PATH would fail only here.
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path)?;
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }
    let mut out = with_verbatim_prefix(wide);
    out.push(0);
    Ok(out)
}

/// Add `\\?\` (or `\\?\UNC\` for `\\server\share`) to a long absolute path. Paths that
/// already are verbatim or device paths (`\\?\`, `\\.\`) and short paths stay as they are.
fn with_verbatim_prefix(path: Vec<u16>) -> Vec<u16> {
    let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    if path.len() < LEGACY_MAX_PATH
        || path.starts_with(&wide(r"\\?\"))
        || path.starts_with(&wide(r"\\.\"))
    {
        return path;
    }
    if let Some(rest) = path.strip_prefix(wide(r"\\").as_slice()) {
        let mut out = wide(r"\\?\UNC\");
        out.extend_from_slice(rest);
        return out;
    }
    let mut out = wide(r"\\?\");
    out.extend_from_slice(&path);
    out
}

pub(super) fn protect(file: &File) -> io::Result<()> {
    let sid = Sid::current_user()?;
    let mut dacl = private_dacl(&sid)?;
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: the borrow keeps the handle open for this synchronous call; the owner SID and the
    // DACL are valid, initialized buffers that outlive the call; the group and SACL are null
    // because their information flags are not set.
    let rc = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sid.as_psid(),
            ptr::null_mut(),
            dacl.as_mut_ptr().cast::<ACL>(),
            ptr::null(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    match check(handle, &sid) {
        Ok(()) => Ok(()),
        Err(Check::Violation(reason)) => Err(io::Error::other(format!(
            "new secret file is still not private: {reason}"
        ))),
        Err(Check::Io(e)) => Err(e),
    }
}

pub(super) fn open_checked(path: &Path) -> Result<File, OpenFailure> {
    // FILE_FLAG_OPEN_REPARSE_POINT opens a link or junction itself instead of its target, and
    // FILE_FLAG_BACKUP_SEMANTICS lets a directory open, so the handle below is the object at
    // this path whatever it is, and the checks below classify it.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenFailure::NotFound),
        Err(e) => return Err(OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e))),
    };
    let meta = file
        .metadata()
        .map_err(|e| OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e)))?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(OpenFailure::Unsafe(UnsafeReason::Symlink, None));
    }
    if !meta.is_file() {
        return Err(OpenFailure::Unsafe(UnsafeReason::NotRegular, None));
    }
    let sid =
        Sid::current_user().map_err(|e| OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e)))?;
    match check(file.as_raw_handle() as HANDLE, &sid) {
        Ok(()) => Ok(file),
        Err(Check::Violation(reason)) => Err(OpenFailure::Unsafe(reason, None)),
        Err(Check::Io(e)) => Err(OpenFailure::Unsafe(UnsafeReason::CheckFailed, Some(e))),
    }
}

/// An open denied with ERROR_ACCESS_DENIED or ERROR_DELETE_PENDING, while the pinned handle of
/// the same file reports DeletePending.
pub(super) fn is_delete_pending(pinned: &File, error: &io::Error) -> bool {
    let denied = matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_ACCESS_DENIED | ERROR_DELETE_PENDING)
    );
    if !denied {
        return false;
    }
    let mut info = std::mem::MaybeUninit::<FILE_STANDARD_INFO>::uninit();
    // SAFETY: the borrow keeps the handle open for this synchronous call; info is aligned,
    // writable storage of exactly one FILE_STANDARD_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            pinned.as_raw_handle() as HANDLE,
            FileStandardInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return false;
    }
    // SAFETY: success for FileStandardInfo initialized the complete FILE_STANDARD_INFO; its
    // BOOLEAN fields are 0 or 1.
    let info = unsafe { info.assume_init() };
    info.DeletePending
}

pub(super) fn file_id(file: &File) -> io::Result<FileId> {
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the borrow keeps the handle open for this synchronous call; info is aligned,
    // writable storage of exactly one FILE_ID_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: success for FileIdInfo initialized the complete FILE_ID_INFO.
    let info = unsafe { info.assume_init() };
    if info.FileId.Identifier == [0; 16] {
        return Err(io::Error::other(
            "filesystem did not supply a usable file ID",
        ));
    }
    Ok(FileId {
        volume: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

pub(super) fn open_for_id(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(super) fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

enum Check {
    Violation(UnsafeReason),
    Io(io::Error),
}

/// Owner = `sid`, DACL present, non-NULL and protected, exactly one non-inherited allow entry
/// for `sid` with FullControl.
fn check(handle: HANDLE, sid: &Sid) -> Result<(), Check> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the caller keeps the handle open; every out pointer is a valid local; group and
    // SACL are not requested so their out pointers may be null.
    let rc = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(Check::Io(io::Error::from_raw_os_error(rc as i32)));
    }
    // owner and dacl point into descriptor, which lives until this guard drops.
    let descriptor = LocalBox(descriptor);

    // SAFETY: owner is null or points to a SID inside the live descriptor; sid is a valid SID.
    if owner.is_null() || unsafe { EqualSid(owner, sid.as_psid()) } == 0 {
        return Err(Check::Violation(UnsafeReason::ForeignOwner));
    }
    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    // SAFETY: descriptor is the live descriptor returned above; both out pointers are locals.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(Check::Io(io::Error::last_os_error()));
    }
    // An absent or NULL DACL grants everyone access even though it has no entries.
    if control & SE_DACL_PRESENT == 0 || dacl.is_null() || control & SE_DACL_PROTECTED == 0 {
        return Err(Check::Violation(UnsafeReason::Dacl));
    }
    // SAFETY: dacl is non-null and points to an ACL inside the live descriptor.
    let count = unsafe { (*dacl).AceCount };
    if count != 1 {
        return Err(Check::Violation(UnsafeReason::Dacl));
    }
    let mut ace: *mut c_void = ptr::null_mut();
    // SAFETY: dacl is a valid ACL with one entry; index 0 is in range; ace is a local.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(Check::Io(io::Error::last_os_error()));
    }
    // SAFETY: GetAce succeeded, so ace points to an entry inside the ACL that starts with an
    // ACE_HEADER.
    let header = unsafe { *ace.cast::<ACE_HEADER>() };
    if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || u32::from(header.AceFlags) & INHERITED_ACE != 0
        || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
    {
        return Err(Check::Violation(UnsafeReason::Dacl));
    }
    let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: the type and size checked above make this entry an ACCESS_ALLOWED_ACE.
    let mask = unsafe { (*allowed).Mask };
    // SAFETY: same entry; SidStart is the first word of the SID stored in the entry.
    let ace_sid: PSID = unsafe { (&raw const (*allowed).SidStart) }
        .cast_mut()
        .cast();
    // SAFETY: ace_sid points to the SID inside the live entry; sid is a valid SID.
    if mask != FILE_ALL_ACCESS || unsafe { EqualSid(ace_sid, sid.as_psid()) } == 0 {
        return Err(Check::Violation(UnsafeReason::Dacl));
    }
    Ok(())
}

/// A copy of a SID in owned, 4-byte aligned storage.
struct Sid(Vec<u32>);

impl Sid {
    fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    fn current_user() -> io::Result<Self> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: GetCurrentProcess has no preconditions and returns a pseudo handle that needs
        // no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: process is the live pseudo handle of this process; token is a local out
        // pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut len: u32 = 0;
        // SAFETY: a size query: null buffer with length 0 and a local for the needed length.
        // The expected failure is ERROR_INSUFFICIENT_BUFFER, detected by len staying 0 otherwise.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: buf is writable, 8-byte aligned and at least len bytes long.
        if unsafe {
            GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: success wrote a TOKEN_USER at the start of the aligned buffer.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        Self::copy(user.User.Sid)
    }

    fn copy(sid: PSID) -> io::Result<Self> {
        // SAFETY: sid points into the live token information buffer of the caller.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::other(
                "the process token has an invalid user SID",
            ));
        }
        // SAFETY: sid was validated above.
        let len = unsafe { GetLengthSid(sid) };
        let mut out = vec![0u32; (len as usize).div_ceil(4)];
        // SAFETY: out is writable and at least len bytes; sid is a valid SID of that length.
        if unsafe { CopySid(len, out.as_mut_ptr().cast(), sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(out))
    }
}

/// A DACL with one allow entry (FullControl) for `sid`, in owned 4-byte aligned storage.
fn private_dacl(sid: &Sid) -> io::Result<Vec<u32>> {
    // SAFETY: sid holds a valid SID.
    let sid_len = unsafe { GetLengthSid(sid.as_psid()) } as usize;
    let size = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_len;
    let mut buf = vec![0u32; size.div_ceil(4)];
    let acl = buf.as_mut_ptr().cast::<ACL>();
    // SAFETY: acl points to writable, aligned storage of at least `size` bytes.
    if unsafe { InitializeAcl(acl, (buf.len() * 4) as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: acl was initialized above with room for exactly this entry; sid is valid.
    if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid.as_psid()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by OpenProcessToken and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalBox(PSECURITY_DESCRIPTOR);

impl Drop for LocalBox {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by GetSecurityInfo/GetNamedSecurityInfoW with
        // LocalAlloc and is freed only here.
        unsafe { LocalFree(self.0) };
    }
}

/// Test helpers that read and change ACLs by path, separately from the handle-based code above.
#[cfg(test)]
pub(crate) mod testing {
    use std::{path::Path, ptr};

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED,
        },
    };

    use super::{LocalBox, Sid};

    /// One DACL entry: whether its SID is the current user, its type, flags and mask.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Entry {
        pub(crate) current_user: bool,
        pub(crate) ace_type: u8,
        pub(crate) flags: u8,
        pub(crate) mask: u32,
    }

    #[derive(Debug)]
    pub(crate) struct AclSummary {
        pub(crate) owner_is_current_user: bool,
        pub(crate) dacl_present: bool,
        pub(crate) dacl_null: bool,
        pub(crate) protected: bool,
        pub(crate) entries: Vec<Entry>,
    }

    /// The same long-path conversion as the product code, so deep test paths can be read.
    fn wide(path: &Path) -> Vec<u16> {
        super::wide_path(path).unwrap()
    }

    pub(crate) fn read_acl(path: &Path) -> AclSummary {
        let sid = Sid::current_user().unwrap();
        let name = wide(path);
        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: name is a NUL-terminated wide string; out pointers are locals.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "GetNamedSecurityInfoW");
        let descriptor = LocalBox(descriptor);
        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: descriptor is live; out pointers are locals.
        let ok = unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) };
        assert_ne!(ok, 0, "GetSecurityDescriptorControl");
        // SAFETY: owner points into the live descriptor; sid is valid.
        let owner_is_current_user =
            !owner.is_null() && unsafe { EqualSid(owner, sid.as_psid()) } != 0;
        let mut entries = Vec::new();
        if !dacl.is_null() {
            // SAFETY: dacl points to an ACL inside the live descriptor.
            let count = unsafe { (*dacl).AceCount };
            for index in 0..u32::from(count) {
                let mut ace = ptr::null_mut();
                // SAFETY: index is below the entry count; ace is a local.
                assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
                // SAFETY: ace points to an entry starting with an ACE_HEADER.
                let header = unsafe { *ace.cast::<ACE_HEADER>() };
                let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
                // SAFETY: allow and deny entries share this layout; the tests create only those.
                let mask = unsafe { (*allowed).Mask };
                // SAFETY: as above; SidStart begins the entry's SID.
                let ace_sid: PSID = unsafe { (&raw const (*allowed).SidStart) }
                    .cast_mut()
                    .cast();
                // SAFETY: ace_sid points into the live entry; sid is valid.
                let current_user = unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0;
                entries.push(Entry {
                    current_user,
                    ace_type: header.AceType,
                    flags: header.AceFlags,
                    mask,
                });
            }
        }
        AclSummary {
            owner_is_current_user,
            dacl_present: control & SE_DACL_PRESENT != 0,
            dacl_null: dacl.is_null(),
            protected: control & SE_DACL_PROTECTED != 0,
            entries,
        }
    }

    /// Replace the DACL of `path` with a NULL DACL (everyone has access) and keep the DACL
    /// protected, so that NULL is the only thing wrong with it.
    pub(crate) fn set_null_dacl(path: &Path) {
        let name = wide(path);
        // SAFETY: name is a NUL-terminated wide string; a null DACL with
        // DACL_SECURITY_INFORMATION sets a NULL DACL; PROTECTED_DACL_SECURITY_INFORMATION keeps
        // SE_DACL_PROTECTED set; other parts are not set.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "SetNamedSecurityInfoW");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_get_the_verbatim_prefix() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let long = "a".repeat(LEGACY_MAX_PATH);
        assert_eq!(with_verbatim_prefix(w(r"C:\x")), w(r"C:\x"));
        assert_eq!(
            with_verbatim_prefix(w(&format!(r"C:\{long}"))),
            w(&format!(r"\\?\C:\{long}"))
        );
        assert_eq!(
            with_verbatim_prefix(w(&format!(r"\\server\share\{long}"))),
            w(&format!(r"\\?\UNC\server\share\{long}"))
        );
        for already in [format!(r"\\?\C:\{long}"), format!(r"\\.\C:\{long}")] {
            assert_eq!(with_verbatim_prefix(w(&already)), w(&already));
        }
    }

    #[test]
    fn a_new_temporary_file_is_private_and_unshared_from_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".secret.tmp-test");
        let file = create_temp(&path).unwrap();
        // Read by path before protect() runs: the creation itself set the security.
        let acl = testing::read_acl(&path);
        assert!(
            acl.owner_is_current_user && acl.protected && !acl.dacl_null,
            "{acl:?}"
        );
        assert_eq!(acl.entries.len(), 1, "{acl:?}");
        assert!(
            acl.entries[0].current_user && acl.entries[0].flags == 0,
            "{acl:?}"
        );
        assert_eq!(acl.entries[0].mask, FILE_ALL_ACCESS, "{acl:?}");
        // ERROR_SHARING_VIOLATION while the creating handle is open.
        let other = File::open(&path).unwrap_err();
        assert_eq!(other.raw_os_error(), Some(32), "{other:?}");
        drop(file);
        File::open(&path).unwrap();
        assert!(matches!(
            create_temp(&path),
            Err(CreateFailure::Create(e)) if e.kind() == io::ErrorKind::AlreadyExists
        ));
    }
}
