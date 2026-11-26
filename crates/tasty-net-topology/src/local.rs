//! The edge list input, its aggregation and the two answer scopes (docs/design/topology.md
//! "엣지 목록 입력", "공개 범위").

use std::collections::HashMap;
use std::time::Duration;

use tasty_net_identity::DeviceId;

use crate::model::{Edge, EdgeKey, Node, NodeStatus, Side, TransportKind, WorkspaceRef};

/// What kind of authenticated link a [`LocalLink`] is. Only [`LinkKind::Attach`] becomes
/// an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// A logical attach, including a surface attach.
    Attach,
    /// A bulk channel.
    Bulk,
    /// A request channel (`purpose = request`).
    Request,
    /// A debug echo connection.
    DebugEcho,
}

/// One authenticated link of this device, as the consumer collects it. The consumer keeps
/// the list current: a link replaced by a reconnect is removed from the input. Aggregation
/// sees only the snapshot it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLink {
    /// The device ID the handshake proved for the other end.
    pub peer: DeviceId,
    /// Whether this device opened ([`Side::Dialer`]) or accepted the link.
    pub local_side: Side,
    /// Which end holds the workspace.
    pub holder: Side,
    pub transport: TransportKind,
    pub rtt: Option<Duration>,
    /// When the link was authenticated, Unix ms.
    pub connected_at: u64,
    /// The workspace of the attach; for a surface attach, the workspace of that surface.
    pub workspace: Option<WorkspaceRef>,
    pub kind: LinkKind,
}

/// What a failed connection attempt ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// Refused during authentication (`PeerTooOld`, `legacy_protocol`, token, signature or
    /// TOFU refusals).
    AuthRejected,
    /// The connection or the handshake timed out.
    TimedOut,
}

/// A failed attempt of this device to connect to a peer. The dialing consumer keeps the
/// last one per remote profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFailure {
    /// The consumer's display name of the peer (tasty: the profile name).
    pub peer_hint: Option<String>,
    /// The expected device ID, when known (the TOFU record).
    pub device_id: Option<DeviceId>,
    pub kind: FailureKind,
    /// Unix ms.
    pub observed_at: u64,
}

/// Which workspace fields a remote answer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkspaceExposure {
    pub id: bool,
    pub name: bool,
}

impl WorkspaceExposure {
    /// Carry no workspace field.
    pub const NONE: Self = Self {
        id: false,
        name: false,
    };
    /// Carry both the ID and the name.
    pub const ALL: Self = Self {
        id: true,
        name: true,
    };

    fn apply(self, workspace: &WorkspaceRef) -> Option<WorkspaceRef> {
        let kept = WorkspaceRef {
            id: workspace.id.clone().filter(|_| self.id),
            name: workspace.name.clone().filter(|_| self.name),
        };
        (kept.id.is_some() || kept.name.is_some()).then_some(kept)
    }
}

/// The per-device sharing setting. The default shares (a device with no setting shares)
/// and carries no workspace field, until the consumer decides what to expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sharing {
    /// `false` makes this device a leaf that answers only itself as
    /// [`NodeStatus::SharingOff`].
    pub enabled: bool,
    pub workspace: WorkspaceExposure,
}

impl Default for Sharing {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace: WorkspaceExposure::NONE,
        }
    }
}

/// This device as it describes itself in answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfInfo {
    pub device_id: DeviceId,
    pub display_name: Option<String>,
}

/// The nodes and edges one device answers about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// The edges of `links`: only [`LinkKind::Attach`] links, with the same
/// `(dialer, acceptor, holder, transport, workspace)` merged into one. A merged edge keeps
/// the earliest `connected_at` and that link's RTT. The order is the order of first
/// appearance.
pub fn aggregate_edges(me: &DeviceId, links: &[LocalLink]) -> Vec<Edge> {
    dedup_edges(
        links
            .iter()
            .filter(|link| link.kind == LinkKind::Attach)
            .map(|link| edge_of(me, link)),
    )
}

