//! A link: the channels one handshake authenticated (docs/design/transport.md "전송 trait").

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_frame::HEARTBEAT_TIMEOUT;
use tasty_net_identity::DeviceId;

use crate::channel::{
    ChannelConfig, ChannelControl, ChannelReader, ChannelShared, ChannelWriter, DEFAULT_DRAIN_CAP,
};
use crate::end::{EndReason, GuardSlot, LinkCloser};
use crate::handshake::{DialConfig, dial_bulk};
use crate::heartbeat::HeartbeatPolicy;
use crate::reject::{HandshakeError, P2pCloseReason};
use crate::role::{Holder, Roles, Side, roles};
use crate::transport::{DialAuth, Endpoint, TransportKind};

/// Timeouts of one link. Both ends are expected to use the same values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkTimeouts {
    /// Read timeout of every channel after the ACK. A Ping renews it. The Ping interval is
    /// [`HEARTBEAT_INTERVAL`](tasty_net_frame::HEARTBEAT_INTERVAL), or a sixth of a shorter
    /// read timeout.
    pub read_timeout: Duration,
    /// Timeout of one blocking write.
    pub write_timeout: Duration,
    /// From message 1 to the ACK. The wait for a role reversal approval is not counted.
    pub handshake_timeout: Duration,
    /// How long the accepting end waits for the consumer to approve a role reversal.
    pub role_proposal_timeout: Duration,
    /// How long the writer's end waits for the reader to end ([`ChannelWriter::finish`]).
    pub drain_cap: Duration,
}

impl Default for LinkTimeouts {
    fn default() -> Self {
        Self {
            read_timeout: HEARTBEAT_TIMEOUT,
            write_timeout: HEARTBEAT_TIMEOUT,
            handshake_timeout: Duration::from_secs(10),
            role_proposal_timeout: Duration::from_secs(60),
            drain_cap: DEFAULT_DRAIN_CAP,
        }
    }
}

/// `now + limit`, or [`HandshakeError::InvalidTimeouts`] when that is past what an
/// [`Instant`] holds.
pub(crate) fn deadline_after(limit: Duration) -> Result<Instant, HandshakeError> {
    Instant::now()
        .checked_add(limit)
        .ok_or(HandshakeError::InvalidTimeouts)
}

impl LinkTimeouts {
    /// Checks that the deadlines these timeouts make can be computed: the handshake timeout
    /// with the role proposal timeout added, the read and write timeouts, and the drain cap.
    pub(crate) fn check(&self) -> Result<(), HandshakeError> {
        let longest = self
            .handshake_timeout
            .checked_add(self.role_proposal_timeout)
            .ok_or(HandshakeError::InvalidTimeouts)?;
        deadline_after(longest)?;
        deadline_after(self.read_timeout)?;
        deadline_after(self.write_timeout)?;
        deadline_after(self.drain_cap)?;
        Ok(())
    }

    pub(crate) fn channel(&self, heartbeat: HeartbeatPolicy) -> ChannelConfig {
        ChannelConfig {
            read_timeout: self.read_timeout,
            write_timeout: self.write_timeout,
            heartbeat,
            drain_cap: self.drain_cap,
        }
    }
}

/// The peer as the handshake proved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub(crate) device_id: DeviceId,
    pub(crate) instance_id: String,
    pub(crate) holder: Option<Holder>,
    pub(crate) caps: Vec<String>,
    pub(crate) transport: TransportKind,
    pub(crate) local_side: Side,
}

impl PeerInfo {
    /// The peer's device ID (SSH: proved by its signature, P2P: the connection's remote ID).
    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// The peer's instance ID.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The holder of an attach link; `None` for a request channel.
    pub fn holder(&self) -> Option<Holder> {
        self.holder
    }

    /// The peer's capability names.
    pub fn caps(&self) -> &[String] {
        &self.caps
    }

    /// Whether the peer named capability `name`.
    pub fn has_cap(&self, name: &str) -> bool {
        self.caps.iter().any(|c| c == name)
    }

    /// The transport of the channel, as this end sees it.
    pub fn transport(&self) -> TransportKind {
        self.transport
    }

    /// Whether this end opened the channel or accepted it.
    pub fn local_side(&self) -> Side {
        self.local_side
    }

    /// Every role of an attach link; `None` for a request channel.
    pub fn roles(&self) -> Option<Roles> {
        self.holder.map(|h| roles(self.transport, h))
    }

    /// Whether this end holds the workspace (false for a request channel).
    pub fn local_holds(&self) -> bool {
        self.holder.is_some_and(|h| h.side() == self.local_side)
    }
}

