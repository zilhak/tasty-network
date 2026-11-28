//! The errors of the bindings (docs/design/mobile.md "API 원칙", "오류 변환").
//!
//! Every core error enum becomes an FFI error enum with the same variants, so that an app
//! branches on the variant (its i18n key) and never on text. The text of an error (its
//! `Display`, exported to Kotlin as `toString()` and to Swift as `description`) is the core
//! error's English sentence: the sentence of the core error it came from, never extended with
//! a raw I/O error's text. Where the core sentence is fixed and the I/O error is only its
//! source, the FFI text is that fixed sentence and the I/O text stays in the `detail` field.
//! Core enums are `#[non_exhaustive]`, so each FFI enum has an `Other` variant for what this
//! version does not know. No variant carries a connection token.

use std::{fmt, io};

use tasty_net_frame::ReadFailure;
use tasty_net_identity::IdentityError;
use tasty_net_p2p::{P2pCloseReason, P2pConnectError, P2pError, StopCause};
use tasty_net_transport::{
    ConnectError, HandshakeError, ReadError, RejectReason, RequestError, TooOldEvidence,
};

use crate::secrets::Secrets;

/// Why the receiving end refused a handshake (`RejectReason`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Rejection {
    /// A message is not in the handshake format.
    Malformed,
    /// The peer speaks handshake v1.
    LegacyProtocol,
    /// The `proto` value is neither 1 nor 2.
    UnsupportedProtocol,
    /// No token.
    TokenMissing,
    /// The token is wrong.
    TokenInvalid,
    /// A signature did not verify.
    SignatureInvalid,
    /// The device key differs from the TOFU record.
    PeerKeyMismatch,
    /// Both ends are the same instance.
    SelfAttach,
    /// The receiving end does not take the requested role.
    RoleRefused,
    /// The bulk credential is unknown or revoked.
    BulkCredentialInvalid,
    /// The consumer's domain check refused; the refusal's `ext` carries its code.
    Domain,
    /// The receiving end cannot take the handshake now.
    Busy,
    /// A reason this version does not know, with its wire form.
    Unknown {
        /// The wire form.
        code: String,
    },
}

impl From<&RejectReason> for Rejection {
    fn from(reason: &RejectReason) -> Self {
        match reason {
            RejectReason::Malformed => Self::Malformed,
            RejectReason::LegacyProtocol => Self::LegacyProtocol,
            RejectReason::UnsupportedProtocol => Self::UnsupportedProtocol,
            RejectReason::TokenMissing => Self::TokenMissing,
            RejectReason::TokenInvalid => Self::TokenInvalid,
            RejectReason::SignatureInvalid => Self::SignatureInvalid,
            RejectReason::PeerKeyMismatch => Self::PeerKeyMismatch,
            RejectReason::SelfAttach => Self::SelfAttach,
            RejectReason::RoleRefused => Self::RoleRefused,
            RejectReason::BulkCredentialInvalid => Self::BulkCredentialInvalid,
            RejectReason::Domain => Self::Domain,
            RejectReason::Busy => Self::Busy,
            RejectReason::Unknown(code) => Self::Unknown { code: code.clone() },
        }
    }
}

impl Rejection {
    /// The reason with its peer-made code scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        match self {
            Self::Unknown { code } => Self::Unknown {
                code: secrets.scrub(&code),
            },
            other => other,
        }
    }

    /// The core value, for the core's rules (`is_retryable`) and texts.
    fn core(&self) -> RejectReason {
        match self {
            Self::Malformed => RejectReason::Malformed,
            Self::LegacyProtocol => RejectReason::LegacyProtocol,
            Self::UnsupportedProtocol => RejectReason::UnsupportedProtocol,
            Self::TokenMissing => RejectReason::TokenMissing,
            Self::TokenInvalid => RejectReason::TokenInvalid,
            Self::SignatureInvalid => RejectReason::SignatureInvalid,
            Self::PeerKeyMismatch => RejectReason::PeerKeyMismatch,
            Self::SelfAttach => RejectReason::SelfAttach,
            Self::RoleRefused => RejectReason::RoleRefused,
            Self::BulkCredentialInvalid => RejectReason::BulkCredentialInvalid,
            Self::Domain => RejectReason::Domain,
            Self::Busy => RejectReason::Busy,
            Self::Unknown { code } => RejectReason::Unknown(code.clone()),
        }
    }
}

