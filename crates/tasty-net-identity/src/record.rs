//! The connection record: port, token and instance ID of one instance, published together
//! (docs/design/identity.md "파일 형식").

use std::{
    ffi::OsStr,
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
};

use crate::{
    error::{FileOp, IdentityError, UnsafeReason},
    secret_file::{self, FileId},
    token::ConnectionToken,
};

/// What one instance publishes so that a dialer reaching it over SSH learns its port and token
/// from one read. The file form is one JSON line
/// `{"port": <u16>, "token": "<64 lowercase hex>", "instance_id": "<string>"}`.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionRecord {
    /// The loopback port of the instance.
    pub port: u16,
    /// The connection token of the instance.
    pub token: ConnectionToken,
    /// The opaque instance ID given by the consumer.
    pub instance_id: String,
}

impl fmt::Debug for ConnectionRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionRecord")
            .field("port", &self.port)
            .field("token", &self.token)
            .field("instance_id", &self.instance_id)
            .finish()
    }
}

/// What a discovery output contains: a record, or one port number (an instance that publishes
/// no token, which the transport reports as `Discovered::LegacyPort`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRecord {
    /// A connection record.
    Record(ConnectionRecord),
    /// Only a decimal port number.
    LegacyPort(u16),
}

/// A record field named in a [`RecordParseError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordField {
    /// `port`
    Port,
    /// `token`
    Token,
    /// `instance_id`
    InstanceId,
}

impl fmt::Display for RecordField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Port => "port",
            Self::Token => "token",
            Self::InstanceId => "instance_id",
        })
    }
}

/// What is wrong with a record text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordProblem {
    /// The text is neither one JSON object nor one decimal port.
    NotRecord,
    /// The field is missing.
    Missing(RecordField),
    /// `port` is not an integer from 1 to 65535.
    BadPort,
    /// `token` is not 64 lowercase hex digits.
    BadToken,
    /// `instance_id` is not a non-empty string.
    BadInstanceId,
    /// The record line is longer than the 4096 bytes a reader takes (refused on publication).
    TooLong,
}

/// A record text was refused. It names only the field or the kind of problem; the refused
/// text (which can hold a token) is never part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordParseError {
    /// The problem.
    pub problem: RecordProblem,
}

impl fmt::Display for RecordParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.problem {
            RecordProblem::NotRecord => f.write_str("connection record is not a JSON object"),
            RecordProblem::Missing(field) => write!(f, "{field}: missing"),
            RecordProblem::BadPort => f.write_str("port: not an integer from 1 to 65535"),
            RecordProblem::BadToken => f.write_str("token: not 64 lowercase hex"),
            RecordProblem::BadInstanceId => f.write_str("instance_id: not a non-empty string"),
            RecordProblem::TooLong => f.write_str("record: longer than 4096 bytes"),
        }
    }
}

impl std::error::Error for RecordParseError {}

fn refuse(problem: RecordProblem) -> RecordParseError {
    RecordParseError { problem }
}

impl ConnectionRecord {
    /// The line [`ConnectionRecord::publish`] writes, after the checks `parse` applies on the
    /// reading side: port not 0, non-empty instance ID, at most `MAX_SECRET_LEN` bytes.
    fn checked_line(&self) -> Result<String, RecordParseError> {
        if self.port == 0 {
            return Err(refuse(RecordProblem::BadPort));
        }
        if self.instance_id.is_empty() {
            return Err(refuse(RecordProblem::BadInstanceId));
        }
        let line = self.to_line();
        if line.len() > secret_file::MAX_SECRET_LEN {
            return Err(refuse(RecordProblem::TooLong));
        }
        Ok(line)
    }

    /// The record as one JSON line with a final line break.
    pub fn to_line(&self) -> String {
        let id = serde_json::Value::String(self.instance_id.clone());
        format!(
            "{{\"port\":{},\"token\":\"{}\",\"instance_id\":{id}}}\n",
            self.port,
            self.token.expose_hex()
        )
    }

