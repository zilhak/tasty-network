//! The connection table as FFI records (docs/design/transport.md "연결 목록", android #8).
//!
//! Each record copies a snapshot of the core value; nothing here holds the table. A cause
//! keeps its core code, English text and `is_retryable()` answer, so that an app branches on
//! [`CauseKind`] and shows the text, and never compares text.

use std::time::{Duration, SystemTime};

use tasty_net_transport::settings::Setting;
use tasty_net_transport::{
    Cause, ConnectionEvent, ConnectionInfo, ConnectionState, Direction, HistoryEntry, Holder,
    LinkInfo, PathKind, Segment, Side, TransportKind, Via,
};

use tasty_net_p2p::{AttemptInfo, NatInfo};

use crate::error::{P2pClose, P2pConnectFailure, Rejection};
use crate::secrets::Secrets;

/// The wire name of a value the rendezvous protocol names in snake case (`NatMapping`,
/// `PunchMode`): this crate reads it without depending on the rendezvous crate.
fn wire_name(value: Result<serde_json::Value, serde_json::Error>) -> String {
    value
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// A NAT mapping judgement (`NatMapping`, docs/design/p2p.md "NAT 판별").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NatMappingKind {
    /// Endpoint-independent mapping, judged with a second server IP.
    Independent,
    /// The two ports of one server IP got the same mapping (no second IP).
    IndependentPortOnly,
    /// The mapping depends on the destination.
    Dependent,
    /// Not judged or incomplete.
    Unknown,
    /// A judgement this version does not know, with its wire name.
    Other {
        /// The wire name.
        code: String,
    },
}

impl NatMappingKind {
    fn from_wire(code: String) -> Self {
        match code.as_str() {
            "independent" => Self::Independent,
            "independent_port_only" => Self::IndependentPortOnly,
            "dependent" => Self::Dependent,
            "unknown" => Self::Unknown,
            _ => Self::Other { code },
        }
    }
}

/// Which candidates a punch uses (`PunchMode`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PunchModeKind {
    /// Every kind.
    All,
    /// Host candidates only (both devices are `dependent`).
    HostOnly,
    /// A mode this version does not know, with its wire name.
    Other {
        /// The wire name.
        code: String,
    },
}

impl PunchModeKind {
    fn from_wire(code: String) -> Self {
        match code.as_str() {
            "all" => Self::All,
            "host_only" => Self::HostOnly,
            _ => Self::Other { code },
        }
    }
}

/// The NAT values of one punch (`NatInfo`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NatRecord {
    /// This device's judgement.
    pub local: NatMappingKind,
    /// The peer's, from the server's `punch_go`.
    pub peer: NatMappingKind,
    /// The punch mode.
    pub mode: PunchModeKind,
}

impl From<&NatInfo> for NatRecord {
    fn from(nat: &NatInfo) -> Self {
        Self {
            local: NatMappingKind::from_wire(wire_name(serde_json::to_value(nat.local))),
            peer: NatMappingKind::from_wire(wire_name(serde_json::to_value(nat.peer))),
            mode: PunchModeKind::from_wire(wire_name(serde_json::to_value(nat.mode))),
        }
    }
}

/// One candidate address of the peer (`RemoteCandidate`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CandidateRecord {
    /// The address, `ip:port`.
    pub addr: String,
    /// `server_reflexive`, `reported_reflexive`, `host` or `peer_reflexive`.
    pub kind: String,
}

/// Where a P2P attempt is, with the values of its state (`AttemptInfo`, docs/design/p2p.md
/// "연결 시도 상태").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AttemptRecord {
    /// The punch attempt number, from 1 (0 before the first `punch_go` on the accepting
    /// side).
    pub number: u32,
    /// While waiting to retry: the time to the next attempt.
    pub retry_in: Option<Duration>,
    /// The time left of the attempt's whole bound (dialing side).
    pub bound_in: Option<Duration>,
    /// While punching: the peer's candidates now, peer reflexive ones included.
    pub candidates: Vec<CandidateRecord>,
    /// While punching: the time to the end of the probe window.
    pub window_end_in: Option<Duration>,
    /// While handshaking: this side's address of the handshake (may be a wildcard).
    pub handshake_local: Option<String>,
    /// While handshaking: the peer's address of the handshake.
    pub handshake_remote: Option<String>,
    /// While handshaking: the time to the handshake deadline.
    pub handshake_deadline_in: Option<Duration>,
}

