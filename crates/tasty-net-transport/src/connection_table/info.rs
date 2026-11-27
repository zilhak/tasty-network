//! What the connection table shows: one [`ConnectionInfo`] per physical connection
//! (docs/design/transport.md "필드").

use std::fmt;
use std::time::{Duration, SystemTime};

use tasty_net_identity::DeviceId;
use tasty_net_p2p::{AttemptInfo, AttemptPhase, NatInfo, P2pCloseReason, P2pConnectError};
use tasty_net_p2p::{RemoteKind, RendezvousServer};
#[cfg(feature = "ssh")]
use tasty_net_ssh::PortDiscoveryFailureKind;

use crate::link_id::LinkId;
use crate::reject::{HandshakeError, RejectReason};
use crate::role::Holder;
use crate::role::Side;
use crate::settings::Setting;
use crate::transport::{ConnectError, TransportKind};

/// The number of an entry in its [`ConnectionTable`](crate::ConnectionTable). It rises with
/// every new entry and is never given again, also after the entry was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(pub(crate) u64);

impl ConnectionId {
    /// The number.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where the connection was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathKind {
    /// An SSH tunnel this side started, for the forward direction the profile chose.
    SshForward,
    /// An SSH tunnel this side started, for the role reversal the profile chose.
    SshRoleReversed,
    /// A loopback TCP connection this side accepted (the far end of an SSH forward).
    SshInbound,
    /// A QUIC connection over a punched UDP path.
    P2pUdp,
}

impl PathKind {
    /// `ssh_forward`, `ssh_role_reversed`, `ssh_inbound` or `p2p_udp`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SshForward => "ssh_forward",
            Self::SshRoleReversed => "ssh_role_reversed",
            Self::SshInbound => "ssh_inbound",
            Self::P2pUdp => "p2p_udp",
        }
    }
}

/// The summary of the directions of the attach links on an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Every attach link has holder = dialer.
    Forward,
    /// Every attach link has holder = acceptor.
    RoleReversed,
    /// Both kinds of attach link are on the entry.
    Mixed,
    /// No attach link is on the entry.
    None,
}

impl Direction {
    /// `forward`, `role_reversed`, `mixed` or `none`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::RoleReversed => "role_reversed",
            Self::Mixed => "mixed",
            Self::None => "none",
        }
    }

    pub(crate) fn of(links: &[LinkInfo]) -> Self {
        let forward = links.iter().any(|l| l.holder == Holder::Dialer);
        let reversed = links.iter().any(|l| l.holder == Holder::Acceptor);
        match (forward, reversed) {
            (true, true) => Self::Mixed,
            (true, false) => Self::Forward,
            (false, true) => Self::RoleReversed,
            (false, false) => Self::None,
        }
    }
}

/// The state of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Being set up (tunnel, port discovery, handshake).
    Connecting,
    /// A link (or channel) stands on it.
    Connected,
    /// Being set up as the reconnection of the entry in `previous`.
    Reconnecting,
    /// Ended. The cause is in `last_cause`.
    Disconnected,
}

impl ConnectionState {
    /// `connecting`, `connected`, `reconnecting` or `disconnected`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Disconnected => "disconnected",
        }
    }
}

/// The step an entry that is not yet connected is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Reading the peer's connection record over SSH.
    PortDiscovery,
    /// Starting the SSH tunnel.
    Tunnel,
    /// Handshake v2 on the first channel.
    Handshake,
    /// The redialer waits for its next attempt.
    RetryWait,
    /// A P2P attempt in this state of the P2P state machine (its values are the entry's
    /// `attempt`).
    P2p(AttemptPhase),
}

impl Phase {
    /// `port_discovery`, `tunnel`, `handshake` or `retry_wait`, or the P2P state's name
    /// (`Requesting`, `Punching` and so on).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PortDiscovery => "port_discovery",
            Self::Tunnel => "tunnel",
            Self::Handshake => "handshake",
            Self::RetryWait => "retry_wait",
            Self::P2p(phase) => phase.name(),
        }
    }
}

