//! Explorations over fake neighbors (docs/design/topology.md "탐색 규칙").
//!
//! `Net` holds one `Agent` per device and a queue of messages in flight. Every message goes
//! through its JSON form (and the size limit) on the way, and is delivered at a simulated
//! time, so the order of arrival and the timeouts are fixed by the test.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_topology::{
    Agent, AgentConfig, ConnectFailure, Exploration, ExplorationState, ExploreRequest, FailureKind,
    LinkKind, Local, LocalLink, MAX_MESSAGE_JSON, MAX_ROUTE_HOPS, Message, NodeReason, NodeStatus,
    Now, Outbox, RouteError, SelfInfo, SendError, Sharing, Side, TopologyError, TransportKind,
    WorkspaceExposure, WorkspaceRef,
};

fn id(n: u8) -> DeviceId {
    DeviceKey::from_seed([n; 32]).id()
}

const A: u8 = 1;
const B: u8 = 2;
const C: u8 = 3;
const D: u8 = 4;
const E: u8 = 5;

const TIMEOUT: Duration = Duration::from_secs(10);

type DelayRule = Box<dyn Fn(DeviceId, DeviceId, &Message) -> Option<u64>>;

struct InFlight {
    at_ms: u64,
    seq: u64,
    from: DeviceId,
    to: DeviceId,
    message: Message,
}

struct Net {
    base: Instant,
    clock_ms: u64,
    seq: u64,
    agents: HashMap<DeviceId, Agent>,
    links: HashMap<DeviceId, Vec<LocalLink>>,
    failures: HashMap<DeviceId, Vec<ConnectFailure>>,
    sharing: HashMap<DeviceId, Sharing>,
    no_caps: HashSet<DeviceId>,
    silent: HashSet<DeviceId>,
    queue: Vec<InFlight>,
    delay: Vec<DelayRule>,
    max_part: u32,
    /// Every delivered message: (link sender, receiver, message).
    log: Vec<(DeviceId, DeviceId, Message)>,
}

struct Sends<'a> {
    me: DeviceId,
    links: &'a [LocalLink],
    no_caps: &'a HashSet<DeviceId>,
    out: Vec<(DeviceId, Message)>,
}

impl Outbox for Sends<'_> {
    fn send(&mut self, to: &DeviceId, message: &Message) -> Result<(), SendError> {
        if !self
            .links
            .iter()
            .any(|l| l.peer == *to && l.kind == LinkKind::Attach)
        {
            return Err(SendError::NoLink);
        }
        if self.no_caps.contains(to) {
            return Err(SendError::NoCapability);
        }
        assert_ne!(*to, self.me);
        // Every message crosses its wire form.
        let json = message.to_json().expect("message fits its frame limit");
        assert!(json.len() <= MAX_MESSAGE_JSON);
        let back = Message::from_json(&json).expect("message reads back");
        assert_eq!(&back, message);
        self.out.push((*to, back));
        Ok(())
    }
}

impl Net {
    fn new(devices: &[u8]) -> Self {
        let config = AgentConfig {
            hop_timeout: Duration::from_secs(2),
            hop_reserve: Duration::from_millis(100),
        };
        let mut net = Self {
            base: Instant::now(),
            clock_ms: 0,
            seq: 0,
            agents: HashMap::new(),
            links: HashMap::new(),
            failures: HashMap::new(),
            sharing: HashMap::new(),
            no_caps: HashSet::new(),
            silent: HashSet::new(),
            queue: Vec::new(),
            delay: Vec::new(),
            max_part: 0,
            log: Vec::new(),
        };
        for &n in devices {
            let me = SelfInfo {
                device_id: id(n),
                display_name: Some(format!("dev-{n}")),
            };
            net.agents.insert(id(n), Agent::new(me, config));
            net.links.insert(id(n), Vec::new());
        }
        net
    }

    /// `a` dials `b`, forward SSH (holder = dialer).
    fn link(&mut self, a: u8, b: u8) -> &mut Self {
        self.link_on(a, b, None)
    }

    fn link_on(&mut self, a: u8, b: u8, workspace: Option<WorkspaceRef>) -> &mut Self {
        let at = 1_000 + u64::from(a) * 10 + u64::from(b);
        let make = |peer: u8, local_side: Side| LocalLink {
            peer: id(peer),
            local_side,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: Some(Duration::from_millis(7)),
            connected_at: at,
            workspace: workspace.clone(),
            kind: LinkKind::Attach,
        };
        let to_b = make(b, Side::Dialer);
        let to_a = make(a, Side::Acceptor);
        self.links.get_mut(&id(a)).unwrap().push(to_b);
        self.links.get_mut(&id(b)).unwrap().push(to_a);
        self
    }

