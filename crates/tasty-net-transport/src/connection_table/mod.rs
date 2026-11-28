//! The connection table: which devices this device is connected with, over which path and
//! address pairs (docs/design/transport.md "연결 목록").
//!
//! The transports, the accepting handshake and the redialer make the entries and change
//! them; a consumer only reads the table.

mod info;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

pub use info::{
    Cause, ConnectionEvent, ConnectionId, ConnectionInfo, ConnectionPeer, ConnectionState,
    Direction, EdgeTransport, HistoryEntry, LinkInfo, PathKind, Phase, RedialProgress, Segment,
    Via,
};

use crate::end::{ChildEnd, EndReason, LinkCloser};
use crate::link::{LinkTimeouts, PeerInfo};
use crate::link_id::LinkId;
use crate::lock::lock;
use crate::role::{Holder, Side};
use crate::settings::{
    EVENT_QUEUE_LIMIT, HISTORY_LIMIT, KEEP_CLOSED, Setting, SettingValue, as_bound, default_count,
    link_timeouts, shown,
};

/// The bounds of a [`ConnectionTable`]. A value not given is the default of
/// [`settings::DEFAULTS`](crate::settings::DEFAULTS).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionTableConfig {
    history_limit: Option<u64>,
    keep_closed: Option<u64>,
    event_queue_limit: Option<u64>,
}

impl ConnectionTableConfig {
    /// Every value at its default.
    pub fn new() -> Self {
        Self::default()
    }

    /// `connections.history_limit`: the transitions kept per entry.
    pub fn with_history_limit(mut self, limit: u64) -> Self {
        self.history_limit = Some(limit);
        self
    }

    /// `connections.keep_closed`: the `disconnected` entries kept.
    pub fn with_keep_closed(mut self, limit: u64) -> Self {
        self.keep_closed = Some(limit);
        self
    }

    /// `connections.event_queue_limit`: the unread events kept.
    pub fn with_event_queue_limit(mut self, limit: u64) -> Self {
        self.event_queue_limit = Some(limit);
        self
    }

    /// The `connections.history_limit` in use.
    pub fn history_limit(&self) -> u64 {
        self.history_limit
            .unwrap_or_else(|| default_count(HISTORY_LIMIT))
    }

    /// The `connections.keep_closed` in use.
    pub fn keep_closed(&self) -> u64 {
        self.keep_closed
            .unwrap_or_else(|| default_count(KEEP_CLOSED))
    }

    /// The `connections.event_queue_limit` in use.
    pub fn event_queue_limit(&self) -> u64 {
        self.event_queue_limit
            .unwrap_or_else(|| default_count(EVENT_QUEUE_LIMIT))
    }

    /// Every value with its source, in the order of the design table.
    pub fn settings(&self) -> Vec<Setting> {
        [
            (HISTORY_LIMIT, self.history_limit),
            (KEEP_CLOSED, self.keep_closed),
            (EVENT_QUEUE_LIMIT, self.event_queue_limit),
        ]
        .into_iter()
        .map(|(key, value)| shown(key, value.map(SettingValue::Count)))
        .collect()
    }
}

/// The table of this device's connections. Clones share it.
///
#[cfg_attr(
    feature = "ssh",
    doc = " Give it to [`SshTransport::with_connections`](crate::SshTransport::with_connections) and"
)]
#[cfg_attr(
    not(feature = "ssh"),
    doc = " Give it to the SSH transport's `with_connections` (with the `system-ssh` feature) and"
)]
/// [`AcceptConfig::with_connections`](crate::AcceptConfig::with_connections); they make one
/// entry per physical connection and change it as the connection goes. There is no API to
/// make or change an entry.
#[derive(Clone)]
pub struct ConnectionTable {
    inner: Arc<Inner>,
}

struct Inner {
    config: ConnectionTableConfig,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The last number given.
    last_id: u64,
    entries: BTreeMap<ConnectionId, Stored>,
    /// The `disconnected` entries, oldest first.
    closed: VecDeque<ConnectionId>,
    events: VecDeque<ConnectionEvent>,
    /// Events dropped since the consumer last read.
    lagged: u64,
}

