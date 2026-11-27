//! Merging the reports of one attach link (docs/design/topology.md "모델"): the edge is one
//! per `link_id`, whatever the two ends report about it (docs/design/testing.md "연결 목록
//! 시험", "토폴로지 합치기").
//!
//! A requester R explores over R → A → B, so both A and B report the links between them
//! and R merges the two reports. The clock stands still: no deadline passes, every message
//! is delivered in the order it was sent.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_topology::{
    Agent, AgentConfig, Edge, Exploration, ExplorationState, ExploreRequest, LinkId, LinkKind,
    Local, LocalLink, Message, Now, Outbox, SelfInfo, SendError, Sharing, Side, TransportKind,
    WorkspaceExposure, WorkspaceRef,
};

const R: u8 = 1;
const A: u8 = 2;
const B: u8 = 3;

fn id(n: u8) -> DeviceId {
    DeviceKey::from_seed([n; 32]).id()
}

fn link_id(n: u8) -> LinkId {
    LinkId::from_bytes([n; 16])
}

fn workspace() -> WorkspaceRef {
    WorkspaceRef {
        id: Some("ws-1".into()),
        name: Some("project".into()),
    }
}

/// What one end of a link reports about it.
struct End {
    rtt_ms: u64,
    connected_at: u64,
    workspace: Option<WorkspaceRef>,
    /// The link ID this end reports, when it is not the link's own (a broken end).
    link_id: Option<u8>,
}

impl End {
    fn plain() -> Self {
        Self {
            rtt_ms: 5,
            connected_at: 100,
            workspace: None,
            link_id: None,
        }
    }
}

struct Net {
    base: Instant,
    agents: HashMap<DeviceId, Agent>,
    links: HashMap<DeviceId, Vec<LocalLink>>,
    sharing: HashMap<DeviceId, Sharing>,
    /// (link sender, receiver, message), delivered first in, first out.
    queue: VecDeque<(DeviceId, DeviceId, Message)>,
}

struct Sends<'a> {
    me: DeviceId,
    links: &'a [LocalLink],
    out: &'a mut VecDeque<(DeviceId, DeviceId, Message)>,
}

impl Outbox for Sends<'_> {
    fn send(&mut self, to: &DeviceId, message: &Message) -> Result<(), SendError> {
        if !self.links.iter().any(|link| link.peer == *to) {
            return Err(SendError::NoLink);
        }
        self.out.push_back((self.me, *to, message.clone()));
        Ok(())
    }
}

impl Net {
    fn new() -> Self {
        let mut net = Self {
            base: Instant::now(),
            agents: HashMap::new(),
            links: HashMap::new(),
            sharing: HashMap::new(),
            queue: VecDeque::new(),
        };
        for n in [R, A, B] {
            let me = SelfInfo {
                device_id: id(n),
                display_name: None,
            };
            net.agents
                .insert(id(n), Agent::new(me, AgentConfig::default()));
            net.links.insert(id(n), Vec::new());
        }
        net
    }

    /// `dialer` dials `acceptor`, forward SSH (holder = dialer); each end reports what its
    /// `End` says.
    fn link(&mut self, link: u8, dialer: u8, acceptor: u8, at_dialer: End, at_acceptor: End) {
        let make = |peer: u8, local_side: Side, end: End| LocalLink {
            link_id: link_id(end.link_id.unwrap_or(link)),
            peer: id(peer),
            local_side,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: Some(Duration::from_millis(end.rtt_ms)),
            connected_at: end.connected_at,
            workspace: end.workspace,
            kind: LinkKind::Attach,
        };
        let to_acceptor = make(acceptor, Side::Dialer, at_dialer);
        let to_dialer = make(dialer, Side::Acceptor, at_acceptor);
        self.links.get_mut(&id(dialer)).unwrap().push(to_acceptor);
        self.links.get_mut(&id(acceptor)).unwrap().push(to_dialer);
    }