    fn now(&self) -> Now {
        Now {
            mono: self.base + Duration::from_millis(self.clock_ms),
            unix_ms: 1_700_000_000_000 + self.clock_ms,
        }
    }

    fn enqueue(&mut self, from: DeviceId, sends: Vec<(DeviceId, Message)>) {
        for (to, message) in sends {
            let delay = self
                .delay
                .iter()
                .find_map(|rule| rule(from, to, &message))
                .unwrap_or(1);
            self.seq += 1;
            self.queue.push(InFlight {
                at_ms: self.clock_ms + delay,
                seq: self.seq,
                from,
                to,
                message,
            });
        }
    }

    /// Runs `f` on the agent of `device` with its local input and an outbox, then queues
    /// what it sent.
    fn with_agent<R>(
        &mut self,
        device: DeviceId,
        f: impl FnOnce(&mut Agent, Local<'_>, &mut Sends<'_>, Now) -> R,
    ) -> R {
        let now = self.now();
        let links = self.links[&device].clone();
        let failures = self.failures.get(&device).cloned().unwrap_or_default();
        let sharing = self.sharing.get(&device).copied().unwrap_or_default();
        let no_caps = self.no_caps.clone();
        let mut sends = Sends {
            me: device,
            links: &links,
            no_caps: &no_caps,
            out: Vec::new(),
        };
        let local = Local {
            links: &links,
            failures: &failures,
            sharing,
        };
        let agent = self.agents.get_mut(&device).unwrap();
        let result = f(agent, local, &mut sends, now);
        let out = sends.out;
        self.enqueue(device, out);
        result
    }

    fn explore(&mut self, requester: u8, request: ExploreRequest) -> Result<String, TopologyError> {
        self.with_agent(id(requester), |agent, local, out, now| {
            agent.explore(request, local, out, now)
        })
    }

    /// Delivers messages and applies deadlines until nothing is left.
    fn run(&mut self) {
        self.run_until(u64::MAX);
    }

    /// Like `run`, but stops before the first event later than `until_ms`.
    fn run_until(&mut self, until_ms: u64) {
        for _ in 0..100_000 {
            let next_message = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| (m.at_ms, m.seq))
                .map(|(i, m)| (i, m.at_ms));
            let next_deadline = self
                .agents
                .values()
                .filter_map(|a| a.next_deadline())
                .min()
                .map(|at| {
                    let ms = at.saturating_duration_since(self.base).as_millis();
                    u64::try_from(ms).unwrap()
                });
            let message_first = match (next_message, next_deadline) {
                (Some((_, at)), Some(deadline)) => at <= deadline,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let next_at = match (next_message, next_deadline) {
                (Some((_, at)), _) if message_first => at,
                (_, Some(deadline)) => deadline,
                (_, None) => 0,
            };
            if next_at > until_ms {
                return;
            }
            match (next_message, next_deadline) {
                (Some((i, _)), _) if message_first => {
                    let m = self.queue.remove(i);
                    self.clock_ms = self.clock_ms.max(m.at_ms);
                    if let Message::Result(part) = &m.message {
                        self.max_part = self.max_part.max(part.part);
                    }
                    self.log.push((m.from, m.to, m.message.clone()));
                    if self.silent.contains(&m.to) {
                        continue;
                    }
                    let from = m.from;
                    self.with_agent(m.to, |agent, local, out, now| {
                        agent.on_message(&from, m.message, local, out, now)
                    });
                }
                (_, Some(deadline)) => {
                    self.clock_ms = self.clock_ms.max(deadline);
                    let devices: Vec<DeviceId> = self.agents.keys().copied().collect();
                    for device in devices {
                        self.with_agent(device, |agent, _, out, now| agent.poll(out, now));
                    }
                }
                (_, None) => return,
            }
        }
        panic!("the exploration did not settle");
    }