struct Stored {
    info: ConnectionInfo,
    /// When the redialer's next attempt starts.
    next_attempt_at: Option<Instant>,
    /// The redialer holds the entry between attempts: the end of one attempt's tunnel does
    /// not end it.
    held: bool,
    /// The consumer chose the path; otherwise the first attach link's holder sets it.
    path_fixed: bool,
    /// The P2P connection (by its number in the process) whose thread changes the entry; the
    /// redialer's next attempt takes the entry from it.
    p2p_owner: Option<u64>,
    /// The channels on the entry's connection (or tunnel) that still hold their place on it,
    /// each with the end of the link it belongs to, once that is known.
    channels: Vec<HeldChannel>,
    /// The links on the entry whose end has not come yet, with their end.
    open_links: Vec<(crate::end::CloserWatch, Arc<LinkEnd>)>,
    /// The end of the link whose channel gave a place back last, of the channels of links: a
    /// channel of no link (a request channel) that gives one back after it leaves it, so an end
    /// after the request channel's takes the cause of the last link that gave its place back.
    /// `None` while no channel of a link gave one back.
    last_released: Option<Arc<LinkEnd>>,
    /// An end by [`EndBy::LastLink`] that waits for that link's end, and the owner it was
    /// asked for.
    end_waits: Option<(Option<u64>, Arc<LinkEnd>)>,
}

/// A channel that holds its place on the entry's connection or tunnel.
struct HeldChannel {
    control: std::sync::Weak<dyn crate::channel::ChannelControl>,
    link: Option<Arc<LinkEnd>>,
}

/// The end of one link on an entry: its cause once it ended.
#[derive(Default)]
pub(crate) struct LinkEnd {
    cause: Mutex<Option<Cause>>,
}

/// What an entry's end takes its cause from.
pub(crate) enum EndBy {
    /// The physical connection's own end, with its cause: it stays whatever link end comes
    /// after.
    Connection(Cause),
    /// This end closed it: the cause of the link whose channel gave the connection's (or the
    /// tunnel's) last place back ([`EntryRef::channel_released`]); when a request channel gave
    /// it, of the link whose channel gave its place back last. While that link's end has not
    /// come yet, the entry ends when it comes, with its cause. Without such a link, the last
    /// cause recorded (`ended` without one).
    LastLink,
}

impl Stored {
    /// Whether `owner` may change the entry: `None` is the attempt that holds the entry while
    /// it punches, a number the P2P connection that must still own it.
    fn owned_by(&self, owner: Option<u64>) -> bool {
        match owner {
            None => true,
            Some(owner) => self.p2p_owner == Some(owner),
        }
    }
}

impl fmt::Debug for ConnectionTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionTable")
            .field("config", &self.inner.config)
            .field("entries", &lock(&self.inner.state).entries.len())
            .finish()
    }
}

impl Default for ConnectionTable {
    fn default() -> Self {
        Self::new(ConnectionTableConfig::default())
    }
}

