//! Rejection reasons, QUIC close reasons and handshake errors (docs/design/handshake.md
//! "거절 사유", "QUIC 종료 코드").

use std::error::Error;
use std::fmt;
use std::io;

use tasty_net_identity::{DeviceId, StoreError};
use tasty_net_ssh::RemoteListenerAbsent;

use crate::transport::ConnectError;

/// Why the receiving end refused a handshake. On the wire it is the snake_case `reason` of
/// a refusing ACK; an unknown value reads as [`RejectReason::Unknown`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// A message is not in the handshake format, or a field has the wrong type.
    Malformed,
    /// The peer speaks v1 (no token).
    LegacyProtocol,
    /// The `proto` value is neither 1 nor 2.
    UnsupportedProtocol,
    /// No token.
    TokenMissing,
    /// The token is wrong (a token that is not 64 hex digits included).
    TokenInvalid,
    /// A signature did not verify.
    SignatureInvalid,
    /// The device key differs from the TOFU record.
    PeerKeyMismatch,
    /// Both ends are the same instance.
    SelfAttach,
    /// The receiving end does not take the requested role (a refused reversal included).
    RoleRefused,
    /// The bulk credential is unknown or revoked.
    BulkCredentialInvalid,
    /// The consumer's domain check refused; the ACK `ext` carries the consumer's code.
    Domain,
    /// The receiving end cannot take the handshake now (too many at once).
    Busy,
    /// A reason this version does not know.
    Unknown(String),
}

impl RejectReason {
    /// The wire form.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Malformed => "malformed",
            Self::LegacyProtocol => "legacy_protocol",
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::TokenMissing => "token_missing",
            Self::TokenInvalid => "token_invalid",
            Self::SignatureInvalid => "signature_invalid",
            Self::PeerKeyMismatch => "peer_key_mismatch",
            Self::SelfAttach => "self_attach",
            Self::RoleRefused => "role_refused",
            Self::BulkCredentialInvalid => "bulk_credential_invalid",
            Self::Domain => "domain",
            Self::Busy => "busy",
            Self::Unknown(other) => other,
        }
    }

    /// The value for a wire form; anything unknown is kept as [`RejectReason::Unknown`].
    pub fn parse(text: &str) -> Self {
        match text {
            "malformed" => Self::Malformed,
            "legacy_protocol" => Self::LegacyProtocol,
            "unsupported_protocol" => Self::UnsupportedProtocol,
            "token_missing" => Self::TokenMissing,
            "token_invalid" => Self::TokenInvalid,
            "signature_invalid" => Self::SignatureInvalid,
            "peer_key_mismatch" => Self::PeerKeyMismatch,
            "self_attach" => Self::SelfAttach,
            "role_refused" => Self::RoleRefused,
            "bulk_credential_invalid" => Self::BulkCredentialInvalid,
            "domain" => Self::Domain,
            "busy" => Self::Busy,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether connecting again can give another result. Only [`RejectReason::Busy`] says
    /// yes. For [`RejectReason::Domain`] the consumer decides from its own code in the ACK
    /// `ext`; this returns false for it.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy)
    }

    /// The fixed English sentence of the ACK `error` field. A v1 client shows it as is.
    pub fn message(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed stream.open handshake",
            Self::LegacyProtocol => crate::wire::LEGACY_PROTOCOL_ERROR,
            Self::UnsupportedProtocol => "unsupported stream proto (server speaks 2)",
            Self::TokenMissing => "stream.open needs a connection token",
            Self::TokenInvalid => "the connection token is not valid",
            Self::SignatureInvalid => "the device signature did not verify",
            Self::PeerKeyMismatch => "the device key differs from the recorded one",
            Self::SelfAttach => "refusing to attach to the same instance",
            Self::RoleRefused => "the requested role was refused",
            Self::BulkCredentialInvalid => "the bulk credential is not valid",
            Self::Domain => "the request was refused",
            Self::Busy => "too many handshakes at once; try again later",
            Self::Unknown(_) => "the handshake was refused",
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a P2P implementation closed a whole QUIC connection. The p2p crate maps the QUIC
/// `error_code` to this value and back ([`P2pCloseReason::code`],
/// [`P2pCloseReason::from_code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum P2pCloseReason {
    /// Code 0 `closed`: the last channel closed, or the endpoint shut down.
    Closed,
    /// Code 1 `not_paired`: the remote ID is not in the pairing list.
    NotPaired,
    /// Code 2 `direct_path_timeout`: no direct path came up in time.
    DirectPathTimeout,
    /// Code 3 `direct_path_lost`: the direct path went away.
    DirectPathLost,
    /// Any other code.
    Unknown(u64),
}

impl P2pCloseReason {
    /// The QUIC `error_code`.
    pub fn code(self) -> u64 {
        match self {
            Self::Closed => 0,
            Self::NotPaired => 1,
            Self::DirectPathTimeout => 2,
            Self::DirectPathLost => 3,
            Self::Unknown(code) => code,
        }
    }

    /// The value for a QUIC `error_code`. The reader decides by the code only.
    pub fn from_code(code: u64) -> Self {
        match code {
            0 => Self::Closed,
            1 => Self::NotPaired,
            2 => Self::DirectPathTimeout,
            3 => Self::DirectPathLost,
            other => Self::Unknown(other),
        }
    }

    /// The ASCII `reason` bytes sent with the code. `Unknown` has none of its own.
    pub fn reason_bytes(self) -> &'static [u8] {
        match self {
            Self::Closed => b"closed",
            Self::NotPaired => b"not_paired",
            Self::DirectPathTimeout => b"direct_path_timeout",
            Self::DirectPathLost => b"direct_path_lost",
            Self::Unknown(_) => b"",
        }
    }

    /// Only a plain close may be connected again.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// How the dialer learned that the peer is too old for v2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TooOldEvidence {
    /// Port discovery returned only a port number: the peer publishes no token.
    PortOnlyDiscovery,
    /// The peer answered with a v1 refusing ACK (`ok: false`, `proto: 1`).
    V1Ack,
}