    /// Delivers every message in time order without ever calling `poll`, as a consumer
    /// whose timer fired late would.
    fn deliver_without_polls(&mut self) {
        while let Some(i) = self
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| (m.at_ms, m.seq))
            .map(|(i, _)| i)
        {
            let m = self.queue.remove(i);
            self.clock_ms = self.clock_ms.max(m.at_ms);
            self.log.push((m.from, m.to, m.message.clone()));
            if self.silent.contains(&m.to) {
                continue;
            }
            let from = m.from;
            self.with_agent(m.to, |agent, local, out, now| {
                agent.on_message(&from, m.message, local, out, now)
            });
        }
    }

    fn result(&self, requester: u8, request_id: &str) -> &Exploration {
        self.agents[&id(requester)]
            .exploration(request_id)
            .expect("exploration kept")
    }
}

fn request(origin: u8, depth: u32) -> ExploreRequest {
    ExploreRequest {
        origin: id(origin),
        route: Vec::new(),
        depth,
        timeout: TIMEOUT,
    }
}

fn node_ids(exploration: &Exploration) -> Vec<DeviceId> {
    let mut ids: Vec<DeviceId> = exploration
        .nodes()
        .iter()
        .filter_map(|n| n.device_id)
        .collect();
    ids.sort();
    ids
}

fn sorted(devices: &[u8]) -> Vec<DeviceId> {
    let mut ids: Vec<DeviceId> = devices.iter().map(|&n| id(n)).collect();
    ids.sort();
    ids
}

fn status(exploration: &Exploration, device: u8) -> NodeStatus {
    exploration.node(&id(device)).expect("node present").status
}

#[test]
fn a_chain_at_depth_two_has_three_nodes_and_two_edges() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
    assert_eq!(x.edges().len(), 2);
    for n in [A, B, C] {
        assert_eq!(status(x, n), NodeStatus::Online);
    }
}

#[test]
fn a_chain_at_depth_one_stops_after_the_first_neighbor() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B]));
    // B at depth 0 still answers its own edges, B–C included.
    assert_eq!(x.edges().len(), 2);
}

#[test]
fn a_cycle_ends_without_duplicate_nodes() {
    let mut net = Net::new(&[A, B, C]);
    // A↔B both ways, and a triangle A–B–C.
    net.link(A, B).link(B, A).link(B, C).link(C, A);
    let rid = net.explore(A, request(A, 5)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(x.nodes().len(), 3);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
    assert_eq!(x.edges().len(), 4);
}

#[test]
fn a_neighbor_with_sharing_off_is_a_leaf() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    net.sharing.insert(
        id(B),
        Sharing {
            enabled: false,
            workspace: WorkspaceExposure::ALL,
        },
    );
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B]));
    assert_eq!(status(x, B), NodeStatus::SharingOff);
    // Only A's own edge to B; nothing of B's.
    assert_eq!(x.edges().len(), 1);
    assert!(x.edges().iter().all(|e| !e.touches(&id(C))));
}

#[test]
fn a_diamond_has_its_far_node_once_and_four_edges() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C).link(B, D).link(C, D);
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(
        x.nodes()
            .iter()
            .filter(|n| n.device_id == Some(id(D)))
            .count(),
        1
    );
    assert_eq!(node_ids(x), sorted(&[A, B, C, D]));
    assert_eq!(x.edges().len(), 4);
}

#[test]
fn a_shorter_path_reached_later_continues_with_its_larger_depth() {
    // A–B–D is the long path, A–D the short one; D's neighbor E is one hop past D.
    let mut net = Net::new(&[A, B, D, E]);
    net.link(A, B).link(B, D).link(A, D).link(D, E);
    // The query on A–D arrives late, after the one through B.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(A) && to == id(D) && matches!(m, Message::Query(_))).then_some(500)
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, D, E]));
    assert_eq!(status(x, E), NodeStatus::Online);
    assert_eq!(x.edges().len(), 4);
}

#[test]
fn a_reached_node_at_depth_zero_does_not_forward_without_a_shorter_path() {
    // The control for the test above: without A–D, D is reached only at depth 0 and E
    // stays out.
    let mut net = Net::new(&[A, B, D, E]);
    net.link(A, B).link(B, D).link(D, E);
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(node_ids(x), sorted(&[A, B, D]));
}

#[test]
fn a_route_slower_than_the_whole_limit_ends_timed_out() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    net.delay.push(Box::new(|from, to, m| {
        (from == id(B) && to == id(C) && matches!(m, Message::Query(_))).then_some(20_000)
    }));
    let rid = net
        .explore(
            A,
            ExploreRequest {
                origin: id(C),
                route: vec![id(B)],
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::TimedOut);
    assert_eq!(status(x, C), NodeStatus::TimedOut);
}