impl ConnectionTable {
    /// An empty table with `config`.
    pub fn new(config: ConnectionTableConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// A snapshot of every entry, oldest first.
    pub fn connections(&self) -> Vec<ConnectionInfo> {
        let state = lock(&self.inner.state);
        state
            .entries
            .values()
            .map(|stored| state.snapshot(stored))
            .collect()
    }

    /// A snapshot of the entry `id`, if the table still has it.
    pub fn connection(&self, id: ConnectionId) -> Option<ConnectionInfo> {
        let state = lock(&self.inner.state);
        state.entries.get(&id).map(|stored| state.snapshot(stored))
    }

    /// Takes the events since the last call, oldest first. When some were dropped, the first
    /// is [`ConnectionEvent::Lagged`] with their number.
    pub fn events(&self) -> Vec<ConnectionEvent> {
        let mut state = lock(&self.inner.state);
        let lagged = std::mem::take(&mut state.lagged);
        let lag = (lagged > 0).then_some(ConnectionEvent::Lagged { dropped: lagged });
        lag.into_iter().chain(state.events.drain(..)).collect()
    }

    /// The table's settings with their sources.
    pub fn settings(&self) -> Vec<Setting> {
        self.inner.config.settings()
    }

    /// Adds an entry.
    pub(crate) fn open(&self, new: NewEntry) -> EntryRef {
        let config = &self.inner.config;
        let mut state = lock(&self.inner.state);
        state.last_id += 1;
        let id = ConnectionId(state.last_id);
        let info = ConnectionInfo {
            id,
            previous: new.previous,
            previous_dropped: false,
            parent: new.parent,
            peer: new.peer,
            path: new.path,
            direction: Direction::None,
            local_side: new.local_side,
            segments: new.segments,
            via: new.via,
            state: new.state,
            phase: new.phase,
            last_cause: None,
            history: Vec::new(),
            history_dropped: 0,
            links: Vec::new(),
            bulk_channels: 0,
            request_channels: 0,
            rtt: None,
            applied: Vec::new(),
            redial: None,
            p2p_id: None,
            attempt: None,
        };
        state.entries.insert(
            id,
            Stored {
                info,
                next_attempt_at: None,
                held: new.held,
                path_fixed: new.path_fixed,
                p2p_owner: None,
                channels: Vec::new(),
                open_links: Vec::new(),
                last_released: None,
                end_waits: None,
            },
        );
        state.record(config, id, None);
        EntryRef {
            table: self.clone(),
            id,
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State, &ConnectionTableConfig) -> R) -> R {
        let mut state = lock(&self.inner.state);
        f(&mut state, &self.inner.config)
    }
}

impl State {
    fn snapshot(&self, stored: &Stored) -> ConnectionInfo {
        let mut info = stored.info.clone();
        info.previous_dropped = info
            .previous
            .is_some_and(|previous| !self.entries.contains_key(&previous));
        info.direction = Direction::of(&info.links);
        if let Some(progress) = &mut info.redial {
            progress.next_attempt_in = stored
                .next_attempt_at
                .map(|at| at.saturating_duration_since(Instant::now()));
        }
        info
    }

    fn push_event(&mut self, config: &ConnectionTableConfig, event: ConnectionEvent) {
        let limit = as_bound(config.event_queue_limit());
        if limit == 0 {
            self.lagged += 1;
            return;
        }
        if self.events.len() >= limit {
            self.events.pop_front();
            self.lagged += 1;
        }
        self.events.push_back(event);
    }

    /// Adds the entry's current state and phase to its history with `cause`, and reports it.
    fn record(&mut self, config: &ConnectionTableConfig, id: ConnectionId, cause: Option<Cause>) {
        let Some(stored) = self.entries.get_mut(&id) else {
            return;
        };
        let entry = HistoryEntry {
            at: SystemTime::now(),
            state: stored.info.state,
            phase: stored.info.phase,
            cause,
        };
        let history = &mut stored.info.history;
        history.push(entry.clone());
        let limit = as_bound(config.history_limit());
        let over = history.len().saturating_sub(limit);
        history.drain(..over);
        stored.info.history_dropped += u64::try_from(over).unwrap_or(u64::MAX);
        self.push_event(config, ConnectionEvent::Transition { id, entry });
    }

    fn transition(
        &mut self,
        config: &ConnectionTableConfig,
        id: ConnectionId,
        to: ConnectionState,
        phase: Option<Phase>,
        cause: Option<Cause>,
    ) {
        let Some(stored) = self.entries.get_mut(&id) else {
            return;
        };
        let info = &mut stored.info;
        if info.state == ConnectionState::Disconnected {
            return;
        }
        let phase = if to == ConnectionState::Disconnected {
            None
        } else {
            phase
        };
        if info.state == to && info.phase == phase && cause.is_none() {
            return;
        }
        info.state = to;
        info.phase = phase;
        if let Some(cause) = &cause {
            info.last_cause = Some(cause.clone());
        } else if to == ConnectionState::Connected {
            // The last transition had no cause.
            info.last_cause = None;
        }
        if to == ConnectionState::Disconnected {
            stored.held = false;
            stored.next_attempt_at = None;
            if let Some(progress) = &mut info.redial {
                progress.next_attempt_in = None;
            }
        }
        self.record(config, id, cause);
        if to == ConnectionState::Disconnected {
            self.closed.push_back(id);
            let keep = as_bound(config.keep_closed());
            while self.closed.len() > keep {
                if let Some(old) = self.closed.pop_front() {
                    self.drop_entry(config, old);
                }
            }
        }
    }

