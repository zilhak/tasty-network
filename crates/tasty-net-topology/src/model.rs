//! Node and edge model (docs/design/topology.md "모델").

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tasty_net_identity::DeviceId;

use crate::ids;

/// One device in a topology answer or in a merged exploration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The device ID, or `None` when the reporter could not learn it (an attempt refused
    /// before the peer proved its key). On the wire `None` is the empty string.
    #[serde(with = "ids::device_id_or_empty")]
    pub device_id: Option<DeviceId>,
    /// The display name the device gives itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: NodeStatus,
    /// Why a route hop could not forward the query (only on the origin node of such an
    /// answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<NodeReason>,
    /// The device that produced this node state. `None` only on the state a device gives
    /// about itself in [`crate::local_view`] and [`crate::remote_answer`] answers, where
    /// the answering device is the node.
    #[serde(
        default,
        with = "ids::device_id_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub reported_by: Option<DeviceId>,
    /// The display name the reporting consumer gave the peer (tasty: the profile name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_hint: Option<String>,
    /// When the answer was received or the state was observed, Unix ms. For display only;
    /// no deadline is judged by it.
    pub explored_at: u64,
}

/// The state of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// The device answered.
    Online,
    /// Not reachable; only the definition file view of tasty makes this state.
    Offline,
    /// The connection was refused during authentication, including an older peer that
    /// cannot authenticate.
    AuthRejected,
    /// The link is up but the peer does not announce [`crate::CAPABILITY`].
    NoCapability,
    /// No answer arrived in time, or a route hop could not forward the query
    /// ([`Node::reason`] tells which).
    TimedOut,
    /// The device has topology sharing turned off and answered only itself.
    SharingOff,
}

/// Why a route hop stopped the query on its way to the origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeReason {
    /// The hop has no authenticated attach link to the next hop.
    RouteBroken,
    /// The next hop does not announce [`crate::CAPABILITY`].
    RouteNoCapability,
    /// The hop has sharing turned off.
    RouteSharingOff,
    /// The route breaks a rule of [`crate::validate_route`].
    RouteInvalid,
    /// The answer of this device did not arrive whole: an entry too large for one message
    /// was left out, or a part is missing.
    Incomplete,
}

/// Which end of a link: the one that opened it or the one that accepted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Dialer,
    Acceptor,
}

/// How an edge is carried: the path kind and the attach direction of one link.
///
/// The value comes from each link's own holder and path kind, never from the `direction`
/// summary of a connection table entry (docs/design/transport.md "토폴로지와의 관계"). Mapping
/// from what a consumer sees on a link of `tasty-net-transport` (`peer()` gives its
/// `TransportKind`, the handshake gives the `holder`):
///
/// | transport `TransportKind` | holder | this value |
/// |---|---|---|
/// | `Ssh` or `Tcp` | dialer | [`TransportKind::SshForward`] |
/// | `Ssh` or `Tcp` | acceptor | [`TransportKind::SshRoleReversed`] |
/// | `P2p` | either | [`TransportKind::P2pUdp`] |
///
/// The accepting end of an SSH forward (its connection table entry has `path = ssh_inbound`)
/// follows the same rows by the holder, so both ends report the same value; `ssh_inbound`
/// is not an edge value. `Memory` links are test links and are not edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// An SSH (or loopback TCP) link whose dialer holds the workspace.
    SshForward,
    /// An SSH (or loopback TCP) link whose acceptor holds the workspace.
    SshRoleReversed,
    /// A QUIC connection over a punched direct UDP path.
    P2pUdp,
}

/// The identifier of one attach link that both ends know (docs/design/handshake.md
/// "bulk 자격"). Its text form is 32 lowercase hex digits. The topology only carries it; the
/// consumer takes it from the link in the transport connection table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId([u8; 16]);

impl LinkId {
    /// The ID from its 16 bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 bytes of the ID.
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkId({self})")
    }
}

impl FromStr for LinkId {
    type Err = LinkIdParseError;

    /// Only 32 lowercase hex digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.as_bytes();
        if text.len() != 32 {
            return Err(LinkIdParseError);
        }
        let mut out = [0u8; 16];
        for (byte, pair) in out.iter_mut().zip(text.chunks_exact(2)) {
            *byte = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Ok(Self(out))
    }
}