/// How the dialer learned that the peer is too old for handshake v2 (`TooOldEvidence`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TooOld {
    /// Port discovery returned only a port number: the peer publishes no token.
    PortOnlyDiscovery,
    /// The peer answered with a v1 refusing ACK.
    V1Ack,
}

impl From<TooOldEvidence> for TooOld {
    fn from(evidence: TooOldEvidence) -> Self {
        match evidence {
            TooOldEvidence::PortOnlyDiscovery => Self::PortOnlyDiscovery,
            TooOldEvidence::V1Ack => Self::V1Ack,
        }
    }
}

/// Why a whole P2P connection was closed (`P2pCloseReason`), with its QUIC code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum P2pClose {
    /// Code 0: closed on purpose.
    Closed,
    /// The peer does not have this device on its pairing list.
    NotPaired,
    /// The connection does not belong to the session it was punched for.
    SessionMismatch,
    /// A code this version does not know.
    Unknown {
        /// The QUIC application close code.
        code: u64,
    },
}

impl From<P2pCloseReason> for P2pClose {
    fn from(reason: P2pCloseReason) -> Self {
        match reason {
            P2pCloseReason::Closed => Self::Closed,
            P2pCloseReason::NotPaired => Self::NotPaired,
            P2pCloseReason::SessionMismatch => Self::SessionMismatch,
            P2pCloseReason::Unknown(code) => Self::Unknown { code },
        }
    }
}

impl P2pClose {
    fn core(self) -> P2pCloseReason {
        match self {
            Self::Closed => P2pCloseReason::Closed,
            Self::NotPaired => P2pCloseReason::NotPaired,
            Self::SessionMismatch => P2pCloseReason::SessionMismatch,
            Self::Unknown { code } => P2pCloseReason::Unknown(code),
        }
    }
}

/// Why the P2P endpoint stopped and does not reconnect (`StopCause`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum P2pStop {
    /// The same device registered again on another connection.
    Superseded,
    /// The server's access policy refused this device.
    AccessDenied,
    /// The server does not know this device's protocol version.
    UnsupportedProto,
    /// The server does not speak this device's ALPN.
    AlpnMismatch,
    /// The server's key is not the configured one.
    ServerKeyMismatch,
    /// The reconnection attempts ran out.
    ReconnectExhausted,
}

impl From<StopCause> for P2pStop {
    fn from(cause: StopCause) -> Self {
        match cause {
            StopCause::Superseded => Self::Superseded,
            StopCause::AccessDenied => Self::AccessDenied,
            StopCause::UnsupportedProto => Self::UnsupportedProto,
            StopCause::AlpnMismatch => Self::AlpnMismatch,
            StopCause::ServerKeyMismatch => Self::ServerKeyMismatch,
            StopCause::ReconnectExhausted => Self::ReconnectExhausted,
        }
    }
}

/// Why the rendezvous server cancelled a punch (`CancelReason` of `punch_cancel`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PunchCancelReason {
    /// The other device's registration is gone.
    PeerGone,
    /// The session TTL passed.
    Expired,
    /// The same requesting device started a new session for this pair.
    Superseded,
    /// The other device reported `limits_invalid`.
    PeerLimitsInvalid,
    /// A reason this version does not know, with its wire name.
    Other {
        /// The wire name.
        code: String,
    },
}

impl PunchCancelReason {
    /// From the wire name; this crate reads it without depending on the rendezvous crate.
    fn from_wire(value: Result<serde_json::Value, serde_json::Error>) -> Self {
        let code = value
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        match code.as_str() {
            "peer_gone" => Self::PeerGone,
            "expired" => Self::Expired,
            "superseded" => Self::Superseded,
            "peer_limits_invalid" => Self::PeerLimitsInvalid,
            _ => Self::Other { code },
        }
    }
}