#[test]
fn a_route_through_a_hop_reaches_the_origin() {
    // The control for the test above: the same route without the delay finishes.
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(B, C).link(C, D);
    let rid = net
        .explore(
            A,
            ExploreRequest {
                origin: id(C),
                route: vec![id(B)],
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[B, C, D]));
}

#[test]
fn the_origin_b_at_depth_one_gives_b_and_its_neighbors() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    let rid = net.explore(A, request(B, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
}

#[test]
fn a_silent_neighbor_times_out_and_the_rest_continues() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C).link(C, D);
    net.silent.insert(id(B));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::TimedOut);
    assert_eq!(x.node(&id(B)).unwrap().reported_by, Some(id(A)));
    assert_eq!(status(x, C), NodeStatus::Online);
    assert_eq!(status(x, D), NodeStatus::Online);
}

#[test]
fn a_neighbor_without_the_capability_is_marked_and_not_queried() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(A, C);
    net.no_caps.insert(id(B));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::NoCapability);
    assert_eq!(status(x, C), NodeStatus::Online);
}

#[test]
fn a_direct_answer_wins_over_a_timeout_seen_on_another_path() {
    // D answers through C, but its query from B is dropped: B reports D as timed out.
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C).link(B, D).link(C, D);
    net.delay.push(Box::new(|from, to, m| {
        (from == id(B) && to == id(D) && matches!(m, Message::Query(_))).then_some(60_000)
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(status(x, D), NodeStatus::Online);
}

#[test]
fn a_connect_failure_becomes_a_reported_node() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    net.failures.insert(
        id(B),
        vec![
            ConnectFailure {
                peer_hint: Some("old-box".into()),
                device_id: None,
                kind: FailureKind::AuthRejected,
                observed_at: 5,
            },
            // A failure toward A, which also has a live edge with B: the edge wins.
            ConnectFailure {
                peer_hint: Some("a".into()),
                device_id: Some(id(A)),
                kind: FailureKind::TimedOut,
                observed_at: 6,
            },
        ],
    );
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    let old = x
        .nodes()
        .iter()
        .find(|n| n.peer_hint.as_deref() == Some("old-box"))
        .expect("failure node");
    assert_eq!(old.status, NodeStatus::AuthRejected);
    assert_eq!(old.device_id, None);
    assert_eq!(old.reported_by, Some(id(B)));
    assert_eq!(status(x, A), NodeStatus::Online);
    assert_eq!(x.nodes().len(), 3);
}

#[test]
fn a_remote_answer_drops_workspace_fields_outside_its_scope() {
    let mut net = Net::new(&[A, B, C]);
    let workspace = WorkspaceRef {
        id: Some("ws-1".into()),
        name: Some("secret project".into()),
    };
    net.link(A, B).link_on(B, C, Some(workspace));
    net.sharing.insert(
        id(B),
        Sharing {
            enabled: true,
            workspace: WorkspaceExposure {
                id: true,
                name: false,
            },
        },
    );
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    let bc = x
        .edges()
        .iter()
        .find(|e| e.touches(&id(C)))
        .expect("B–C edge");
    let seen = bc.workspace.as_ref().expect("the ID stays");
    assert_eq!(seen.id.as_deref(), Some("ws-1"));
    assert_eq!(seen.name, None);

    // B's own exploration is local: the name is there.
    let rid = net.explore(B, request(B, 0)).unwrap();
    net.run();
    let x = net.result(B, &rid);
    let bc = x.edges().iter().find(|e| e.touches(&id(C))).unwrap();
    assert_eq!(
        bc.workspace.as_ref().unwrap().name.as_deref(),
        Some("secret project")
    );
}

#[test]
fn route_over_hop_limit_is_rejected() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    let route: Vec<DeviceId> = (10..10 + MAX_ROUTE_HOPS as u8 + 1).map(id).collect();
    let err = net
        .explore(
            A,
            ExploreRequest {
                origin: id(B),
                route,
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap_err();
    assert_eq!(err, TopologyError::InvalidRoute(RouteError::TooLong));
    assert!(net.queue.is_empty());

    // The limit itself is allowed (the first hop is then simply missing).
    let route: Vec<DeviceId> = (10..10 + MAX_ROUTE_HOPS as u8).map(id).collect();
    let err = net
        .explore(
            A,
            ExploreRequest {
                origin: id(B),
                route,
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap_err();
    assert_eq!(err, TopologyError::FirstHop(SendError::NoLink));
}

#[test]
fn route_with_repeated_device_is_rejected() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(B, C).link(C, D);
    let err = net
        .explore(
            A,
            ExploreRequest {
                origin: id(D),
                route: vec![id(B), id(C), id(B)],
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap_err();
    assert_eq!(err, TopologyError::InvalidRoute(RouteError::Repeated));
    assert!(net.queue.is_empty());
}

#[test]
fn a_route_hop_without_a_link_to_the_next_hop_reports_route_broken() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B);
    let rid = net
        .explore(
            A,
            ExploreRequest {
                origin: id(C),
                route: vec![id(B)],
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    let c = x.node(&id(C)).unwrap();
    assert_eq!(c.status, NodeStatus::TimedOut);
    assert_eq!(c.reason, Some(NodeReason::RouteBroken));
    assert_eq!(c.reported_by, Some(id(B)));
}

#[test]
fn a_route_hop_with_sharing_off_answers_itself_and_stops() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    net.sharing.insert(
        id(B),
        Sharing {
            enabled: false,
            workspace: WorkspaceExposure::NONE,
        },
    );
    let rid = net
        .explore(
            A,
            ExploreRequest {
                origin: id(C),
                route: vec![id(B)],
                depth: 1,
                timeout: TIMEOUT,
            },
        )
        .unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::SharingOff);
    assert_eq!(
        x.node(&id(C)).unwrap().reason,
        Some(NodeReason::RouteSharingOff)
    );
}

#[test]
fn a_route_whose_first_hop_did_not_get_it_from_the_requester_is_invalid() {
    // C claims to be the requester, but B received the query from A.
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(B, D).link(B, C);
    let forged = tasty_net_topology::Query {
        request_id: "r".into(),
        requester: id(C),
        origin: id(D),
        route: vec![id(B)],
        path: Vec::new(),
        depth: 1,
        remaining_ms: 5_000,
        branch: "b".into(),
    };
    net.with_agent(id(B), |agent, local, out, now| {
        agent.on_message(&id(A), Message::Query(forged), local, out, now)
    });
    // B stops it: nothing goes to D, and C gets the origin as timed out for route_invalid.
    assert!(net.queue.iter().all(|m| m.to == id(C)));
    let reasons: Vec<Option<NodeReason>> = net
        .queue
        .iter()
        .filter_map(|m| match &m.message {
            Message::Result(part) => Some(part.nodes.iter().map(|n| n.reason)),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(reasons, vec![Some(NodeReason::RouteInvalid)]);
    assert!(
        net.queue
            .iter()
            .any(|m| matches!(&m.message, Message::Done(d) if d.is_final))
    );
}

#[test]
fn a_large_answer_crosses_in_parts_and_merges_whole() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B has thousands of other attaches; at depth 0 it only reports them.
    let peers = 5_000u32;
    for n in 0..peers {
        let mut seed = [0xEEu8; 32];
        seed[..4].copy_from_slice(&n.to_be_bytes());
        let peer = DeviceKey::from_seed(seed).id();
        net.links.get_mut(&id(B)).unwrap().push(LocalLink {
            peer,
            local_side: Side::Acceptor,
            holder: Side::Dialer,
            transport: TransportKind::P2pRelay,
            rtt: None,
            connected_at: u64::from(n),
            workspace: None,
            kind: LinkKind::Attach,
        });
    }
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert!(net.max_part > 0, "the answer of B took more than one part");
    assert_eq!(x.edges().len(), 1 + peers as usize);
    assert_eq!(node_ids(x), sorted(&[A, B]));
}

#[test]
fn results_show_up_before_the_exploration_ends() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(A, C);
    // C answers late.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(A) && to == id(C) && matches!(m, Message::Query(_))).then_some(1_000)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run_until(500);
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Running);
    assert_eq!(node_ids(x), sorted(&[A, B]));
    let b_seen = x.node(&id(B)).unwrap().explored_at;

    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
    // Each node carries the time its answer arrived.
    assert!(x.node(&id(C)).unwrap().explored_at > b_seen);
}

/// The result parts `from` made that crossed the link into `to`.
fn parts_into(net: &Net, from: u8, to: u8) -> Vec<tasty_net_topology::ResultPart> {
    net.log
        .iter()
        .filter(|(_, t, _)| *t == id(to))
        .filter_map(|(_, _, m)| match m {
            Message::Result(part) if part.from == id(from) => Some(part.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_requester_visited_as_a_neighbor_sends_only_its_remote_answer() {
    let mut net = Net::new(&[A, B]);
    let workspace = WorkspaceRef {
        id: Some("ws-a".into()),
        name: Some("private name".into()),
    };
    net.link_on(A, B, Some(workspace));
    net.sharing.insert(
        id(A),
        Sharing {
            enabled: true,
            workspace: WorkspaceExposure {
                id: true,
                name: false,
            },
        },
    );
    let rid = net.explore(A, request(B, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);

    // What B received from A carries no workspace name.
    let parts = parts_into(&net, A, B);
    assert!(!parts.is_empty());
    for part in &parts {
        for edge in &part.edges {
            assert_eq!(edge.workspace.as_ref().unwrap().name, None);
        }
    }
    // A's own screen has the name, once.
    let names: Vec<_> = x
        .edges()
        .iter()
        .filter_map(|e| e.workspace.as_ref().and_then(|w| w.name.clone()))
        .collect();
    assert_eq!(names, vec!["private name".to_string()]);
}

#[test]
fn the_requester_with_sharing_off_does_not_send_its_edges_or_forward() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(A, C);
    net.sharing.insert(
        id(A),
        Sharing {
            enabled: false,
            workspace: WorkspaceExposure::ALL,
        },
    );
    let rid = net.explore(A, request(B, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);

    let parts = parts_into(&net, A, B);
    assert!(!parts.is_empty());
    assert!(parts.iter().all(|p| p.edges.is_empty()));
    assert!(
        parts
            .iter()
            .flat_map(|p| &p.nodes)
            .all(|n| n.status == NodeStatus::SharingOff)
    );
    // A is a leaf on the link: C, reachable only through A, got nothing.
    assert!(net.log.iter().all(|(_, to, _)| *to != id(C)));
    // A's own screen still has all its edges and itself online.
    assert_eq!(status(x, A), NodeStatus::Online);
    assert!(x.edges().iter().any(|e| e.touches(&id(C))));
}

#[test]
fn a_query_whose_path_does_not_match_its_link_is_dropped() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(B, C);
    let query = |path: Vec<DeviceId>| tasty_net_topology::Query {
        request_id: "r".into(),
        requester: id(A),
        origin: id(A),
        route: Vec::new(),
        path,
        depth: 1,
        remaining_ms: 5_000,
        branch: "b".into(),
    };
    // The last device of the path is D, but the query came on the link with B.
    for path in [
        vec![id(A), id(D)],
        // Does not start at the origin.
        vec![id(D), id(B)],
        // Contains C itself.
        vec![id(A), id(C), id(B)],
        // A device twice.
        vec![id(A), id(B), id(A), id(B)],
    ] {
        net.with_agent(id(C), |agent, local, out, now| {
            agent.on_message(&id(B), Message::Query(query(path)), local, out, now)
        });
        assert!(net.queue.is_empty());
    }
    // The control: a path that ends at the link peer is answered.
    net.with_agent(id(C), |agent, local, out, now| {
        agent.on_message(
            &id(B),
            Message::Query(query(vec![id(A), id(B)])),
            local,
            out,
            now,
        )
    });
    assert!(!net.queue.is_empty());
}

#[test]
fn a_later_route_hop_checks_the_previous_hop() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C).link(B, C).link(C, D);
    let forged = tasty_net_topology::Query {
        request_id: "r".into(),
        requester: id(A),
        origin: id(D),
        route: vec![id(B), id(C)],
        path: Vec::new(),
        depth: 1,
        remaining_ms: 5_000,
        branch: "b".into(),
    };
    // C is the second hop; the query must come from B, not from A.
    net.with_agent(id(C), |agent, local, out, now| {
        agent.on_message(&id(A), Message::Query(forged), local, out, now)
    });
    assert!(net.queue.iter().all(|m| m.to != id(D)));
    assert!(net.queue.iter().any(|m| matches!(
        &m.message,
        Message::Result(p) if p.nodes.iter().any(|n| n.reason == Some(NodeReason::RouteInvalid))
    )));
}

#[test]
fn replies_from_unexpected_links_or_senders_are_dropped() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C);
    net.delay.push(Box::new(|from, to, m| {
        (from == id(A) && to == id(B) && matches!(m, Message::Query(_))).then_some(1_000)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run_until(100);
    assert_eq!(net.result(A, &rid).state(), ExplorationState::Running);

    // D was never asked: its result is not merged.
    let fake = tasty_net_topology::ResultPart {
        request_id: rid.clone(),
        branch: "x".into(),
        from: id(D),
        return_path: vec![id(A)],
        part: 0,
        last: true,
        nodes: vec![tasty_net_topology::Node {
            device_id: Some(id(D)),
            display_name: None,
            status: NodeStatus::Online,
            reason: None,
            reported_by: None,
            peer_hint: None,
            explored_at: 0,
        }],
        edges: Vec::new(),
    };
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(D), Message::Result(fake), local, out, now)
    });
    // B is still pending, but a final done made by B (not the origin) does not end it.
    let fake_final = tasty_net_topology::Done {
        request_id: rid.clone(),
        branch: "x".into(),
        from: id(B),
        return_path: vec![id(A)],
        timed_out: Vec::new(),
        is_final: true,
        more: false,
    };
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(B), Message::Done(fake_final), local, out, now)
    });
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Running);
    assert!(x.node(&id(D)).is_none());

    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
}