    /// Parse a record text (a file or a discovery command output). Surrounding white space is
    /// ignored and unknown fields are ignored. A text of decimal digits only is
    /// [`ParsedRecord::LegacyPort`].
    pub fn parse(text: &str) -> Result<ParsedRecord, RecordParseError> {
        let text = text.trim();
        if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            return match text.parse::<u16>() {
                Ok(port) if port != 0 => Ok(ParsedRecord::LegacyPort(port)),
                _ => Err(refuse(RecordProblem::BadPort)),
            };
        }
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| refuse(RecordProblem::NotRecord))?;
        let object = value
            .as_object()
            .ok_or_else(|| refuse(RecordProblem::NotRecord))?;
        let field = |name: &str, which| {
            object
                .get(name)
                .ok_or_else(|| refuse(RecordProblem::Missing(which)))
        };
        let port = field("port", RecordField::Port)?
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|&p| p != 0)
            .ok_or_else(|| refuse(RecordProblem::BadPort))?;
        let token = field("token", RecordField::Token)?
            .as_str()
            .and_then(|t| t.parse::<ConnectionToken>().ok())
            .ok_or_else(|| refuse(RecordProblem::BadToken))?;
        let instance_id = field("instance_id", RecordField::InstanceId)?
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| refuse(RecordProblem::BadInstanceId))?
            .to_owned();
        Ok(ParsedRecord::Record(Self {
            port,
            token,
            instance_id,
        }))
    }

    /// Publish this record at `path`: a private temporary file in the same directory replaces
    /// the path, under the publication lock of the path. The directory is made private like
    /// the key directory. A link or non-regular file at `path` is refused.
    ///
    /// A record that a reader would refuse (port 0, empty instance ID, a line longer than
    /// 4096 bytes) is [`IdentityError::InvalidRecord`] before anything is written, and the
    /// existing file stays. When a step after the replacement fails, the new file is removed
    /// again under the lock before the error is returned.
    ///
    /// The returned handle is the only way to remove the record again
    /// ([`RecordPublication::retire`]).
    pub fn publish(&self, path: &Path) -> Result<RecordPublication, IdentityError> {
        let name = record_name(path)?;
        let dir = parent_of(path);
        secret_file::prepare_dir(dir)?;
        let target = target_of(path)?;
        let _lock = secret_file::lock(&target)?;
        refuse_non_regular(&target)?;
        let contents = self.checked_line().map_err(IdentityError::InvalidRecord)?;
        let temp = secret_file::write_temp(dir, name, contents.as_bytes())?;
        // The ID of the written file, taken from its own handle; a rename keeps it.
        let id = match temp.id {
            Ok(id) => id,
            Err(e) => {
                let cause = IdentityError::io(FileOp::Read, &temp.path, e);
                return Err(secret_file::fail_with_cleanup(&temp.path, cause));
            }
        };
        if let Err(e) = std::fs::rename(&temp.path, &target) {
            let cause = IdentityError::io(FileOp::Publish, &target, e);
            return Err(secret_file::fail_with_cleanup(&temp.path, cause));
        }
        let published = match confirm(&target, &contents, id) {
            Ok(file) => file,
            // Still under the lock, the file at the target is the one just renamed there.
            Err(cause) => return Err(secret_file::fail_with_cleanup(&target, cause)),
        };
        tracing::info!(path = %path.display(), "published connection record");
        Ok(RecordPublication {
            path: path.to_path_buf(),
            target,
            contents,
            id,
            _pin: published,
        })
    }
}

/// Read the replaced target back through the privacy checks and return the open handle, which
/// keeps this generation's file ID from being reused while the publication lives.
fn confirm(target: &Path, contents: &str, id: FileId) -> Result<File, IdentityError> {
    #[cfg(test)]
    if secret_file::fault::after_replace_fails() {
        return Err(IdentityError::io(
            FileOp::Read,
            target,
            io::Error::other("injected failure after the replacement"),
        ));
    }
    let published = secret_file::open_private(target)?.ok_or_else(|| {
        IdentityError::io(
            FileOp::Read,
            target,
            io::Error::new(io::ErrorKind::NotFound, "record vanished after publication"),
        )
    })?;
    let same_id = secret_file::file_id(&published.file)
        .map_err(|e| IdentityError::io(FileOp::Read, target, e))?
        == id;
    if !same_id || published.contents != contents.as_bytes() {
        return Err(IdentityError::io(
            FileOp::Read,
            target,
            io::Error::other("record changed during publication"),
        ));
    }
    Ok(published.file)
}

/// The outcome of [`RecordPublication::retire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireOutcome {
    /// The record was this publication and was removed.
    Removed,
    /// Another publication (a newer instance) is at the path now; it was left alone.
    Replaced,
    /// Nothing is at the path any more.
    Missing,
}

