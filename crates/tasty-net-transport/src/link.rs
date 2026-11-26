//! A link: the channels one handshake authenticated (docs/design/transport.md "전송 trait").

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_frame::HEARTBEAT_TIMEOUT;
use tasty_net_identity::DeviceId;

use crate::channel::{
    ChannelConfig, ChannelControl, ChannelReader, ChannelWriter, DEFAULT_DRAIN_CAP,
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
    /// with the role proposal timeout added, and the drain cap.
    pub(crate) fn check(&self) -> Result<(), HandshakeError> {
        let longest = self
            .handshake_timeout
            .checked_add(self.role_proposal_timeout)
            .ok_or(HandshakeError::InvalidTimeouts)?;
        deadline_after(longest)?;
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
    /// Force closes a link that is still open. A link that already ended normally is left
    /// as it is: its ordered channel was half-closed both ways, and closing it again would
    /// turn that end into a reset.
    fn drop(&mut self) {
        if !self.closer.ended_normally() {
            self.closer.force_close(EndReason::Forced);
        }
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