fn edge_of(me: &DeviceId, link: &LocalLink) -> Edge {
    let (dialer, acceptor) = match link.local_side {
        Side::Dialer => (*me, link.peer),
        Side::Acceptor => (link.peer, *me),
    };
    Edge {
        dialer,
        acceptor,
        holder: link.holder,
        transport: link.transport,
        rtt_ms: link
            .rtt
            .map(|rtt| u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX)),
        connected_at: link.connected_at,
        workspace: link.workspace.clone(),
    }
}

fn dedup_edges(edges: impl IntoIterator<Item = Edge>) -> Vec<Edge> {
    let mut out: Vec<Edge> = Vec::new();
    let mut index: HashMap<EdgeKey, usize> = HashMap::new();
    for edge in edges {
        match index.get(&edge.key()) {
            Some(&at) => {
                if edge.connected_at < out[at].connected_at {
                    out[at] = edge;
                }
            }
            None => {
                index.insert(edge.key(), out.len());
                out.push(edge);
            }
        }
    }
    out
}

/// The view of this device for its own screen: every edge and failure, whatever the
/// sharing setting.
pub fn local_view(
    me: &SelfInfo,
    links: &[LocalLink],
    failures: &[ConnectFailure],
    now_unix_ms: u64,
) -> View {
    let edges = aggregate_edges(&me.device_id, links);
    let mut nodes = vec![self_node(me, NodeStatus::Online, now_unix_ms)];
    nodes.extend(failure_nodes(me, &edges, failures));
    View { nodes, edges }
}

/// The view of this device for a remote query. With sharing off it is only this device as
/// [`NodeStatus::SharingOff`]. With sharing on, workspace fields outside
/// [`Sharing::workspace`] are dropped (edges that then become equal are merged).
pub fn remote_answer(
    me: &SelfInfo,
    links: &[LocalLink],
    failures: &[ConnectFailure],
    sharing: Sharing,
    now_unix_ms: u64,
) -> View {
    if !sharing.enabled {
        return View {
            nodes: vec![self_node(me, NodeStatus::SharingOff, now_unix_ms)],
            edges: Vec::new(),
        };
    }
    let edges = dedup_edges(
        aggregate_edges(&me.device_id, links)
            .into_iter()
            .map(|mut edge| {
                edge.workspace = edge
                    .workspace
                    .as_ref()
                    .and_then(|w| sharing.workspace.apply(w));
                edge
            }),
    );
    let mut nodes = vec![self_node(me, NodeStatus::Online, now_unix_ms)];
    nodes.extend(failure_nodes(me, &edges, failures));
    View { nodes, edges }
}

pub(crate) fn self_node(me: &SelfInfo, status: NodeStatus, now_unix_ms: u64) -> Node {
    Node {
        device_id: Some(me.device_id),
        display_name: me.display_name.clone(),
        status,
        reason: None,
        reported_by: None,
        peer_hint: None,
        explored_at: now_unix_ms,
    }
}

