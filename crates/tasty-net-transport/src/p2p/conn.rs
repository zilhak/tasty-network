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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use tasty_net_p2p::{P2pCloseReason, P2pConnection, P2pStream};

use crate::cancel::Cancellable;
use crate::channel::{ChannelControl, RawChannel};
use crate::connection_table::{Cause, ConnectionState, EndBy, EntryRef};
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
    /// The connection's number in this process, which bulk credentials are bound to
    /// (docs/design/handshake.md "bulk 자격") and which owns its entry
    /// ([`EntryRef::own_p2p`]). `P2pConnection::id` is unique only within its endpoint
    /// (docs/design/p2p.md "연결 정보"), and endpoints may share one acceptor's credentials.
    pub(crate) scope: u64,
    pub(crate) side: ConnSide,
    /// How long the accepting side waits for the dialer's close, and how long a stream's finish
    /// waits for the peer's end.
    pub(crate) drain_cap: Duration,
    channels: Mutex<Channels>,
    /// Wakes the drain timer of the accepting side when a channel comes or the connection
    /// closes.
    channels_changed: Condvar,
    /// The connection's entry, with a connection table.
    pub(crate) entry: Option<EntryRef>,
    followed: Mutex<Followed>,
}

/// The next connection's [`ConnState::scope`].
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Channels {
    /// Channels alive on the connection.
    open: usize,
    /// When the last channel went (the accepting side's drain wait).
    idle_since: Option<Instant>,
    /// This end closes or closed the connection: no new channel takes it.
    closing: bool,
    /// The QUIC connection ended (its thread saw the end, whoever closed it): no channel
    /// takes it and no drain timer waits on it.
    ended: bool,
}

