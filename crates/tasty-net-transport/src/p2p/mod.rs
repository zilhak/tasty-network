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
//! - With a [`ConnectionTable`] every QUIC connection is one `p2p_udp` entry on both sides,
//!   paired with the P2P connection's number (`P2pConnectionInfo::id`); its links and
//!   channels are rows and counts of that entry.

mod conn;
mod listener;
mod stream;
mod table;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tasty_net_p2p::settings::{DEFAULTS, Value};
use tasty_net_p2p::{
    AttemptPhase, P2pCancel, P2pConnectError, P2pConnection, P2pConnectionInfo, P2pEndpoint,
    P2pError,
};

use crate::cancel::{AttemptCancel, Cancelled, Registration};
use crate::channel::{DEFAULT_DRAIN_CAP, RawChannel};
use crate::connection_table::{
    Cause, ConnectionPeer, ConnectionTable, EntryRef, JoinGuard, Joined, Phase,
};
use crate::lock::lock;
use crate::role::Side;
use crate::transport::{
    ConnectError, ConnectOptions, Connection, DialAuth, Endpoint, EndpointHandle, Transport,
};

pub use listener::{P2pInbound, P2pListener};

use conn::{ChannelSlot, ConnSide, ConnState, OwnedConn, PunchCancel, watch};
use table::{Followed, follow, open_entry, take_pending};

/// The key of the interval a connection's thread wakes at.
const POLL_KEY: &str = "path.poll_interval";

/// The interval a connection's thread wakes at: the endpoint's `path.poll_interval` in effect
/// (the interval it reads the peer address at too), or that setting's default in the settings
/// table of `tasty-net-p2p` when the endpoint shows no settings. The value and its source are
/// the ones `P2pEndpoint::settings` shows; the adapter has no value of its own.
pub(crate) fn poll_interval(endpoint: &P2pEndpoint) -> Duration {
    let duration = |value: Value| match value {
        Value::Duration(d) => Some(d),
        _ => None,
    };
    endpoint
        .settings()
        .into_iter()
        .find(|entry| entry.key == POLL_KEY)
        .and_then(|entry| duration(entry.value))
        .or_else(|| {
            DEFAULTS
                .iter()
                .find(|setting| setting.key == POLL_KEY)
                .and_then(|setting| duration(setting.default))
        })
        .expect("tasty-net-p2p's settings table has path.poll_interval as a duration")
}

/// The shared connections, by peer device.
type Shared = Arc<Mutex<HashMap<DeviceId, Weak<ConnState>>>>;