impl From<&AttemptInfo> for AttemptRecord {
    fn from(attempt: &AttemptInfo) -> Self {
        Self {
            number: attempt.number,
            retry_in: attempt.retry_in,
            bound_in: attempt.bound_in,
            candidates: attempt
                .candidates
                .iter()
                .map(|c| CandidateRecord {
                    addr: c.addr.to_string(),
                    kind: c.kind.name().to_string(),
                })
                .collect(),
            window_end_in: attempt.window_end_in,
            handshake_local: attempt
                .handshake_pair
                .and_then(|pair| pair.local)
                .map(|addr| addr.to_string()),
            handshake_remote: attempt.handshake_pair.map(|pair| pair.remote.to_string()),
            handshake_deadline_in: attempt.handshake_deadline_in,
        }
    }
}

/// Where a connection was observed (`PathKind`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionPath {
    /// An SSH tunnel this side started, forward.
    SshForward,
    /// An SSH tunnel this side started, for a role reversal.
    SshRoleReversed,
    /// A loopback TCP connection this side accepted.
    SshInbound,
    /// A QUIC connection over a punched UDP path.
    P2pUdp,
    /// A path this version does not know, with its code.
    Other {
        /// The core code.
        code: String,
    },
}

impl From<PathKind> for ConnectionPath {
    fn from(path: PathKind) -> Self {
        match path {
            PathKind::SshForward => Self::SshForward,
            PathKind::SshRoleReversed => Self::SshRoleReversed,
            PathKind::SshInbound => Self::SshInbound,
            PathKind::P2pUdp => Self::P2pUdp,
            other => Self::Other {
                code: other.as_str().to_string(),
            },
        }
    }
}

/// The summary of the directions of a connection's attach links (`Direction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LinkDirection {
    /// Every link is forward.
    Forward,
    /// Every link is a role reversal.
    RoleReversed,
    /// Both kinds.
    Mixed,
    /// No attach link.
    NoLinks,
}

impl From<Direction> for LinkDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Forward => Self::Forward,
            Direction::RoleReversed => Self::RoleReversed,
            Direction::Mixed => Self::Mixed,
            Direction::None => Self::NoLinks,
        }
    }
}

/// The state of a connection (`ConnectionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionStatus {
    /// Being set up.
    Connecting,
    /// Up.
    Connected,
    /// Being set up again after a loss.
    Reconnecting,
    /// Ended.
    Disconnected,
}

impl From<ConnectionState> for ConnectionStatus {
    fn from(state: ConnectionState) -> Self {
        match state {
            ConnectionState::Connecting => Self::Connecting,
            ConnectionState::Connected => Self::Connected,
            ConnectionState::Reconnecting => Self::Reconnecting,
            ConnectionState::Disconnected => Self::Disconnected,
        }
    }
}

/// Which end of a connection or link this device is (`Side`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LocalSide {
    /// This device opened it.
    Dialer,
    /// This device accepted it.
    Acceptor,
}

impl From<Side> for LocalSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Dialer => Self::Dialer,
            Side::Acceptor => Self::Acceptor,
        }
    }
}

/// Which end of an attach link holds the workspace (`Holder`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LinkHolder {
    /// The dialer holds: the forward direction.
    Dialer,
    /// The acceptor holds: the role reversal.
    Acceptor,
}

impl From<Holder> for LinkHolder {
    fn from(holder: Holder) -> Self {
        match holder {
            Holder::Dialer => Self::Dialer,
            Holder::Acceptor => Self::Acceptor,
        }
    }
}