impl Channels {
    /// Whether the drain cap `cap` ran out since the last channel went, on a connection
    /// nobody closes yet.
    fn drained(&self, cap: Duration) -> bool {
        !self.closing && !self.ended && self.idle_since.is_some_and(|since| since.elapsed() >= cap)
    }
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
        let id = conn.id();
        let scope = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = &entry {
            // From here on the entry shows this connection, and only its thread changes it.
            entry.own_p2p(scope, |info| info.p2p_id = Some(id));
        }
        let state = Arc::new(Self {
            conn,
            scope,
            side,
            drain_cap,
            channels: Mutex::new(Channels::default()),
            channels_changed: Condvar::new(),
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

    /// Copies what `tasty-net-p2p` shows of the connection into its entry, while the
    /// connection owns the entry: a redialer's entry moves on to the next attempt, and the
    /// thread of an earlier connection then no longer changes it.
    fn follow(&self) {
        let Some(entry) = &self.entry else {
            return;
        };
        if let Some(info) = self.conn.info() {
            follow(entry, &info, &mut lock(&self.followed), Some(self.scope));
        }
    }

    /// Ends the entry of the closed connection. When this end closed it, the cause is the
    /// one its last link or its failed attempt left ([`Cause::Ended`] without one); when the
    /// peer (or `tasty-net-p2p`) closed it, the code it was closed with, and [`Cause::Lost`]
    /// for an end without a code (the idle timeout). A failure noted before the entry was
    /// connected stays. An entry that moved on to another connection is left as it is.
    fn end_entry(&self) {
        let Some(entry) = &self.entry else {
            return;
        };
        let closing = lock(&self.channels).closing;
        entry.end_owned(self.scope, |state, recorded| {
            end_cause(closing, || self.conn.close_reason(), state, recorded)
        });
    }

    /// Whether the QUIC connection ended. A zero wait answers at once; in an async task
    /// `tasty-net-p2p` refuses it, which says nothing of the connection: open, as far as known.
    pub(crate) fn is_closed(&self) -> bool {
        match self.conn.wait_closed(Duration::ZERO) {
            Ok(end) => end.is_some(),
            Err(error) => !stream::is_in_async_context(&error),
        }
    }

    /// A place for one more channel, unless the connection is closing.
    pub(crate) fn acquire(self: &Arc<Self>) -> Option<ChannelSlot> {
        let mut channels = lock(&self.channels);
        if channels.closing || channels.ended || self.conn.close_reason().is_some() {
            return None;
        }
        channels.open += 1;
        channels.idle_since = None;
        drop(channels);
        self.channels_changed.notify_all();
        Some(ChannelSlot {
            conn: Arc::clone(self),
            control: None,
        })
    }

    /// Whether a new channel may still take the connection.
    pub(crate) fn is_usable(&self) -> bool {
        let channels = lock(&self.channels);
        !channels.closing && !channels.ended && self.conn.close_reason().is_none()
    }

    /// Whether the connection ended, with a close code or without one (the idle timeout, which
    /// leaves no [`P2pConnection::close_reason`]).
    pub(crate) fn has_ended(&self) -> bool {
        lock(&self.channels).ended || self.conn.close_reason().is_some() || self.is_closed()
    }

    /// Whether this end closes or closed the connection.
    pub(crate) fn closed_here(&self) -> bool {
        lock(&self.channels).closing
    }

    /// The connection's thread saw the connection end: wakes the drain timer, which then
    /// lets the connection go, and no later channel end starts one.
    fn mark_ended(&self) {
        lock(&self.channels).ended = true;
        self.channels_changed.notify_all();
    }

    /// Closes the whole connection with `reason`; no new channel takes it.
    pub(crate) fn close(&self, reason: P2pCloseReason) {
        lock(&self.channels).closing = true;
        self.channels_changed.notify_all();
        self.conn.close(reason);
    }

    fn release(self: &Arc<Self>) {
        {
            let mut channels = lock(&self.channels);
            channels.open = channels.open.saturating_sub(1);
            if channels.open > 0 {
                return;
            }
            match self.side {
                ConnSide::Dialer => channels.closing = true,
                // An ended connection has nothing to wait for.
                ConnSide::Acceptor if channels.ended => return,
                ConnSide::Acceptor => channels.idle_since = Some(Instant::now()),
            }
        }
        match self.side {
            ConnSide::Dialer => self.conn.close(P2pCloseReason::Closed),
            ConnSide::Acceptor => self.close_after_drain_cap(),
        }
    }

    /// The accepting side's close at the drain cap, on `channels` (the lock held): when the cap
    /// ran out since the last channel went, marks the connection closing in the same step, so
    /// a channel that takes it meanwhile ([`ConnState::acquire`], under the same lock) either
    /// came first and keeps it open, or finds it closing. Whether it closed.
    fn close_if_drained(&self, mut channels: std::sync::MutexGuard<'_, Channels>) -> bool {
        if !channels.drained(self.drain_cap) {
            return false;
        }
        channels.closing = true;
        drop(channels);
        self.channels_changed.notify_all();
        self.conn.close(P2pCloseReason::Closed);
        true
    }

    /// On the accepting side, once the last channel is gone: closes the connection with code 0
    /// at the drain cap unless a channel came, this end closed it or it ended first. The
    /// connection's thread checks the same between its waits for a stream, which may be
    /// longer (`path.poll_interval`).
    fn close_after_drain_cap(self: &Arc<Self>) {
        let state = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("p2p-drain".into())
            .spawn(move || {
                let mut channels = lock(&state.channels);
                loop {
                    if channels.closing || channels.ended {
                        return;
                    }
                    let Some(since) = channels.idle_since else {
                        // A channel came.
                        return;
                    };
                    let left = state.drain_cap.saturating_sub(since.elapsed());
                    if left.is_zero() {
                        state.close_if_drained(channels);
                        return;
                    }
                    channels = state
                        .channels_changed
                        .wait_timeout(channels, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            });
        if let Err(error) = spawned {
            // The connection's thread still closes it, at its next wake.
            tracing::debug!(%error, "no thread for the P2P drain timer");
        }
    }

    /// On the accepting side: how long until the drain wait after the last channel ends, `None`
    /// while channels are open (or none ever was), and once this end closes the connection.
    fn drain_left(&self) -> Option<Duration> {
        let channels = lock(&self.channels);
        if channels.closing {
            return None;
        }
        let since = channels.idle_since?;
        Some(self.drain_cap.saturating_sub(since.elapsed()))
    }
}

/// What [`ConnState::end_entry`] ends the entry with: `closing` when this end closed the
/// connection, `close_reason` the code it was closed with, on an entry in `state` whose last
/// cause is `recorded`.
fn end_cause(
    closing: bool,
    close_reason: impl FnOnce() -> Option<P2pCloseReason>,
    state: ConnectionState,
    recorded: Option<&Cause>,
) -> EndBy {
    if closing {
        return EndBy::LastLink;
    }
    // A copied P2P transition is no failure of this end.
    let failure = recorded.filter(|c| !matches!(c, Cause::P2p(_))).cloned();
    match failure {
        Some(cause) if state != ConnectionState::Connected => EndBy::Connection(cause),
        _ => EndBy::Connection(match close_reason() {
            Some(reason) => Cause::P2pClosed(reason),
            None => Cause::Lost,
        }),
    }
}

/// One channel's place on its connection. Dropping it (with the channel's last half) releases
/// the place; the dialer closes the connection when it was the last one.
pub(crate) struct ChannelSlot {
    conn: Arc<ConnState>,
    /// The channel's control, once the channel holds the place, in the connection's entry.
    control: Option<Weak<dyn ChannelControl>>,
}

impl ChannelSlot {
    /// The channel of `control` holds this place: the connection's entry follows it, so a
    /// local end takes the cause of the link whose channel gives the last place back
    /// ([`EndBy::LastLink`]).
    pub(crate) fn track(&mut self, control: &Arc<dyn ChannelControl>) {
        if let Some(entry) = &self.conn.entry {
            entry.channel_opened(control);
            self.control = Some(Arc::downgrade(control));
        }
    }
}

impl Drop for ChannelSlot {
    fn drop(&mut self) {
        // Before the release, which may close the connection and end its entry.
        if let (Some(entry), Some(control)) = (&self.conn.entry, &self.control) {
            entry.channel_released(control);
        }
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
        state.mark_ended();
        state.end_entry();
    }
}

fn run(state: &Arc<ConnState>, queue: Option<&Weak<Queue>>, poll: Duration) {
    serve(state, queue, poll);
    // Before the waiting channels go: their ends start no drain timer.
    state.mark_ended();
    // The listener hands out no channel of the ended connection.
    if let Some(queue) = queue.and_then(Weak::upgrade) {
        queue.remove(state);
    }
    state.end_entry();
}

fn serve(state: &Arc<ConnState>, queue: Option<&Weak<Queue>>, poll: Duration) {
    loop {
        state.follow();
        if state.is_closed() {
            return;
        }
        if state.close_if_drained(lock(&state.channels)) {
            return;
        }
        let drain = state.drain_left();
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use tasty_net_identity::DeviceKey;

    use super::*;
    use crate::connection_table::{ConnectionEvent, ConnectionId, ConnectionPeer, ConnectionTable};
    use crate::end::{EndReason, GuardSlot, LinkCloser};
    use crate::link::{LinkTimeouts, PeerInfo};
    use crate::link_id::LinkId;
    use crate::memory::memory_channel_pair;
    use crate::p2p::table::open_entry;
    use crate::role::{Holder, Side};
    use crate::transport::TransportKind;

    /// Which comes first on a connected entry: the end hook of the link whose channel gave the
    /// last place back, or the end of its connection.
    #[derive(Clone, Copy, Debug)]
    enum First {
        LinkEnd,
        ConnectionEnd,
    }

    /// How a link ends.
    #[derive(Clone, Copy, Debug)]
    enum LinkEnd {
        Normal,
        Forced,
        Lost,
    }

    /// A `p2p_udp` entry of `side` owned by connection 1, on which links stand.
    struct Scene {
        table: ConnectionTable,
        entry: EntryRef,
        side: Side,
    }

    /// A link on the scene: its closer and its first channel's control, whose place on the
    /// connection the test gives back ([`Scene::release`]) as a stream's slot does.
    struct TestLink {
        closer: LinkCloser,
        control: Arc<dyn ChannelControl>,
    }

    impl Scene {
        fn new(side: Side) -> Self {
            let table = ConnectionTable::default();
            let entry = open_entry(&table, side, None, ConnectionPeer::default());
            entry.own_p2p(1, |_| {});
            Self { table, entry, side }
        }

        /// A link numbered `n` stands on the entry, as the stream and the handshake leave it.
        fn link(&self, n: u8) -> TestLink {
            let (channel, _peer) = memory_channel_pair();
            let control = channel.control();
            self.entry.channel_opened(&control);
            let closer = LinkCloser::new(Arc::clone(&control));
            let peer = PeerInfo {
                device_id: DeviceKey::from_seed([1; 32]).id(),
                instance_id: "peer".into(),
                holder: Some(Holder::Dialer),
                caps: Vec::new(),
                transport: TransportKind::P2p,
                local_side: self.side,
            };
            let link_id = LinkId::of_credential(&[n; 32]);
            self.entry
                .link_up(&peer, link_id, &closer, &LinkTimeouts::default());
            self.entry.note_end_of(&closer, link_id);
            TestLink { closer, control }
        }

        /// The link's channel gives its place on the connection back.
        fn release(&self, link: &TestLink) {
            self.entry.channel_released(&Arc::downgrade(&link.control));
        }

        /// The connection ends: this end closed it (`closing`), or the peer with code 0.
        fn end_connection(&self, closing: bool) {
            self.entry.end_owned(1, |state, recorded| {
                end_cause(closing, || Some(P2pCloseReason::Closed), state, recorded)
            });
        }

        fn state(&self) -> ConnectionState {
            self.table
                .connection(self.entry.id())
                .expect("the entry")
                .state
        }

        /// The entry's last cause, the cause of the transition that ended it in its history,
        /// and in the table's events.
        fn ended(&self) -> [Option<Cause>; 3] {
            let id = self.entry.id();
            let info = self.table.connection(id).expect("the ended entry is kept");
            assert_eq!(info.state, ConnectionState::Disconnected, "{info:#?}");
            let history = info.history.last().and_then(|h| h.cause.clone());
            [info.last_cause, history, ended_event(&self.table, id)]
        }
    }

    /// The cause of the event that moved `id` to `disconnected`; exactly one such event.
    fn ended_event(table: &ConnectionTable, id: ConnectionId) -> Option<Cause> {
        let ends: Vec<_> = table
            .events()
            .into_iter()
            .filter_map(|event| match event {
                ConnectionEvent::Transition { id: at, entry }
                    if at == id && entry.state == ConnectionState::Disconnected =>
                {
                    Some(entry.cause)
                }
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1, "{ends:?}");
        ends.into_iter().next().flatten()
    }

    fn end_link(link: &TestLink, how: LinkEnd) {
        match how {
            LinkEnd::Normal => link.closer.end_normally(),
            LinkEnd::Forced => link.closer.force_close(EndReason::Forced),
            LinkEnd::Lost => link.closer.force_close(EndReason::Lost),
        }
    }

    /// The cause a `p2p_udp` entry of `side` ends with, when its one link ends (`how`), gives
    /// its place back, and its connection ends (`closing` if this end closed it, else the peer
    /// with code 0), the link's end hook and the connection's end in the order `first`: the
    /// same in its last cause, its history and its events.
    fn last_cause(side: Side, how: LinkEnd, closing: bool, first: First) -> Option<Cause> {
        let scene = Scene::new(side);
        let link = scene.link(7);
        match first {
            First::LinkEnd => {
                end_link(&link, how);
                scene.release(&link);
                scene.end_connection(closing);
            }
            First::ConnectionEnd => {
                scene.release(&link);
                scene.end_connection(closing);
                end_link(&link, how);
            }
        }
        same(scene.ended(), first)
    }

    fn same([last, history, event]: [Option<Cause>; 3], first: First) -> Option<Cause> {
        assert_eq!(history, last, "history, {first:?}");
        assert_eq!(event, last, "event, {first:?}");
        last
    }

    /// The accepting side's link ends normally and the dialer closes the connection with code
    /// 0: the entry ends with that code whether the link's end hook ran before the connection's
    /// end or after it (docs/design/transport.md "필드", `last_cause`).
    #[test]
    fn a_peer_close_names_the_end_whichever_comes_first() {
        for first in [First::LinkEnd, First::ConnectionEnd] {
            assert_eq!(
                last_cause(Side::Acceptor, LinkEnd::Normal, false, first),
                Some(Cause::P2pClosed(P2pCloseReason::Closed)),
                "{first:?}"
            );
        }
    }

    /// This end closes the connection after its link ended: the entry ends with the link's
    /// cause whether the link's end hook ran before the connection's end or after it.
    #[test]
    fn a_close_here_takes_the_link_cause_whichever_comes_first() {
        for first in [First::LinkEnd, First::ConnectionEnd] {
            for (how, want) in [
                (LinkEnd::Forced, Cause::ClosedHere),
                (LinkEnd::Normal, Cause::Ended),
                (LinkEnd::Lost, Cause::Lost),
            ] {
                assert_eq!(
                    last_cause(Side::Dialer, how, true, first),
                    Some(want),
                    "{how:?} {first:?}"
                );
            }
        }
    }

    /// Two links one after another on one connection (a request channel keeps it between
    /// them): the first is closed here, then the second is lost, gives the last place back and
    /// this end closes the connection. The entry ends with the second link's cause, not the
    /// first one's that the entry still shows, whichever of the second link's end hook and the
    /// connection's end comes first.
    #[test]
    fn a_close_here_takes_the_last_link_cause_on_a_shared_connection() {
        for first in [First::LinkEnd, First::ConnectionEnd] {
            let scene = Scene::new(Side::Dialer);
            let a = scene.link(7);
            end_link(&a, LinkEnd::Forced);
            scene.release(&a);
            let b = scene.link(8);
            let shown = scene.table.connection(scene.entry.id()).expect("the entry");
            assert_eq!(shown.state, ConnectionState::Connected, "{shown:#?}");
            assert_eq!(shown.last_cause, Some(Cause::ClosedHere), "{shown:#?}");
            match first {
                First::LinkEnd => {
                    end_link(&b, LinkEnd::Lost);
                    scene.release(&b);
                    scene.end_connection(true);
                }
                First::ConnectionEnd => {
                    scene.release(&b);
                    scene.end_connection(true);
                    assert_eq!(scene.state(), ConnectionState::Connected);
                    end_link(&b, LinkEnd::Lost);
                }
            }
            assert_eq!(same(scene.ended(), first), Some(Cause::Lost), "{first:?}");
        }
    }

    /// A consumer's bulk guard whose drop waits until the test lets it go, telling the test
    /// when the drop began.
    struct HeldGuard {
        began: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Drop for HeldGuard {
        fn drop(&mut self) {
            // A test that went no longer waits for either.
            let _began = self.began.send(());
            let _released = self.release.recv();
        }
    }

    /// Ends `link` (`how`) on another thread with a bulk guard ([`crate::DomainAccept::guard`])
    /// under it: the end drops the guards before it runs the end hooks, so they wait until
    /// the returned sender lets the guard go. Returns once the guard's drop began.
    fn end_behind_a_guard(
        link: &TestLink,
        how: LinkEnd,
    ) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (began, began_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let guard: Arc<GuardSlot> = Arc::new(Mutex::new(Some(Box::new(HeldGuard {
            began,
            release: release_rx,
        }))));
        link.closer.adopt_guard(&guard);
        let closer = link.closer.clone();
        let ending = std::thread::spawn(move || {
            let _guard = guard;
            match how {
                LinkEnd::Normal => closer.end_normally(),
                LinkEnd::Forced => closer.force_close(EndReason::Forced),
                LinkEnd::Lost => closer.force_close(EndReason::Lost),
            }
        });
        began_rx.recv().expect("the guard's drop began");
        (release, ending)
    }

    /// Two links open at once on one accepted connection, a bulk guard under the first. The
    /// first is closed here: its streams give their places back at once (the forced close
    /// aborts them) and its end hooks wait behind the guard's drop. The second is then lost,
    /// gives the last place back, and this end closes the connection at the drain cap. The
    /// entry ends with `lost` in its last cause, its history and its event, whether the
    /// first link's late hooks come after the connection's end or the guard lets them go
    /// before the second link ends.
    #[test]
    fn a_late_hook_of_a_link_that_ended_first_keeps_the_last_link_cause() {
        for guard_first in [false, true] {
            let scene = Scene::new(Side::Acceptor);
            let a = scene.link(7);
            let b = scene.link(8);
            let (release, ending) = end_behind_a_guard(&a, LinkEnd::Forced);
            scene.release(&a);
            let shown = scene.table.connection(scene.entry.id()).expect("the entry");
            assert_eq!(
                shown.links.len(),
                2,
                "the first link's hooks wait: {shown:#?}"
            );
            let mut ending = Some(ending);
            let mut let_go = || {
                release.send(()).expect("the guard waits");
                if let Some(ending) = ending.take() {
                    ending.join().expect("the first link's close");
                }
            };
            if guard_first {
                let_go();
            }
            end_link(&b, LinkEnd::Lost);
            scene.release(&b);
            scene.end_connection(true);
            if !guard_first {
                assert_eq!(scene.state(), ConnectionState::Disconnected);
                let_go();
            }
            let first = if guard_first {
                First::LinkEnd
            } else {
                First::ConnectionEnd
            };
            assert_eq!(
                same(scene.ended(), first),
                Some(Cause::Lost),
                "guard first {guard_first}"
            );
        }
    }

    /// The other order on one accepted connection: the first link's normal end (its reader
    /// dropped after the peer's end) waits behind a bulk guard while its ordered channel
    /// still holds its place; the second link is then lost and gives its place back. The
    /// guard goes, and the first link's channel gives the last place back. The entry ends
    /// with the first link's `ended`, whether this end's close comes after the first link's
    /// end hook or before it.
    #[test]
    fn the_link_that_gives_the_last_place_back_names_the_end() {
        for first in [First::LinkEnd, First::ConnectionEnd] {
            let scene = Scene::new(Side::Acceptor);
            let a = scene.link(7);
            let b = scene.link(8);
            let (release, ending) = end_behind_a_guard(&a, LinkEnd::Normal);
            end_link(&b, LinkEnd::Lost);
            scene.release(&b);
            assert_eq!(scene.state(), ConnectionState::Connected);
            match first {
                First::LinkEnd => {
                    release.send(()).expect("the guard waits");
                    ending.join().expect("the first link's end");
                    scene.release(&a);
                    scene.end_connection(true);
                }
                First::ConnectionEnd => {
                    scene.release(&a);
                    scene.end_connection(true);
                    assert_eq!(scene.state(), ConnectionState::Connected);
                    release.send(()).expect("the guard waits");
                    ending.join().expect("the first link's end");
                }
            }
            assert_eq!(same(scene.ended(), first), Some(Cause::Ended), "{first:?}");
        }
    }

    /// Two links and a request channel of no link on one accepted connection, a bulk guard
    /// under the first link. The first is closed here: its streams give their places back at
    /// once and its end hooks wait behind the guard. The second is lost (its end noted) and
    /// gives its place back; then the request channel is closed here and gives the last place
    /// back, and this end closes the connection at the drain cap. A request channel is no
    /// link: the entry ends with the cause of the link that gave its place back last, the
    /// second's `lost`, whether the guard lets the first link's hooks go after the
    /// connection's end or before the second link ends.
    #[test]
    fn a_request_channel_that_gives_the_last_place_back_leaves_the_last_link_cause() {
        for guard_first in [false, true] {
            let scene = Scene::new(Side::Acceptor);
            let a = scene.link(7);
            let b = scene.link(8);
            let (request, _peer) = memory_channel_pair();
            let request = request.control();
            scene.entry.channel_opened(&request);
            let (release, ending) = end_behind_a_guard(&a, LinkEnd::Forced);
            scene.release(&a);
            let mut ending = Some(ending);
            let mut let_go = || {
                release.send(()).expect("the guard waits");
                if let Some(ending) = ending.take() {
                    ending.join().expect("the first link's close");
                }
            };
            if guard_first {
                let_go();
            }
            end_link(&b, LinkEnd::Lost);
            scene.release(&b);
            scene.entry.channel_released(&Arc::downgrade(&request));
            assert_eq!(scene.state(), ConnectionState::Connected);
            scene.end_connection(true);
            if !guard_first {
                assert_eq!(scene.state(), ConnectionState::Disconnected);
                let_go();
            }
            let first = if guard_first {
                First::LinkEnd
            } else {
                First::ConnectionEnd
            };
            assert_eq!(
                same(scene.ended(), first),
                Some(Cause::Lost),
                "guard first {guard_first}"
            );
        }
    }
}