    fn with_agent<T>(
        &mut self,
        device: DeviceId,
        f: impl FnOnce(&mut Agent, Local<'_>, &mut dyn Outbox, Now) -> T,
    ) -> T {
        let mut agent = self.agents.remove(&device).unwrap();
        let links = &self.links[&device];
        let sharing = self.sharing.get(&device).copied().unwrap_or_default();
        let local = Local {
            links,
            failures: &[],
            sharing,
        };
        let mut out = Sends {
            me: device,
            links,
            out: &mut self.queue,
        };
        let now = Now {
            mono: self.base,
            unix_ms: 1_000,
        };
        let result = f(&mut agent, local, &mut out, now);
        self.agents.insert(device, agent);
        result
    }

    /// R explores from itself at depth 2 and every message is delivered.
    fn explore_from_r(&mut self) -> Exploration {
        self.explore(R, 2)
    }

    /// R explores from `origin`, a neighbor or R itself, and every message is delivered.
    fn explore(&mut self, origin: u8, depth: u32) -> Exploration {
        let request = ExploreRequest {
            origin: id(origin),
            route: Vec::new(),
            depth,
            timeout: Duration::from_secs(10),
        };
        let rid = self
            .with_agent(id(R), |agent, local, out, now| {
                agent.explore(request, local, out, now)
            })
            .unwrap();
        for _ in 0..10_000 {
            let Some((from, to, message)) = self.queue.pop_front() else {
                break;
            };
            self.with_agent(to, |agent, local, out, now| {
                agent.on_message(&from, message, local, out, now);
            });
        }
        assert!(self.queue.is_empty(), "the exploration did not settle");
        let x = self.agents[&id(R)].exploration(&rid).unwrap().clone();
        assert_eq!(x.state(), ExplorationState::Finished);
        x
    }
}

/// The edges between A and B.
fn a_b_edges(x: &Exploration) -> Vec<&Edge> {
    x.edges()
        .iter()
        .filter(|e| e.touches(&id(A)) && e.touches(&id(B)))
        .collect()
}

#[test]
fn both_ends_report_one_edge() {
    let mut net = Net::new();
    net.link(1, R, A, End::plain(), End::plain());
    // The two ends of A → B measured different RTTs and saw the link authenticated at
    // slightly different times.
    net.link(
        2,
        A,
        B,
        End {
            rtt_ms: 9,
            connected_at: 205,
            workspace: None,
            link_id: None,
        },
        End {
            rtt_ms: 4,
            connected_at: 200,
            workspace: None,
            link_id: None,
        },
    );
    let x = net.explore_from_r();
    let ab = a_b_edges(&x);
    assert_eq!(ab.len(), 1, "{ab:?}");
    let edge = ab[0];
    assert_eq!(edge.link_id, link_id(2));
    assert_eq!((edge.dialer, edge.acceptor), (id(A), id(B)));
    assert_eq!(edge.holder, Side::Dialer);
    assert_eq!(edge.transport, TransportKind::SshForward);
    assert_eq!(edge.rtt_ms, Some(4));
    assert_eq!(edge.connected_at, 200);
    // R → A is one edge too, and nothing else.
    assert_eq!(x.edges().len(), 2, "{:?}", x.edges());
}

#[test]
fn parallel_attaches_to_one_workspace_stay_apart() {
    let mut net = Net::new();
    net.link(1, R, A, End::plain(), End::plain());
    // A attaches twice to the same workspace of B (two surfaces): same ends, holder,
    // transport and workspace, two links.
    for link in [2, 3] {
        let end = || End {
            workspace: Some(workspace()),
            ..End::plain()
        };
        net.link(link, A, B, end(), end());
    }
    for device in [A, B] {
        net.sharing.insert(
            id(device),
            Sharing {
                enabled: true,
                workspace: WorkspaceExposure::ALL,
            },
        );
    }
    let x = net.explore_from_r();
    let ab = a_b_edges(&x);
    let mut links: Vec<LinkId> = ab.iter().map(|e| e.link_id).collect();
    links.sort();
    assert_eq!(links, vec![link_id(2), link_id(3)], "{ab:?}");
    assert!(ab.iter().all(|e| e.workspace == Some(workspace())));
}

#[test]
fn ends_with_different_exposure_merge_into_one() {
    let mut net = Net::new();
    net.link(1, R, A, End::plain(), End::plain());
    let end = || End {
        workspace: Some(workspace()),
        ..End::plain()
    };
    net.link(2, A, B, end(), end());
    // A shares its workspace fields, B shares none: only A's report carries the workspace.
    net.sharing.insert(
        id(A),
        Sharing {
            enabled: true,
            workspace: WorkspaceExposure::ALL,
        },
    );
    net.sharing.insert(id(B), Sharing::default());
    let x = net.explore_from_r();
    let ab = a_b_edges(&x);
    assert_eq!(ab.len(), 1, "{ab:?}");
    assert_eq!(ab[0].link_id, link_id(2));
    assert_eq!(ab[0].workspace, Some(workspace()));
}

/// The edges of `x` as (link ID, dialer, acceptor), sorted.
fn edge_ends(x: &Exploration) -> Vec<(LinkId, DeviceId, DeviceId)> {
    let mut ends: Vec<_> = x
        .edges()
        .iter()
        .map(|e| (e.link_id, e.dialer, e.acceptor))
        .collect();
    ends.sort();
    ends
}

#[test]
fn a_report_naming_other_ends_with_an_own_link_id_keeps_the_local_copy() {
    // A reports R's link 1 under ID 9, and its link to B under ID 1, R's own link ID. B
    // reports the A → B link under its real ID 2.
    let net = || {
        let mut net = Net::new();
        net.link(
            1,
            R,
            A,
            End::plain(),
            End {
                link_id: Some(9),
                ..End::plain()
            },
        );
        net.link(
            2,
            A,
            B,
            End {
                link_id: Some(1),
                ..End::plain()
            },
            End::plain(),
        );
        net
    };
    let expected = {
        let mut e = vec![(link_id(1), id(R), id(A)), (link_id(2), id(A), id(B))];
        e.sort();
        e
    };
    // R's local view is merged first, A's report comes after it.
    let local_first = net().explore(R, 2);
    assert_eq!(edge_ends(&local_first), expected);
    // A's report comes first; R's local view is merged when A's query visits R.
    let report_first = net().explore(A, 1);
    assert_eq!(edge_ends(&report_first), expected);
    for x in [&local_first, &report_first] {
        let own = x.edges().iter().find(|e| e.link_id == link_id(1)).unwrap();
        assert_eq!(own.rtt_ms, Some(5));
    }
}