impl From<LinkHolder> for Holder {
    fn from(holder: LinkHolder) -> Self {
        match holder {
            LinkHolder::Dialer => Self::Dialer,
            LinkHolder::Acceptor => Self::Acceptor,
        }
    }
}

/// Which transport a channel came from (`TransportKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChannelTransport {
    /// Through an SSH tunnel this side opened.
    Ssh,
    /// A TCP connection accepted on a loopback listener.
    Tcp,
    /// A QUIC stream of a P2P connection.
    P2p,
    /// An in-process pipe (tests).
    Memory,
}

impl From<TransportKind> for ChannelTransport {
    fn from(kind: TransportKind) -> Self {
        match kind {
            TransportKind::Ssh => Self::Ssh,
            TransportKind::Tcp => Self::Tcp,
            TransportKind::P2p => Self::P2p,
            TransportKind::Memory => Self::Memory,
        }
    }
}

/// The kind of a cause (`Cause`), with the values an app branches on.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum CauseKind {
    /// The peer's record has no token: it is too old for handshake v2.
    PeerTooOld,
    /// The SSH tunnel did not come up.
    TunnelFailed,
    /// The SSH tunnel exited.
    TunnelExited,
    /// Nothing listens on the remote port behind the SSH tunnel.
    RemoteListenerAbsent,
    /// The channel could not be opened.
    ConnectFailed,
    /// The peer refused the handshake.
    Rejected {
        /// The peer's reason.
        reason: Rejection,
    },
    /// This side refused the peer.
    Refused {
        /// The reason.
        reason: Rejection,
    },
    /// The handshake timed out.
    HandshakeTimedOut,
    /// The peer closed the channel during the handshake.
    PeerClosed,
    /// Another handshake failure that connecting again does not change.
    HandshakeFailed,
    /// The attempt was cancelled.
    Cancelled,
    /// The link or channel was lost.
    Lost,
    /// This side closed it.
    ClosedHere,
    /// It ended normally.
    Ended,
    /// The connection uses the live tunnel of entry `id`.
    SharedTunnel {
        /// The entry it joined.
        id: u64,
    },
    /// The redialer gave up.
    RedialExhausted,
    /// The P2P endpoint could not make the connection.
    P2pConnect {
        /// Why.
        failure: P2pConnectFailure,
    },
    /// A transition of the P2P connection, named as `tasty-net-p2p` names it.
    P2p {
        /// The P2P cause name.
        name: String,
    },
    /// The P2P connection was closed with this code.
    P2pClosed {
        /// The close reason.
        reason: P2pClose,
    },
    /// The P2P endpoint cannot make connections.
    P2pUnavailable,
    /// A cause this version does not know.
    Other,
}

/// Why an entry changed: the cause of a transition.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionCause {
    /// The kind.
    pub kind: CauseKind,
    /// The core code (`peer_too_old`, `lost` and so on), for logs.
    pub code: String,
    /// The core's English text.
    pub text: String,
    /// Whether connecting again can give another result (the core's `is_retryable()`).
    pub retryable: bool,
}

impl ConnectionCause {
    /// The cause with its peer-made reason code and text scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        let kind = match self.kind {
            CauseKind::Rejected { reason } => CauseKind::Rejected {
                reason: reason.scrubbed(secrets),
            },
            CauseKind::Refused { reason } => CauseKind::Refused {
                reason: reason.scrubbed(secrets),
            },
            other => other,
        };
        Self {
            kind,
            code: secrets.scrub(&self.code),
            text: secrets.scrub(&self.text),
            retryable: self.retryable,
        }
    }
}