/// What kind of P2P endpoint failure an error is (`P2pError`, its variants other than a
/// failed attempt's details).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum P2pFailureKind {
    /// P2P is off on this device.
    Disabled,
    /// A connection attempt failed.
    Connect,
    /// The P2P settings are not valid. Nothing was opened.
    Settings,
    /// P2P is on but no rendezvous server is given.
    NoServer,
    /// Another P2P endpoint that is on uses this device key, in this process or in another
    /// one (through the key directory's lock).
    EndpointInUse,
    /// The blocking API was called inside an async task.
    InAsyncContext,
    /// The endpoint is closed.
    Closed,
    /// No connection came within the given time.
    TimedOut,
    /// The socket, the runtime or the lock file failed.
    Io,
    /// The TLS configuration could not be made from the device key.
    Tls,
}

impl From<&P2pError> for P2pFailureKind {
    fn from(error: &P2pError) -> Self {
        match error {
            P2pError::Disabled => Self::Disabled,
            P2pError::Connect(_) => Self::Connect,
            P2pError::Settings(_) => Self::Settings,
            P2pError::NoServer => Self::NoServer,
            P2pError::EndpointInUse => Self::EndpointInUse,
            P2pError::InAsyncContext => Self::InAsyncContext,
            P2pError::Closed => Self::Closed,
            P2pError::TimedOut => Self::TimedOut,
            P2pError::Io(_) => Self::Io,
            P2pError::Tls(_) => Self::Tls,
        }
    }
}

/// Why a P2P connection attempt ended without a connection (`P2pConnectError`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum P2pConnectFailure {
    /// P2P is off on this device.
    Disabled,
    /// The endpoint is not registered on the server; when it stopped, the stop cause.
    RendezvousUnavailable {
        /// The stop cause, when the endpoint stopped.
        stopped: Option<P2pStop>,
    },
    /// The coordination connection was lost before the punch started.
    RendezvousLost,
    /// No server answer in time.
    RequestTimeout,
    /// The server has no such peer.
    PeerNotFound,
    /// The peer does not have this device on its pairing list.
    Declined,
    /// Both NATs are address dependent and no host candidate reached.
    NatUnsupported,
    /// The server cancelled the punch in progress.
    PunchCancelled {
        /// The server's reason.
        reason: PunchCancelReason,
    },
    /// The peer refused the server's limits.
    PeerLimitsInvalid,
    /// No punch attempt succeeded in time.
    PunchFailed,
    /// A server limit.
    Busy,
    /// A server value does not fit this device's setting `key`.
    ServerLimitsInvalid {
        /// The setting.
        key: String,
    },
    /// The QUIC peer's key is not the requested device ID.
    PeerIdentityMismatch,
    /// The requested device ID is this device's.
    SelfAttach,
    /// The peer speaks another protocol version.
    AlpnMismatch,
    /// The attempt was cancelled.
    Cancelled,
}

impl From<&P2pConnectError> for P2pConnectFailure {
    fn from(error: &P2pConnectError) -> Self {
        match error {
            P2pConnectError::Disabled => Self::Disabled,
            P2pConnectError::RendezvousUnavailable { stopped } => Self::RendezvousUnavailable {
                stopped: stopped.map(P2pStop::from),
            },
            P2pConnectError::RendezvousLost => Self::RendezvousLost,
            P2pConnectError::RequestTimeout => Self::RequestTimeout,
            P2pConnectError::PeerNotFound => Self::PeerNotFound,
            P2pConnectError::Declined => Self::Declined,
            P2pConnectError::NatUnsupported => Self::NatUnsupported,
            P2pConnectError::PunchCancelled { reason } => Self::PunchCancelled {
                reason: PunchCancelReason::from_wire(serde_json::to_value(reason)),
            },
            P2pConnectError::PeerLimitsInvalid => Self::PeerLimitsInvalid,
            P2pConnectError::PunchFailed => Self::PunchFailed,
            P2pConnectError::Busy => Self::Busy,
            P2pConnectError::ServerLimitsInvalid { key } => Self::ServerLimitsInvalid {
                key: (*key).to_string(),
            },
            P2pConnectError::PeerIdentityMismatch => Self::PeerIdentityMismatch,
            P2pConnectError::SelfAttach => Self::SelfAttach,
            P2pConnectError::AlpnMismatch => Self::AlpnMismatch,
            P2pConnectError::Cancelled => Self::Cancelled,
        }
    }
}