/// What the dialer of an attach link needs to open its bulk channels.
pub(crate) struct BulkDial {
    pub endpoint: Endpoint,
    pub credential: [u8; 32],
    pub config: DialConfig,
    pub auth: DialAuth,
}

/// An authenticated attach link: the ordered channel and the bulk channels opened under it.
///
/// Dropping the link ends it: every channel of the link, the taken ordered halves included,
/// is closed both ways, and the bulk credential is revoked. Keep it alive as long as the
/// link is used.
pub struct Link {
    pub(crate) ordered: Option<(ChannelReader, ChannelWriter)>,
    pub(crate) closer: LinkCloser,
    pub(crate) control: Arc<dyn ChannelControl>,
    pub(crate) peer: PeerInfo,
    pub(crate) client_id: Option<u32>,
    pub(crate) ext: Option<String>,
    pub(crate) bulk: Option<BulkDial>,
    pub(crate) _guard: Option<Box<dyn Any + Send>>,
    /// The ordered channel's shared state, so dropping the link stops and joins its
    /// heartbeat even while the consumer still holds the taken writer.
    pub(crate) ordered_shared: Arc<ChannelShared>,
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("peer", &self.peer)
            .field("client_id", &self.client_id)
            .field("ordered_taken", &self.ordered.is_none())
            .finish_non_exhaustive()
    }
}

impl Link {
    /// The reader and writer of the ordered channel. Only the first call returns them.
    pub fn take_ordered(&mut self) -> Option<(ChannelReader, ChannelWriter)> {
        self.ordered.take()
    }

    /// Opens a bulk channel under this link with its bulk credential. Only the dialer of an
    /// attach link can; the accepting end receives bulk channels through
    /// [`accept_handshake`](crate::accept_handshake).
    pub fn open_bulk(&self, ext: Option<&str>) -> Result<BulkChannel, HandshakeError> {
        dial_bulk(self, ext)
    }

    /// The peer.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// The closer of the link.
    pub fn closer(&self) -> LinkCloser {
        self.closer.clone()
    }

    /// The ACK `client_id`: the number the accepting end gave the connection.
    pub fn client_id(&self) -> Option<u32> {
        self.client_id
    }

    /// The domain `ext`: on the dialer the success ACK's, on the accepting end the request's.
    pub fn ext(&self) -> Option<&str> {
        self.ext.as_deref()
    }

    /// Why the QUIC connection under the link was closed, once it was closed with a code
    /// (P2P only).
    pub fn close_reason(&self) -> Option<P2pCloseReason> {
        self.control.close_reason()
    }
}

impl Drop for Link {
    /// Force closes a link that is still open. A link whose normal end was already decided
    /// (even if its end hook is still on the way) is left as it is: its ordered channel was
    /// half-closed both ways, and closing it again would turn that end into a reset. The
    /// decision and this check share one lock in the channel's state.
    fn drop(&mut self) {
        if self.ordered_shared.claim_forced_end() {
            self.closer.force_close(EndReason::Forced);
        }
        self.ordered_shared.stop_heartbeat();
    }
}

/// A bulk channel. Its own end is separate from the parent link's, but the parent's end
/// closes it.
pub struct BulkChannel {
    /// Read half.
    pub reader: ChannelReader,
    /// Write half.
    pub writer: ChannelWriter,
    /// The closer of this bulk channel alone.
    pub closer: LinkCloser,
    pub(crate) peer: PeerInfo,
    pub(crate) ext: Option<String>,
    /// The consumer's registration guard; the parent's end drops it too.
    pub(crate) _guard: Arc<GuardSlot>,
}

impl fmt::Debug for BulkChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BulkChannel")
            .field("peer", &self.peer)
            .field("ext", &self.ext)
            .finish_non_exhaustive()
    }
}