impl From<&Cause> for ConnectionCause {
    fn from(cause: &Cause) -> Self {
        let kind = match cause {
            Cause::PeerTooOld => CauseKind::PeerTooOld,
            Cause::TunnelFailed(_) => CauseKind::TunnelFailed,
            Cause::TunnelExited => CauseKind::TunnelExited,
            Cause::RemoteListenerAbsent => CauseKind::RemoteListenerAbsent,
            Cause::ConnectFailed(_) => CauseKind::ConnectFailed,
            Cause::Rejected(reason) => CauseKind::Rejected {
                reason: reason.into(),
            },
            Cause::Refused(reason) => CauseKind::Refused {
                reason: reason.into(),
            },
            Cause::HandshakeTimedOut => CauseKind::HandshakeTimedOut,
            Cause::PeerClosed => CauseKind::PeerClosed,
            Cause::HandshakeFailed(_) => CauseKind::HandshakeFailed,
            Cause::Cancelled => CauseKind::Cancelled,
            Cause::Lost => CauseKind::Lost,
            Cause::ClosedHere => CauseKind::ClosedHere,
            Cause::Ended => CauseKind::Ended,
            Cause::SharedTunnel(id) => CauseKind::SharedTunnel { id: id.get() },
            Cause::RedialExhausted => CauseKind::RedialExhausted,
            Cause::P2pConnect(error) => CauseKind::P2pConnect {
                failure: error.into(),
            },
            Cause::P2p(p2p) => CauseKind::P2p {
                name: p2p.name.to_string(),
            },
            Cause::P2pClosed(reason) => CauseKind::P2pClosed {
                reason: (*reason).into(),
            },
            Cause::P2pUnavailable(_) => CauseKind::P2pUnavailable,
            _ => CauseKind::Other,
        };
        Self {
            kind,
            code: cause.as_str().to_string(),
            text: cause.to_string(),
            retryable: cause.is_retryable(),
        }
    }
}

/// One address segment of a connection (`Segment`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AddressSegment {
    /// `local_forward`, `ssh`, `remote_listener`, `loopback` or `udp`.
    pub name: String,
    /// The local address, when the segment has one.
    pub local: Option<String>,
    /// The remote address, when the segment has one.
    pub remote: Option<String>,
}

impl From<&Segment> for AddressSegment {
    fn from(segment: &Segment) -> Self {
        Self {
            name: segment.name.to_string(),
            local: segment.local.clone(),
            remote: segment.remote.clone(),
        }
    }
}

/// How a connection was reached (`Via`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionVia {
    /// Through SSH.
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
        /// The server's address, once the connection shows it.
        server_addr: Option<String>,
        /// The server's key, once the connection shows it.
        server_key: Option<String>,
        /// The kind of the peer candidate that succeeded (`host`, `server_reflexive` and so
        /// on).
        candidate: Option<String>,
        /// Both NAT judgements and the punch mode, once the punch shows them.
        nat: Option<NatRecord>,
    },
    /// A way this version does not know.
    Other,
}

impl From<&Via> for ConnectionVia {
    fn from(via: &Via) -> Self {
        match via {
            Via::Ssh {
                target,
                remote_port,
            } => Self::Ssh {
                target: target.clone(),
                remote_port: *remote_port,
            },
            Via::Loopback => Self::Loopback,
            Via::Rendezvous {
                server,
                candidate,
                nat,
            } => Self::Rendezvous {
                server_addr: server.as_ref().map(|s| s.addr.to_string()),
                server_key: server.as_ref().map(|s| s.public_key.to_string()),
                candidate: candidate.map(|c| c.name().to_string()),
                nat: nat.as_ref().map(NatRecord::from),
            },
            _ => Self::Other,
        }
    }
}

/// The peer of a connection (`ConnectionPeer`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionPeerInfo {
    /// The device ID the handshake proved.
    pub device_id: Option<String>,
    /// The peer's instance ID.
    pub instance_id: Option<String>,
    /// Whether a handshake confirmed `instance_id`.
    pub instance_verified: bool,
    /// The display name the app gave.
    pub name: Option<String>,
}

/// One transition of a connection (`HistoryEntry`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Transition {
    /// When it happened.
    pub at: SystemTime,
    /// The state after it.
    pub state: ConnectionStatus,
    /// The phase after it (`port_discovery`, `handshake`, a P2P phase name and so on).
    pub phase: Option<String>,
    /// Its cause.
    pub cause: Option<ConnectionCause>,
}