/// Why an entry changed: the cause of its last transition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cause {
    /// Port discovery over SSH failed.
    #[cfg(feature = "ssh")]
    Discovery(PortDiscoveryFailureKind),
    /// The peer's record has no token: it is too old for handshake v2.
    PeerTooOld,
    /// The SSH tunnel did not come up (the tunnel error's text).
    TunnelFailed(String),
    /// The SSH tunnel exited.
    TunnelExited,
    /// Nothing listens on the remote port behind the SSH tunnel.
    RemoteListenerAbsent,
    /// The channel could not be opened (the I/O error's text).
    ConnectFailed(String),
    /// The peer refused the handshake.
    Rejected(RejectReason),
    /// This side refused the peer.
    Refused(RejectReason),
    /// The handshake timed out.
    HandshakeTimedOut,
    /// The peer closed the channel during the handshake.
    PeerClosed,
    /// Another handshake failure, one that connecting again does not change (the error's
    /// text).
    HandshakeFailed(String),
    /// The attempt was cancelled.
    Cancelled,
    /// The link or channel was lost: the peer or the path went away, or a heartbeat expired.
    Lost,
    /// This side closed it.
    ClosedHere,
    /// It ended normally (both directions closed), or its last channel was dropped.
    Ended,
    /// The new connection found a live tunnel to the same record and uses that tunnel's
    /// entry.
    SharedTunnel(ConnectionId),
    /// The redialer gave up (`redial.give_up_after` or `redial.max_attempts`).
    RedialExhausted,
    /// The P2P endpoint could not make the connection, a self attach included (the attempt
    /// error of `tasty-net-p2p`).
    P2pConnect(P2pConnectError),
    /// A transition of the P2P connection, as `tasty-net-p2p` names its cause (the punch
    /// steps, `quic_established`, `remote_address_observed_change` and so on).
    P2p(tasty_net_p2p::Cause),
    /// The P2P connection was closed by the peer, or by `tasty-net-p2p`, with this code.
    P2pClosed(P2pCloseReason),
    /// The P2P endpoint cannot make connections: it is off, closed, set up wrong or used by
    /// another endpoint (the error's text). Connecting again does not change it.
    P2pUnavailable(String),
}

