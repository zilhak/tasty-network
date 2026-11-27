//! The result types of a resolution (docs/design/ssh-backends.md "결과 API").

use std::fmt;
use std::path::PathBuf;

use crate::options::ProvidedBy;

/// Where a value or a note comes from (docs/design/ssh-backends.md "옵션 적용 순서").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Origin {
    /// A command-line value (`-o`, `-p`, `-i`, `-l`, the target) with the caller's label
    /// (for example `forced`, `target`, `override`, `setting:ssh.connect_timeout`).
    CommandLine {
        /// The caller's label.
        label: String,
    },
    /// A line of a configuration file.
    File {
        /// The file.
        path: PathBuf,
        /// The line, from 1.
        line: usize,
    },
    /// The OpenSSH default (`fill_default_options`).
    Default,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::CommandLine { label } => f.write_str(label),
            Origin::File { path, line } => write!(f, "file:{}:{line}", path.display()),
            Origin::Default => f.write_str("default"),
        }
    }
}

/// A value a keyword gave and where it came from. Accumulating keywords have one entry per
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSource {
    /// The keyword as `ssh -G` names it.
    pub keyword: &'static str,
    /// The value as written.
    pub value: String,
    /// Where it came from.
    pub origin: Origin,
}

/// An `IdentityFile` or `CertificateFile` entry as `ssh` loads it before authenticating
/// (`load_public_identity_files`, `ssh.c:2354`-`:2497`). The crate does not open the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    /// The value as written, the way `ssh -G` shows it.
    pub written: String,
    /// The file `ssh` reads: `~` expanded (`tilde_expand_filename`), then the `%` tokens and
    /// `${NAME}` (`default_client_percent_dollar_expand`). `None` when the path is under a
    /// home that is not known: a missing file, which `ssh` skips (docs/design/ssh-backends.md
    /// "내장 백엔드가 읽는 파일").
    pub path: Option<String>,
    /// Who gave it (`identity_file_userprovided`, `certificate_file_userprovided`).
    pub provided_by: ProvidedBy,
    /// Where it was written: a file line, the command line (`-i`, `-o`), or
    /// [`Origin::Default`] for the default key files `fill_default_options` adds when no
    /// identity is given.
    pub origin: Origin,
}

/// What the resolver did with a keyword (`ConfigNote.action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteAction {
    /// Read and not applied; the connection goes on.
    Shown,
    /// The resolution is refused.
    Refused,
    /// Skipped without effect (`Protocol`, an unknown keyword matched by `IgnoreUnknown`).
    Ignored,
}

/// The kind of a note (`ConfigNote.class`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteClass {
    /// A keyword OpenSSH does not know (`unknown_keyword`).
    UnknownKeyword,
    /// An unknown keyword that `IgnoreUnknown` matched.
    IgnoredUnknownKeyword,
    /// `oDeprecated`, `oUnsupported` or `oIgnore`.
    DroppedByOpenSsh,
    /// "읽었지만 미지원": read and not applied.
    NotSupported,
    /// "보안 관련 미구현": refused when applied.
    SecurityNotImplemented,
    /// `StrictHostKeyChecking no`/`off` or `NoHostAuthenticationForLocalhost yes`: not
    /// applied, the host key is still checked.
    HostKeyCheckKept,
    /// A written `PubkeyAuthentication yes`: authenticates unbound (no host-bound
    /// authentication).
    PubkeyUnbound,
    /// A `SessionType` line whose value is not the one in effect.
    SessionTypeNotApplied,
}

/// A note about a keyword. It carries no value (values can be secrets).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigNote {
    /// The keyword, lower-cased as written (an unknown name as written).
    pub keyword: String,
    /// Where it is written.
    pub origin: Origin,
    /// What was done.
    pub action: NoteAction,
    /// The kind.
    pub class: NoteClass,
}

/// Why a resolution is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// OpenSSH also stops here (a bad value, a bad include, a fatal check of `ssh.c`).
    ConfigError,
    /// A file has a keyword OpenSSH does not know.
    UnknownKeyword,
    /// An applied value this crate refuses ("보안 관련 미구현").
    SecurityNotImplemented,
    /// `RefuseConnection` applied.
    RefuseConnection,
    /// The value needs something this crate does not have: running a program
    /// (`Match exec`), the local interfaces (`Match localnetwork`), a local value the caller
    /// did not give, a name service lookup, or the numeric address rule of the C library of
    /// an OS this crate has no rule for, where the libraries read the host differently.
    Undeterminable,
}

/// A refused resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The reason.
    pub kind: RefusalKind,
    /// The keyword, if one is the cause.
    pub keyword: Option<String>,
    /// Where, if known.
    pub origin: Option<Origin>,
    /// The message (OpenSSH's message where OpenSSH also stops).
    pub message: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.origin {
            Some(o) => write!(f, "{o}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}
