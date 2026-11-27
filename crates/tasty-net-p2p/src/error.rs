//! Errors (docs/design/p2p.md "재시도와 포기", docs/design/crates.md "오류 규칙").

use std::fmt;
use std::io;

use tasty_net_rendezvous::wire::message::CancelReason;

use crate::settings::SettingsError;

/// Why the endpoint stopped and does not reconnect (`Stopped{cause}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StopCause {
    /// The same device registered again on another connection (server code 2).
    Superseded,
    /// The server's access policy refused this device (server code 1).
    AccessDenied,
    /// The server does not know `hello.proto` (server code 3).
    UnsupportedProto,
    /// The server does not speak `tasty-rendezvous/1` (TLS `no_application_protocol`).
    AlpnMismatch,
    /// The server's raw public key is not the configured one.
    ServerKeyMismatch,
    /// `rendezvous.reconnect.max_attempts` attempts failed.
    ReconnectExhausted,
}

impl StopCause {
    /// The cause name of the design tables.
    pub fn name(self) -> &'static str {
        match self {
            StopCause::Superseded => "superseded",
            StopCause::AccessDenied => "access_denied",
            StopCause::UnsupportedProto => "unsupported_proto",
            StopCause::AlpnMismatch => "alpn_mismatch",
            StopCause::ServerKeyMismatch => "server_key_mismatch",
            StopCause::ReconnectExhausted => "reconnect_exhausted",
        }
    }
}

impl fmt::Display for StopCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a connection attempt ended without a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum P2pConnectError {
    /// P2P is off on this device. Nothing was sent.
    Disabled,
    /// The endpoint is not registered on the server (`Connecting`, `Backoff` or `Stopped`; for
    /// `Stopped` the cause).
    RendezvousUnavailable {
        /// The stop cause when the endpoint stopped.
        stopped: Option<StopCause>,
    },
    /// The coordination connection was lost before `punch_go`.
    RendezvousLost,
    /// No server answer within `punch.request_timeout`.
    RequestTimeout,
    /// The server has no such peer (off, on another server, no answer).
    PeerNotFound,
    /// The peer does not have this device on its pairing list.
    Declined,
    /// Both devices are `dependent` and host candidates did not reach (or there are none).
    NatUnsupported,
    /// The server cancelled the punch in progress (`peer_gone`, `expired`, `superseded`).
    PunchCancelled {
        /// The server's reason.
        reason: CancelReason,
    },
    /// The peer refused the server's limits as not fitting its settings.
    PeerLimitsInvalid,
    /// No punch attempt succeeded in time.
    PunchFailed,
    /// A server limit (concurrent punches and the like).
    Busy,
    /// A server value does not fit this device's settings (`welcome.limits` or `punch_go`).
    ServerLimitsInvalid {
        /// The setting it does not fit.
        key: &'static str,
    },
    /// The QUIC peer's key is not the requested device ID.
    PeerIdentityMismatch,
    /// The requested device ID is this device's.
    SelfAttach,
    /// The peer speaks another ALPN (`proto`).
    AlpnMismatch,
    /// The caller cancelled the attempt.
    Cancelled,
}

impl P2pConnectError {
    /// Whether a new attempt may succeed (docs/design/p2p.md "재시도와 포기" table).
    pub fn is_retryable(&self) -> bool {
        match self {
            P2pConnectError::RendezvousUnavailable { stopped } => stopped.is_none(),
            P2pConnectError::RendezvousLost
            | P2pConnectError::RequestTimeout
            | P2pConnectError::PeerNotFound
            | P2pConnectError::PunchCancelled { .. }
            | P2pConnectError::Busy => true,
            P2pConnectError::Disabled
            | P2pConnectError::Declined
            | P2pConnectError::NatUnsupported
            | P2pConnectError::PeerLimitsInvalid
            | P2pConnectError::PunchFailed
            | P2pConnectError::ServerLimitsInvalid { .. }
            | P2pConnectError::PeerIdentityMismatch
            | P2pConnectError::SelfAttach
            | P2pConnectError::AlpnMismatch
            | P2pConnectError::Cancelled => false,
        }
    }
}

