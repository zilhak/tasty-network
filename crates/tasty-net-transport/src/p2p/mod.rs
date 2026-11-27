//! The P2P adapter: channels over the QUIC connections of `tasty-net-p2p`
//! (docs/design/transport.md "채널 대응", "P2P의 닫기 범위", docs/design/p2p.md "스트림과 연결
//! (어댑터와의 경계)").
//!
//! `tasty-net-p2p` gives connections and byte streams; this module turns each bidirectional
//! stream into a [`RawChannel`] and keeps the rules the transport contract adds on top:
//!
//! - [`P2pTransport`] dials a device: it reuses the shared QUIC connection to that device, or
//!   punches a new one. A new connection belongs to the attempt that made it (its
//!   [`AttemptCancel`] closes it) until that attempt's first handshake succeeds; then it is
//!   shared ([`EndpointHandle::publish`]). More channels to the same device (bulk, request)
//!   are new streams on the same connection.
//! - [`P2pListener`] takes the connections the endpoint accepts and the streams their dialers
//!   open. Streams are opened only by the side that opened the connection; a stream the other
//!   side opens is refused.
//! - A handshake refusal, a request channel's end, a link's end and a forced close close their
//!   streams only. The dialer closes the connection with code 0 after its last channel; the
//!   accepting side closes it after the drain cap when the dialer did not.

mod conn;
mod listener;
mod stream;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{P2pCancel, P2pConnectError, P2pEndpoint, P2pError};

use crate::cancel::{AttemptCancel, Cancelled, Registration};
use crate::channel::{DEFAULT_DRAIN_CAP, RawChannel};
use crate::lock::lock;
use crate::transport::{
    ConnectError, ConnectOptions, Connection, DialAuth, Endpoint, EndpointHandle, Transport,
};

pub use listener::{P2pInbound, P2pListener};

use conn::{ConnSide, ConnState, OwnedConn, PunchCancel, watch};

/// How often a connection's thread looks at its connection when the endpoint does not show
/// `path.poll_interval` (its default there).
const FALLBACK_POLL: Duration = Duration::from_secs(1);

/// The interval a connection's thread wakes at: the endpoint's `path.poll_interval`, the
/// interval it reads the peer address at too.
pub(crate) fn poll_interval(endpoint: &P2pEndpoint) -> Duration {
    endpoint
        .settings()
        .into_iter()
        .find_map(|entry| match (entry.key, entry.value) {
            ("path.poll_interval", Value::Duration(d)) if !d.is_zero() => Some(d),
            _ => None,
        })
        .unwrap_or(FALLBACK_POLL)
}

/// The shared connections, by peer device.
type Shared = Arc<Mutex<HashMap<DeviceId, Weak<ConnState>>>>;

/// The transport over P2P: the target is the peer's device ID.
///
/// The peer must be on this device's pairing list and registered on the same rendezvous
/// server. Connecting to this device's own ID fails with [`ConnectError::SelfAttach`] before
/// anything is sent.
pub struct P2pTransport {
    endpoint: P2pEndpoint,
    shared: Shared,
    poll: Duration,
}

impl fmt::Debug for P2pTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pTransport")
            .field("device", &self.endpoint.id())
            .field("shared", &lock(&self.shared).len())
            .finish_non_exhaustive()
    }
}

impl P2pTransport {
    /// A transport that dials over `endpoint`.
    pub fn new(endpoint: P2pEndpoint) -> Self {
        let poll = poll_interval(&endpoint);
        Self {
            endpoint,
            shared: Arc::default(),
            poll,
        }
    }

    /// The endpoint it dials over.
    pub fn endpoint(&self) -> &P2pEndpoint {
        &self.endpoint
    }

    /// How many shared connections are open.
    pub fn live_connections(&self) -> usize {
        lock(&self.shared)
            .values()
            .filter(|c| c.upgrade().is_some_and(|c| c.is_usable()))
            .count()
    }

    /// The shared connection to `peer` that may take a new channel.
    fn shared_conn(&self, peer: &DeviceId) -> Option<Arc<ConnState>> {
        let mut shared = lock(&self.shared);
        shared.retain(|_, c| c.upgrade().is_some_and(|c| c.is_usable()));
        shared.get(peer).and_then(Weak::upgrade)
    }

