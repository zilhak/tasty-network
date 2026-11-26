//! Node and edge model (docs/design/topology.md "모델").

use serde::{Deserialize, Serialize};
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
}

/// Which end of a link: the one that opened it or the one that accepted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Dialer,
    Acceptor,
}

/// How an edge is carried.
///
/// Mapping from what a consumer sees on a link of `tasty-net-transport` (`peer()` gives
/// its `TransportKind` and the handshake `holder`, docs/design/transport.md "역할"):
///
/// | transport `TransportKind` | holder | this value |
/// |---|---|---|
/// | `Ssh` or `Tcp` | dialer | [`TransportKind::SshForward`] |
/// | `Ssh` or `Tcp` | acceptor | [`TransportKind::SshRoleReversed`] |
/// | `P2p`, selected path `is_ip` | either | [`TransportKind::P2pDirect`] |
/// | `P2p`, selected path `is_relay` | either | [`TransportKind::P2pRelay`] |
///
/// `Memory` links are test links and are not edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    SshForward,
    SshRoleReversed,
    P2pDirect,
    P2pRelay,
}

/// The workspace an attach is on. Which fields a remote answer carries is decided by the
/// consumer ([`crate::WorkspaceExposure`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One live attach between two devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
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

/// The fields that make two edges the same edge (docs/design/topology.md "모델": the same
/// `(dialer, acceptor, holder, transport, workspace)` is one edge).
pub(crate) type EdgeKey = (
    DeviceId,
    DeviceId,
    Side,
    TransportKind,
    Option<WorkspaceRef>,
);

impl Edge {
    pub(crate) fn key(&self) -> EdgeKey {
        (
            self.dialer,
            self.acceptor,
            self.holder,
            self.transport,
            self.workspace.clone(),
        )
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

    #[test]
    fn an_edge_round_trips_through_json() {
        let edge = Edge {
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
            transport: TransportKind::P2pRelay,
            ..edge
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("rtt_ms") && !json.contains("workspace"));
        assert_eq!(serde_json::from_str::<Edge>(&json).unwrap(), bare);
    }

    #[test]
    fn the_wire_names_follow_the_design() {
        let edge = Edge {
            dialer: id(1),
            acceptor: id(2),
            holder: Side::Dialer,
            transport: TransportKind::P2pDirect,
            rtt_ms: None,
            connected_at: 0,
            workspace: None,
        };
        let value = serde_json::to_value(&edge).unwrap();
        assert_eq!(value["dialer"], id(1).to_string());
        assert_eq!(value["holder"], "dialer");
        assert_eq!(value["transport"], "p2p_direct");
        for (kind, name) in [
            (TransportKind::SshForward, "ssh_forward"),
            (TransportKind::SshRoleReversed, "ssh_role_reversed"),
            (TransportKind::P2pRelay, "p2p_relay"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
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
            r#"{{"dialer":"{}","acceptor":"zz","holder":"dialer","transport":"ssh_forward","connected_at":0}}"#,
            id(1)
        );
        assert!(serde_json::from_str::<Edge>(&json).is_err());
    }
}
