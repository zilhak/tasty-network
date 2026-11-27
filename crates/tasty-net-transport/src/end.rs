//! The end of a link: a reason that only rises, one settle point, and the forced close that
//! wakes every blocked read (docs/design/transport.md "시도 취소, 닫기, 회수").

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::channel::ChannelControl;
use crate::lock::lock;

const SETTLED: u8 = 0x80;
const REASON: u8 = 0x7f;

/// Why a link ended. Ordered so that a forced end outranks a plain loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndReason {
    /// The peer or the path went away, or the drain cap ran out.
    Lost = 1,
    /// This side (or its consumer) closed the link on purpose.
    Forced = 2,
}

fn reason_of(bits: u8) -> Option<EndReason> {
    match bits {
        1 => Some(EndReason::Lost),
        2 => Some(EndReason::Forced),
        _ => None,
    }
}

/// Raised reason plus a settled bit in one atomic byte.
#[derive(Debug, Default)]
struct EndState(AtomicU8);

impl EndState {
    fn raise(&self, reason: EndReason) {
        self.0.fetch_max(reason as u8, Ordering::AcqRel);
    }

    fn settle(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                (v & SETTLED == 0 && v & REASON != 0).then_some(v | SETTLED)
            })
            .is_ok()
    }

    fn settled(&self) -> Option<EndReason> {
        let v = self.0.load(Ordering::Acquire);
        if v & SETTLED == 0 {
            return None;
        }
        reason_of(v & REASON)
    }

    fn is_ending(&self) -> bool {
        self.0.load(Ordering::Acquire) & REASON != 0
    }

    fn raised(&self) -> Option<EndReason> {
        reason_of(self.0.load(Ordering::Acquire) & REASON)
    }
}

type CloseHook = Box<dyn Fn() + Send + Sync>;

/// The consumer's registration guard of a bulk channel, shared by the channel and its parent:
/// whichever ends first drops it.
pub(crate) type GuardSlot = Mutex<Option<Box<dyn Any + Send>>>;

struct CloserInner {
    end: EndState,
    /// The link's first channel (the ordered channel of an attach link).
    first: Weak<dyn ChannelControl>,
    /// The bulk channels opened under the link. The list holds them weakly: a channel whose halves (and handshake) are gone closes its
    /// socket at once and drops out here, instead of living as long as the parent link.
    channels: Mutex<Vec<Weak<dyn ChannelControl>>>,
    /// The registration guards of the bulk channels under the link, dropped when it ends.
    guards: Mutex<Vec<Weak<GuardSlot>>>,
    /// Run once when the link ends: the first forced close or the normal end (the bulk
    /// credential revocation of an attach link).
    hooks: Mutex<Vec<CloseHook>>,
    /// The link ended (forced or normal); no channel is adopted any more.
    ended: AtomicBool,
}

/// The cloneable closer of one link.
///
/// The end reason only rises and is settled once: the first caller of [`LinkCloser::settle`]
/// wins and the reason it saw is final. Waking blocked socket I/O
/// ([`LinkCloser::force_close`]) happens whatever the settle result; telling the consumer
/// about the end is for the winner of `settle` only.
#[derive(Clone)]
pub struct LinkCloser {
    inner: Arc<CloserInner>,
}

impl std::fmt::Debug for LinkCloser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkCloser")
            .field("settled", &self.settled())
            .field("ending", &self.is_ending())
            .finish_non_exhaustive()
    }
}

impl LinkCloser {
    pub(crate) fn new(first: Arc<dyn ChannelControl>) -> Self {
        Self {
            inner: Arc::new(CloserInner {
                end: EndState::default(),
                first: Arc::downgrade(&first),
                channels: Mutex::new(Vec::new()),
                guards: Mutex::new(Vec::new()),
                hooks: Mutex::new(Vec::new()),
                ended: AtomicBool::new(false),
            }),
        }
    }

    /// Adds a channel that the end of this link also closes. Returns false, adding nothing, when
    /// the link already ended or is ending.
    pub(crate) fn adopt(&self, channel: &Arc<dyn ChannelControl>) -> bool {
        let mut channels = lock(&self.inner.channels);
        if self.has_ended() {
            return false;
        }
        channels.retain(|c| c.strong_count() > 0);
        channels.push(Arc::downgrade(channel));
        true
    }