    /// Punches a new connection to `peer`, registered with `cancel` until it is shared.
    fn new_conn(
        &self,
        peer: DeviceId,
        cancel: &AttemptCancel,
    ) -> Result<(Arc<ConnState>, Registration), ConnectError> {
        let punch = P2pCancel::new();
        let punching = cancel
            .register(Arc::new(PunchCancel(punch.clone())))
            .map_err(|_| ConnectError::Cancelled)?;
        let connected = self.endpoint.connect_with_cancel(peer, &punch);
        drop(punching);
        let conn = connected.map_err(|e| connect_error(e, cancel))?;
        let state = ConnState::new(conn, ConnSide::Dialer, DEFAULT_DRAIN_CAP);
        // A cancel that came meanwhile closes the connection here.
        let owned = cancel
            .register(Arc::new(OwnedConn(Arc::clone(&state))))
            .map_err(|_| ConnectError::Cancelled)?;
        watch(Arc::clone(&state), None, self.poll);
        Ok((state, owned))
    }
}

/// The error of a failed punch.
fn connect_error(error: P2pError, cancel: &AttemptCancel) -> ConnectError {
    match error.connect_error() {
        _ if cancel.is_cancelled() => ConnectError::Cancelled,
        Some(P2pConnectError::Cancelled) => ConnectError::Cancelled,
        Some(P2pConnectError::SelfAttach) => ConnectError::SelfAttach,
        _ => ConnectError::P2p(error),
    }
}

/// Opens a stream on `state` as a new channel.
fn open_channel(
    state: &Arc<ConnState>,
    cancel: &AttemptCancel,
) -> Result<RawChannel, ConnectError> {
    if cancel.is_cancelled() {
        return Err(ConnectError::Cancelled);
    }
    let Some(slot) = state.acquire() else {
        return Err(ConnectError::Io(io::Error::new(
            io::ErrorKind::NotConnected,
            "the P2P connection is closed",
        )));
    };
    let stream = state.conn.open_bi().map_err(ConnectError::Io)?;
    Ok(stream::channel(
        stream,
        &state.conn,
        state.side,
        state.drain_cap,
        slot,
    ))
}

impl Transport for P2pTransport {
    type Target = DeviceId;

    fn connect(
        &self,
        target: &DeviceId,
        cancel: &AttemptCancel,
    ) -> Result<Connection, ConnectError> {
        self.connect_with(target, cancel, &ConnectOptions::new())
    }

    fn connect_with(
        &self,
        target: &DeviceId,
        cancel: &AttemptCancel,
        options: &ConnectOptions,
    ) -> Result<Connection, ConnectError> {
        let _unused = options;
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        if *target == self.endpoint.id() {
            return Err(ConnectError::SelfAttach);
        }
        // A shared connection that closes meanwhile is passed over for a new one.
        let reused = self
            .shared_conn(target)
            .and_then(|state| Some((open_channel(&state, cancel).ok()?, state)));
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let (channel, state, owned) = match reused {
            Some((channel, state)) => (channel, state, None),
            None => {
                let (state, owned) = self.new_conn(*target, cancel)?;
                (open_channel(&state, cancel)?, state, Some(owned))
            }
        };
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(P2pEndpointHandle {
                state,
                owned: Mutex::new(owned),
                shared: Arc::clone(&self.shared),
            })),
        })
    }
}

/// The endpoint of a P2P connection: more channels are more streams on it.
struct P2pEndpointHandle {
    state: Arc<ConnState>,
    /// The attempt's registration while the connection is not shared yet.
    owned: Mutex<Option<Registration>>,
    shared: Shared,
}

impl EndpointHandle for P2pEndpointHandle {
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        open_channel(&self.state, cancel)
    }

    fn auth(&self) -> Option<DialAuth> {
        Some(DialAuth::P2p {
            remote_id: self.state.conn.remote_device_id(),
        })
    }

    fn publish(&self) -> Result<(), Cancelled> {
        let Some(owned) = lock(&self.owned).take() else {
            return Ok(());
        };
        owned.release()?;
        lock(&self.shared).insert(
            self.state.conn.remote_device_id(),
            Arc::downgrade(&self.state),
        );
        Ok(())
    }
}
