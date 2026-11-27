//! The QUIC close codes of a device-to-device connection and [`P2pCloseReason`]
//! (docs/design/handshake.md "QUIC 종료 코드").
//!
//! The numbers are this crate's wire values: a connection is closed with
//! `Connection::close(code, reason)` and the reader decides by the code only. 2 and 3 are
//! values of an earlier design and are not used again; they read as `Unknown(2)`·`Unknown(3)`.

use std::fmt;

/// Wire value: normal close (`closed`).
const CODE_CLOSED: u64 = 0;
/// Wire value: the peer is not on the pairing allow list (`not_paired`).
const CODE_NOT_PAIRED: u64 = 1;
/// Wire value: the TLS device ID is not the punch session's peer (`session_mismatch`).
const CODE_SESSION_MISMATCH: u64 = 4;

/// Why a device-to-device QUIC connection was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum P2pCloseReason {
    /// Normal close: the last channel closed, or the endpoint ended (code 0).
    Closed,
    /// The receiving side does not have the peer on its pairing allow list (code 1).
    NotPaired,
    /// The receiving side's TLS-checked device ID is not the peer of the punch session the
    /// connection's address belongs to (code 4, docs/design/p2p.md "펀칭 절차" 9).
    SessionMismatch,
    /// Any other code, including the reserved 2 and 3.
    Unknown(u64),
}

impl P2pCloseReason {
    /// The reason for a received code.
    pub fn from_code(code: u64) -> Self {
        match code {
            CODE_CLOSED => P2pCloseReason::Closed,
            CODE_NOT_PAIRED => P2pCloseReason::NotPaired,
            CODE_SESSION_MISMATCH => P2pCloseReason::SessionMismatch,
            other => P2pCloseReason::Unknown(other),
        }
    }

    /// The code sent in CONNECTION_CLOSE.
    pub fn code(self) -> u64 {
        match self {
            P2pCloseReason::Closed => CODE_CLOSED,
            P2pCloseReason::NotPaired => CODE_NOT_PAIRED,
            P2pCloseReason::SessionMismatch => CODE_SESSION_MISMATCH,
            P2pCloseReason::Unknown(code) => code,
        }
    }

    /// The reason bytes sent with the code: the name, empty for an unknown code.
    pub fn reason(self) -> &'static [u8] {
        match self {
            P2pCloseReason::Unknown(_) => b"",
            known => known.name().as_bytes(),
        }
    }

    /// `closed`, `not_paired`, `session_mismatch` or `unknown`.
    pub fn name(self) -> &'static str {
        match self {
            P2pCloseReason::Closed => "closed",
            P2pCloseReason::NotPaired => "not_paired",
            P2pCloseReason::SessionMismatch => "session_mismatch",
            P2pCloseReason::Unknown(_) => "unknown",
        }
    }

    /// Whether a new connection may succeed: only after a normal close.
    pub fn is_retryable(self) -> bool {
        matches!(self, P2pCloseReason::Closed)
    }

    /// The code as a QUIC `VarInt`. A code past the `VarInt` range (only possible for
    /// [`P2pCloseReason::Unknown`]) is sent as the largest `VarInt`.
    pub fn var_int(self) -> quinn::VarInt {
        quinn::VarInt::from_u64(self.code()).unwrap_or(quinn::VarInt::MAX)
    }
}

impl fmt::Display for P2pCloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            P2pCloseReason::Unknown(code) => write!(f, "unknown close code {code}"),
            known => f.write_str(known.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_follow_the_handshake_table() {
        let table = [
            (0, P2pCloseReason::Closed, "closed", true),
            (1, P2pCloseReason::NotPaired, "not_paired", false),
            (2, P2pCloseReason::Unknown(2), "", false),
            (3, P2pCloseReason::Unknown(3), "", false),
            (
                4,
                P2pCloseReason::SessionMismatch,
                "session_mismatch",
                false,
            ),
            (5, P2pCloseReason::Unknown(5), "", false),
        ];
        for (code, reason, bytes, retry) in table {
            assert_eq!(P2pCloseReason::from_code(code), reason);
            assert_eq!(reason.code(), code);
            assert_eq!(reason.reason(), bytes.as_bytes());
            assert_eq!(reason.is_retryable(), retry);
        }
    }
}