    /// [`EntryRef::step_owned`] on the entry `id`.
    fn step(
        &mut self,
        config: &ConnectionTableConfig,
        id: ConnectionId,
        phase: Phase,
        cause: Cause,
    ) {
        let Some(current) = self.entries.get(&id).map(|s| s.info.state) else {
            return;
        };
        match current {
            ConnectionState::Connecting | ConnectionState::Reconnecting => {
                self.transition(config, id, current, Some(phase), Some(cause));
            }
            ConnectionState::Connected => self.record(config, id, Some(cause)),
            ConnectionState::Disconnected => {}
        }
    }

    /// [`EntryRef::end`] on the entry `id`.
    fn end(&mut self, config: &ConnectionTableConfig, id: ConnectionId, cause: Cause) {
        let Some(stored) = self.entries.get_mut(&id) else {
            return;
        };
        if stored.held && stored.info.state != ConnectionState::Connected {
            stored.info.last_cause = Some(cause);
            return;
        }
        self.transition(config, id, ConnectionState::Disconnected, None, Some(cause));
    }

    /// [`EntryRef::end_owned`] on the entry `id`, once `owner` owns it; `rule` chooses from the
    /// entry's state and last cause.
    fn end_by(
        &mut self,
        config: &ConnectionTableConfig,
        id: ConnectionId,
        owner: Option<u64>,
        rule: impl FnOnce(ConnectionState, Option<&Cause>) -> EndBy,
    ) {
        let Some(stored) = self.entries.get_mut(&id) else {
            return;
        };
        if !stored.owned_by(owner) {
            return;
        }
        match rule(stored.info.state, stored.info.last_cause.as_ref()) {
            EndBy::Connection(cause) => self.end(config, id, cause),
            EndBy::LastLink => {
                let last = stored.last_released.clone();
                let ended = last.as_ref().map(|link| lock(&link.cause).clone());
                match (last, ended) {
                    (Some(link), Some(None)) if stored.info.state == ConnectionState::Connected => {
                        stored.end_waits = Some((owner, link));
                    }
                    (_, Some(Some(cause))) => self.end(config, id, cause),
                    _ => {
                        let cause = last_link_cause(&stored.info);
                        self.end(config, id, cause);
                    }
                }
            }
        }
    }

    fn drop_entry(&mut self, config: &ConnectionTableConfig, id: ConnectionId) {
        if self.entries.remove(&id).is_some() {
            self.closed.retain(|c| *c != id);
            self.push_event(config, ConnectionEvent::Removed { id });
        }
    }
}

/// What a new entry starts with.
pub(crate) struct NewEntry {
    pub previous: Option<ConnectionId>,
    pub parent: Option<ConnectionId>,
    pub peer: ConnectionPeer,
    pub path: PathKind,
    pub local_side: Side,
    pub segments: Vec<Segment>,
    pub via: Via,
    pub state: ConnectionState,
    pub phase: Option<Phase>,
    pub held: bool,
    pub path_fixed: bool,
}

/// A reconnection that found a live shared tunnel: its own entry (`pending`) waits for the
/// handshake on the shared tunnel and then ends naming the entry it joined (`into`). A
/// failed handshake leaves `pending` to the redialer (or ends it with the failure).
#[derive(Clone, Debug)]
pub(crate) struct Joined {
    pub pending: EntryRef,
    pub into: ConnectionId,
    /// The handshake's result was recorded (shared by the clones).
    settled: Arc<AtomicBool>,
}

impl Joined {
    pub(crate) fn new(pending: EntryRef, into: ConnectionId) -> Self {
        Self {
            pending,
            into,
            settled: Arc::default(),
        }
    }

    /// The handshake on the shared tunnel succeeded.
    pub(crate) fn succeeded(&self) {
        self.settled.store(true, Ordering::Release);
        self.pending.transition(
            ConnectionState::Disconnected,
            None,
            Some(Cause::SharedTunnel(self.into)),
        );
    }

    /// The handshake on the shared tunnel failed with `cause`.
    pub(crate) fn failed(&self, cause: Cause) {
        self.settled.store(true, Ordering::Release);
        self.pending.end(cause);
    }

    /// The channel went without a handshake result: it ends as `ended` (a redialer's entry
    /// only keeps the cause). After a result nothing changes.
    pub(crate) fn abandoned(&self) {
        if !self.settled.load(Ordering::Acquire) {
            self.pending.end(Cause::Ended);
        }
    }
}

/// Ends a reconnection entry whose attempt on a shared connection was dropped without a
/// handshake result (a redialer's entry only keeps the cause).
pub(crate) struct JoinGuard(pub(crate) Joined);

impl Drop for JoinGuard {
    fn drop(&mut self) {
        self.0.abandoned();
    }
}

/// Which counted channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Counted {
    Bulk,
    Request,
}