impl fmt::Display for P2pConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            P2pConnectError::Disabled => write!(f, "P2P is off on this device"),
            P2pConnectError::RendezvousUnavailable { stopped: None } => {
                write!(f, "the device is not registered on the rendezvous server")
            }
            P2pConnectError::RendezvousUnavailable { stopped: Some(c) } => {
                write!(f, "the rendezvous connection stopped ({c})")
            }
            P2pConnectError::RendezvousLost => {
                write!(
                    f,
                    "the rendezvous connection was lost before the punch started"
                )
            }
            P2pConnectError::RequestTimeout => {
                write!(
                    f,
                    "the rendezvous server did not answer the punch request in time"
                )
            }
            P2pConnectError::PeerNotFound => {
                write!(f, "the peer is not on the rendezvous server")
            }
            P2pConnectError::Declined => write!(f, "the peer did not pair with this device"),
            P2pConnectError::NatUnsupported => {
                write!(
                    f,
                    "both NATs are address dependent and no host path reached"
                )
            }
            P2pConnectError::PunchCancelled { reason } => {
                write!(f, "the rendezvous server cancelled the punch ({reason:?})")
            }
            P2pConnectError::PeerLimitsInvalid => {
                write!(f, "the peer refused the rendezvous server's limits")
            }
            P2pConnectError::PunchFailed => write!(f, "hole punching did not succeed in time"),
            P2pConnectError::Busy => write!(f, "the rendezvous server is busy"),
            P2pConnectError::ServerLimitsInvalid { key } => {
                write!(
                    f,
                    "a rendezvous server value does not fit the setting {key}"
                )
            }
            P2pConnectError::PeerIdentityMismatch => {
                write!(f, "the peer's key is not the requested device")
            }
            P2pConnectError::SelfAttach => write!(f, "the requested device is this device"),
            P2pConnectError::AlpnMismatch => write!(f, "the peer speaks another protocol version"),
            P2pConnectError::Cancelled => write!(f, "the connection attempt was cancelled"),
        }
    }
}

impl std::error::Error for P2pConnectError {}

/// An error of the endpoint API.
#[derive(Debug)]
pub enum P2pError {
    /// P2P is off on this device (`accept` on a disabled endpoint).
    Disabled,
    /// A connection attempt failed.
    Connect(P2pConnectError),
    /// The settings are not valid. Nothing was opened.
    Settings(SettingsError),
    /// P2P is on but no rendezvous server is given.
    NoServer,
    /// Another P2P endpoint that is on uses this device key: in this process, or in another
    /// one through the key directory's lock.
    EndpointInUse,
    /// The blocking API was called inside an async task. Call it from a blocking thread (for
    /// example `spawn_blocking`).
    InAsyncContext,
    /// The endpoint is closed.
    Closed,
    /// No connection came within the given time.
    TimedOut,
    /// The socket, the runtime or the lock file failed.
    Io(io::Error),
    /// The TLS configuration could not be made from the device key.
    Tls(tasty_net_rendezvous::tls::TlsError),
}

impl P2pError {
    /// The connection attempt error, if this is one.
    pub fn connect_error(&self) -> Option<&P2pConnectError> {
        match self {
            P2pError::Connect(e) => Some(e),
            _ => None,
        }
    }
}

impl fmt::Display for P2pError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            P2pError::Disabled => write!(f, "P2P is off on this device"),
            P2pError::Connect(e) => write!(f, "P2P connection failed: {e}"),
            P2pError::Settings(e) => write!(f, "P2P settings are invalid: {e}"),
            P2pError::NoServer => write!(f, "P2P is on but no rendezvous server is set"),
            P2pError::EndpointInUse => {
                write!(f, "another P2P endpoint uses this device key")
            }
            P2pError::InAsyncContext => {
                write!(f, "the blocking P2P API was called inside an async task")
            }
            P2pError::Closed => write!(f, "the P2P endpoint is closed"),
            P2pError::TimedOut => write!(f, "no P2P connection came in time"),
            P2pError::Io(e) => write!(f, "P2P I/O failed: {e}"),
            P2pError::Tls(e) => write!(f, "P2P TLS could not be set up: {e}"),
        }
    }
}

impl std::error::Error for P2pError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            P2pError::Connect(e) => Some(e),
            P2pError::Settings(e) => Some(e),
            P2pError::Io(e) => Some(e),
            P2pError::Tls(e) => Some(e),
            _ => None,
        }
    }
}

impl From<P2pConnectError> for P2pError {
    fn from(e: P2pConnectError) -> Self {
        P2pError::Connect(e)
    }
}