/// Why a handshake did not give a link.
#[derive(Debug)]
#[non_exhaustive]
pub enum HandshakeError {
    /// The peer refused with a refusing ACK.
    Rejected {
        /// The reason the peer gave.
        reason: RejectReason,
        /// The peer's English sentence.
        error: String,
        /// The consumer's code for [`RejectReason::Domain`].
        ext: Option<String>,
    },
    /// This end refused the peer and sent a refusing ACK with this reason.
    Refused(RejectReason),
    /// The peer speaks only v1. Do not connect again.
    PeerTooOld {
        /// How it was found out.
        evidence: TooOldEvidence,
    },
    /// The peer's signature did not verify (this end closed without sending more).
    SignatureInvalid,
    /// The peer is this instance.
    SelfAttach,
    /// The peer's device ID differs from the TOFU record, which was left unchanged.
    PeerKeyMismatch {
        /// The recorded ID.
        expected: DeviceId,
        /// The ID the peer proved.
        got: DeviceId,
    },
    /// The P2P `hello` named another device than the connection's remote ID.
    PeerMismatch,
    /// The peer sent something that is not the expected handshake message.
    Protocol(&'static str),
    /// The handshake deadline passed.
    TimedOut,
    /// The attempt was cancelled.
    Cancelled,
    /// The peer closed the channel during the handshake.
    PeerClosed,
    /// The channel through the SSH tunnel dropped and ssh reported that nothing listens on
    /// the remote port (the remote instance is not running, or its record is stale).
    RemoteListenerAbsent(RemoteListenerAbsent),
    /// Message 1 would be longer than [`MAX_HANDSHAKE_LINE`](crate::MAX_HANDSHAKE_LINE) (a
    /// long `ext`); nothing was sent.
    MessageTooLong,
    /// The QUIC connection was closed with a code during the handshake.
    Closed(P2pCloseReason),
    /// The endpoint has no credential to dial with (no token and no remote ID).
    NoCredential,
    /// The link has no bulk credential (only an attach link opened as a dialer has one).
    NotBulkCapable,
    /// The parent link ended before the bulk channel was registered under it.
    ParentEnded,
    /// The TOFU store failed.
    Store(StoreError),
    /// Opening the channel failed.
    Connect(ConnectError),
    /// Any other I/O failure, the OS random source included.
    Io(io::Error),
}

impl HandshakeError {
    /// Whether connecting again can give another result: a timeout, a closed channel, a busy
    /// peer, a plain QUIC close or an I/O failure. Refusals for authentication, version and
    /// role, cancellation and a TOFU mismatch are not retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Rejected { reason, .. } => reason.is_retryable(),
            Self::TimedOut | Self::PeerClosed | Self::RemoteListenerAbsent(_) | Self::Io(_) => true,
            Self::Closed(reason) => reason.is_retryable(),
            Self::Connect(e) => {
                !matches!(e, ConnectError::Cancelled | ConnectError::PeerTooOld { .. })
            }
            _ => false,
        }
    }
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { reason, .. } => write!(f, "the peer refused the handshake ({reason})"),
            Self::Refused(reason) => write!(f, "the handshake was refused here ({reason})"),
            Self::PeerTooOld { evidence } => match evidence {
                TooOldEvidence::PortOnlyDiscovery => f.write_str(
                    "the peer publishes no connection token; it runs a version without handshake v2",
                ),
                TooOldEvidence::V1Ack => {
                    f.write_str("the peer answered with handshake v1; it needs an update")
                }
            },
            Self::SignatureInvalid => f.write_str("the peer's device signature did not verify"),
            Self::SelfAttach => f.write_str("the peer is this instance"),
            Self::PeerKeyMismatch { expected, got } => write!(
                f,
                "the peer's device key {got} differs from the recorded {expected}"
            ),
            Self::PeerMismatch => {
                f.write_str("the peer named another device than the connection proved")
            }
            Self::Protocol(what) => write!(f, "the peer broke the handshake: {what}"),
            Self::TimedOut => f.write_str("the handshake timed out"),
            Self::Cancelled => f.write_str("the handshake was cancelled"),
            Self::PeerClosed => f.write_str("the peer closed the channel during the handshake"),
            Self::RemoteListenerAbsent(_) => {
                f.write_str("nothing listens on the remote port behind the SSH tunnel")
            }
            Self::MessageTooLong => {
                f.write_str("message 1 is longer than the handshake line limit")
            }
            Self::Closed(reason) => write!(f, "the P2P connection was closed ({reason:?})"),
            Self::NoCredential => f.write_str("the endpoint has no token or remote ID to dial with"),
            Self::NotBulkCapable => f.write_str("this link cannot open bulk channels"),
            Self::ParentEnded => f.write_str("the parent link ended before the bulk channel opened"),
            Self::Store(_) => f.write_str("the peer key store failed"),
            Self::Connect(_) => f.write_str("the channel could not be opened"),
            Self::Io(_) => f.write_str("an I/O operation of the handshake failed"),
        }
    }
}