impl P2pConnectFailure {
    /// The core table's answer (`P2pConnectError::is_retryable`); the tests compare the two
    /// for every variant.
    fn is_retryable(&self) -> bool {
        match self {
            Self::RendezvousUnavailable { stopped } => stopped.is_none(),
            Self::RendezvousLost
            | Self::RequestTimeout
            | Self::PeerNotFound
            | Self::PunchCancelled { .. }
            | Self::Busy => true,
            Self::Disabled
            | Self::Declined
            | Self::NatUnsupported
            | Self::PeerLimitsInvalid
            | Self::PunchFailed
            | Self::ServerLimitsInvalid { .. }
            | Self::PeerIdentityMismatch
            | Self::SelfAttach
            | Self::AlpnMismatch
            | Self::Cancelled => false,
        }
    }
}

/// Why opening a link, a request channel or a bulk channel failed: `HandshakeError` with the
/// `ConnectError` it can wrap, flattened into one enum. Both paths of a peer too old for v2
/// (`ConnectError::PeerTooOld` from port discovery, `HandshakeError::PeerTooOld` from a v1
/// answer, also inside `HandshakeError::Connect`) are [`HandshakeFailure::PeerTooOld`] with
/// their evidence.
#[uniffi::export(Display)]
#[derive(Debug, uniffi::Error)]
pub enum HandshakeFailure {
    /// The peer refused with a refusing ACK.
    Rejected {
        /// The reason the peer gave.
        reason: Rejection,
        /// The peer's English sentence.
        peer_text: String,
        /// The consumer's code for [`Rejection::Domain`].
        ext: Option<String>,
    },
    /// This end refused the peer.
    Refused {
        /// The reason.
        reason: Rejection,
    },
    /// The peer speaks only v1. Do not connect again.
    PeerTooOld {
        /// How it was found out.
        evidence: TooOld,
    },
    /// The peer's signature did not verify.
    SignatureInvalid,
    /// The peer is this instance, or the P2P target is this device.
    SelfAttach,
    /// The peer's device ID differs from the record in the app's [`PeerKeyStore`]
    /// (crate::PeerKeyStore). The record was left unchanged; only the user re-registers.
    PeerKeyMismatch {
        /// The recorded device ID.
        expected: String,
        /// The device ID the peer proved.
        got: String,
    },
    /// The P2P `hello` named another device than the connection proved.
    PeerMismatch,
    /// The peer broke the handshake.
    Protocol {
        /// What it sent wrong.
        detail: String,
    },
    /// The handshake deadline passed.
    TimedOut,
    /// The attempt was cancelled.
    Cancelled,
    /// The peer closed the channel during the handshake.
    PeerClosed,
    /// A link timeout is too large to make a deadline.
    InvalidTimeouts,
    /// The first handshake message would be longer than the handshake line limit.
    MessageTooLong,
    /// The QUIC connection was closed with a code during the handshake.
    Closed {
        /// The close reason.
        reason: P2pClose,
    },
    /// The endpoint has no credential to dial with.
    NoCredential,
    /// The link cannot open bulk channels.
    NotBulkCapable,
    /// The parent link ended before the bulk channel was registered under it.
    ParentEnded,
    /// The app's [`PeerKeyStore`](crate::PeerKeyStore) failed; its own text.
    Store {
        /// The store's error text.
        detail: String,
    },
    /// The socket or stream could not be opened.
    ConnectIo {
        /// The I/O error's text.
        detail: String,
    },
    /// The P2P endpoint could not make the connection.
    P2pConnect {
        /// Why.
        failure: P2pConnectFailure,
        /// The core error's text.
        detail: String,
    },
    /// The P2P endpoint did not answer in time.
    P2pTimedOut,
    /// The P2P endpoint failed on I/O.
    P2pIo {
        /// The error's text.
        detail: String,
    },
    /// The P2P endpoint cannot make connections (off, closed, in use, set up wrong).
    P2pUnavailable {
        /// Which failure.
        kind: P2pFailureKind,
        /// The core error's text.
        detail: String,
    },
    /// This [`TastyNet`](crate::TastyNet) was made without P2P, the only path of this
    /// version.
    NoPath,
    /// The peer device ID is not a valid device ID.
    InvalidDeviceId {
        /// The parse error's text.
        detail: String,
    },
    /// The object was closed before.
    AlreadyClosed,
    /// Any other I/O failure of the handshake.
    Io {
        /// The error's text.
        detail: String,
    },
    /// A failure this version does not know.
    Other {
        /// The error's text.
        detail: String,
    },
}