fn hex_digit(c: u8) -> Result<u8, LinkIdParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(LinkIdParseError),
    }
}

impl Serialize for LinkId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LinkId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The text is not 32 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkIdParseError;

impl fmt::Display for LinkIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("link ID is not 32 lowercase hex digits")
    }
}

impl std::error::Error for LinkIdParseError {}

/// The workspace an attach is on. Which fields a remote answer carries is decided by the
/// consumer ([`crate::WorkspaceExposure`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One live attach link between two devices. Edges are one per link: two reports with the
/// same [`Edge::link_id`] are the same edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The attach link this edge is.
    pub link_id: LinkId,
    /// The device that opened the link.
    #[serde(with = "ids::device_id")]
    pub dialer: DeviceId,
    /// The device that accepted the link.
    #[serde(with = "ids::device_id")]
    pub acceptor: DeviceId,
    /// Which end holds the workspace (mirrors it).
    pub holder: Side,
    pub transport: TransportKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    /// When the link was authenticated, Unix ms.
    pub connected_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRef>,
}

/// Which workspace fields of a (merged) edge report came from the device that holds the
/// workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FromHolder {
    id: bool,
    name: bool,
}

impl FromHolder {
    /// The fields of `edge` as `reporter` reported it.
    pub(crate) fn of(edge: &Edge, reporter: &DeviceId) -> Self {
        let from_holder = edge.holder_device() == *reporter;
        Self {
            id: from_holder,
            name: from_holder,
        }
    }
}

/// Keeps one workspace field value: a value from the holder's report wins, then the smaller
/// value (an order of all candidates, so the order of the reports does not matter).
fn pick(
    kept: &mut Option<String>,
    kept_from_holder: &mut bool,
    new: Option<String>,
    new_from_holder: bool,
) {
    let Some(new) = new else {
        return;
    };
    let take = match kept {
        None => true,
        Some(kept) => {
            (new_from_holder, std::cmp::Reverse(&new))
                > (*kept_from_holder, std::cmp::Reverse(&*kept))
        }
    };
    if take {
        *kept = Some(new);
        *kept_from_holder = new_from_holder;
    }
}

/// What makes two edges the same edge (docs/design/topology.md "모델": the merge key is the
/// `link_id` alone).
pub(crate) type EdgeKey = LinkId;

impl Edge {
    pub(crate) fn key(&self) -> EdgeKey {
        self.link_id
    }