impl Cause {
    /// The cause's code.
    pub fn as_str(&self) -> &'static str {
        match self {
            #[cfg(feature = "ssh")]
            Self::Discovery(_) => "discovery_failed",
            Self::PeerTooOld => "peer_too_old",
            Self::TunnelFailed(_) => "tunnel_failed",
            Self::TunnelExited => "tunnel_exited",
            Self::RemoteListenerAbsent => "remote_listener_absent",
            Self::ConnectFailed(_) => "connect_failed",
            Self::Rejected(_) => "rejected",
            Self::Refused(_) => "refused",
            Self::HandshakeTimedOut => "handshake_timed_out",
            Self::PeerClosed => "peer_closed",
            Self::HandshakeFailed(_) => "handshake_failed",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
            Self::ClosedHere => "closed_here",
            Self::Ended => "ended",
            Self::SharedTunnel(_) => "shared_tunnel",
            Self::RedialExhausted => "redial_exhausted",
            Self::P2pConnect(_) => "p2p_connect_failed",
            Self::P2p(cause) => cause.name,
            Self::P2pClosed(_) => "p2p_closed",
            Self::P2pUnavailable(_) => "p2p_unavailable",
        }
    }

    /// Whether connecting again can give another result, as
    /// [`HandshakeError::is_retryable`] says for the error the cause came from. Not retried:
    /// this side closed it, the attempt was cancelled, the peer is too old, this side refused
    /// the peer, the peer refused it for a reason other than [`RejectReason::Busy`], and the
    /// other handshake failures ([`Cause::HandshakeFailed`]).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Rejected(reason) => reason.is_retryable(),
            Self::P2pConnect(error) => error.is_retryable(),
            Self::P2pClosed(reason) => reason.is_retryable(),
            Self::P2p(cause) => match &cause.detail {
                Some(tasty_net_p2p::CauseDetail::Close(reason)) => reason.is_retryable(),
                Some(tasty_net_p2p::CauseDetail::Connect(error)) => error.is_retryable(),
                _ => true,
            },
            Self::PeerTooOld
            | Self::Refused(_)
            | Self::HandshakeFailed(_)
            | Self::P2pUnavailable(_)
            | Self::Cancelled
            | Self::ClosedHere => false,
            #[cfg(feature = "ssh")]
            Self::Discovery(_) => true,
            Self::TunnelFailed(_)
            | Self::TunnelExited
            | Self::RemoteListenerAbsent
            | Self::ConnectFailed(_)
            | Self::HandshakeTimedOut
            | Self::PeerClosed
            | Self::Lost
            | Self::Ended
            | Self::SharedTunnel(_)
            | Self::RedialExhausted => true,
        }
    }

    pub(crate) fn of_connect(error: &ConnectError) -> Self {
        match error {
            ConnectError::Cancelled => Self::Cancelled,
            ConnectError::Io(e) => Self::ConnectFailed(e.to_string()),
            #[cfg(feature = "ssh")]
            ConnectError::Discovery(e) => Self::Discovery(e.kind()),
            #[cfg(feature = "ssh")]
            ConnectError::Tunnel(e) => Self::TunnelFailed(e.to_string()),
            ConnectError::PeerTooOld { .. } => Self::PeerTooOld,
            ConnectError::SelfAttach => Self::P2pConnect(P2pConnectError::SelfAttach),
            ConnectError::P2p(e) => match e.connect_error() {
                Some(attempt) => Self::P2pConnect(*attempt),
                // A timeout or an I/O failure: connecting again can give another result.
                None if error.is_retryable() => Self::ConnectFailed(e.to_string()),
                None => Self::P2pUnavailable(e.to_string()),
            },
        }
    }

    pub(crate) fn of_handshake(error: &HandshakeError) -> Self {
        match error {
            HandshakeError::Rejected { reason, .. } => Self::Rejected(reason.clone()),
            HandshakeError::Refused(reason) => Self::Refused(reason.clone()),
            HandshakeError::PeerTooOld { .. } => Self::PeerTooOld,
            HandshakeError::TimedOut => Self::HandshakeTimedOut,
            HandshakeError::Cancelled => Self::Cancelled,
            HandshakeError::PeerClosed => Self::PeerClosed,
            #[cfg(feature = "ssh")]
            HandshakeError::RemoteListenerAbsent(_) => Self::RemoteListenerAbsent,
            HandshakeError::Connect(e) => Self::of_connect(e),
            HandshakeError::Io(e) => Self::ConnectFailed(e.to_string()),
            // A QUIC close during the handshake keeps its code, the plain close (code 0)
            // too (docs/design/transport.md "필드", `last_cause`).
            HandshakeError::Closed(reason) => Self::P2pClosed(*reason),
            other => Self::HandshakeFailed(other.to_string()),
        }
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "ssh")]
            Self::Discovery(kind) => write!(f, "{}: {kind}", self.as_str()),
            Self::TunnelFailed(text)
            | Self::ConnectFailed(text)
            | Self::HandshakeFailed(text)
            | Self::P2pUnavailable(text) => write!(f, "{}: {text}", self.as_str()),
            Self::Rejected(reason) | Self::Refused(reason) => {
                write!(f, "{}: {reason}", self.as_str())
            }
            Self::SharedTunnel(id) => write!(f, "{}: {id}", self.as_str()),
            Self::P2pConnect(error) => write!(f, "{}: {error}", self.as_str()),
            Self::P2p(cause) => write!(f, "{cause}"),
            Self::P2pClosed(reason) => write!(f, "{}: {reason}", self.as_str()),
            other => f.write_str(other.as_str()),
        }
    }
}