/// Failures become nodes reported by this device, except a failure toward a device that
/// also has a live edge here (the edge wins).
fn failure_nodes<'a>(
    me: &'a SelfInfo,
    edges: &'a [Edge],
    failures: &'a [ConnectFailure],
) -> impl Iterator<Item = Node> + 'a {
    failures
        .iter()
        .filter(|failure| {
            failure
                .device_id
                .is_none_or(|id| !edges.iter().any(|e| e.touches(&id)))
        })
        .map(|failure| Node {
            device_id: failure.device_id,
            display_name: None,
            status: match failure.kind {
                FailureKind::AuthRejected => NodeStatus::AuthRejected,
                FailureKind::TimedOut => NodeStatus::TimedOut,
            },
            reason: None,
            reported_by: Some(me.device_id),
            peer_hint: failure.peer_hint.clone(),
            explored_at: failure.observed_at,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    fn id(n: u8) -> DeviceId {
        DeviceKey::from_seed([n; 32]).id()
    }

    fn me() -> SelfInfo {
        SelfInfo {
            device_id: id(1),
            display_name: Some("me".into()),
        }
    }

    fn link(peer: u8, kind: LinkKind, connected_at: u64, workspace: Option<&str>) -> LocalLink {
        LocalLink {
            peer: id(peer),
            local_side: Side::Dialer,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: Some(Duration::from_millis(connected_at)),
            connected_at,
            workspace: workspace.map(|name| WorkspaceRef {
                id: Some(format!("id-{name}")),
                name: Some(name.into()),
            }),
            kind,
        }
    }

    #[test]
    fn only_attach_links_become_edges_and_equal_ones_merge() {
        let links = [
            link(2, LinkKind::Attach, 30, Some("w")),
            link(2, LinkKind::Attach, 10, Some("w")),
            link(2, LinkKind::Attach, 20, Some("other")),
            link(3, LinkKind::Bulk, 1, None),
            link(3, LinkKind::Request, 1, None),
            link(3, LinkKind::DebugEcho, 1, None),
        ];
        let edges = aggregate_edges(&id(1), &links);
        assert_eq!(edges.len(), 2);
        // The merged edge keeps the earliest link and its RTT.
        assert_eq!(edges[0].connected_at, 10);
        assert_eq!(edges[0].rtt_ms, Some(10));
        assert_eq!(
            edges[1].workspace.as_ref().unwrap().name.as_deref(),
            Some("other")
        );
        assert!(
            edges
                .iter()
                .all(|e| e.dialer == id(1) && e.acceptor == id(2))
        );
    }

    #[test]
    fn an_accepted_link_puts_the_peer_as_dialer() {
        let mut accepted = link(2, LinkKind::Attach, 1, None);
        accepted.local_side = Side::Acceptor;
        accepted.holder = Side::Acceptor;
        accepted.transport = TransportKind::SshRoleReversed;
        let edges = aggregate_edges(&id(1), &[accepted]);
        assert_eq!((edges[0].dialer, edges[0].acceptor), (id(2), id(1)));
        assert_eq!(edges[0].holder, Side::Acceptor);
    }

    #[test]
    fn the_local_view_keeps_what_the_remote_answer_drops() {
        let links = [
            link(2, LinkKind::Attach, 1, Some("a")),
            link(2, LinkKind::Attach, 2, Some("b")),
        ];
        let local = local_view(&me(), &links, &[], 0);
        assert_eq!(local.edges.len(), 2);
        assert!(
            local
                .edges
                .iter()
                .all(|e| e.workspace.as_ref().unwrap().name.is_some())
        );

        let names_hidden = Sharing {
            enabled: true,
            workspace: WorkspaceExposure {
                id: true,
                name: false,
            },
        };
        let remote = remote_answer(&me(), &links, &[], names_hidden, 0);
        assert_eq!(remote.edges.len(), 2);
        assert!(remote.edges.iter().all(|e| {
            let w = e.workspace.as_ref().unwrap();
            w.id.is_some() && w.name.is_none()
        }));

        // With no workspace field exposed the two attaches become one edge.
        let remote = remote_answer(&me(), &links, &[], Sharing::default(), 0);
        assert_eq!(remote.edges.len(), 1);
        assert_eq!(remote.edges[0].workspace, None);
    }

    #[test]
    fn sharing_off_answers_only_this_device() {
        let links = [link(2, LinkKind::Attach, 1, None)];
        let failures = [ConnectFailure {
            peer_hint: None,
            device_id: Some(id(5)),
            kind: FailureKind::TimedOut,
            observed_at: 0,
        }];
        let off = Sharing {
            enabled: false,
            workspace: WorkspaceExposure::ALL,
        };
        let view = remote_answer(&me(), &links, &failures, off, 42);
        assert!(view.edges.is_empty());
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].status, NodeStatus::SharingOff);
        assert_eq!(view.nodes[0].device_id, Some(id(1)));

        // The local view ignores the setting.
        let view = local_view(&me(), &links, &failures, 42);
        assert_eq!(view.edges.len(), 1);
        assert_eq!(view.nodes.len(), 2);
        assert_eq!(view.nodes[1].status, NodeStatus::TimedOut);
        assert_eq!(view.nodes[1].reported_by, Some(id(1)));
    }
}