    /// Drops the guard in `slot` when this link ends, unless the bulk channel that holds the
    /// slot dropped it first. A link that already ended drops it at once.
    pub(crate) fn adopt_guard(&self, slot: &Arc<GuardSlot>) {
        let mut guards = lock(&self.inner.guards);
        if self.has_ended() {
            drop(guards);
            drop(lock(slot).take());
            return;
        }
        guards.retain(|g| g.strong_count() > 0);
        guards.push(Arc::downgrade(slot));
    }

    /// Drops the guards of the bulk channels still alive under the link.
    fn drop_guards(&self) {
        let guards = std::mem::take(&mut *lock(&self.inner.guards));
        for slot in guards.iter().filter_map(Weak::upgrade) {
            drop(lock(&slot).take());
        }
    }

    /// Runs `hook` once when the link ends. A link that already ended runs it at once.
    pub(crate) fn on_end(&self, hook: CloseHook) {
        let mut hooks = lock(&self.inner.hooks);
        if self.has_ended() {
            drop(hooks);
            hook();
            return;
        }
        hooks.push(hook);
    }

    /// A handle that reads the end reason without keeping the closer alive.
    pub(crate) fn watch(&self) -> CloserWatch {
        CloserWatch(Arc::downgrade(&self.inner))
    }

    /// Whether the link ended normally: it ended and no reason was raised.
    #[cfg(test)]
    pub(crate) fn ended_normally(&self) -> bool {
        self.inner.ended.load(Ordering::Acquire) && !self.inner.end.is_ending()
    }