/// One entry of a table, for the code that changes it.
#[derive(Clone)]
pub(crate) struct EntryRef {
    table: ConnectionTable,
    id: ConnectionId,
}

impl fmt::Debug for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EntryRef").field(&self.id).finish()
    }
}

impl EntryRef {
    pub(crate) fn id(&self) -> ConnectionId {
        self.id
    }

    /// Moves the entry to `to` and `phase` with `cause`. A `disconnected` entry stays so.
    pub(crate) fn transition(
        &self,
        to: ConnectionState,
        phase: Option<Phase>,
        cause: Option<Cause>,
    ) {
        self.table
            .with_state(|state, config| state.transition(config, self.id, to, phase, cause));
    }

    /// The path the consumer's profile chose (`with_holder`); the first link no longer
    /// changes it.
    #[cfg(feature = "ssh")]
    pub(crate) fn choose_path(&self, holder: Holder) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                stored.info.path = match holder {
                    Holder::Dialer => PathKind::SshForward,
                    Holder::Acceptor => PathKind::SshRoleReversed,
                };
                stored.path_fixed = true;
            }
        });
    }

    /// Moves a connection being set up to `phase`, keeping its state.
    pub(crate) fn phase(&self, phase: Phase) {
        self.table.with_state(|state, config| {
            let Some(current) = state.entries.get(&self.id).map(|s| s.info.state) else {
                return;
            };
            if matches!(
                current,
                ConnectionState::Connecting | ConnectionState::Reconnecting
            ) {
                state.transition(config, self.id, current, Some(phase), None);
            }
        });
    }

    /// Moves a connection being set up to `phase` with `cause`, keeping its state. On an entry
    /// past its setup (`connected`) it only adds `cause` to the history, with the state and
    /// phase it has; on a `disconnected` entry it does nothing.
    /// All of it only while `owner` owns the entry ([`EntryRef::own_p2p`]; `None` always),
    /// checked in one step with the change.
    pub(crate) fn step_owned(&self, owner: Option<u64>, phase: Phase, cause: Cause) {
        self.table.with_state(|state, config| {
            if state
                .entries
                .get(&self.id)
                .is_some_and(|s| s.owned_by(owner))
            {
                state.step(config, self.id, phase, cause);
            }
        });
    }

    /// The end of a failed attempt or of the physical connection: `disconnected` with
    /// `cause`, unless the redialer holds the entry, which then only keeps the cause.
    pub(crate) fn end(&self, cause: Cause) {
        self.table
            .with_state(|state, config| state.end(config, self.id, cause));
    }

    /// [`EntryRef::end`] while the P2P connection `owner` owns the entry, checked in one step
    /// with the end; `rule` chooses the cause ([`EndBy`]) from the entry's state and last cause
    /// in the same step.
    pub(crate) fn end_owned(
        &self,
        owner: u64,
        rule: impl FnOnce(ConnectionState, Option<&Cause>) -> EndBy,
    ) {
        self.table
            .with_state(|state, config| state.end_by(config, self.id, Some(owner), rule));
    }

    /// [`EntryRef::end_owned`] for an entry no P2P connection owns (an SSH tunnel's).
    #[cfg(feature = "ssh")]
    pub(crate) fn end_by(&self, rule: impl FnOnce(ConnectionState, Option<&Cause>) -> EndBy) {
        self.table
            .with_state(|state, config| state.end_by(config, self.id, None, rule));
    }

    /// Makes the P2P connection `owner` (its number in the process) the one whose thread
    /// changes the entry from here on, and applies `f`, in one step.
    pub(crate) fn own_p2p(&self, owner: u64, f: impl FnOnce(&mut ConnectionInfo)) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                stored.p2p_owner = Some(owner);
                f(&mut stored.info);
            }
        });
    }

    /// Takes the entry from the P2P connection that owned it and applies `f`, in one step:
    /// the redialer's next attempt fills it, and the thread of the earlier connection no
    /// longer changes it.
    pub(crate) fn disown_p2p(&self, f: impl FnOnce(&mut ConnectionInfo)) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                stored.p2p_owner = None;
                f(&mut stored.info);
            }
        });
    }

    /// `f` on the entry while `owner` owns it, checked in one step with the change; whether
    /// it ran.
    pub(crate) fn update_owned(
        &self,
        owner: Option<u64>,
        f: impl FnOnce(&mut ConnectionInfo),
    ) -> bool {
        self.table
            .with_state(|state, _| match state.entries.get_mut(&self.id) {
                Some(stored) if stored.owned_by(owner) => {
                    f(&mut stored.info);
                    true
                }
                _ => false,
            })
    }

    /// Records `cause` as the last one without a transition, while the entry is not yet
    /// connected.
    pub(crate) fn note_cause(&self, cause: Cause) {
        self.update(|info| {
            if info.state != ConnectionState::Connected {
                info.last_cause = Some(cause);
            }
        });
    }

    /// Takes a connection that joined another entry out of the table.
    pub(crate) fn remove(&self) {
        self.table
            .with_state(|state, config| state.drop_entry(config, self.id));
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut ConnectionInfo)) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                f(&mut stored.info);
            }
        });
    }

    /// The entry's state and last cause, if the table still has it.
    pub(crate) fn status(&self) -> Option<(ConnectionState, Option<Cause>)> {
        self.table.with_state(|state, _| {
            state
                .entries
                .get(&self.id)
                .map(|s| (s.info.state, s.info.last_cause.clone()))
        })
    }

    /// Sets whether the redialer holds the entry, and when its next attempt starts.
    pub(crate) fn hold(&self, held: bool, next_attempt_at: Option<Instant>) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                stored.held = held;
                stored.next_attempt_at = next_attempt_at;
            }
        });
    }

    /// An attach link stands on the entry: the peer the handshake proved, the link's row
    /// (until the link ends) and the applied timeouts; the entry is `connected`.
    pub(crate) fn link_up(
        &self,
        peer: &PeerInfo,
        link_id: LinkId,
        closer: &LinkCloser,
        timeouts: &LinkTimeouts,
    ) {
        let Some(holder) = peer.holder else {
            return;
        };
        let link = LinkInfo {
            link_id,
            holder,
            local_side: peer.local_side,
            transport: EdgeTransport::of(peer.transport, holder),
            opened_at: SystemTime::now(),
            rtt: None,
            applied: link_timeouts(timeouts),
        };
        self.table.with_state(|state, _| {
            let Some(stored) = state.entries.get_mut(&self.id) else {
                return;
            };
            let info = &mut stored.info;
            if !stored.path_fixed && info.links.is_empty() {
                match (info.path, holder) {
                    (PathKind::SshForward, Holder::Acceptor) => {
                        info.path = PathKind::SshRoleReversed;
                    }
                    (PathKind::SshRoleReversed, Holder::Dialer) => {
                        info.path = PathKind::SshForward;
                    }
                    _ => {}
                }
                stored.path_fixed = true;
            }
            info.links.push(link);
        });
        self.peer_up(peer, timeouts);
        let entry = self.clone();
        on_end(closer, move |_| {
            entry.update(|info| info.links.retain(|l| l.link_id != link_id));
        });
    }

    /// A bulk or request channel stands on the entry (counted until it ends, or until its
    /// `parent` link ends, which closes it); the entry is `connected`.
    pub(crate) fn channel_up(
        &self,
        kind: Counted,
        peer: &PeerInfo,
        closer: &LinkCloser,
        parent: Option<&LinkCloser>,
        timeouts: &LinkTimeouts,
    ) {
        self.update(|info| *counter(info, kind) += 1);
        if let Some(parent) = parent {
            let (channel, parent) = (closer.first(), parent.watch());
            self.table.with_state(|state, _| {
                if let Some(stored) = state.entries.get_mut(&self.id) {
                    let link = stored
                        .open_links
                        .iter()
                        .find(|(w, _)| w.same(&parent))
                        .map(|(_, link)| Arc::clone(link));
                    stored.hold_for(&channel, link);
                }
            });
        }
        self.peer_up(peer, timeouts);
        let entry = self.clone();
        on_end_under(closer, parent, move |_| {
            entry.update(|info| {
                let n = counter(info, kind);
                *n = n.saturating_sub(1);
            });
        });
    }

    /// Ends the entry (see [`EntryRef::end`]) when the link or channel of `closer` ends, or
    /// when its `parent` link ends, which closes it.
    pub(crate) fn end_with(&self, closer: &LinkCloser, parent: Option<&LinkCloser>) {
        let entry = self.clone();
        on_end_under(closer, parent, move |cause| entry.end(cause));
    }

    /// The link of `closer` is on the entry: when it ends, the entry keeps its cause as the
    /// last one if it is the entry's only link left, and its channels (its first, and the bulk
    /// ones [`EntryRef::channel_up`] adds) carry its end, which an end by [`EndBy::LastLink`]
    /// takes when one of them gave the last place back. An entry that already ended keeps its
    /// cause (docs/design/transport.md "필드", `last_cause`).
    pub(crate) fn note_end_of(&self, closer: &LinkCloser, link_id: LinkId) {
        let id = self.id;
        let end = Arc::new(LinkEnd::default());
        let (first, watch) = (closer.first(), closer.watch());
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&id) {
                stored.open_links.push((watch, Arc::clone(&end)));
                stored.hold_for(&first, Some(Arc::clone(&end)));
            }
        });
        let table = self.table.clone();
        on_end(closer, move |cause| {
            *lock(&end.cause) = Some(cause.clone());
            table.with_state(|state, config| {
                let Some(stored) = state.entries.get_mut(&id) else {
                    return;
                };
                stored
                    .open_links
                    .retain(|(_, link)| !Arc::ptr_eq(link, &end));
                let info = &mut stored.info;
                if info.state != ConnectionState::Disconnected
                    && info.links.iter().all(|l| l.link_id == link_id)
                {
                    info.last_cause = Some(cause);
                }
                let waited = stored
                    .end_waits
                    .take_if(|(_, waited)| Arc::ptr_eq(waited, &end));
                if let Some((owner, _)) = waited
                    && stored.info.state == ConnectionState::Connected
                {
                    state.end_by(config, id, owner, |_, _| EndBy::LastLink);
                }
            });
        });
    }

    /// `control`'s channel holds a place on the entry's connection or tunnel, until
    /// [`EntryRef::channel_released`].
    pub(crate) fn channel_opened(&self, control: &Arc<dyn crate::channel::ChannelControl>) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                stored.channels.push(HeldChannel {
                    control: Arc::downgrade(control),
                    link: None,
                });
            }
        });
    }

    /// `control`'s channel gave its place back: the end of its link, if it is a link's, is the
    /// one an end by [`EndBy::LastLink`] takes, until another link's channel gives one back. A
    /// channel of no link (a request channel) leaves the last link's.
    pub(crate) fn channel_released(
        &self,
        control: &std::sync::Weak<dyn crate::channel::ChannelControl>,
    ) {
        self.table.with_state(|state, _| {
            if let Some(stored) = state.entries.get_mut(&self.id) {
                let held = stored
                    .channels
                    .iter()
                    .position(|c| c.control.ptr_eq(control))
                    .map(|at| stored.channels.swap_remove(at));
                if let Some(link) = held.and_then(|c| c.link) {
                    stored.last_released = Some(link);
                }
            }
        });
    }
}

