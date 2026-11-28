//! What the settings query shows: the settings, the backend choice, and (with the embedded
//! backend's resolver) the configuration files, notes and the resolved target
//! (docs/design/ssh-backends.md "설정 조회", "연결 정보").

use std::fmt;
use std::path::PathBuf;

use crate::backend::{SelectedBackend, SshBackendError};
use crate::settings::Source;

/// What a resolution is for. The OpenSSH `Match command` and `Match sessiontype` can choose a
/// different host, user or key per purpose, so each purpose is resolved on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshPurpose {
    /// A `-L` tunnel (`sessiontype none`, no command).
    Tunnel,
    /// Port discovery (`sessiontype exec`, the discovery command).
    Discovery,
    /// A remote command (`sessiontype exec`).
    Exec {
        /// The command.
        command: String,
    },
    /// SFTP (`sessiontype subsystem`, command `sftp`).
    Sftp,
}

/// One line of the settings of an inspection: numeric and text settings alike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedSetting {
    /// The key.
    pub key: &'static str,
    /// The value in effect, as the design table writes it (`-` when there is none).
    pub value: String,
    /// Where it comes from.
    pub source: Source,
}

impl fmt::Display for InspectedSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {} ({})", self.key, self.value, self.source)
    }
}

/// Which resolver an inspection's file and target diagnostics come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolverState {
    /// The embedded backend is chosen; the diagnostics are what it connects with.
    Embedded,
    /// The system backend is chosen (or no backend): the embedded resolver's diagnostics are
    /// shown for reference; `ssh` itself connects.
    EmbeddedPreview,
    /// This build has no resolver (no `embedded-ssh`).
    NotBuilt,
}

impl ResolverState {
    /// `embedded`, `embedded_preview` or `not_built`.
    pub fn name(self) -> &'static str {
        match self {
            ResolverState::Embedded => "embedded",
            ResolverState::EmbeddedPreview => "embedded_preview",
            ResolverState::NotBuilt => "not_built",
        }
    }
}

/// What a note's directive led to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteAction {
    /// Read and not applied; the connection goes on.
    Shown,
    /// The connection is refused before it reaches the network.
    Refused,
    /// Skipped without effect.
    Ignored,
}

impl NoteAction {
    /// `shown`, `refused` or `ignored`.
    pub fn name(self) -> &'static str {
        match self {
            NoteAction::Shown => "shown",
            NoteAction::Refused => "refused",
            NoteAction::Ignored => "ignored",
        }
    }
}

/// The kind of a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoteClass {
    /// A keyword OpenSSH does not know.
    UnknownKeyword,
    /// An unknown keyword that `IgnoreUnknown` matched.
    IgnoredUnknownKeyword,
    /// A directive OpenSSH itself drops (`oDeprecated`, `oUnsupported`, `oIgnore`).
    DroppedByOpenSsh,
    /// Read and not supported by the embedded backend.
    NotSupported,
    /// A security directive the embedded backend does not implement: refused when applied.
    SecurityNotImplemented,
    /// A value that would turn host key checking off: not applied, the key is still checked.
    HostKeyCheckKept,
    /// A written `PubkeyAuthentication yes`: authenticates unbound.
    PubkeyUnbound,
    /// A `SessionType` value the purpose overrides.
    SessionTypeNotApplied,
    /// A key file in a format OpenSSH does not read (PuTTY `ppk`) or that needs a provider
    /// (`-sk` keys): skipped.
    UnsupportedKeyFormat,
    /// A `@cert-authority` line for the host: not trusted.
    CertAuthorityDistrusted,
    /// Algorithm names the embedded backend does not implement were left out.
    AlgorithmNotImplemented,
    /// A value was cut to what the embedded backend takes (`RekeyLimit`).
    Clamped,
    /// No agent socket: agent authentication is skipped.
    AgentUnavailable,
}

impl NoteClass {
    /// The design's name of the class (`unknown_keyword`, ...).
    pub fn name(self) -> &'static str {
        match self {
            NoteClass::UnknownKeyword => "unknown_keyword",
            NoteClass::IgnoredUnknownKeyword => "ignored_unknown_keyword",
            NoteClass::DroppedByOpenSsh => "dropped_by_openssh",
            NoteClass::NotSupported => "not_supported",
            NoteClass::SecurityNotImplemented => "security_not_implemented",
            NoteClass::HostKeyCheckKept => "host_key_check_kept",
            NoteClass::PubkeyUnbound => "pubkey_unbound",
            NoteClass::SessionTypeNotApplied => "session_type_not_applied",
            NoteClass::UnsupportedKeyFormat => "unsupported_key_format",
            NoteClass::CertAuthorityDistrusted => "cert_authority_distrusted",
            NoteClass::AlgorithmNotImplemented => "algorithm_not_implemented",
            NoteClass::Clamped => "clamped",
            NoteClass::AgentUnavailable => "agent_unavailable",
        }
    }
}