    fn children(&self) -> Vec<Arc<dyn ChannelControl>> {
        lock(&self.inner.channels)
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn has_ended(&self) -> bool {
        self.inner.ended.load(Ordering::Acquire) || self.inner.end.is_ending()
    }

    /// The normal end of the link (both directions half-closed and the reader ended): ends the
    /// first channel's sending direction only (TCP FIN, QUIC `finish`, so its last frames still
    /// arrive), force closes the bulk channels still open under the link, and runs the end
    /// hooks, without raising a reason (docs/design/transport.md "P2P의 닫기 범위").
    pub(crate) fn end_normally(&self) {
        let hooks = self.mark_ended(None);
        if let Some(first) = self.inner.first.upgrade()
            && let Err(error) = first.shutdown_write()
        {
            tracing::debug!(%error, "ending the sending direction of the first channel failed");
        }
        for child in self.children() {
            shutdown_quietly(child.as_ref());
        }
        self.drop_guards();
        for hook in hooks {
            hook();
        }
    }

    /// Marks the link ended (raising `reason` if any) under the locks `adopt` and `on_end`
    /// read the state under, and takes the hooks. A channel or hook added alongside is either
    /// in the lists or handled by its caller.
    fn mark_ended(&self, reason: Option<EndReason>) -> Vec<CloseHook> {
        let channels = lock(&self.inner.channels);
        let guards = lock(&self.inner.guards);
        let mut hooks = lock(&self.inner.hooks);
        if let Some(reason) = reason {
            self.inner.end.raise(reason);
        }
        self.inner.ended.store(true, Ordering::Release);
        drop(guards);
        drop(channels);
        std::mem::take(&mut *hooks)
    }

    /// Raises the reason. A lower reason never replaces a higher one, and a settled reason
    /// stays.
    pub fn raise(&self, reason: EndReason) {
        self.inner.end.raise(reason);
    }

    /// Settles a raised, unsettled end. True for exactly one caller per link.
    pub fn settle(&self) -> bool {
        self.inner.end.settle()
    }

    /// The final reason, once settled.
    pub fn settled(&self) -> Option<EndReason> {
        self.inner.end.settled()
    }

    /// Whether any reason was raised, settled or not.
    pub fn is_ending(&self) -> bool {
        self.inner.end.is_ending()
    }

    /// Raises `reason`, then closes every channel of the link both ways and wakes reads that
    /// are blocked on them (on Windows this cancels the pending socket I/O too). Calling it
    /// again only raises the reason and wakes again.
    pub fn force_close(&self, reason: EndReason) {
        let hooks = self.mark_ended(Some(reason));
        self.wake();
        self.drop_guards();
        for hook in hooks {
            hook();
        }
    }

    /// Closes every channel both ways and wakes blocked reads, without raising a reason.
    pub(crate) fn wake(&self) {
        if let Some(first) = self.inner.first.upgrade() {
            shutdown_quietly(first.as_ref());
        }
        for child in self.children() {
            shutdown_quietly(child.as_ref());
        }
    }
}

/// Reads a closer's raised reason without keeping it alive (the connection table's end
/// hooks hold it, and the closer holds the hooks).
pub(crate) struct CloserWatch(Weak<CloserInner>);

impl CloserWatch {
    /// The highest reason raised, settled or not; `None` for a normal end or a closer that is
    /// gone.
    pub(crate) fn raised(&self) -> Option<EndReason> {
        self.0.upgrade().and_then(|inner| inner.end.raised())
    }
}

/// Closes a channel both ways, logging instead of failing: the socket may already be gone.
pub(crate) fn shutdown_quietly(channel: &dyn ChannelControl) {
    if let Err(error) = channel.shutdown_and_wake() {
        tracing::debug!(%error, "closing a channel both ways failed");
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

    #[test]
    fn an_open_end_does_not_settle() {
        let c = closer();
        assert!(!c.is_ending());
        assert!(!c.settle());
        assert_eq!(c.settled(), None);
    }

    #[test]
    fn a_raised_reason_only_rises_before_settling() {
        let c = closer();
        c.raise(EndReason::Forced);
        c.raise(EndReason::Lost);
        assert_eq!(c.settled(), None);
        assert!(c.settle());
        assert_eq!(c.settled(), Some(EndReason::Forced));
    }

    #[test]
    fn the_first_settle_wins_and_its_reason_is_final() {
        let c = closer();
        c.raise(EndReason::Lost);
        let other = c.clone();
        assert!(c.settle());
        assert!(!other.settle());
        other.force_close(EndReason::Forced);
        assert_eq!(c.settled(), Some(EndReason::Lost));
    }
    #[test]
    fn a_hook_added_after_the_forced_close_runs_at_once() {
        let c = closer();
        let ran = Arc::new(AtomicU8::new(0));
        let seen = Arc::clone(&ran);
        c.on_end(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        c.force_close(EndReason::Forced);
        c.force_close(EndReason::Forced);
        assert_eq!(ran.load(Ordering::SeqCst), 1, "a hook runs once");
        let late = Arc::clone(&ran);
        c.on_end(Box::new(move || {
            late.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(ran.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_ended_link_adopts_nothing() {
        let c = closer();
        let (extra, _peer) = memory_channel_pair();
        assert!(c.adopt(&extra.control()));
        c.end_normally();
        assert!(!c.is_ending(), "a normal end raises no reason");
        let (late, _peer2) = memory_channel_pair();
        assert!(!c.adopt(&late.control()));
    }

    #[test]
    fn a_dropped_channel_leaves_the_list_and_a_live_guard_drops_with_the_link() {
        let c = closer();
        let (gone, _peer) = memory_channel_pair();
        let control = gone.control();
        assert!(c.adopt(&control));
        drop(gone);
        drop(control);
        let (kept, _peer2) = memory_channel_pair();
        assert!(c.adopt(&kept.control()));
        // The dropped bulk channel was pruned; only the kept one is still listed.
        assert_eq!(lock(&c.inner.channels).len(), 1);
        let dropped = Arc::new(AtomicU8::new(0));
        struct Guard(Arc<AtomicU8>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let slot: Arc<GuardSlot> =
            Arc::new(Mutex::new(Some(Box::new(Guard(Arc::clone(&dropped))))));
        c.adopt_guard(&slot);
        c.force_close(EndReason::Forced);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "the parent's end drops it"
        );
        assert!(lock(&slot).is_none());
    }

    /// A normal end only ends the first channel's sending direction, so its peer's last
    /// bytes are still read; a bulk channel still open is closed both ways.
    #[test]
    fn a_normal_end_half_closes_the_first_channel_and_closes_the_bulk_ones() {
        use std::io::{Read, Write};
        let (mut first, mut first_peer) = memory_channel_pair();
        let (mut bulk, mut bulk_peer) = memory_channel_pair();
        let c = LinkCloser::new(first.control());
        assert!(c.adopt(&bulk.control()));
        first_peer.write_all(b"last").expect("write");
        bulk_peer.write_all(b"lost").expect("write");
        c.end_normally();
        assert!(c.ended_normally());
        let mut got = [0u8; 4];
        first
            .read_exact(&mut got)
            .expect("the first channel still reads");
        assert_eq!(&got, b"last");
        let mut eof = [0u8; 1];
        assert_eq!(
            first_peer.read(&mut eof).expect("eof"),
            0,
            "the peer sees FIN"
        );
        assert_eq!(
            bulk.read(&mut eof).expect("closed"),
            0,
            "the bulk bytes are dropped"
        );
        c.force_close(EndReason::Forced);
        assert!(!c.ended_normally());
    }
}