impl BulkChannel {
    /// The peer of the parent link.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// The domain `ext`: on the dialer the success ACK's, on the accepting end the request's.
    pub fn ext(&self) -> Option<&str> {
        self.ext.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memory_channel_pair;
    use tasty_net_identity::DeviceKey;

    fn peer() -> PeerInfo {
        PeerInfo {
            device_id: DeviceKey::from_seed([1; 32]).id(),
            instance_id: "peer".into(),
            holder: Some(Holder::Dialer),
            caps: Vec::new(),
            transport: TransportKind::Memory,
            local_side: Side::Dialer,
        }
    }

    /// The normal end of taken halves and the link's drop overlap: the end is decided (both
    /// directions done) but its hook has not yet marked the closer when the link drops. The
    /// drop must not reset the ordered channel, so the peer still reads the last frame and
    /// then EOF.
    #[test]
    fn a_link_dropped_while_its_normal_end_runs_does_not_reset_the_channel() {
        use std::sync::Barrier;
        use tasty_net_frame::StreamTag;

        let (a, b) = memory_channel_pair();
        let control = a.control();
        let framed = a
            .framed(&LinkTimeouts::default().channel(HeartbeatPolicy::WhenIdle))
            .expect("framed");
        let mut peer_side = b
            .framed(&LinkTimeouts::default().channel(HeartbeatPolicy::WhenIdle))
            .expect("peer framed");
        let closer = framed.closer.clone();
        // The hook stops at the barrier before it marks the closer.
        let decided = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let (hook_decided, hook_resume, normal) =
            (Arc::clone(&decided), Arc::clone(&resume), closer.clone());
        framed.on_normal_end(Box::new(move || {
            hook_decided.wait();
            hook_resume.wait();
            normal.end_normally();
        }));
        let mut link = Link {
            ordered_shared: framed.shared(),
            ordered: Some((framed.reader, framed.writer)),
            closer,
            control,
            peer: peer(),
            client_id: None,
            ext: None,
            bulk: None,
            _guard: None,
        };
        let (mut reader, mut writer) = link.take_ordered().expect("ordered");
        writer.write_frame(StreamTag::Data, b"last").expect("write");
        writer.half_close().expect("half close");
        // The peer ends its side; our reader sees EOF and the normal end is decided.
        peer_side.writer.half_close().expect("peer half close");
        let ending = std::thread::spawn(move || {
            assert!(reader.read_frame().is_err(), "EOF");
        });
        decided.wait();
        drop(link);
        resume.wait();
        ending.join().expect("reader thread");
        assert_eq!(
            peer_side
                .reader
                .read_frame()
                .expect("the last frame")
                .payload,
            b"last"
        );
        assert!(peer_side.reader.read_frame().is_err(), "then EOF");
        drop(writer);
    }

    /// Dropping the link stops and joins the ordered channel's heartbeat even though the
    /// consumer still holds the writer it took.
    #[test]
    fn dropping_a_link_joins_the_heartbeat_of_a_kept_writer() {
        let (a, _b) = memory_channel_pair();
        let control = a.control();
        let framed = a
            .framed(&LinkTimeouts::default().channel(HeartbeatPolicy::Periodic))
            .expect("framed");
        let shared = framed.shared();
        let mut link = Link {
            ordered: Some((framed.reader, framed.writer)),
            closer: framed.closer,
            control,
            peer: PeerInfo {
                device_id: DeviceKey::from_seed([1; 32]).id(),
                instance_id: "peer".into(),
                holder: Some(Holder::Dialer),
                caps: Vec::new(),
                transport: TransportKind::Memory,
                local_side: Side::Dialer,
            },
            client_id: None,
            ext: None,
            bulk: None,
            _guard: None,
            ordered_shared: Arc::clone(&shared),
        };
        let (_reader, writer) = link.take_ordered().expect("ordered");
        assert!(shared.heartbeat_running());
        drop(link);
        assert!(!shared.heartbeat_running(), "the link's drop joined it");
        drop(writer);
    }

    /// A link dropped while another caller (the writer) is still joining the heartbeat waits
    /// until that join completed, so its drop never returns with the worker still alive.
    #[test]
    fn dropping_a_link_waits_for_a_heartbeat_join_in_progress() {
        use crate::channel::HeartbeatSlot;

        let (a, _b) = memory_channel_pair();
        let control = a.control();
        let framed = a
            .framed(&LinkTimeouts::default().channel(HeartbeatPolicy::Periodic))
            .expect("framed");
        let shared = framed.shared();
        let mut link = Link {
            ordered_shared: Arc::clone(&shared),
            ordered: Some((framed.reader, framed.writer)),
            closer: framed.closer,
            control,
            peer: peer(),
            client_id: None,
            ext: None,
            bulk: None,
            _guard: None,
        };
        let halves = link.take_ordered().expect("ordered");
        // Another caller took the worker out and is joining it.
        shared.set_heartbeat_slot(HeartbeatSlot::Stopping);
        assert!(shared.heartbeat_running(), "not joined yet");
        let dropping = std::thread::spawn(move || drop(link));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!dropping.is_finished(), "the drop waits for the join");
        shared.set_heartbeat_slot(HeartbeatSlot::Stopped);
        dropping.join().expect("drop");
        assert!(!shared.heartbeat_running());
        drop(halves);
    }
}