/// The transport over P2P: the target is the peer's device ID.
///
/// The peer must be on this device's pairing list and registered on the same rendezvous
/// server. Connecting to this device's own ID fails with [`ConnectError::SelfAttach`] before
/// anything is sent.
///
/// With a [`ConnectionTable`] ([`P2pTransport::with_connections`]) every QUIC connection it
/// punches is one `p2p_udp` entry, made when the attempt starts. A connection that reuses a
/// shared QUIC connection is on that connection's entry; with
/// [`ConnectOptions::after`] (or the redialer's entry) it shows as its own `reconnecting`
/// entry until its handshake, which then ends naming the entry it joined.
pub struct P2pTransport {
    endpoint: P2pEndpoint,
    shared: Shared,
    /// The punches of this transport in progress, by peer device.
    punching: Mutex<HashMap<DeviceId, usize>>,
    poll: Duration,
    connections: Option<ConnectionTable>,
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
            punching: Mutex::default(),
            poll,
            connections: None,
        }
    }

    /// Reports every QUIC connection it punches in `table`.
    pub fn with_connections(mut self, table: ConnectionTable) -> Self {
        self.connections = Some(table);
        self
    }

    /// The table it reports in, if any.
    pub fn connections(&self) -> Option<&ConnectionTable> {
        self.connections.as_ref()
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

    /// The entry of a new attempt: the redialer's, or a new one (`reconnecting` when
    /// `options` names the previous entry).
    fn start_entry(&self, peer: DeviceId, options: &ConnectOptions) -> Option<EntryRef> {
        if let Some(pending) = &options.pending {
            take_pending(pending);
            pending.update(|info| info.peer.device_id = Some(peer));
            return Some(pending.clone());
        }
        let table = self.connections.as_ref()?;
        let peer = ConnectionPeer {
            device_id: Some(peer),
            name: options.name.clone(),
            ..ConnectionPeer::default()
        };
        Some(open_entry(table, Side::Dialer, options.previous, peer))
    }

    /// A new channel on the shared connection to `peer`, if there is one. A shared connection
    /// that closed meanwhile is passed over for a new one (`None`); any other failure of the
    /// open (the attempt's cancel, a call in an async task, no thread for the open) is the
    /// attempt's, as it is for [`EndpointHandle::connect_more`].
    fn reuse(
        &self,
        peer: &DeviceId,
        cancel: &AttemptCancel,
    ) -> Result<Option<(RawChannel, Arc<ConnState>)>, ConnectError> {
        let Some(state) = self.shared_conn(peer) else {
            return Ok(None);
        };
        match open_on(&state, cancel) {
            Ok(channel) => Ok(Some((channel, state))),
            Err(OpenError::Closed(error)) => {
                tracing::debug!(%error, "a shared P2P connection closed: a new one is punched");
                Ok(None)
            }
            Err(OpenError::Failed(error)) => Err(error),
        }
    }

    /// Punches a new connection to `peer`, registered with `cancel` until it is shared.
    /// `entry` follows the attempt while it runs.
    fn new_conn(
        &self,
        peer: DeviceId,
        cancel: &AttemptCancel,
        entry: Option<EntryRef>,
    ) -> Result<(Arc<ConnState>, Registration), ConnectError> {
        let punch = P2pCancel::new();
        let punching = cancel
            .register(Arc::new(PunchCancel(punch.clone())))
            .map_err(|_| ConnectError::Cancelled)?;
        let followed = Mutex::new(Followed::default());
        let connected = match &entry {
            Some(entry) => self.punch_followed(peer, &punch, entry, &followed),
            None => {
                // Counted too: the entry of another punch of this transport to `peer` must not
                // take this attempt for its own.
                let _punching = Punching::start(self, peer);
                self.endpoint.connect_with_cancel(peer, &punch)
            }
        };
        drop(punching);
        let conn = connected.map_err(|e| connect_error(e, cancel))?;
        let followed = followed
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = ConnState::new(conn, ConnSide::Dialer, DEFAULT_DRAIN_CAP, entry, followed);
        // A cancel that came meanwhile closes the connection here.
        let owned = cancel
            .register(Arc::new(OwnedConn(Arc::clone(&state))))
            .map_err(|_| ConnectError::Cancelled)?;
        watch(Arc::clone(&state), None, self.poll);
        Ok((state, owned))
    }

    /// Punches to `peer` while a thread copies the attempt into `entry` at every poll
    /// interval.
    fn punch_followed(
        &self,
        peer: DeviceId,
        punch: &P2pCancel,
        entry: &EntryRef,
        followed: &Mutex<Followed>,
    ) -> Result<P2pConnection, P2pError> {
        let done = (Mutex::new(false), Condvar::new());
        // `before`: the attempts to `peer` before this one; this attempt is none of them.
        let (_punching, before) = Punching::start(self, peer);
        std::thread::scope(|scope| {
            let follower = std::thread::Builder::new()
                .name("p2p-punch".into())
                .spawn_scoped(scope, || {
                    let mut finished = lock(&done.0);
                    while !*finished {
                        finished = done
                            .1
                            .wait_timeout(finished, self.poll)
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0;
                        if *finished {
                            break;
                        }
                        let mut followed = lock(followed);
                        if let Some(info) = self.attempt_of(peer, followed.id(), &before) {
                            follow(entry, &info, &mut followed, None);
                        }
                    }
                });
            if let Err(error) = follower {
                // The entry still gets the attempt's history once the connection stands.
                tracing::debug!(%error, "no thread to follow a P2P attempt");
            }
            let connected = self.endpoint.connect_with_cancel(peer, punch);
            *lock(&done.0) = true;
            done.1.notify_all();
            connected
        })
    }

    /// The attempt an entry follows: the one it followed already, or else this attempt once it
    /// is the only one it can be. `tasty-net-p2p` does not say which attempt a dial is, so
    /// while this transport punches to `peer` more than once (with an entry or without), or
    /// more than one new attempt to `peer` is in progress, the entry waits; the connection's
    /// history is copied whole once it stands. The count and the attempts are read under the
    /// lock a punch of this transport starts under ([`Punching::start`]), so no punch of it
    /// starts between the two.
    fn attempt_of(
        &self,
        peer: DeviceId,
        id: Option<u64>,
        before: &[u64],
    ) -> Option<P2pConnectionInfo> {
        if let Some(id) = id {
            return self
                .endpoint
                .connections()
                .into_iter()
                .find(|info| info.id == id);
        }
        let punching = lock(&self.punching);
        if punching.get(&peer).copied() != Some(1) {
            return None;
        }
        let mut new = self
            .attempts_to(peer)
            .filter(|info| !before.contains(&info.id));
        let attempt = new.next()?;
        let only = new.next().is_none().then_some(attempt);
        drop(punching);
        only
    }

    /// The attempts this device dials to `peer` that are in progress.
    fn attempts_to(&self, peer: DeviceId) -> impl Iterator<Item = P2pConnectionInfo> {
        self.endpoint.connections().into_iter().filter(move |info| {
            info.peer == peer
                && info.side == tasty_net_p2p::Side::Dialer
                && !matches!(info.state, AttemptPhase::Connected | AttemptPhase::Closed)
        })
    }
}