impl Transition {
    /// The transition with its cause scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        Self {
            cause: self.cause.map(|c| c.scrubbed(secrets)),
            ..self
        }
    }
}

impl From<&HistoryEntry> for Transition {
    fn from(entry: &HistoryEntry) -> Self {
        Self {
            at: entry.at,
            state: entry.state.into(),
            phase: entry.phase.map(|p| p.as_str().to_string()),
            cause: entry.cause.as_ref().map(ConnectionCause::from),
        }
    }
}

/// One setting with its value in effect and source.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SettingEntry {
    /// The key.
    pub key: String,
    /// The value in effect, as the design tables write it (`50 ms`, `16`); a time value of this
    /// crate keeps the precision it was given (`1500 µs`, `1000000001 ns`).
    pub value: String,
    /// `default`, `configured` or a P2P source.
    pub source: String,
}

impl From<&Setting> for SettingEntry {
    fn from(setting: &Setting) -> Self {
        Self {
            key: setting.key.to_string(),
            value: setting.value.to_string(),
            source: setting.source.as_str().to_string(),
        }
    }
}

/// An attach link on a connection (`LinkInfo`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LinkRecord {
    /// The link ID in hex, the same on both ends.
    pub link_id: String,
    /// Which end holds the workspace.
    pub holder: LinkHolder,
    /// Whether this device opened the link.
    pub local_side: LocalSide,
    /// The topology edge value (`ssh_forward`, `ssh_role_reversed`, `p2p_udp`).
    pub edge: String,
    /// When the link was opened.
    pub opened_at: SystemTime,
    /// The round-trip time (none for SSH).
    pub rtt: Option<Duration>,
    /// The `link.*` values the link runs with.
    pub applied: Vec<SettingEntry>,
}

impl From<&LinkInfo> for LinkRecord {
    fn from(link: &LinkInfo) -> Self {
        Self {
            link_id: link.link_id.to_hex(),
            holder: link.holder.into(),
            local_side: link.local_side.into(),
            edge: link.transport.as_str().to_string(),
            opened_at: link.opened_at,
            rtt: link.rtt,
            applied: link.applied.iter().map(SettingEntry::from).collect(),
        }
    }
}

/// The redialer's progress on a connection (`RedialProgress`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RedialRecord {
    /// The attempt the redialer is at (1 for the first).
    pub attempt: u64,
    /// The time left until the next attempt, while it waits.
    pub next_attempt_in: Option<Duration>,
}

/// One physical connection of the connection table (`ConnectionInfo`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionRecord {
    /// The entry's number, never given again.
    pub id: u64,
    /// The entry this one reconnects.
    pub previous: Option<u64>,
    /// Whether `previous` names an entry the table already dropped.
    pub previous_dropped: bool,
    /// The entry of the parent link (a bulk channel accepted under it).
    pub parent: Option<u64>,
    /// The peer.
    pub peer: ConnectionPeerInfo,
    /// Where the connection was observed.
    pub path: ConnectionPath,
    /// The summary of the directions of its attach links.
    pub direction: LinkDirection,
    /// Whether this device opened the physical connection.
    pub local_side: LocalSide,
    /// The address segments.
    pub segments: Vec<AddressSegment>,
    /// How it was reached.
    pub via: ConnectionVia,
    /// Its state.
    pub state: ConnectionStatus,
    /// The step of a connection being set up.
    pub phase: Option<String>,
    /// The cause of the last transition that had one.
    pub last_cause: Option<ConnectionCause>,
    /// The latest transitions, oldest first.
    pub history: Vec<Transition>,
    /// How many older transitions were dropped from `history`.
    pub history_dropped: u64,
    /// The attach links on it.
    pub links: Vec<LinkRecord>,
    /// The open bulk channels on it.
    pub bulk_channels: u64,
    /// The open request channels on it.
    pub request_channels: u64,
    /// The round-trip time (none for SSH).
    pub rtt: Option<Duration>,
    /// The settings applied to it.
    pub applied: Vec<SettingEntry>,
    /// The redialer's progress, when the redialer set it up.
    pub redial: Option<RedialRecord>,
    /// The number of the P2P connection of a `p2p_udp` entry.
    pub p2p_id: Option<u64>,
    /// The P2P attempt's number, times and candidates of a `p2p_udp` entry.
    pub attempt: Option<AttemptRecord>,
}