impl Stored {
    /// The channel of `control` belongs to the link of `link`.
    fn hold_for(
        &mut self,
        control: &std::sync::Weak<dyn crate::channel::ChannelControl>,
        link: Option<Arc<LinkEnd>>,
    ) {
        if let Some(held) = self.channels.iter_mut().find(|c| c.control.ptr_eq(control)) {
            held.link = link;
        }
    }
}

impl EntryRef {
    fn peer_up(&self, peer: &PeerInfo, timeouts: &LinkTimeouts) {
        self.update(|info| {
            info.peer.device_id = Some(peer.device_id);
            info.peer.instance_id = Some(peer.instance_id.clone());
            info.peer.instance_verified = true;
            // The first channel's values stay: a later link or channel on the same tunnel
            // shows its own on its link row, without changing what the others run with.
            let applied = link_timeouts(timeouts);
            if !info
                .applied
                .iter()
                .any(|s| applied.iter().any(|a| a.key == s.key))
            {
                info.applied.splice(0..0, applied);
            }
        });
        self.transition(ConnectionState::Connected, None, None);
    }
}

/// The cause of an end by [`EndBy::LastLink`]: the last one recorded, unless it is a copied P2P
/// transition, which is no failure of this end; `ended` without one.
fn last_link_cause(info: &ConnectionInfo) -> Cause {
    info.last_cause
        .clone()
        .filter(|c| !matches!(c, Cause::P2p(_)))
        .unwrap_or(Cause::Ended)
}