/// One punch of a transport to a peer, counted while it runs.
struct Punching<'a> {
    punching: &'a Mutex<HashMap<DeviceId, usize>>,
    peer: DeviceId,
}

impl<'a> Punching<'a> {
    /// Counts a punch of `transport` to `peer` from here on; with it, the numbers of the
    /// attempts to `peer` in progress before it, read in the same step.
    fn start(transport: &'a P2pTransport, peer: DeviceId) -> (Self, Vec<u64>) {
        let mut punching = lock(&transport.punching);
        let before = transport.attempts_to(peer).map(|info| info.id).collect();
        *punching.entry(peer).or_default() += 1;
        drop(punching);
        (
            Self {
                punching: &transport.punching,
                peer,
            },
            before,
        )
    }
}

impl Drop for Punching<'_> {
    fn drop(&mut self) {
        let mut punching = lock(self.punching);
        if let Some(count) = punching.get_mut(&self.peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                punching.remove(&self.peer);
            }
        }
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

/// The error of a channel on a connection that closed.
fn closed() -> ConnectError {
    ConnectError::Io(io::Error::new(
        io::ErrorKind::NotConnected,
        "the P2P connection is closed",
    ))
}

/// Opens a stream on `state` as a new channel.
fn open_channel(
    state: &Arc<ConnState>,
    cancel: &AttemptCancel,
) -> Result<RawChannel, ConnectError> {
    open_on(state, cancel).map_err(OpenError::into_error)
}

/// Why a channel did not open on a connection, judged when the open failed: the release of
/// the channel's slot that follows may close the connection, and that close is no cause.
#[derive(Debug)]
enum OpenError {
    /// The connection was closed then: no place for a channel, or the open itself failed on
    /// a connection that had ended.
    Closed(ConnectError),
    /// Any other failure (the attempt's cancel, a call in an async task, no thread for the
    /// open, an open that failed on a live connection) is the attempt's.
    Failed(ConnectError),
}

impl OpenError {
    fn into_error(self) -> ConnectError {
        match self {
            Self::Closed(error) | Self::Failed(error) => error,
        }
    }

