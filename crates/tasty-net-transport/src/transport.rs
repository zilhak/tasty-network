//! The transport traits: how a channel is opened and accepted (docs/design/transport.md
//! "전송 trait").

use std::error::Error;
use std::fmt;
use std::io;
use std::net::TcpListener;
use std::sync::Arc;

use tasty_net_identity::{ConnectionToken, DeviceId};
use tasty_net_ssh::{PortDiscoveryError, TunnelError};

use crate::cancel::AttemptCancel;
use crate::channel::RawChannel;
use crate::reject::TooOldEvidence;

/// Which transport a channel came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// Through an SSH `-L` tunnel this side opened.
    Ssh,
    /// A TCP connection accepted on a loopback listener. The receiving side of an SSH tunnel
    /// sees its channels this way: it cannot tell the tunnel from a local client.
    Tcp,
    /// A QUIC stream of an iroh connection.
    P2p,
    /// An in-process pipe (tests).
    Memory,
}

/// A way to open channels to a peer.
///
/// [`Transport::connect`] opens the ordered channel of a new link (or a request channel) and
/// returns the [`Endpoint`] it reached, through which more channels to the same peer are
/// opened (the bulk channels of the link).
pub trait Transport: Send + Sync {
    /// What `connect` reaches, such as an SSH target.
    type Target: ?Sized;

    /// Opens a channel to `target`. Sockets, streams and not yet shared tunnels the attempt
    /// creates are registered with `cancel`.
    fn connect(
        &self,
        target: &Self::Target,
        cancel: &AttemptCancel,
    ) -> Result<Connection, ConnectError>;

    /// Opens one more channel to an endpoint this transport reached.
    fn connect_more(
        &self,
        endpoint: &Endpoint,
        cancel: &AttemptCancel,
    ) -> Result<RawChannel, ConnectError> {
        endpoint.connect_more(cancel)
    }
}

/// A new channel and the endpoint it reached.
#[derive(Debug)]
pub struct Connection {
    /// The channel. The handshake runs on it next.
    pub channel: RawChannel,
    /// The endpoint, for more channels to the same peer.
    pub endpoint: Endpoint,
}

/// The transport-specific side of an [`Endpoint`]. A transport implementation provides it.
pub trait EndpointHandle: Send + Sync {
    /// Opens one more channel to the same peer.
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError>;

    /// How a dialer proves itself to this endpoint. `None` means it cannot dial a handshake.
    fn auth(&self) -> Option<DialAuth>;

    /// Called once the first channel to this endpoint finished its handshake: the endpoint
    /// (a new SSH tunnel, a new QUIC connection) may now be shared with other channels and
    /// leaves the attempt that created it.
    fn publish(&self) {}
}

/// What a dialer proves itself with, from the endpoint it reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialAuth {
    /// The SSH path: the connection token from the peer's connection record.
    Token(ConnectionToken),
    /// The P2P path: the peer's device ID, which the QUIC connection already proved.
    P2p {
        /// The connection's remote ID.
        remote_id: DeviceId,
    },
}

/// A peer endpoint a transport reached: the SSH tunnel, the QUIC connection, the memory
/// listener. Clones share it.
#[derive(Clone)]
pub struct Endpoint {
    handle: Arc<dyn EndpointHandle>,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint").finish_non_exhaustive()
    }
}

impl Endpoint {
    /// An endpoint from its transport-specific side.
    pub fn new(handle: Arc<dyn EndpointHandle>) -> Self {
        Self { handle }
    }

    /// Opens one more channel to the same peer.
    pub fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        self.handle.connect_more(cancel)
    }

    /// How a dialer proves itself to this endpoint.
    pub fn auth(&self) -> Option<DialAuth> {
        self.handle.auth()
    }

    pub(crate) fn publish(&self) {
        self.handle.publish();
    }
}

/// Why a channel could not be opened.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectError {
    /// The attempt was cancelled.
    Cancelled,
    /// The socket or stream could not be opened.
    Io(io::Error),
    /// Port discovery over SSH failed; its kind tells why.
    Discovery(PortDiscoveryError),
    /// The SSH tunnel did not come up.
    Tunnel(TunnelError),
    /// The peer is too old for handshake v2 (its record has no token). Do not connect again.
    PeerTooOld {
        /// How it was found out.
        evidence: TooOldEvidence,
    },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("the connection attempt was cancelled"),
            Self::Io(_) => f.write_str("the channel could not be opened"),
            Self::Discovery(_) => f.write_str("the peer's connection record could not be read"),
            Self::Tunnel(_) => f.write_str("the SSH tunnel did not come up"),
            Self::PeerTooOld { .. } => f.write_str(
                "the peer publishes no connection token; it runs a version without handshake v2",
            ),
        }
    }
}

impl Error for ConnectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Discovery(e) => Some(e),
            Self::Tunnel(e) => Some(e),
            Self::Cancelled | Self::PeerTooOld { .. } => None,
        }
    }
}

/// The receiving side of a transport.
pub trait Listener {
    /// Waits for the next channel.
    fn accept(&self) -> Result<RawChannel, AcceptError>;
}

/// A loopback TCP listener of the consumer: each accepted socket becomes a channel
/// ([`RawChannel::from_tcp`]).
impl Listener for TcpListener {
    fn accept(&self) -> Result<RawChannel, AcceptError> {
        let (stream, _) = TcpListener::accept(self).map_err(AcceptError::new)?;
        RawChannel::from_tcp(stream).map_err(AcceptError::new)
    }
}

/// Why no channel could be accepted.
#[derive(Debug)]
pub struct AcceptError(io::Error);

impl AcceptError {
    pub fn new(error: io::Error) -> Self {
        Self(error)
    }
}

impl fmt::Display for AcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no channel could be accepted")
    }
}

impl Error for AcceptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}