fn counter(info: &mut ConnectionInfo, kind: Counted) -> &mut u64 {
    match kind {
        Counted::Bulk => &mut info.bulk_channels,
        Counted::Request => &mut info.request_channels,
    }
}

/// Runs `f` with the end's cause once, when the link of `closer` ends or, if it never ends,
/// when its closer is dropped.
fn on_end(closer: &LinkCloser, f: impl FnOnce(Cause) + Send + 'static) {
    on_end_under(closer, None, f);
}

/// [`on_end`] for a channel under the link of `parent`: the parent's end closes the channel
/// without ending its own closer (a consumer may still hold it), so it also runs `f`, with
/// the parent's cause. The parent holds the mark weakly and drops its record once the
/// channel's own end ran.
fn on_end_under(
    closer: &LinkCloser,
    parent: Option<&LinkCloser>,
    f: impl FnOnce(Cause) + Send + 'static,
) {
    let mark = Arc::new(EndMark {
        run: Mutex::new(Some(Box::new(f))),
        watch: closer.watch(),
    });
    if let Some(parent) = parent {
        parent.on_end_of_child(&(Arc::clone(&mark) as Arc<dyn ChildEnd>));
    }
    closer.on_end(Box::new(move || mark.fire()));
}

type EndFn = Box<dyn FnOnce(Cause) + Send>;