    /// Merge another report of the same link into this one (docs/design/topology.md "모델"):
    /// a workspace field carried by either report is kept, the smaller `rtt_ms` and the
    /// earlier `connected_at` win. When both carry a field with different values, the value
    /// from the report of the device that holds the workspace wins, and otherwise the
    /// smaller value; `mine` and `theirs` tell which fields came from that device, so the
    /// result does not depend on the order of the reports. Both ends of an honest link report the same ends, holder
    /// and transport, which the handshake fixes; a report of the same `link_id` that differs
    /// in them is not that link and is dropped, so the report that came first stays.
    pub(crate) fn merge_report(&mut self, mine: &mut FromHolder, other: Edge, theirs: FromHolder) {
        debug_assert_eq!(self.link_id, other.link_id);
        if !self.same_link_as(&other) {
            return;
        }
        let ours = self.workspace.take().unwrap_or(WorkspaceRef {
            id: None,
            name: None,
        });
        let (mut id, mut name) = (ours.id, ours.name);
        if let Some(w) = other.workspace {
            pick(&mut id, &mut mine.id, w.id, theirs.id);
            pick(&mut name, &mut mine.name, w.name, theirs.name);
        }
        self.workspace = (id.is_some() || name.is_some()).then_some(WorkspaceRef { id, name });
        self.rtt_ms = match (self.rtt_ms, other.rtt_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.connected_at = self.connected_at.min(other.connected_at);
    }

    /// The device that holds the workspace of this link.
    pub(crate) fn holder_device(&self) -> DeviceId {
        match self.holder {
            Side::Dialer => self.dialer,
            Side::Acceptor => self.acceptor,
        }
    }

    /// Whether `other` describes the same ends, holder and transport.
    fn same_link_as(&self, other: &Edge) -> bool {
        (self.dialer, self.acceptor, self.holder, self.transport)
            == (other.dialer, other.acceptor, other.holder, other.transport)
    }

    /// Whether `device` is one end of this edge.
    pub fn touches(&self, device: &DeviceId) -> bool {
        self.dialer == *device || self.acceptor == *device
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    fn id(n: u8) -> DeviceId {
        DeviceKey::from_seed([n; 32]).id()
    }

    const LINK: &str = "00112233445566778899aabbccddeeff";

    fn link_id() -> LinkId {
        LINK.parse().unwrap()
    }

    #[test]
    fn an_edge_round_trips_through_json() {
        let edge = Edge {
            link_id: link_id(),
            dialer: id(1),
            acceptor: id(2),
            holder: Side::Acceptor,
            transport: TransportKind::SshRoleReversed,
            rtt_ms: Some(12),
            connected_at: 1_700_000_000_123,
            workspace: Some(WorkspaceRef {
                id: Some("ws".into()),
                name: Some("main".into()),
            }),
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert_eq!(serde_json::from_str::<Edge>(&json).unwrap(), edge);

        let bare = Edge {
            rtt_ms: None,
            workspace: None,
            transport: TransportKind::P2pUdp,
            ..edge
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("rtt_ms") && !json.contains("workspace"));
        assert_eq!(serde_json::from_str::<Edge>(&json).unwrap(), bare);
    }

    #[test]
    fn the_wire_names_follow_the_design() {
        let edge = Edge {
            link_id: link_id(),
            dialer: id(1),
            acceptor: id(2),
            holder: Side::Dialer,
            transport: TransportKind::P2pUdp,
            rtt_ms: None,
            connected_at: 0,
            workspace: None,
        };
        let value = serde_json::to_value(&edge).unwrap();
        assert_eq!(value["dialer"], id(1).to_string());
        assert_eq!(value["holder"], "dialer");
        assert_eq!(value["link_id"], LINK);
        assert_eq!(value["transport"], "p2p_udp");
        for (kind, name) in [
            (TransportKind::SshForward, "ssh_forward"),
            (TransportKind::SshRoleReversed, "ssh_role_reversed"),
            (TransportKind::P2pUdp, "p2p_udp"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<TransportKind>(name.into()).unwrap(),
                kind
            );
        }
        // The P2P values of the earlier design are gone, and no old value is read.
        for old in ["p2p_direct", "p2p_relay", "ssh_inbound"] {
            assert!(serde_json::from_value::<TransportKind>(old.into()).is_err());
        }
        for (status, name) in [
            (NodeStatus::Online, "online"),
            (NodeStatus::Offline, "offline"),
            (NodeStatus::AuthRejected, "auth_rejected"),
            (NodeStatus::NoCapability, "no_capability"),
            (NodeStatus::TimedOut, "timed_out"),
            (NodeStatus::SharingOff, "sharing_off"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), name);
        }
        assert_eq!(
            serde_json::to_value(NodeReason::RouteNoCapability).unwrap(),
            "route_no_capability"
        );
    }

    #[test]
    fn an_unknown_device_id_is_the_empty_string() {
        let node = Node {
            device_id: None,
            display_name: None,
            status: NodeStatus::AuthRejected,
            reason: None,
            reported_by: Some(id(3)),
            peer_hint: Some("box".into()),
            explored_at: 9,
        };
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["device_id"], "");
        assert_eq!(value["reported_by"], id(3).to_string());
        assert_eq!(serde_json::from_value::<Node>(value).unwrap(), node);
    }

    #[test]
    fn a_malformed_device_id_is_refused() {
        let json = format!(
            r#"{{"link_id":"{LINK}","dialer":"{}","acceptor":"zz","holder":"dialer","transport":"ssh_forward","connected_at":0}}"#,
            id(1)
        );
        assert!(serde_json::from_str::<Edge>(&json).is_err());
    }

    fn edge_json(link_id: &str) -> String {
        format!(
            r#"{{"link_id":"{link_id}","dialer":"{}","acceptor":"{}","holder":"dialer","transport":"ssh_forward","connected_at":0}}"#,
            id(1),
            id(2)
        )
    }

    #[test]
    fn a_link_id_is_32_lowercase_hex_digits() {
        assert_eq!(link_id().to_string(), LINK);
        assert_eq!(LinkId::from_bytes(link_id().to_bytes()), link_id());
        assert!(serde_json::from_str::<Edge>(&edge_json(LINK)).is_ok());
        for bad in [
            LINK.to_ascii_uppercase(),
            LINK[..31].to_owned(),
            format!("{LINK}0"),
            "g".repeat(32),
            String::new(),
        ] {
            assert_eq!(bad.parse::<LinkId>(), Err(LinkIdParseError), "{bad:?}");
            assert!(
                serde_json::from_str::<Edge>(&edge_json(&bad)).is_err(),
                "{bad:?}"
            );
        }
        // An edge without a link ID is refused.
        let json = edge_json(LINK).replace(&format!(r#""link_id":"{LINK}","#), "");
        assert!(serde_json::from_str::<Edge>(&json).is_err());
    }

    #[test]
    fn two_reports_of_one_link_merge_field_by_field() {
        let report = |rtt_ms, connected_at, workspace| Edge {
            link_id: link_id(),
            dialer: id(1),
            acceptor: id(2),
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt_ms,
            connected_at,
            workspace,
        };
        let id_only = WorkspaceRef {
            id: Some("ws".into()),
            name: None,
        };
        let name_only = WorkspaceRef {
            id: None,
            name: Some("main".into()),
        };
        let any = FromHolder::default();
        let mut a = report(Some(30), 20, None);
        let mut a_from = any;
        // A report of the same link ID that names other ends is not this link: dropped.
        let mut other_ends = report(Some(1), 1, Some(id_only.clone()));
        other_ends.dialer = id(3);
        a.merge_report(&mut a_from, other_ends, any);
        assert_eq!(a, report(Some(30), 20, None));
        a.merge_report(
            &mut a_from,
            report(Some(10), 25, Some(id_only.clone())),
            any,
        );
        assert_eq!(a, report(Some(10), 20, Some(id_only.clone())));
        a.merge_report(&mut a_from, report(None, 5, Some(name_only)), any);
        assert_eq!(
            a,
            report(
                Some(10),
                5,
                Some(WorkspaceRef {
                    id: Some("ws".into()),
                    name: Some("main".into()),
                })
            )
        );
    }

    /// Two reports of one link whose workspace names differ give the same name in either
    /// order: the holder's (here the dialer, id(1)) when one of them is the holder's, the
    /// smaller one when neither or both are.
    #[test]
    fn differing_workspace_values_merge_the_same_in_either_order() {
        let report = |name: &str| Edge {
            link_id: link_id(),
            dialer: id(1),
            acceptor: id(2),
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt_ms: None,
            connected_at: 0,
            workspace: Some(WorkspaceRef {
                id: None,
                name: Some(name.into()),
            }),
        };
        let merged = |first: (&str, &DeviceId), second: (&str, &DeviceId)| {
            let mut edge = report(first.0);
            let mut from = FromHolder::of(&edge, first.1);
            let other = report(second.0);
            let other_from = FromHolder::of(&other, second.1);
            edge.merge_report(&mut from, other, other_from);
            edge.workspace.unwrap().name.unwrap()
        };
        let (holder, other, third) = (id(1), id(2), id(3));
        // The holder's report says "zeta": it wins over the smaller "alpha".
        assert_eq!(merged(("zeta", &holder), ("alpha", &other)), "zeta");
        assert_eq!(merged(("alpha", &other), ("zeta", &holder)), "zeta");
        // Neither report is the holder's: the smaller value.
        assert_eq!(merged(("zeta", &other), ("alpha", &third)), "alpha");
        assert_eq!(merged(("alpha", &third), ("zeta", &other)), "alpha");
        // Both are the holder's: the smaller value too.
        assert_eq!(merged(("zeta", &holder), ("alpha", &holder)), "alpha");
        assert_eq!(merged(("alpha", &holder), ("zeta", &holder)), "alpha");
    }
}