    /// The failure of an open that ran on its thread, with whether the connection had ended
    /// while the channel's slot was still held.
    fn of_open(error: ConnectError, ended: bool) -> Self {
        match error {
            ConnectError::Cancelled => Self::Failed(error),
            error if ended => Self::Closed(error),
            error => Self::Failed(error),
        }
    }
}

/// [`open_channel`] that tells a closed connection from the attempt's own failure.
fn open_on(state: &Arc<ConnState>, cancel: &AttemptCancel) -> Result<RawChannel, OpenError> {
    if cancel.is_cancelled() {
        return Err(OpenError::Failed(ConnectError::Cancelled));
    }
    let slot = state.acquire().ok_or_else(|| OpenError::Closed(closed()))?;
    let opening = stream::start_open(&state.conn, cancel).map_err(OpenError::Failed)?;
    match opening.wait() {
        Ok(stream) => Ok(state.entered(stream::channel(
            stream,
            &state.conn,
            state.side,
            state.drain_cap,
            slot,
        ))),
        // Read while the slot holds the connection: no release of this end closed it yet.
        Err(error) => Err(OpenError::of_open(error, state.has_ended())),
    }
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
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        if *target == self.endpoint.id() {
            return Err(ConnectError::SelfAttach);
        }
        let reused = self.reuse(target, cancel);
        // A cancelled attempt ends `Cancelled`, whatever the reuse's failure was
        // (docs/design/transport.md "시도 취소, 닫기, 회수").
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let reused = reused?;
        let (channel, state, owned, joined) = match reused {
            Some((channel, state)) => {
                // A reconnection stays visible: once its handshake on the shared connection
                // succeeds it ends naming the entry it joined.
                let reconnects = options.previous.is_some() || options.pending.is_some();
                let joined = match (&state.entry, reconnects) {
                    (Some(shared), true) => self.start_entry(*target, options).map(|pending| {
                        pending.phase(Phase::Handshake);
                        Joined::new(pending, shared.id())
                    }),
                    _ => None,
                };
                let channel = match &joined {
                    Some(joined) => channel.with_joined(joined.clone()),
                    None => channel,
                };
                (channel, state, None, joined)
            }
            None => {
                let entry = self.start_entry(*target, options);
                let made =
                    self.new_conn(*target, cancel, entry.clone())
                        .and_then(|(state, owned)| {
                            // The hold stays while the first stream's failure is read: its end
                            // would close the connection here, and that close is no code of
                            // the peer's.
                            let hold = state.acquire();
                            let failure = match &hold {
                                Some(_) => match open_on(&state, cancel) {
                                    Ok(channel) => return Ok((channel, state, owned, hold)),
                                    Err(failure) => failure,
                                },
                                None => OpenError::Closed(closed()),
                            };
                            // The peer closed the new connection with a code before its first
                            // stream (a refused pairing): the first channel is a closed one, so
                            // its handshake ends with that code (docs/design/handshake.md "흐름
                            // (P2P 경로)"). A close of this end (the attempt's cancel) is not,
                            // nor a failure of the attempt's own on a live connection.
                            let peer_code = match &failure {
                                OpenError::Closed(_) => state
                                    .conn
                                    .close_reason()
                                    .filter(|_| !cancel.is_cancelled() && !state.closed_here()),
                                OpenError::Failed(_) => None,
                            };
                            let error = failure.into_error();
                            drop(hold);
                            match peer_code {
                                Some(reason) => {
                                    let channel =
                                        state.entered(stream::closed(&state.conn, reason));
                                    Ok((channel, state, owned, None))
                                }
                                _ => {
                                    // No channel holds the new connection: close it here.
                                    state.close(tasty_net_p2p::P2pCloseReason::Closed);
                                    Err(error)
                                }
                            }
                        });
                let (channel, state, owned, hold) = made.inspect_err(|error| {
                    if let Some(entry) = &entry {
                        entry.end(Cause::of_connect(error));
                    }
                })?;
                (channel, state, Some((owned, hold)), None)
            }
        };
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(P2pEndpointHandle {
                state,
                owned: Mutex::new(owned),
                shared: Arc::clone(&self.shared),
                _joined: joined.map(JoinGuard),
            })),
        })
    }
}