/// Runs its function once: when the end hook runs, or when the hook is dropped unrun.
struct EndMark {
    run: Mutex<Option<EndFn>>,
    watch: crate::end::CloserWatch,
}

impl EndMark {
    fn fire(&self) {
        self.fire_by(&self.watch);
    }

    /// Runs the function with the cause `watch` reads.
    fn fire_by(&self, watch: &crate::end::CloserWatch) {
        let Some(run) = lock(&self.run).take() else {
            return;
        };
        let cause = match watch.raised() {
            Some(EndReason::Lost) => Cause::Lost,
            // A link whose read failed first was lost, though this side closed it after.
            Some(EndReason::Forced) if watch.lost_seen() => Cause::Lost,
            Some(EndReason::Forced) => Cause::ClosedHere,
            None => Cause::Ended,
        };
        run(cause);
    }
}

impl ChildEnd for EndMark {
    fn parent_ended(&self, parent: &crate::end::CloserWatch) {
        self.fire_by(parent);
    }
}

impl Drop for EndMark {
    fn drop(&mut self) {
        self.fire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memory_channel_pair;

    fn closer() -> LinkCloser {
        let (a, _b) = memory_channel_pair();
        LinkCloser::new(a.control())
    }

    /// Bulk channels opened and closed one after another under one parent link leave no
    /// growing list of end records or hooks on the parent.
    #[test]
    fn closed_channels_leave_no_end_records_on_their_parent() {
        let parent = closer();
        let ended = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..8 {
            let child = closer();
            // As the accepting side registers a bulk channel: its count and its entry.
            for _ in 0..2 {
                let ended = Arc::clone(&ended);
                on_end_under(&child, Some(&parent), move |cause| {
                    lock(&ended).push(cause);
                });
            }
            child.force_close(EndReason::Forced);
            drop(child);
            assert!(
                parent.child_end_count() <= 2,
                "{} records",
                parent.child_end_count()
            );
            assert_eq!(parent.hook_count(), 0);
        }
        assert_eq!(
            lock(&ended).len(),
            16,
            "each channel's own end ran its records"
        );

        // A channel still open when the parent ends hears it, with the parent's cause.
        let child = closer();
        let ended = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&ended);
        on_end_under(&child, Some(&parent), move |cause| lock(&seen).push(cause));
        parent.force_close(EndReason::Forced);
        assert_eq!(*lock(&ended), [Cause::ClosedHere]);
        assert_eq!(parent.child_end_count(), 0);
        drop(child);
        assert_eq!(lock(&ended).len(), 1, "the record ran once");
    }
}