impl ConnectionRecord {
    /// The record with the peer's instance ID and its causes scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        Self {
            peer: ConnectionPeerInfo {
                instance_id: secrets.scrub_opt(self.peer.instance_id),
                ..self.peer
            },
            last_cause: self.last_cause.map(|c| c.scrubbed(secrets)),
            history: self
                .history
                .into_iter()
                .map(|t| t.scrubbed(secrets))
                .collect(),
            ..self
        }
    }
}

impl From<&ConnectionInfo> for ConnectionRecord {
    fn from(info: &ConnectionInfo) -> Self {
        Self {
            id: info.id.get(),
            previous: info.previous.map(|id| id.get()),
            previous_dropped: info.previous_dropped,
            parent: info.parent.map(|id| id.get()),
            peer: ConnectionPeerInfo {
                device_id: info.peer.device_id.map(|id| id.to_string()),
                instance_id: info.peer.instance_id.clone(),
                instance_verified: info.peer.instance_verified,
                name: info.peer.name.clone(),
            },
            path: info.path.into(),
            direction: info.direction.into(),
            local_side: info.local_side.into(),
            segments: info.segments.iter().map(AddressSegment::from).collect(),
            via: (&info.via).into(),
            state: info.state.into(),
            phase: info.phase.map(|p| p.as_str().to_string()),
            last_cause: info.last_cause.as_ref().map(ConnectionCause::from),
            history: info.history.iter().map(Transition::from).collect(),
            history_dropped: info.history_dropped,
            links: info.links.iter().map(LinkRecord::from).collect(),
            bulk_channels: info.bulk_channels,
            request_channels: info.request_channels,
            rtt: info.rtt,
            applied: info.applied.iter().map(SettingEntry::from).collect(),
            redial: info.redial.as_ref().map(|r| RedialRecord {
                attempt: r.attempt,
                next_attempt_in: r.next_attempt_in,
            }),
            p2p_id: info.p2p_id,
            attempt: info.attempt.as_ref().map(AttemptRecord::from),
        }
    }
}

/// What the connection table reports as it changes (`ConnectionEvent`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NetEvent {
    /// A connection was added (its first transition) or made a transition.
    Changed {
        /// The entry.
        id: u64,
        /// The transition.
        transition: Transition,
    },
    /// A `disconnected` entry was dropped, or an entry that joined a shared tunnel was taken
    /// out.
    Removed {
        /// The entry.
        id: u64,
    },
    /// This many older events were dropped because nobody read them. Read the connection
    /// list again ([`TastyNet::connections`](crate::TastyNet::connections)).
    Lagged {
        /// How many.
        dropped: u64,
    },
    /// An event this version does not know. Read the connection list again.
    Other,
}

impl NetEvent {
    /// The event with its cause scrubbed of this end's tokens.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        match self {
            Self::Changed { id, transition } => Self::Changed {
                id,
                transition: transition.scrubbed(secrets),
            },
            other => other,
        }
    }
}

impl From<&ConnectionEvent> for NetEvent {
    fn from(event: &ConnectionEvent) -> Self {
        match event {
            ConnectionEvent::Transition { id, entry } => Self::Changed {
                id: id.get(),
                transition: entry.into(),
            },
            ConnectionEvent::Removed { id } => Self::Removed { id: id.get() },
            ConnectionEvent::Lagged { dropped } => Self::Lagged { dropped: *dropped },
            _ => Self::Other,
        }
    }
}
