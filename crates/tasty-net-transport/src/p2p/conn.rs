//! One QUIC connection under the adapter: how many channels it carries, when it is closed, and
//! the thread that takes the peer's streams (docs/design/transport.md "P2P의 닫기 범위").
//!
//! `tasty-net-p2p` does not close a connection whose handles are all dropped while the endpoint
//! lives (docs/design/p2p.md "스트림과 연결"), so the adapter closes every connection itself
//! with [`P2pConnection::close`]:
//!
//! - The side that opened the connection closes it with code 0 once its last channel is gone:
//!   its reader read the acceptor's end, or the channel was closed by force.
//! - The side that accepted it waits for that close after its last channel is gone, and closes
//!   with code 0 itself when it did not come within the drain cap.
//! - A connection whose attempt was cancelled before its first handshake succeeded is closed by
//!   the cancel.
//!
//! With a connection table the connection is one `p2p_udp` entry. The connection's thread
//! copies what `tasty-net-p2p` shows of it into the entry at every wake, and ends the entry
//! when the connection closed.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tasty_net_p2p::{P2pCloseReason, P2pConnection, P2pStream};

use crate::cancel::Cancellable;
use crate::channel::RawChannel;
use crate::connection_table::{Cause, ConnectionState, EntryRef};
use crate::lock::lock;

use super::listener::Queue;
use super::stream;
use super::table::{Followed, follow};

/// Which side of the QUIC connection this end is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnSide {
    /// This end opened the connection; only it opens streams on it.
    Dialer,
    /// This end accepted the connection.
    Acceptor,
}

/// The adapter's view of one QUIC connection. Every channel on it holds it through its
/// [`ChannelSlot`].
pub(crate) struct ConnState {
    pub(crate) conn: P2pConnection,
    pub(crate) side: ConnSide,
    /// How long the accepting side waits for the dialer's close, and how long a stream's finish
    /// waits for the peer's end.
    pub(crate) drain_cap: Duration,
    channels: Mutex<Channels>,
    /// The connection's entry, with a connection table.
    pub(crate) entry: Option<EntryRef>,
    followed: Mutex<Followed>,
}

#[derive(Default)]
struct Channels {
    /// Channels alive on the connection.
    open: usize,
    /// When the last channel went (the accepting side's drain wait).
    idle_since: Option<Instant>,
    /// The connection is closed or being closed: no new channel takes it.
    closing: bool,
}

impl ConnState {
    /// The state of `conn`, whose entry is `entry`; the entry follows the connection from
    /// here on.
    pub(crate) fn new(
        conn: P2pConnection,
        side: ConnSide,
        drain_cap: Duration,
        entry: Option<EntryRef>,
        followed: Followed,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            conn,
            side,
            drain_cap,
            channels: Mutex::new(Channels::default()),
            entry,
            followed: Mutex::new(followed),
        });
        state.follow();
        state
    }

    /// `channel` on this connection, in the connection's entry.
    pub(crate) fn entered(&self, channel: RawChannel) -> RawChannel {
        match &self.entry {
            Some(entry) => channel.with_entry(entry.clone()),
            None => channel,
        }
    }

    /// Copies what `tasty-net-p2p` shows of the connection into its entry.
    fn follow(&self) {
        let Some(entry) = &self.entry else {
            return;
        };
        if let Some(info) = self.conn.info() {
            follow(entry, &info, &mut lock(&self.followed));
        }
    }

    /// Ends the entry of the closed connection. When this end closed it, the cause is the
    /// one its last link or its failed attempt left ([`Cause::Ended`] without one); when the
    /// peer (or `tasty-net-p2p`) closed it, the code it was closed with, and [`Cause::Lost`]
    /// for an end without a code (the idle timeout). A failure noted before the entry was
    /// connected stays.
    fn end_entry(&self) {
        let Some(entry) = &self.entry else {
            return;
        };
        let closing = lock(&self.channels).closing;
        let (state, recorded) = entry
            .status()
            .unwrap_or((ConnectionState::Disconnected, None));
        // A copied P2P transition is no failure of this end.
        let failure = recorded.filter(|c| !matches!(c, Cause::P2p(_)));
        let cause = match failure {
            Some(cause) if closing || state != ConnectionState::Connected => cause,
            _ if closing => Cause::Ended,
            _ => match self.conn.close_reason() {
                Some(reason) => Cause::P2pClosed(reason),
                None => Cause::Lost,
            },
        };
        entry.end(cause);
    }

    /// Whether the QUIC connection ended.
    pub(crate) fn is_closed(&self) -> bool {
        // A zero wait answers at once: closed or still open.
        matches!(self.conn.wait_closed(Duration::ZERO), Ok(Some(_)) | Err(_))
    }

    /// A place for one more channel, unless the connection is closing.
    pub(crate) fn acquire(self: &Arc<Self>) -> Option<ChannelSlot> {
        let mut channels = lock(&self.channels);
        if channels.closing || self.conn.close_reason().is_some() {
            return None;
        }
        channels.open += 1;
        channels.idle_since = None;
        Some(ChannelSlot {
            conn: Arc::clone(self),
        })
    }

    /// Whether a new channel may still take the connection.
    pub(crate) fn is_usable(&self) -> bool {
        !lock(&self.channels).closing && self.conn.close_reason().is_none()
    }

    /// Closes the whole connection with `reason`; no new channel takes it.
    pub(crate) fn close(&self, reason: P2pCloseReason) {
        lock(&self.channels).closing = true;
        self.conn.close(reason);
    }

    fn release(&self) {
        let close = {
            let mut channels = lock(&self.channels);
            channels.open = channels.open.saturating_sub(1);
            if channels.open > 0 {
                return;
            }
            match self.side {
                ConnSide::Dialer => {
                    channels.closing = true;
                    true
                }
                ConnSide::Acceptor => {
                    channels.idle_since = Some(Instant::now());
                    false
                }
            }
        };
        if close {
            self.conn.close(P2pCloseReason::Closed);
        }
    }

    /// On the accepting side: how long until the drain wait after the last channel ends, `None`
    /// while channels are open (or none ever was).
    fn drain_left(&self) -> Option<Duration> {
        let since = lock(&self.channels).idle_since?;
        Some(self.drain_cap.saturating_sub(since.elapsed()))
    }
}