impl From<&ConnectError> for HandshakeFailure {
    fn from(error: &ConnectError) -> Self {
        match error {
            ConnectError::Cancelled => Self::Cancelled,
            ConnectError::Io(e) => Self::ConnectIo {
                detail: e.to_string(),
            },
            ConnectError::PeerTooOld { evidence } => Self::PeerTooOld {
                evidence: (*evidence).into(),
            },
            ConnectError::SelfAttach => Self::SelfAttach,
            ConnectError::P2p(e) => Self::from(e),
            other => Self::Other {
                detail: other.to_string(),
            },
        }
    }
}

impl From<&P2pError> for HandshakeFailure {
    fn from(error: &P2pError) -> Self {
        match error {
            P2pError::Connect(e) => Self::P2pConnect {
                failure: e.into(),
                detail: e.to_string(),
            },
            P2pError::TimedOut => Self::P2pTimedOut,
            P2pError::Io(e) => Self::P2pIo {
                detail: e.to_string(),
            },
            P2pError::Disabled
            | P2pError::Settings(_)
            | P2pError::NoServer
            | P2pError::EndpointInUse
            | P2pError::InAsyncContext
            | P2pError::Closed
            | P2pError::Tls(_) => Self::P2pUnavailable {
                kind: error.into(),
                detail: error.to_string(),
            },
        }
    }
}

impl From<HandshakeError> for HandshakeFailure {
    fn from(error: HandshakeError) -> Self {
        Self::from(&error)
    }
}

impl From<&HandshakeError> for HandshakeFailure {
    fn from(error: &HandshakeError) -> Self {
        match error {
            HandshakeError::Rejected { reason, error, ext } => Self::Rejected {
                reason: reason.into(),
                peer_text: error.clone(),
                ext: ext.clone(),
            },
            HandshakeError::Refused(reason) => Self::Refused {
                reason: reason.into(),
            },
            HandshakeError::PeerTooOld { evidence } => Self::PeerTooOld {
                evidence: (*evidence).into(),
            },
            HandshakeError::SignatureInvalid => Self::SignatureInvalid,
            HandshakeError::SelfAttach => Self::SelfAttach,
            HandshakeError::PeerKeyMismatch { expected, got } => Self::PeerKeyMismatch {
                expected: expected.to_string(),
                got: got.to_string(),
            },
            HandshakeError::PeerMismatch => Self::PeerMismatch,
            HandshakeError::Protocol(what) => Self::Protocol {
                detail: (*what).to_string(),
            },
            HandshakeError::TimedOut => Self::TimedOut,
            HandshakeError::Cancelled => Self::Cancelled,
            HandshakeError::PeerClosed => Self::PeerClosed,
            HandshakeError::InvalidTimeouts => Self::InvalidTimeouts,
            HandshakeError::MessageTooLong => Self::MessageTooLong,
            HandshakeError::Closed(reason) => Self::Closed {
                reason: (*reason).into(),
            },
            HandshakeError::NoCredential => Self::NoCredential,
            HandshakeError::NotBulkCapable => Self::NotBulkCapable,
            HandshakeError::ParentEnded => Self::ParentEnded,
            HandshakeError::Store(e) => Self::Store {
                detail: store_text(e),
            },
            HandshakeError::Connect(e) => Self::from(e),
            HandshakeError::Io(e) => Self::Io {
                detail: e.to_string(),
            },
            other => Self::Other {
                detail: other.to_string(),
            },
        }
    }
}

/// The app's own error text inside a store failure (the core `Display` is one fixed sentence).
pub(crate) fn store_text(error: &tasty_net_identity::StoreError) -> String {
    use std::error::Error as _;
    match error.source() {
        Some(inner) => inner.to_string(),
        None => error.to_string(),
    }
}