impl Error for HandshakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::RemoteListenerAbsent(e) => Some(e),
            Self::Connect(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_and_unknown_is_kept() {
        let all = [
            RejectReason::Malformed,
            RejectReason::LegacyProtocol,
            RejectReason::UnsupportedProtocol,
            RejectReason::TokenMissing,
            RejectReason::TokenInvalid,
            RejectReason::SignatureInvalid,
            RejectReason::PeerKeyMismatch,
            RejectReason::SelfAttach,
            RejectReason::RoleRefused,
            RejectReason::BulkCredentialInvalid,
            RejectReason::Domain,
            RejectReason::Busy,
        ];
        for r in &all {
            assert_eq!(&RejectReason::parse(r.as_str()), r);
            assert_eq!(r.is_retryable(), *r == RejectReason::Busy, "{r}");
        }
        assert_eq!(
            RejectReason::parse("new_reason"),
            RejectReason::Unknown("new_reason".into())
        );
    }

    #[test]
    fn quic_close_codes_round_trip() {
        for r in [
            P2pCloseReason::Closed,
            P2pCloseReason::NotPaired,
            P2pCloseReason::DirectPathTimeout,
            P2pCloseReason::DirectPathLost,
            P2pCloseReason::Unknown(77),
        ] {
            assert_eq!(P2pCloseReason::from_code(r.code()), r);
        }
        assert_eq!(P2pCloseReason::NotPaired.reason_bytes(), b"not_paired");
        assert!(P2pCloseReason::Closed.is_retryable());
        assert!(!P2pCloseReason::DirectPathLost.is_retryable());
    }

    #[test]
    fn the_legacy_refusal_sentence_is_the_fixed_one() {
        assert_eq!(
            RejectReason::LegacyProtocol.message(),
            "unsupported stream proto 1 (server speaks 2); update Tasty on the connecting machine"
        );
    }
}