/// A completed record publication. It remembers the published path, the actual target, the
/// file identity and the contents, so that [`RecordPublication::retire`] removes only its own
/// generation.
pub struct RecordPublication {
    path: PathBuf,
    target: PathBuf,
    contents: String,
    id: FileId,
    _pin: File,
}

impl fmt::Debug for RecordPublication {
    /// The contents hold the token, so only their length is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordPublication")
            .field("path", &self.path)
            .field("target", &self.target)
            .field("id", &self.id)
            .field("contents_len", &self.contents.len())
            .finish_non_exhaustive()
    }
}

impl RecordPublication {
    /// The path given to [`ConnectionRecord::publish`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the record if it is still this publication: under the publication lock, the
    /// path must still lead to the same target, and the file there must have the same file
    /// identity and the same contents. Otherwise the file is left as it is.
    pub fn retire(self) -> Result<RetireOutcome, IdentityError> {
        record_name(&self.target)?;
        // The lock file is created when missing, so NotFound means the directory is gone, and
        // with it the record.
        let _lock = match secret_file::lock(&self.target) {
            Ok(lock) => lock,
            Err(IdentityError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(RetireOutcome::Missing);
            }
            Err(e) => return Err(e),
        };
        match target_of(&self.path) {
            Ok(target) if target == self.target => {}
            Ok(_) => return Ok(RetireOutcome::Replaced),
            Err(IdentityError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(RetireOutcome::Missing);
            }
            Err(e) => return Err(e),
        }
        match std::fs::symlink_metadata(&self.target) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                return Ok(RetireOutcome::Replaced);
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RetireOutcome::Missing),
            Err(e) => return Err(IdentityError::io(FileOp::Read, &self.target, e)),
        }
        let Some(current) = secret_file::open_private(&self.target)? else {
            return Ok(RetireOutcome::Missing);
        };
        let same_id = secret_file::file_id(&current.file)
            .map_err(|e| IdentityError::io(FileOp::Read, &self.target, e))?
            == self.id;
        if !same_id || current.contents != self.contents.as_bytes() {
            return Ok(RetireOutcome::Replaced);
        }
        drop(current);
        match std::fs::remove_file(&self.target) {
            Ok(()) => Ok(RetireOutcome::Removed),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RetireOutcome::Missing),
            Err(e) => Err(IdentityError::io(FileOp::Remove, &self.target, e)),
        }
    }
}

/// Why a record path is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordPathProblem {
    /// The path has no file name.
    NoFileName,
    /// The file name ends with `.lock` (any case), the suffix of the publication lock files.
    /// Such a record would share its name with the lock file of another record.
    LockSuffix,
}

impl fmt::Display for RecordPathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoFileName => "it has no file name",
            Self::LockSuffix => "its name ends with .lock",
        })
    }
}

fn record_name(path: &Path) -> Result<&OsStr, IdentityError> {
    let refuse = |problem| IdentityError::InvalidRecordPath {
        path: path.to_path_buf(),
        problem,
    };
    let name = path
        .file_name()
        .ok_or_else(|| refuse(RecordPathProblem::NoFileName))?;
    let bytes = name.as_encoded_bytes();
    let suffix = b".lock";
    if bytes.len() >= suffix.len()
        && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        return Err(refuse(RecordPathProblem::LockSuffix));
    }
    Ok(name)
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// The actual target: the canonical parent directory joined with the file name. The file name
/// itself must not be a link (see `refuse_non_regular`).
fn target_of(path: &Path) -> Result<PathBuf, IdentityError> {
    let dir = parent_of(path);
    let name: &OsStr = path.file_name().unwrap_or_default();
    dir.canonicalize()
        .map(|dir| dir.join(name))
        .map_err(|e| IdentityError::io(FileOp::Publish, dir, e))
}

fn refuse_non_regular(target: &Path) -> Result<(), IdentityError> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(IdentityError::unsafe_file(target, UnsafeReason::Symlink))
        }
        Ok(meta) if !meta.is_file() => {
            Err(IdentityError::unsafe_file(target, UnsafeReason::NotRegular))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(IdentityError::io(FileOp::Publish, target, e)),
    }
}

#[cfg(test)]
#[path = "record/tests.rs"]
mod tests;