impl HandshakeFailure {
    /// The failure with every text a peer can fill scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        match self {
            Self::Rejected {
                reason,
                peer_text,
                ext,
            } => Self::Rejected {
                reason: reason.scrubbed(secrets),
                peer_text: secrets.scrub(&peer_text),
                ext: secrets.scrub_opt(ext),
            },
            Self::Refused { reason } => Self::Refused {
                reason: reason.scrubbed(secrets),
            },
            Self::Protocol { detail } => Self::Protocol {
                detail: secrets.scrub(&detail),
            },
            Self::Other { detail } => Self::Other {
                detail: secrets.scrub(&detail),
            },
            other => other,
        }
    }
}

#[uniffi::export]
impl HandshakeFailure {
    /// Whether connecting again can give another result, as the core's `is_retryable` says
    /// for the error this one came from.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Rejected { reason, .. } => reason.core().is_retryable(),
            Self::TimedOut
            | Self::PeerClosed
            | Self::Io { .. }
            | Self::ConnectIo { .. }
            | Self::P2pTimedOut
            | Self::P2pIo { .. } => true,
            Self::Closed { reason } => reason.core().is_retryable(),
            Self::P2pConnect { failure, .. } => failure.is_retryable(),
            _ => false,
        }
    }
}

impl fmt::Display for HandshakeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { reason, .. } => {
                write!(f, "the peer refused the handshake ({})", reason.core())
            }
            Self::Refused { reason } => {
                write!(f, "the handshake was refused here ({})", reason.core())
            }
            Self::PeerTooOld { evidence } => {
                let core = match evidence {
                    TooOld::PortOnlyDiscovery => TooOldEvidence::PortOnlyDiscovery,
                    TooOld::V1Ack => TooOldEvidence::V1Ack,
                };
                write!(f, "{}", HandshakeError::PeerTooOld { evidence: core })
            }
            Self::SignatureInvalid => write!(f, "{}", HandshakeError::SignatureInvalid),
            Self::SelfAttach => write!(f, "{}", HandshakeError::SelfAttach),
            Self::PeerKeyMismatch { expected, got } => write!(
                f,
                "the peer's device key {got} differs from the recorded {expected}"
            ),
            Self::PeerMismatch => write!(f, "{}", HandshakeError::PeerMismatch),
            Self::Protocol { detail } => write!(f, "the peer broke the handshake: {detail}"),
            Self::TimedOut => write!(f, "{}", HandshakeError::TimedOut),
            Self::Cancelled => write!(f, "{}", HandshakeError::Cancelled),
            Self::PeerClosed => write!(f, "{}", HandshakeError::PeerClosed),
            Self::InvalidTimeouts => write!(f, "{}", HandshakeError::InvalidTimeouts),
            Self::MessageTooLong => write!(f, "{}", HandshakeError::MessageTooLong),
            Self::Closed { reason } => write!(f, "{}", HandshakeError::Closed(reason.core())),
            Self::NoCredential => write!(f, "{}", HandshakeError::NoCredential),
            Self::NotBulkCapable => write!(f, "{}", HandshakeError::NotBulkCapable),
            Self::ParentEnded => write!(f, "{}", HandshakeError::ParentEnded),
            Self::Store { .. } => write!(
                f,
                "{}",
                HandshakeError::Store(tasty_net_identity::StoreError::new(String::new()))
            ),
            Self::ConnectIo { .. } => write!(f, "{}", ConnectError::Io(io::Error::other(""))),
            // These come only from `ConnectError::P2p` (`open_attach`, `open_request`), whose
            // sentence is fixed whatever the P2P error; the kind and the core text stay in
            // the fields.
            Self::P2pConnect { .. }
            | Self::P2pTimedOut
            | Self::P2pIo { .. }
            | Self::P2pUnavailable { .. } => {
                write!(f, "{}", ConnectError::P2p(P2pError::TimedOut))
            }
            Self::NoPath => f.write_str("no transport is set up to reach the peer"),
            Self::InvalidDeviceId { detail } => write!(f, "the device ID is not valid: {detail}"),
            Self::AlreadyClosed => f.write_str("the object was closed"),
            Self::Io { .. } => write!(f, "{}", HandshakeError::Io(io::Error::other(""))),
            Self::Other { detail } => f.write_str(detail),
        }
    }
}

