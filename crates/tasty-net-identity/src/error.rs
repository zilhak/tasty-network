//! The error of the file operations of this crate (docs/design/crates.md "오류 규칙").

use std::{error::Error, fmt, io, path::PathBuf};

/// The file operation that failed in [`IdentityError::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileOp {
    /// Creating the secret directory or narrowing its permissions.
    CreateDir,
    /// Creating the private temporary file.
    CreateTemp,
    /// Writing the secret into the private temporary file.
    Write,
    /// Publishing the temporary file under its final name.
    Publish,
    /// Opening or reading an existing secret file.
    Read,
    /// Taking the publication lock.
    Lock,
    /// Removing a retired publication.
    Remove,
}

impl fmt::Display for FileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreateDir => "prepare the directory",
            Self::CreateTemp => "create a temporary file",
            Self::Write => "write",
            Self::Publish => "publish",
            Self::Read => "read",
            Self::Lock => "lock",
            Self::Remove => "remove",
        })
    }
}

/// Why a device key file is corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptKind {
    /// The file is larger than any valid key file.
    TooLarge,
    /// The first line is not `tasty-net-device-key-v1`.
    Header,
    /// The second line is not 64 lowercase hex digits.
    Seed,
    /// Something follows the seed line other than one line break.
    TrailingData,
}

impl fmt::Display for CorruptKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "too large",
            Self::Header => "unknown header",
            Self::Seed => "seed is not 64 lowercase hex",
            Self::TrailingData => "trailing data",
        })
    }
}

/// Why an existing secret file is refused (docs/design/identity.md "파일 권한").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsafeReason {
    /// The path is a symbolic link (Unix) or a reparse point (Windows).
    Symlink,
    /// The path is not a regular file (or, for the secret directory, not a directory).
    NotRegular,
    /// The file is not owned by the current user.
    ForeignOwner,
    /// Unix: the mode allows more than owner read and write.
    WideMode {
        /// The permission bits that were found.
        mode: u32,
    },
    /// Windows: the DACL is absent, NULL, not protected, or not exactly one allow entry for
    /// the current user.
    Dacl,
    /// The check itself failed; the source holds the OS error.
    CheckFailed,
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => f.write_str("it is a link"),
            Self::NotRegular => f.write_str("it is not a regular file"),
            Self::ForeignOwner => f.write_str("another user owns it"),
            Self::WideMode { mode } => write!(f, "its mode {mode:o} is wider than 600"),
            Self::Dacl => f.write_str("its access list is not private"),
            Self::CheckFailed => f.write_str("its access could not be checked"),
        }
    }
}

/// The error of key loading and record publication.
#[derive(Debug)]
#[non_exhaustive]
pub enum IdentityError {
    /// A file system call failed.
    Io {
        /// What this crate was doing.
        op: FileOp,
        /// The path of the operation.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The device key file does not have the documented format. It is left untouched.
    Corrupt {
        /// The key file.
        path: PathBuf,
        /// The first problem found.
        kind: CorruptKind,
    },
    /// An existing secret file or directory is not private to the current user.
    UnsafeFile {
        /// The refused path.
        path: PathBuf,
        /// What is wrong with it.
        reason: UnsafeReason,
        /// The OS error when `reason` is [`UnsafeReason::CheckFailed`].
        source: Option<io::Error>,
    },
    /// Restricting a new secret file to the current user failed. No secret was written.
    Protect {
        /// The temporary file that was being protected (already removed).
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// A connection record that a reader would refuse; nothing was written.
    InvalidRecord(crate::RecordParseError),
    /// A file this crate wrote could not be removed. It can still hold the secret; the
    /// consumer can report or remove `leftover`.
    Leftover {
        /// The file that is left.
        leftover: PathBuf,
        /// The failure that made the removal necessary. `None` when the operation itself
        /// succeeded and only the temporary file stayed.
        cause: Option<Box<IdentityError>>,
        /// The OS error of the removal.
        cleanup: io::Error,
    },
}

impl IdentityError {
    pub(crate) fn io(op: FileOp, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn unsafe_file(path: impl Into<PathBuf>, reason: UnsafeReason) -> Self {
        Self::UnsafeFile {
            path: path.into(),
            reason,
            source: None,
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, .. } => write!(f, "could not {op} {}", path.display()),
            Self::Corrupt { path, kind } => {
                write!(f, "device key file {} is corrupt ({kind})", path.display())
            }
            Self::UnsafeFile { path, reason, .. } => {
                write!(
                    f,
                    "secret file {} is refused because {reason}",
                    path.display()
                )
            }
            Self::Protect { path, .. } => {
                write!(f, "could not restrict access to {}", path.display())
            }
            Self::InvalidRecord(problem) => {
                write!(f, "connection record cannot be published ({problem})")
            }
            Self::Leftover { leftover, .. } => {
                write!(f, "could not remove the secret file {}", leftover.display())
            }
        }
    }
}

impl Error for IdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Protect { source, .. } => Some(source),
            Self::UnsafeFile {
                source: Some(source),
                ..
            } => Some(source),
            Self::Leftover {
                cause: Some(cause), ..
            } => Some(&**cause),
            Self::Leftover {
                cause: None,
                cleanup,
                ..
            } => Some(cleanup),
            Self::InvalidRecord(problem) => Some(problem),
            Self::Corrupt { .. } | Self::UnsafeFile { source: None, .. } => None,
        }
    }
}