/// The endpoint of a P2P connection: more channels are more streams on it.
struct P2pEndpointHandle {
    state: Arc<ConnState>,
    /// The attempt's registration while the connection is not shared yet, and a place on the
    /// connection that keeps it open until the first handshake's result is noted: a failed
    /// first handshake drops its channel before the caller notes why, and the connection's end
    /// must not end the entry before that (the endpoint is dropped after the note).
    owned: Mutex<Option<(Registration, Option<ChannelSlot>)>>,
    shared: Shared,
    /// A reconnection waiting on a shared connection, ended if the attempt is dropped
    /// unfinished.
    _joined: Option<JoinGuard>,
}

impl EndpointHandle for P2pEndpointHandle {
    /// A new connection takes more channels only once its first handshake succeeded and it
    /// is shared (docs/design/transport.md "연결 공유의 시작").
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        if lock(&self.owned).is_some() {
            return Err(ConnectError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "the P2P connection is not shared yet: its first handshake has not succeeded",
            )));
        }
        open_channel(&self.state, cancel)
    }

    fn auth(&self) -> Option<DialAuth> {
        Some(DialAuth::P2p {
            remote_id: self.state.conn.remote_device_id(),
        })
    }

    /// The share takes the attempt's cancel off the connection and lists it as shared, both
    /// under the lock [`P2pEndpointHandle::connect_more`] reads `owned` under: no more channel
    /// takes the connection before both are done, nor after a cancel that came first.
    fn publish(&self) -> Result<(), Cancelled> {
        let mut owned = lock(&self.owned);
        let Some((registration, _hold)) = owned.take() else {
            return Ok(());
        };
        if let Err(cancelled) = registration.release() {
            // The cancel came first and closes the connection; closed here as well before the
            // lock goes, as the cancel may not have reached it yet.
            self.state.close(tasty_net_p2p::P2pCloseReason::Closed);
            return Err(cancelled);
        }
        lock(&self.shared).insert(
            self.state.conn.remote_device_id(),
            Arc::downgrade(&self.state),
        );
        drop(owned);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_interval_comes_from_the_p2p_settings_table() {
        let default = DEFAULTS
            .iter()
            .find(|setting| setting.key == POLL_KEY)
            .expect("the key is in the table");
        assert!(matches!(default.default, Value::Duration(_)));
        // An endpoint that is off shows the settings it was given, defaults included.
        let off = P2pEndpoint::bind(tasty_net_p2p::P2pConfig::new(
            tasty_net_identity::DeviceKey::from_seed([3; 32]),
            Arc::new(tasty_net_p2p::PairingList::new()),
        ))
        .expect("an endpoint that is off");
        assert_eq!(Value::Duration(poll_interval(&off)), default.default);
    }

    /// An open that ran and failed is the connection's close only when the connection had
    /// ended while the channel's slot still held it; the attempt's cancel never is. A failure
    /// before the open ran (a call in an async task, no thread) never reaches this judgement:
    /// `open_on` gives it as the attempt's.
    #[test]
    fn a_failed_open_is_a_closed_connection_only_if_it_had_ended() {
        let failed = || ConnectError::Io(io::Error::other("the open failed"));
        assert!(matches!(
            OpenError::of_open(failed(), true),
            OpenError::Closed(ConnectError::Io(_))
        ));
        assert!(matches!(
            OpenError::of_open(failed(), false),
            OpenError::Failed(ConnectError::Io(_))
        ));
        for ended in [true, false] {
            assert!(matches!(
                OpenError::of_open(ConnectError::Cancelled, ended),
                OpenError::Failed(ConnectError::Cancelled)
            ));
        }
    }
}