/// One channel's place on its connection. Dropping it (with the channel's last half) releases
/// the place; the dialer closes the connection when it was the last one.
pub(crate) struct ChannelSlot {
    conn: Arc<ConnState>,
}

impl Drop for ChannelSlot {
    fn drop(&mut self) {
        self.conn.release();
    }
}

/// A connection the attempt that opened it still owns: its cancel closes it.
pub(crate) struct OwnedConn(pub(crate) Arc<ConnState>);

impl Cancellable for OwnedConn {
    fn cancel(&self) {
        self.0.close(P2pCloseReason::Closed);
    }
}

/// The cancel of a punch in progress.
pub(crate) struct PunchCancel(pub(crate) tasty_net_p2p::P2pCancel);

impl Cancellable for PunchCancel {
    fn cancel(&self) {
        self.0.cancel();
    }
}

/// Starts the thread that takes the peer's streams on `state` until the connection ends. On
/// the dialing side every stream the peer opens is refused (only the dialer opens streams); on
/// the accepting side each one goes to `queue` as a new channel, or is refused when the
/// listener is gone. Between streams it closes an idle accepted connection after the drain cap.
pub(crate) fn watch(state: Arc<ConnState>, queue: Option<Weak<Queue>>, poll: Duration) {
    let watched = Arc::clone(&state);
    let spawned = std::thread::Builder::new()
        .name("p2p-conn".into())
        .spawn(move || run(&watched, queue.as_ref(), poll));
    if let Err(error) = spawned {
        // Without its thread nobody takes its streams or closes it when idle: close it now.
        tracing::debug!(%error, "no thread for a P2P connection");
        state.close(P2pCloseReason::Closed);
        state.end_entry();
    }
}

fn run(state: &Arc<ConnState>, queue: Option<&Weak<Queue>>, poll: Duration) {
    serve(state, queue, poll);
    state.end_entry();
}

fn serve(state: &Arc<ConnState>, queue: Option<&Weak<Queue>>, poll: Duration) {
    loop {
        state.follow();
        if state.is_closed() {
            return;
        }
        let drain = state.drain_left();
        if drain.is_some_and(|left| left.is_zero()) {
            state.close(P2pCloseReason::Closed);
            return;
        }
        let wait = drain.map_or(poll, |left| left.min(poll));
        match state.conn.accept_bi(Some(wait)) {
            Ok(stream) => take(state, queue, stream),
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => {
                tracing::debug!(%error, "a P2P connection stopped taking streams");
                return;
            }
        }
    }
}

fn take(state: &Arc<ConnState>, queue: Option<&Weak<Queue>>, stream: P2pStream) {
    let queue = match (state.side, queue.and_then(Weak::upgrade)) {
        (ConnSide::Acceptor, Some(queue)) => queue,
        _ => {
            stream::refuse(stream);
            return;
        }
    };
    let Some(slot) = state.acquire() else {
        stream::refuse(stream);
        return;
    };
    let channel = stream::channel(stream, &state.conn, state.side, state.drain_cap, slot);
    queue.push(Arc::clone(state), state.entered(channel));
}