/// A note about a directive or a file. It carries no value (values can be secrets): only the
/// keyword, where it is written, what was done and the kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConfigNote {
    /// The keyword (or, for a key file note, the file's keyword such as `identityfile`).
    pub keyword: String,
    /// Where it is written: `file:<path>:<line>`, a command-line label (`target`, `override`,
    /// `forced`, `setting:<key>`) or `default`.
    pub origin: String,
    /// The file, when the note is about a file line or a file.
    pub file: Option<PathBuf>,
    /// The line, from 1, when the note is about a file line.
    pub line: Option<usize>,
    /// What was done.
    pub action: NoteAction,
    /// The kind.
    pub class: NoteClass,
}

impl fmt::Display for ConfigNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} ({}, {})",
            self.class.name(),
            self.keyword,
            self.origin,
            self.action.name()
        )
    }
}

/// A value of the resolution and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueSource {
    /// The keyword as `ssh -G` names it.
    pub keyword: String,
    /// The value as written.
    pub value: String,
    /// Where it came from (as [`ConfigNote::origin`]).
    pub source: String,
}

/// One jump host of a resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JumpHop {
    /// The host connected to.
    pub hostname: String,
    /// The port.
    pub port: u16,
    /// The remote user.
    pub user: String,
}

/// The embedded backend's resolution of a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// The host connected to.
    pub hostname: String,
    /// The remote user (`None` when neither the target, the configuration nor
    /// `embedded.local_user` gives one).
    pub user: Option<String>,
    /// The port.
    pub port: u16,
    /// The key files tried, in order.
    pub identity_files: Vec<PathBuf>,
    /// The jump hosts, first hop first.
    pub proxy_jump: Vec<JumpHop>,
    /// The values that choose where the connection goes (`hostname`, `user`, `port`,
    /// `hostkeyalias`, `proxyjump`, `identityfile`, `certificatefile`) with their sources, in
    /// order. Other values can hold secrets and are not carried.
    pub sources: Vec<ValueSource>,
}

/// Why a resolution was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalNote {
    /// The kind (`config_error`, `unknown_keyword`, `security_not_implemented`,
    /// `refuse_connection`, `undeterminable`).
    pub kind: &'static str,
    /// The keyword, when one is the cause.
    pub keyword: Option<String>,
    /// Where (as [`ConfigNote::origin`]), when known.
    pub origin: Option<String>,
    /// The message.
    pub message: String,
}

impl fmt::Display for RefusalNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.origin {
            Some(o) => write!(f, "{o}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// How a configuration file was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigFileState {
    /// Read.
    Loaded,
    /// Not there.
    Missing,
    /// Refused (its mode, or an unknown keyword at `line`).
    Refused {
        /// The line, when a line is the cause.
        line: Option<usize>,
        /// The keyword, when one is the cause.
        keyword: Option<String>,
    },
}

/// A configuration file of an inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFileReport {
    /// The file.
    pub path: PathBuf,
    /// How it was read.
    pub state: ConfigFileState,
}

/// The resolution of the target an inspection was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInspection {
    /// The resolution (up to the refusal, when it was refused).
    pub resolved: Option<ResolvedTarget>,
    /// The notes of the target's resolution.
    pub notes: Vec<ConfigNote>,
    /// Why the resolution was refused, if it was.
    pub refusal: Option<RefusalNote>,
}

/// The settings query of a connector: no connection, no other program
/// ([`crate::SshConnector::inspect`]).
#[derive(Clone, Debug)]
pub struct SshInspection {
    /// Every setting with its value and source.
    pub settings: Vec<InspectedSetting>,
    /// The backend choice for the target (or, without one, for the launcher's program).
    pub backend: Result<SelectedBackend, SshBackendError>,
    /// Which resolver the diagnostics below come from.
    pub resolver: ResolverState,
    /// The configuration files read (empty without a resolver).
    pub files: Vec<ConfigFileReport>,
    /// The file-level notes (empty without a resolver).
    pub file_notes: Vec<ConfigNote>,
    /// The target's resolution (`None` without a target or without a resolver).
    pub target: Option<TargetInspection>,
}