impl std::error::Error for HandshakeFailure {}

/// How a channel's reading ended (`ReadFailure`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ReadEnd {
    /// No frame within the read timeout: the peer is gone without closing.
    HeartbeatTimeout,
    /// The peer closed its writing direction.
    PeerClosed,
    /// The bytes are not a frame.
    Protocol,
    /// Another I/O failure.
    Other,
}

impl ReadEnd {
    fn core(self) -> ReadFailure {
        match self {
            Self::HeartbeatTimeout => ReadFailure::HeartbeatTimeout,
            Self::PeerClosed => ReadFailure::PeerClosed,
            Self::Protocol => ReadFailure::Protocol,
            Self::Other => ReadFailure::Other,
        }
    }
}

impl From<ReadFailure> for ReadEnd {
    fn from(failure: ReadFailure) -> Self {
        match failure {
            ReadFailure::HeartbeatTimeout => Self::HeartbeatTimeout,
            ReadFailure::PeerClosed => Self::PeerClosed,
            ReadFailure::Protocol => Self::Protocol,
            ReadFailure::Other => Self::Other,
        }
    }
}

/// Why a frame could not be read or written on a channel.
#[uniffi::export(Display)]
#[derive(Debug, uniffi::Error)]
pub enum ChannelFailure {
    /// Reading ended (`ReadError`).
    Read {
        /// How.
        end: ReadEnd,
        /// The core error's text.
        detail: String,
    },
    /// The frame could not be written.
    Write {
        /// The I/O error's text.
        detail: String,
    },
    /// The channel (its reading or writing half) was finished or closed before.
    AlreadyClosed,
}

impl From<&ReadError> for ChannelFailure {
    fn from(error: &ReadError) -> Self {
        Self::Read {
            end: error.failure().into(),
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for ChannelFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { detail, .. } => f.write_str(detail),
            Self::Write { .. } => f.write_str("the frame could not be written"),
            Self::AlreadyClosed => f.write_str("the channel was closed"),
        }
    }
}

impl std::error::Error for ChannelFailure {}

/// Why a request call gave no result (`RequestError`).
#[uniffi::export(Display)]
#[derive(Debug, uniffi::Error)]
pub enum RequestFailure {
    /// The peer answered with `{id, error}`; this is the `error` value as JSON text.
    Remote {
        /// The `error` value.
        error_json: String,
    },
    /// No answer within the call's timeout. The channel stays usable.
    TimedOut,
    /// The channel ended before the answer.
    Closed {
        /// How the reading ended.
        end: ReadEnd,
    },
    /// The request could not be written.
    Io {
        /// The I/O error's text.
        detail: String,
    },
    /// The peer's answer was not a valid answer.
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The peer's answer did not fit in one frame (`net.response_too_large`).
    ResponseTooLarge,
    /// The parameters are not JSON text.
    InvalidParams {
        /// The parse error's text.
        detail: String,
    },
    /// The request channel was closed before.
    AlreadyClosed,
    /// A failure this version does not know.
    Other {
        /// The error's text.
        detail: String,
    },
}

impl From<&RequestError> for RequestFailure {
    fn from(error: &RequestError) -> Self {
        match error {
            RequestError::Remote(value) => Self::Remote {
                error_json: value.to_string(),
            },
            RequestError::TimedOut => Self::TimedOut,
            RequestError::Closed(failure) => Self::Closed {
                end: (*failure).into(),
            },
            RequestError::Io(e) => Self::Io {
                detail: e.to_string(),
            },
            RequestError::Protocol(what) => Self::Protocol {
                detail: (*what).to_string(),
            },
            RequestError::ResponseTooLarge => Self::ResponseTooLarge,
            other => Self::Other {
                detail: other.to_string(),
            },
        }
    }
}

impl RequestFailure {
    /// The failure with the peer's `error` value and texts scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        match self {
            Self::Remote { error_json } => Self::Remote {
                error_json: secrets.scrub(&error_json),
            },
            Self::Protocol { detail } => Self::Protocol {
                detail: secrets.scrub(&detail),
            },
            Self::Other { detail } => Self::Other {
                detail: secrets.scrub(&detail),
            },
            other => other,
        }
    }
}

