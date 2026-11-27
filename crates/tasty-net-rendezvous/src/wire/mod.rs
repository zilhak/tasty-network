//! The wire format: coordination frames and messages, the STUN subset with the punch probe,
//! the first-byte packet classes and the candidate address rule.
//!
//! This module is the definition of the format for the server and for `tasty-net-p2p`
//! (docs/design/rendezvous.md "후보 주소 규칙", "펀칭 프로브"). Its numbers are format
//! definitions, not behaviour values (docs/design/p2p.md "와이어 형식 상수").

pub mod candidate;
pub mod demux;
pub mod frame;
pub mod message;
pub mod stun;

/// ALPN of the coordination connection to the rendezvous server.
pub const ALPN_RENDEZVOUS: &[u8] = b"tasty-rendezvous/1";

/// ALPN of a direct QUIC connection between two devices.
pub const ALPN_P2P: &[u8] = b"tasty-net/2";

/// The coordination protocol number sent in `hello.proto`.
pub const PROTO: u32 = 1;

/// The QUIC application close codes of the coordination connection
/// (docs/design/rendezvous.md "조정 연결의 QUIC 종료 코드와 TLS 단계 실패").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseCode {
    /// Normal close (server shutdown, the device turned P2P off).
    Closed = 0,
    /// The access policy refused the device.
    AccessDenied = 1,
    /// The same device ID registered again on a new connection.
    Superseded = 2,
    /// The server does not know `hello.proto`.
    UnsupportedProto = 3,
    /// A format error, a limit, a frame deadline or a second stream.
    Malformed = 4,
    /// `max_devices` is reached.
    Busy = 5,
    /// `hello` did not arrive in time.
    HelloTimeout = 6,
}

impl CloseCode {
    /// Every code, in code order.
    pub const ALL: [CloseCode; 7] = [
        CloseCode::Closed,
        CloseCode::AccessDenied,
        CloseCode::Superseded,
        CloseCode::UnsupportedProto,
        CloseCode::Malformed,
        CloseCode::Busy,
        CloseCode::HelloTimeout,
    ];

    /// The numeric code sent in the QUIC CONNECTION_CLOSE frame.
    pub fn code(self) -> u32 {
        self as u32
    }

    /// The code for a received number. Unknown numbers give `None`.
    pub fn from_code(code: u64) -> Option<CloseCode> {
        CloseCode::ALL
            .into_iter()
            .find(|c| u64::from(c.code()) == code)
    }

    /// The protocol name of the code (`access_denied` and so on).
    pub fn name(self) -> &'static str {
        match self {
            CloseCode::Closed => "closed",
            CloseCode::AccessDenied => "access_denied",
            CloseCode::Superseded => "superseded",
            CloseCode::UnsupportedProto => "unsupported_proto",
            CloseCode::Malformed => "malformed",
            CloseCode::Busy => "busy",
            CloseCode::HelloTimeout => "hello_timeout",
        }
    }

    /// The QUIC close code as a `VarInt`.
    pub fn var_int(self) -> quinn::VarInt {
        quinn::VarInt::from_u32(self.code())
    }

    /// The close reason bytes: the protocol name.
    pub fn reason(self) -> &'static [u8] {
        self.name().as_bytes()
    }
}
