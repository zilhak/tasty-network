//! Rejection reasons, QUIC close reasons and handshake errors (docs/design/handshake.md
//! "거절 사유", "QUIC 종료 코드").

use std::error::Error;
use std::fmt;
use std::io;

use tasty_net_identity::{DeviceId, StoreError};
/// Why a whole P2P connection was closed: the QUIC close codes are `tasty-net-p2p`'s wire
/// values, so the enum is defined there and shown here again (docs/design/handshake.md "QUIC
/// 종료 코드").
pub use tasty_net_p2p::P2pCloseReason;
#[cfg(feature = "ssh")]
use tasty_net_ssh::{ChannelOpenRefused, RemoteListenerAbsent};

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
    #[cfg(feature = "ssh")]
    RemoteListenerAbsent(RemoteListenerAbsent),
    /// The channel through the SSH tunnel dropped because the SSH server refused to open it
    /// with a reason other than a failed connect (the embedded backend sees the code). The
    /// session stays up. Whether connecting again can give another result depends on the code
    /// ([`ChannelOpenRefused::is_retryable`]).
    #[cfg(feature = "ssh")]
    ChannelOpenRefused(ChannelOpenRefused),
    /// A timeout of the [`LinkTimeouts`](crate::LinkTimeouts) is too large to add to the
    /// current time (such as [`std::time::Duration::MAX`]): a configuration error.
    InvalidTimeouts,
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
    /// peer, a plain QUIC close, an I/O failure or a tunnel channel refused for a passing
    /// reason. Refusals for authentication, version and role, cancellation, a TOFU mismatch
    /// and a tunnel channel the server's configuration refuses are not retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Rejected { reason, .. } => reason.is_retryable(),
            Self::TimedOut | Self::PeerClosed | Self::Io(_) => true,
            #[cfg(feature = "ssh")]
            Self::RemoteListenerAbsent(_) => true,
            #[cfg(feature = "ssh")]
            Self::ChannelOpenRefused(refused) => refused.is_retryable(),
            Self::Closed(reason) => reason.is_retryable(),
            Self::Connect(e) => e.is_retryable(),
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
            #[cfg(feature = "ssh")]
            Self::RemoteListenerAbsent(_) => {
                f.write_str("nothing listens on the remote port behind the SSH tunnel")
            }
            #[cfg(feature = "ssh")]
            Self::ChannelOpenRefused(refused) => write!(
                f,
                "the SSH server refused to open the tunnel channel (reason code {})",
                refused.code
            ),
            Self::InvalidTimeouts => f.write_str("a link timeout is too large to make a deadline"),
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
            #[cfg(feature = "ssh")]
            Self::RemoteListenerAbsent(e) => Some(e),
            #[cfg(feature = "ssh")]
            Self::ChannelOpenRefused(e) => Some(e),
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
            P2pCloseReason::SessionMismatch,
            P2pCloseReason::Unknown(77),
        ] {
            assert_eq!(P2pCloseReason::from_code(r.code()), r);
        }
        assert_eq!(P2pCloseReason::NotPaired.reason(), b"not_paired");
        assert!(P2pCloseReason::Closed.is_retryable());
    }

    /// 2 and 3 are reserved: no reason uses them any more, and a received one reads as an
    /// unknown code, which is not retried.
    #[test]
    fn the_retired_close_codes_read_as_unknown() {
        for code in [2, 3] {
            let reason = P2pCloseReason::from_code(code);
            assert_eq!(reason, P2pCloseReason::Unknown(code));
            assert!(!reason.is_retryable());
            assert!(!HandshakeError::Closed(reason).is_retryable());
        }
    }

    #[test]
    fn a_session_mismatch_is_not_retried() {
        let reason = P2pCloseReason::from_code(4);
        assert_eq!(reason, P2pCloseReason::SessionMismatch);
        assert_eq!(reason.reason(), b"session_mismatch");
        assert!(!reason.is_retryable());
        assert!(!HandshakeError::Closed(reason).is_retryable());
    }

    /// Reason codes 1 and 3 come from the server's configuration and are not retried; 4 and a
    /// code outside RFC 4254 are. The cause keeps one name for every code.
    #[cfg(feature = "ssh")]
    #[test]
    fn a_refused_tunnel_channel_is_retried_only_for_a_passing_reason() {
        use crate::connection_table::Cause;
        for (code, retried) in [(1, false), (3, false), (4, true), (7, true)] {
            let error = HandshakeError::ChannelOpenRefused(ChannelOpenRefused { code });
            assert_eq!(error.is_retryable(), retried, "code {code}");
            let cause = Cause::of_handshake(&error);
            assert_eq!(cause, Cause::ChannelOpenRefused(code));
            assert_eq!(cause.as_str(), "channel_open_refused");
            assert_eq!(cause.is_retryable(), retried, "code {code}");
        }
    }

    #[test]
    fn the_legacy_refusal_sentence_is_the_fixed_one() {
        assert_eq!(
            RejectReason::LegacyProtocol.message(),
            "unsupported stream proto 1 (server speaks 2); update Tasty on the connecting machine"
        );
    }
}