/// One address segment of an entry. Which side holds an address depends on the segment:
///
/// | `name` | `local` | `remote` |
/// |---|---|---|
/// | `local_forward` | the tunnel's local end, `127.0.0.1:<port>` | none (the channels opened to it) |
/// | `ssh` | none | the profile's SSH target, `<host>` or `<host>:<port>` as given |
/// | `remote_listener` | none | the discovered port of the peer's loopback listener |
/// | `loopback` | the listener's address | the peer's address (the sshd side) |
/// | `udp` | `local_bind=<addr>, local_ip=<ip>, local_reflexive=<addr>` (`unknown` for a value not known) | the peer address the QUIC connection uses now |
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// `local_forward`, `ssh`, `remote_listener`, `loopback` or `udp`.
    pub name: &'static str,
    /// The local address, when the segment has one.
    pub local: Option<String>,
    /// The remote address, when the segment has one. On `local_forward` it is the local
    /// addresses of the channels open on the forwarding end, joined with
    #[cfg_attr(
        feature = "ssh",
        doc = " [`FORWARDED_SEPARATOR`](crate::FORWARDED_SEPARATOR), and none without channels."
    )]
    #[cfg_attr(
        not(feature = "ssh"),
        doc = " `FORWARDED_SEPARATOR` (an item of the SSH transport), and none without channels."
    )]
    pub remote: Option<String>,
}

/// How the connection was reached.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Via {
    /// Through SSH: the profile's target and the discovered remote port (none before
    /// discovery).
    Ssh {
        /// The SSH target as the profile gives it.
        target: String,
        /// The discovered port of the peer's loopback listener.
        remote_port: Option<u16>,
    },
    /// Accepted on a loopback listener.
    Loopback,
    /// Introduced by a rendezvous server and punched (P2P).
    Rendezvous {
        /// The server's address and key, once the P2P connection shows them.
        server: Option<RendezvousServer>,
        /// The kind of the peer candidate that succeeded.
        candidate: Option<RemoteKind>,
        /// Both NAT mappings and the punch mode.
        nat: Option<NatInfo>,
    },
}

/// The peer of an entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionPeer {
    /// The device ID the handshake proved.
    pub device_id: Option<DeviceId>,
    /// The instance ID (from the record before the handshake, from the handshake after it).
    pub instance_id: Option<String>,
    /// Whether a handshake confirmed `instance_id`. False while it only comes from the
    /// discovery record, which is what a connection that failed before its handshake keeps.
    pub instance_verified: bool,
    /// The display name the consumer gave.
    pub name: Option<String>,
}

/// One transition of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// When it happened.
    pub at: SystemTime,
    /// The state after it.
    pub state: ConnectionState,
    /// The phase after it.
    pub phase: Option<Phase>,
    /// Its cause.
    pub cause: Option<Cause>,
}

/// The value a topology edge of an attach link gets, from the link's holder and the path
/// kind (docs/design/transport.md "토폴로지와의 관계"). It is decided per link, never from
/// the entry's [`Direction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeTransport {
    /// An SSH link with holder = dialer.
    SshForward,
    /// An SSH link with holder = acceptor.
    SshRoleReversed,
    /// A P2P link.
    P2pUdp,
}

impl EdgeTransport {
    /// `ssh_forward`, `ssh_role_reversed` or `p2p_udp`, the values of topology's
    /// `TransportKind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SshForward => "ssh_forward",
            Self::SshRoleReversed => "ssh_role_reversed",
            Self::P2pUdp => "p2p_udp",
        }
    }

    /// The value of a link on `kind` with `holder`. Both ends of an SSH link give the same
    /// value: the side that started the tunnel sees `Ssh`, the side that accepted the
    /// loopback connection `Tcp`.
    pub(crate) fn of(kind: TransportKind, holder: Holder) -> Self {
        match (kind, holder) {
            (TransportKind::P2p, _) => Self::P2pUdp,
            (_, Holder::Dialer) => Self::SshForward,
            (_, Holder::Acceptor) => Self::SshRoleReversed,
        }
    }
}