impl fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote { .. } => write!(f, "{}", RequestError::Remote(serde_json::Value::Null)),
            Self::TimedOut => write!(f, "{}", RequestError::TimedOut),
            Self::Closed { end } => write!(f, "{}", RequestError::Closed(end.core())),
            Self::Io { .. } => write!(f, "{}", RequestError::Io(io::Error::other(""))),
            Self::Protocol { detail } => {
                write!(f, "the peer's answer is not a valid answer: {detail}")
            }
            Self::ResponseTooLarge => write!(f, "{}", RequestError::ResponseTooLarge),
            Self::InvalidParams { detail } => write!(f, "the parameters are not JSON: {detail}"),
            Self::AlreadyClosed => f.write_str("the request channel was closed"),
            Self::Other { detail } => f.write_str(detail),
        }
    }
}

impl std::error::Error for RequestFailure {}

/// What an identity failure was (`IdentityError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum IdentityFailureKind {
    /// A file system call failed.
    Io,
    /// The device key file does not have the documented format; it is left untouched.
    Corrupt,
    /// An existing secret file or directory is not private to the current user.
    UnsafeFile,
    /// Restricting a new secret file to the current user failed.
    Protect,
    /// The OS random source failed.
    Random,
    /// A file could not be removed and may still hold the secret.
    Leftover,
    /// Another failure.
    Other,
}

/// Why [`TastyNet::new`](crate::TastyNet::new) or a setting failed.
#[uniffi::export(Display)]
#[derive(Debug, uniffi::Error)]
pub enum SetupFailure {
    /// The device key could not be made or read in the key directory.
    Identity {
        /// What failed.
        kind: IdentityFailureKind,
        /// The core error's text.
        detail: String,
    },
    /// The P2P endpoint could not be opened.
    P2p {
        /// Which failure (`EndpointInUse` when this device key already has an endpoint).
        kind: P2pFailureKind,
        /// The core error's text.
        detail: String,
    },
    /// The rendezvous server address is not `host:port` with an IP host.
    InvalidAddress {
        /// The parse error's text.
        detail: String,
    },
    /// A device ID (the server key or a paired device) is not valid.
    InvalidDeviceId {
        /// The parse error's text.
        detail: String,
    },
    /// A setting was refused: unknown key, wrong type or out of range.
    InvalidSetting {
        /// Why.
        detail: String,
    },
    /// The instance ID is empty; a peer refuses a handshake whose `instance_id` is empty
    /// (`malformed`).
    InvalidInstanceId,
    /// A capability name is not valid.
    InvalidCapabilities,
}

impl From<&IdentityError> for SetupFailure {
    fn from(error: &IdentityError) -> Self {
        let kind = match error {
            IdentityError::Io { .. } => IdentityFailureKind::Io,
            IdentityError::Corrupt { .. } => IdentityFailureKind::Corrupt,
            IdentityError::UnsafeFile { .. } => IdentityFailureKind::UnsafeFile,
            IdentityError::Protect { .. } => IdentityFailureKind::Protect,
            IdentityError::Random(_) => IdentityFailureKind::Random,
            IdentityError::Leftover { .. } => IdentityFailureKind::Leftover,
            _ => IdentityFailureKind::Other,
        };
        Self::Identity {
            kind,
            detail: error.to_string(),
        }
    }
}

impl From<&P2pError> for SetupFailure {
    fn from(error: &P2pError) -> Self {
        Self::P2p {
            kind: error.into(),
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for SetupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity { detail, .. } | Self::P2p { detail, .. } => f.write_str(detail),
            Self::InvalidAddress { detail } => {
                write!(f, "the server address is not valid: {detail}")
            }
            Self::InvalidDeviceId { detail } => write!(f, "the device ID is not valid: {detail}"),
            Self::InvalidSetting { detail } => write!(f, "the setting was refused: {detail}"),
            Self::InvalidInstanceId => f.write_str("the instance ID is empty"),
            Self::InvalidCapabilities => f.write_str("a capability name is not valid"),
        }
    }
}

impl std::error::Error for SetupFailure {}