#[test]
fn replies_after_the_requester_deadline_are_not_merged() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B's answer reaches A only after A's whole limit, and nobody polled in between.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(B) && to == id(A) && !matches!(m, Message::Query(_))).then_some(20_000)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.deliver_without_polls();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::TimedOut);
    assert!(
        x.node(&id(B))
            .is_none_or(|n| n.status != NodeStatus::Online)
    );
}

#[test]
fn a_neighbor_given_up_is_not_relayed_later() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    // C answers B after B's per-neighbor limit (2 s).
    net.delay.push(Box::new(|from, to, m| {
        (from == id(C) && to == id(B) && !matches!(m, Message::Query(_))).then_some(3_000)
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, C), NodeStatus::TimedOut);
    assert_eq!(x.node(&id(C)).unwrap().reported_by, Some(id(B)));
    // The late answer of C reached B but went no further.
    assert!(net.log.iter().any(|(f, t, _)| *f == id(C) && *t == id(B)));
    assert!(
        net.log.iter().all(|(_, t, m)| {
            *t != id(A) || !matches!(m, Message::Result(p) if p.from == id(C))
        })
    );
}

#[test]
fn many_timed_out_neighbors_come_back_in_several_dones() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B has 9,000 neighbors that never answer: one done could not carry them all.
    let silent = 9_000u32;
    for n in 0..silent {
        let mut seed = [0xDDu8; 32];
        seed[..4].copy_from_slice(&n.to_be_bytes());
        let peer = DeviceKey::from_seed(seed).id();
        net.silent.insert(peer);
        net.links.get_mut(&id(B)).unwrap().push(LocalLink {
            peer,
            local_side: Side::Dialer,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: None,
            connected_at: 0,
            workspace: None,
            kind: LinkKind::Attach,
        });
    }
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    let dones: Vec<_> = net
        .log
        .iter()
        .filter_map(|(f, t, m)| match m {
            Message::Done(d) if *f == id(B) && *t == id(A) => Some(d.clone()),
            _ => None,
        })
        .collect();
    assert!(dones.len() > 1, "{} dones", dones.len());
    assert!(dones[..dones.len() - 1].iter().all(|d| d.more));
    assert!(!dones.last().unwrap().more);
    let timed_out = x
        .nodes()
        .iter()
        .filter(|n| n.status == NodeStatus::TimedOut && n.reported_by == Some(id(B)))
        .count();
    assert_eq!(timed_out, silent as usize);
}

#[test]
fn an_entry_too_large_to_send_marks_the_answer_incomplete() {
    let mut net = Net::new(&[A, B, C]);
    let huge = WorkspaceRef {
        id: Some("ws".into()),
        name: Some("n".repeat(600_000)),
    };
    net.link(A, B).link_on(B, C, Some(huge));
    net.sharing.insert(
        id(B),
        Sharing {
            enabled: true,
            workspace: WorkspaceExposure::ALL,
        },
    );
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
    // What fit still arrived; the edge that did not is left out.
    assert_eq!(x.edges().len(), 1);
}