/// An attach link on an entry.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkInfo {
    /// The link ID, the same on both ends.
    pub link_id: LinkId,
    /// Which end of the link holds the workspace.
    pub holder: Holder,
    /// Whether this device opened the link or accepted it. It may differ from the entry's
    /// `local_side` (which is about the physical connection).
    pub local_side: Side,
    /// The topology edge value of this link.
    pub transport: EdgeTransport,
    /// When the link was opened.
    pub opened_at: SystemTime,
    /// The round-trip time (none for SSH).
    pub rtt: Option<Duration>,
    /// The `link.*` values this link runs with, with their sources. Links on one entry may
    /// run with different values; the entry's own `applied` keeps those of the channel that
    /// first connected it.
    pub applied: Vec<Setting>,
}

/// The progress of the redialer on an entry it set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedialProgress {
    /// The attempt the redialer is at (1 for the first).
    pub attempt: u64,
    /// The time left until the next attempt, while it waits.
    pub next_attempt_in: Option<Duration>,
}

/// One entry of the connection table: one physical connection.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ConnectionInfo {
    /// This entry's number.
    pub id: ConnectionId,
    /// The entry this one reconnects.
    pub previous: Option<ConnectionId>,
    /// Whether `previous` names an entry the table already dropped.
    pub previous_dropped: bool,
    /// The entry of the parent link (a bulk channel accepted under it).
    pub parent: Option<ConnectionId>,
    /// The peer.
    pub peer: ConnectionPeer,
    /// Where the connection was observed.
    pub path: PathKind,
    /// The summary of the directions of its attach links.
    pub direction: Direction,
    /// Whether this device opened the physical connection or accepted it.
    pub local_side: Side,
    /// The address segments.
    pub segments: Vec<Segment>,
    /// How it was reached.
    pub via: Via,
    /// Its state.
    pub state: ConnectionState,
    /// The step of a connection being set up.
    pub phase: Option<Phase>,
    /// The cause of the last transition that had one.
    pub last_cause: Option<Cause>,
    /// The latest transitions, oldest first (at most `connections.history_limit`).
    pub history: Vec<HistoryEntry>,
    /// How many older transitions were dropped from `history`.
    pub history_dropped: u64,
    /// The attach links on it.
    pub links: Vec<LinkInfo>,
    /// The open bulk channels on it.
    pub bulk_channels: u64,
    /// The open request channels on it.
    pub request_channels: u64,
    /// The round-trip time (none for SSH).
    pub rtt: Option<Duration>,
    /// The settings applied to it, with their sources.
    pub applied: Vec<Setting>,
    /// The redialer's progress, when the redialer set this entry up.
    pub redial: Option<RedialProgress>,
    /// The number of the P2P connection of a `p2p_udp` entry (`P2pConnectionInfo::id`), once
    /// the attempt shows it.
    pub p2p_id: Option<u64>,
    /// The P2P attempt's number and times of a `p2p_udp` entry, as the P2P connection shows
    /// them.
    pub attempt: Option<AttemptInfo>,
}

/// What the table reports as it changes.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConnectionEvent {
    /// An entry was added (its first transition) or made a transition.
    Transition {
        /// The entry.
        id: ConnectionId,
        /// The transition.
        entry: HistoryEntry,
    },
    /// A `disconnected` entry was dropped (`connections.keep_closed`), or an entry for a
    /// connection that joined a shared tunnel was taken out.
    Removed {
        /// The entry.
        id: ConnectionId,
    },
    /// This many older events were dropped because nobody read them
    /// (`connections.event_queue_limit`).
    Lagged {
        /// How many.
        dropped: u64,
    },
}
