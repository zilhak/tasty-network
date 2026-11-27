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
type FailRule = Box<dyn Fn(DeviceId, DeviceId, &Message) -> bool>;

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
    /// Sends matching a rule fail with `SendError::Failed`.
    fail: Vec<FailRule>,
}

struct Sends<'a> {
    me: DeviceId,
    links: &'a [LocalLink],
    no_caps: &'a HashSet<DeviceId>,
    fail: &'a [FailRule],
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
        if self.fail.iter().any(|rule| rule(self.me, *to, message)) {
            return Err(SendError::Failed);
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
            fail: Vec::new(),
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
            // A silent device never answers: keep its messages out of the queue.
            if self.silent.contains(&to) {
                self.log.push((from, to, message));
                continue;
            }
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
        let fail = std::mem::take(&mut self.fail);
        let mut sends = Sends {
            me: device,
            links: &links,
            no_caps: &no_caps,
            fail: &fail,
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
        self.fail = fail;
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
    // B never answered: timed out with a reason, and the exploration is not Finished.
    assert_eq!(x.state(), ExplorationState::Incomplete);
    assert_eq!(status(x, B), NodeStatus::TimedOut);
    assert_eq!(x.node(&id(B)).unwrap().reason, Some(NodeReason::Incomplete));
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
    assert_eq!(x.node(&id(D)).unwrap().reason, None);
    assert_eq!(x.state(), ExplorationState::Finished);
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
    // B drops it: nothing goes to D, nor to the claimed requester C.
    assert!(net.queue.is_empty());
}

#[test]
fn a_route_that_breaks_the_rules_is_answered_with_route_invalid() {
    // The link matches (B got it from the requester A), but the route repeats C.
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(B, C).link(C, D);
    let broken = tasty_net_topology::Query {
        request_id: "r".into(),
        requester: id(A),
        origin: id(D),
        route: vec![id(B), id(C), id(C)],
        path: Vec::new(),
        depth: 1,
        remaining_ms: 5_000,
        branch: "b".into(),
    };
    net.with_agent(id(B), |agent, local, out, now| {
        agent.on_message(&id(A), Message::Query(broken), local, out, now)
    });
    // B answers A (the checked previous hop) with the origin timed out for route_invalid.
    assert!(net.queue.iter().all(|m| m.to == id(A)));
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
            transport: TransportKind::P2pUdp,
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
    // C is the second hop; the query must come from B, not from A. It is dropped.
    net.with_agent(id(C), |agent, local, out, now| {
        agent.on_message(&id(A), Message::Query(forged.clone()), local, out, now)
    });
    assert!(net.queue.is_empty());
    // The control: from B it is forwarded to D.
    net.with_agent(id(C), |agent, local, out, now| {
        agent.on_message(&id(B), Message::Query(forged), local, out, now)
    });
    assert!(net.queue.iter().any(|m| m.to == id(D)));
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
        part: 0,
        incomplete: Vec::new(),
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
    assert_eq!(x.state(), ExplorationState::Incomplete);
    assert_eq!(status(x, C), NodeStatus::TimedOut);
    assert_eq!(x.node(&id(C)).unwrap().reason, Some(NodeReason::Incomplete));
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
    assert_eq!(x.state(), ExplorationState::Incomplete);
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

fn add_many_links(net: &mut Net, device: u8, count: u32) {
    for n in 0..count {
        let mut seed = [0xCCu8; 32];
        seed[..4].copy_from_slice(&n.to_be_bytes());
        net.links.get_mut(&id(device)).unwrap().push(LocalLink {
            peer: DeviceKey::from_seed(seed).id(),
            local_side: Side::Acceptor,
            holder: Side::Dialer,
            transport: TransportKind::P2pUdp,
            rtt: None,
            connected_at: u64::from(n),
            workspace: None,
            kind: LinkKind::Attach,
        });
    }
}

#[test]
fn a_part_that_fails_to_send_leaves_the_answer_incomplete() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    add_many_links(&mut net, B, 5_000);
    // B's second part does not go out.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B) && to == id(A) && matches!(m, Message::Result(p) if p.part == 1)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
    // After the failed part B sent nothing more: no later part, no done.
    assert!(net.log.iter().all(|(f, t, m)| {
        !(*f == id(B) && *t == id(A)) || matches!(m, Message::Result(p) if p.part == 0)
    }));
}

#[test]
fn a_relay_that_fails_stops_the_rest_and_the_requester_sees_the_gap() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    add_many_links(&mut net, C, 5_000);
    // B cannot relay C's second part to A.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B)
            && to == id(A)
            && matches!(m, Message::Result(p) if p.from == id(C) && p.part == 1)
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    assert_eq!(x.node(&id(C)).unwrap().reason, Some(NodeReason::Incomplete));
    assert_eq!(status(x, B), NodeStatus::Online);
    // Nothing of C after the failed part crossed B → A.
    assert!(net.log.iter().all(|(f, t, m)| {
        !(*f == id(B) && *t == id(A))
            || !matches!(m, Message::Result(p) if p.from == id(C) && p.part > 0)
                && !matches!(m, Message::Done(d) if d.from == id(C))
    }));
}

#[test]
fn a_first_part_that_is_not_relayed_is_reported_by_the_relaying_neighbor() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    // B cannot relay C's first part to A.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B)
            && to == id(A)
            && matches!(m, Message::Result(p) if p.from == id(C) && p.part == 0)
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let c = x.node(&id(C)).unwrap();
    assert_eq!(c.status, NodeStatus::TimedOut);
    assert_eq!(c.reason, Some(NodeReason::Incomplete));
    assert_eq!(c.reported_by, Some(id(B)));
    assert_eq!(status(x, B), NodeStatus::Online);
    // B's done carried the reason and did not wait for C's timeout.
    assert!(net.log.iter().any(|(f, t, m)| {
        *f == id(B)
            && *t == id(A)
            && matches!(m, Message::Done(d) if d.from == id(B)
            && d.incomplete == vec![id(C)] && d.timed_out.is_empty())
    }));
}

#[test]
fn a_done_whose_answer_never_arrived_marks_its_sender_incomplete() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    let rid = net.explore(A, request(A, 1)).unwrap();
    let branch = queued_branch(&net, A, B);
    // B's answer never arrives; only its done does.
    net.queue.clear();
    let done = Message::Done(tasty_net_topology::Done {
        request_id: rid.clone(),
        branch,
        from: id(B),
        return_path: vec![id(A)],
        timed_out: Vec::new(),
        is_final: false,
        more: false,
        part: 0,
        incomplete: Vec::new(),
    });
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(B), done, local, out, now)
    });
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
}

#[test]
fn a_missing_done_between_others_marks_its_sender_incomplete() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    let rid = net.explore(A, request(A, 1)).unwrap();
    let branch = queued_branch(&net, A, B);
    // B's answer is whole, but of its three dones the middle one never arrives.
    net.queue.clear();
    let part = Message::Result(tasty_net_topology::ResultPart {
        request_id: rid.clone(),
        branch: branch.clone(),
        from: id(B),
        return_path: vec![id(A)],
        part: 0,
        last: true,
        nodes: Vec::new(),
        edges: Vec::new(),
    });
    let done = |part: u32, more: bool| {
        Message::Done(tasty_net_topology::Done {
            request_id: rid.clone(),
            branch: branch.clone(),
            from: id(B),
            return_path: vec![id(A)],
            timed_out: Vec::new(),
            is_final: false,
            more,
            part,
            incomplete: Vec::new(),
        })
    };
    for message in [part, done(0, true), done(2, false)] {
        net.with_agent(id(A), |agent, local, out, now| {
            agent.on_message(&id(B), message, local, out, now)
        });
    }
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
}

#[test]
fn a_last_done_that_fails_to_send_is_reported_by_the_waiting_parent() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    // C answers, but its done does not reach B.
    net.fail.push(Box::new(|from, to, m| {
        from == id(C) && to == id(B) && matches!(m, Message::Done(_))
    }));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let c = x.node(&id(C)).unwrap();
    assert_eq!(c.status, NodeStatus::TimedOut);
    assert_eq!(c.reason, Some(NodeReason::Incomplete));
    // C's own answer had arrived; the state is now B's report.
    assert_eq!(c.reported_by, Some(id(B)));
    assert_eq!(status(x, B), NodeStatus::Online);
    // B's done carries C as incomplete, not as timed out.
    assert!(net.log.iter().any(|(f, t, m)| {
        *f == id(B)
            && *t == id(A)
            && matches!(m, Message::Done(d) if d.from == id(B)
            && d.incomplete == vec![id(C)] && d.timed_out.is_empty())
    }));
}

#[test]
fn a_remote_origin_that_fails_after_its_first_part_ends_incomplete_at_the_deadline() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    add_many_links(&mut net, B, 5_000);
    // The origin B sends its first part, then its second fails: no final done comes.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B) && to == id(A) && matches!(m, Message::Result(p) if p.part == 1)
    }));
    let rid = net.explore(A, request(B, 0)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
    assert!(
        !net.log
            .iter()
            .any(|(_, t, m)| *t == id(A) && matches!(m, Message::Done(_)))
    );
}

#[test]
fn the_requesters_own_edges_are_its_local_view_even_when_a_remote_copy_comes_first() {
    let mut net = Net::new(&[A, B]);
    let workspace = WorkspaceRef {
        id: Some("ws-1".into()),
        name: Some("my project".into()),
    };
    net.link_on(A, B, Some(workspace));
    // B shares no workspace field, and its query back to A arrives after its answer.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(B) && to == id(A) && matches!(m, Message::Query(_))).then_some(50)
    }));
    let rid = net.explore(A, request(B, 1)).unwrap();
    net.run();
    let order: Vec<bool> = net
        .log
        .iter()
        .filter(|(f, t, _)| *f == id(B) && *t == id(A))
        .filter_map(|(_, _, m)| match m {
            Message::Result(p) if p.from == id(B) => Some(true),
            Message::Query(_) => Some(false),
            _ => None,
        })
        .collect();
    assert_eq!(order.first(), Some(&true), "B's remote copy came first");
    let x = net.result(A, &rid);
    let ab: Vec<_> = x
        .edges()
        .iter()
        .filter(|e| e.touches(&id(A)) && e.touches(&id(B)))
        .collect();
    assert_eq!(ab.len(), 1, "{ab:?}");
    assert_eq!(
        ab[0].workspace.as_ref().and_then(|w| w.name.as_deref()),
        Some("my project")
    );
}

#[test]
fn an_answer_that_failed_after_forwarding_is_not_answered_again() {
    let mut net = Net::new(&[A, B, C, D, E]);
    net.link(A, B).link(A, C).link(B, D).link(C, D).link(D, E);
    // D gets the query through B first, then through C with the same depth.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(C) && to == id(D) && matches!(m, Message::Query(_))).then_some(20)
    }));
    // D forwards to E, then its first part to B fails.
    net.fail.push(Box::new(|from, to, m| {
        from == id(D) && to == id(B) && matches!(m, Message::Result(_))
    }));
    let rid = net.explore(A, request(A, 3)).unwrap();
    net.run();
    assert!(
        net.log
            .iter()
            .any(|(f, t, m)| { *f == id(D) && *t == id(E) && matches!(m, Message::Query(_)) })
    );
    // The query through C is suppressed instead of answered without E.
    assert!(
        net.log
            .iter()
            .any(|(f, t, m)| { *f == id(D) && *t == id(C) && matches!(m, Message::Suppressed(_)) })
    );
    assert!(!net.log.iter().any(|(f, t, m)| {
        *f == id(D) && *t == id(C) && matches!(m, Message::Result(p) if p.from == id(D))
    }));
    let x = net.result(A, &rid);
    assert_eq!(status(x, D), NodeStatus::TimedOut);
}

/// A explores itself at depth 1 with neighbors B and D. B's done reports D incomplete
/// (B could not relay D's answer on a longer path) and D's own answer on its direct
/// branch arrives whole; `report_first` puts B's done before D's answer. The messages
/// arrive 50 ms apart; also returns the Unix time B's done arrived at.
fn incomplete_report_against_a_direct_answer(report_first: bool) -> (Exploration, u64) {
    let mut net = Net::new(&[A, B, D]);
    net.link(A, B).link(A, D);
    let rid = net.explore(A, request(A, 1)).unwrap();
    let to_b = queued_branch(&net, A, B);
    let to_d = queued_branch(&net, A, D);
    net.queue.clear();
    let part = |branch: &str, from: u8| {
        Message::Result(tasty_net_topology::ResultPart {
            request_id: rid.clone(),
            branch: branch.to_owned(),
            from: id(from),
            return_path: vec![id(A)],
            part: 0,
            last: true,
            nodes: vec![tasty_net_topology::Node {
                device_id: Some(id(from)),
                display_name: None,
                status: NodeStatus::Online,
                reason: None,
                reported_by: None,
                peer_hint: None,
                explored_at: 0,
            }],
            edges: Vec::new(),
        })
    };
    let done = |branch: &str, from: u8, incomplete: Vec<DeviceId>| {
        Message::Done(tasty_net_topology::Done {
            request_id: rid.clone(),
            branch: branch.to_owned(),
            from: id(from),
            return_path: vec![id(A)],
            timed_out: Vec::new(),
            is_final: false,
            more: false,
            part: 0,
            incomplete,
        })
    };
    let report = vec![(B, part(&to_b, B)), (B, done(&to_b, B, vec![id(D)]))];
    let direct = vec![(D, part(&to_d, D)), (D, done(&to_d, D, Vec::new()))];
    let order = if report_first {
        report.into_iter().chain(direct).collect::<Vec<_>>()
    } else {
        direct.into_iter().chain(report).collect()
    };
    let mut reported_at = 0;
    for (i, (link, message)) in order.into_iter().enumerate() {
        net.clock_ms = 100 + 50 * u64::try_from(i).unwrap();
        if link == B && matches!(message, Message::Done(_)) {
            reported_at = net.now().unix_ms;
        }
        net.with_agent(id(A), |agent, local, out, now| {
            agent.on_message(&id(link), message, local, out, now)
        });
    }
    net.run();
    let x = net
        .agents
        .get_mut(&id(A))
        .unwrap()
        .take_exploration(&rid)
        .unwrap();
    (x, reported_at)
}

#[test]
fn a_reported_incomplete_device_stays_incomplete_in_either_order() {
    for report_first in [true, false] {
        let (x, reported_at) = incomplete_report_against_a_direct_answer(report_first);
        assert_eq!(x.state(), ExplorationState::Incomplete, "{report_first}");
        let d = x.node(&id(D)).unwrap();
        assert_eq!(d.status, NodeStatus::TimedOut, "{report_first}");
        assert_eq!(d.reason, Some(NodeReason::Incomplete), "{report_first}");
        assert_eq!(d.reported_by, Some(id(B)), "{report_first}");
        // The time of B's report, not of D's later answer or of the end.
        assert_eq!(d.explored_at, reported_at, "{report_first}");
        assert_eq!(status(&x, B), NodeStatus::Online, "{report_first}");
    }
}

#[test]
fn a_remote_origin_whose_only_done_is_lost_ends_incomplete() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B's whole answer arrives; its done does not go out.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B) && to == id(A) && matches!(m, Message::Done(_))
    }));
    let rid = net.explore(A, request(B, 0)).unwrap();
    net.run();
    assert!(net.log.iter().any(|(f, t, m)| {
        *f == id(B) && *t == id(A) && matches!(m, Message::Result(p) if p.last)
    }));
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
}

#[test]
fn a_remote_origin_whose_done_a_route_hop_cannot_relay_ends_incomplete() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    // The route hop B relays C's answer but not C's done.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B) && to == id(A) && matches!(m, Message::Done(d) if d.from == id(C))
    }));
    let request = ExploreRequest {
        origin: id(C),
        route: vec![id(B)],
        depth: 0,
        timeout: TIMEOUT,
    };
    let rid = net.explore(A, request).unwrap();
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    assert_eq!(x.node(&id(C)).unwrap().reason, Some(NodeReason::Incomplete));
}

#[test]
fn a_neighbor_whose_first_part_fails_to_send_is_timed_out_with_a_reason() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B gets the query, but its first part does not go out: A hears nothing from B.
    net.fail.push(Box::new(|from, to, m| {
        from == id(B) && to == id(A) && matches!(m, Message::Result(p) if p.part == 0)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run();
    assert!(!net.log.iter().any(|(f, t, _)| *f == id(B) && *t == id(A)));
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Incomplete);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
    assert_eq!(b.reported_by, Some(id(A)));
}

fn final_done(rid: &str, branch: &str, from: u8, return_path: Vec<DeviceId>) -> Message {
    Message::Done(tasty_net_topology::Done {
        request_id: rid.to_owned(),
        branch: branch.to_owned(),
        from: id(from),
        return_path,
        timed_out: Vec::new(),
        is_final: true,
        more: false,
        part: 0,
        incomplete: Vec::new(),
    })
}

#[test]
fn a_final_done_of_another_branch_does_not_end_a_remote_exploration() {
    let mut net = Net::new(&[A, B]);
    net.link(A, B);
    // B's real answer comes late.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(B) && to == id(A))
            .then_some(100)
            .filter(|_| !matches!(m, Message::Query(_)))
    }));
    let rid = net.explore(A, request(B, 0)).unwrap();
    net.run_until(50);
    // On B's link, B's final done for a branch A never sent.
    let wrong = final_done(&rid, "another-branch", B, vec![id(A)]);
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(B), wrong, local, out, now)
    });
    assert_eq!(net.result(A, &rid).state(), ExplorationState::Running);
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::Online);
}

#[test]
fn a_route_hop_relays_only_the_final_done_of_the_branch_it_forwarded() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    // C's real answer comes late.
    net.delay.push(Box::new(|from, to, m| {
        (from == id(C) && to == id(B))
            .then_some(100)
            .filter(|_| !matches!(m, Message::Query(_)))
    }));
    let request = ExploreRequest {
        origin: id(C),
        route: vec![id(B)],
        depth: 0,
        timeout: TIMEOUT,
    };
    let rid = net.explore(A, request).unwrap();
    net.run_until(50);
    // On C's link, C's final done for a branch B never forwarded.
    let wrong = final_done(&rid, "another-branch", C, vec![id(B), id(A)]);
    net.with_agent(id(B), |agent, local, out, now| {
        agent.on_message(&id(C), wrong, local, out, now)
    });
    // B relayed nothing of it to A (C's real done is still on its way to B).
    assert!(
        net.queue
            .iter()
            .all(|m| m.from != id(B) || !matches!(&m.message, Message::Done(_)))
    );
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, C), NodeStatus::Online);
}

#[test]
fn the_requesters_own_edges_are_its_local_view_when_it_is_not_visited_again() {
    let mut net = Net::new(&[A, B]);
    let workspace = WorkspaceRef {
        id: Some("ws-1".into()),
        name: Some("my project".into()),
    };
    net.link_on(A, B, Some(workspace));
    // Depth 0 at B: B answers with its edges and never queries A.
    let rid = net.explore(A, request(B, 0)).unwrap();
    net.run();
    assert!(
        !net.log
            .iter()
            .any(|(f, t, m)| { *f == id(B) && *t == id(A) && matches!(m, Message::Query(_)) })
    );
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    let ab: Vec<_> = x
        .edges()
        .iter()
        .filter(|e| e.touches(&id(A)) && e.touches(&id(B)))
        .collect();
    assert_eq!(ab.len(), 1, "{ab:?}");
    assert_eq!(
        ab[0].workspace.as_ref().and_then(|w| w.name.as_deref()),
        Some("my project")
    );
}

#[test]
fn two_attaches_of_one_link_stay_two_local_edges_when_not_visited_again() {
    // A dials B twice, the same way, for two workspaces. B shares the workspace ID only,
    // or no workspace field at all.
    for exposure in [
        WorkspaceExposure {
            id: true,
            name: false,
        },
        WorkspaceExposure::NONE,
    ] {
        let mut net = Net::new(&[A, B]);
        for (ws, name) in [("ws-1", "one"), ("ws-2", "two")] {
            let workspace = WorkspaceRef {
                id: Some(ws.into()),
                name: Some(name.into()),
            };
            net.link_on(A, B, Some(workspace));
        }
        net.sharing.insert(
            id(B),
            Sharing {
                enabled: true,
                workspace: exposure,
            },
        );
        let rid = net.explore(A, request(B, 0)).unwrap();
        net.run();
        let x = net.result(A, &rid);
        let mut names: Vec<_> = x
            .edges()
            .iter()
            .filter(|e| e.touches(&id(A)) && e.touches(&id(B)))
            .map(|e| e.workspace.as_ref().and_then(|w| w.name.clone()))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![Some("one".to_owned()), Some("two".to_owned())],
            "{exposure:?}"
        );
    }
}

#[test]
fn a_neighbor_that_never_answered_wins_over_an_old_connect_timeout() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(A, C);
    // C once timed out connecting to B and reports that; B never answers A's query.
    net.failures.insert(
        id(C),
        vec![ConnectFailure {
            peer_hint: Some("b".into()),
            device_id: Some(id(B)),
            kind: FailureKind::TimedOut,
            observed_at: 5,
        }],
    );
    net.silent.insert(id(B));
    let rid = net.explore(A, request(A, 1)).unwrap();
    net.run_until(1_000);
    // C's observation came first.
    let early = net.result(A, &rid).node(&id(B)).unwrap().clone();
    assert_eq!(early.status, NodeStatus::TimedOut);
    assert_eq!(early.reason, None);
    assert_eq!(early.reported_by, Some(id(C)));
    net.run();
    let x = net.result(A, &rid);
    let b = x.node(&id(B)).unwrap();
    assert_eq!(b.status, NodeStatus::TimedOut);
    assert_eq!(b.reason, Some(NodeReason::Incomplete));
    assert_eq!(b.reported_by, Some(id(A)));
    assert_eq!(x.state(), ExplorationState::Incomplete);
}

#[test]
fn a_first_hop_without_the_capability_stays_in_the_result() {
    let mut net = Net::new(&[A, B, C]);
    net.link(A, B).link(B, C);
    net.no_caps.insert(id(B));
    // B is the origin.
    let rid = net.explore(A, request(B, 1)).unwrap();
    assert!(net.queue.is_empty());
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::NoCapability);
    assert_eq!(x.node(&id(B)).unwrap().reported_by, Some(id(A)));

    // B is the first route hop toward C.
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
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(status(x, B), NodeStatus::NoCapability);
    let c = x.node(&id(C)).unwrap();
    assert_eq!(c.status, NodeStatus::TimedOut);
    assert_eq!(c.reason, Some(NodeReason::RouteNoCapability));
}

#[test]
fn a_hop_timeout_of_duration_max_does_not_panic() {
    let mut net = Net::new(&[A, B, C]);
    let config = AgentConfig {
        hop_timeout: Duration::MAX,
        hop_reserve: Duration::from_millis(100),
    };
    for n in [A, B, C] {
        let me = SelfInfo {
            device_id: id(n),
            display_name: None,
        };
        net.agents.insert(id(n), Agent::new(me, config));
    }
    net.link(A, B).link(B, C);
    net.silent.insert(id(C));
    let rid = net.explore(A, request(A, 2)).unwrap();
    net.run();
    let x = net.result(A, &rid);
    // C never answers; B gives it up at its branch deadline and the exploration ends
    // with C as an answer that did not come.
    assert_eq!(x.state(), ExplorationState::Incomplete);
    assert_eq!(status(x, C), NodeStatus::TimedOut);
}

/// The branch of the query `from` sent to `to`, still in flight.
fn queued_branch(net: &Net, from: u8, to: u8) -> String {
    net.queue
        .iter()
        .find_map(|m| match &m.message {
            Message::Query(q) if m.from == id(from) && m.to == id(to) => Some(q.branch.clone()),
            _ => None,
        })
        .expect("query in flight")
}

#[test]
fn a_reply_on_a_waited_link_but_for_another_branch_is_dropped() {
    let mut net = Net::new(&[A, B, C, D]);
    net.link(A, B).link(A, C);
    // Both queries are still in flight, so A waits on both B and C.
    net.delay.push(Box::new(|from, _, m| {
        (from == id(A) && matches!(m, Message::Query(_))).then_some(1_000)
    }));
    let rid = net.explore(A, request(A, 1)).unwrap();
    let c_branch = queued_branch(&net, A, C);

    // From B's link, a result claiming to be C's answer to C's branch.
    let forged = tasty_net_topology::ResultPart {
        request_id: rid.clone(),
        branch: c_branch,
        from: id(C),
        return_path: vec![id(A)],
        part: 0,
        last: true,
        nodes: vec![tasty_net_topology::Node {
            device_id: Some(id(D)),
            display_name: None,
            status: NodeStatus::Online,
            reason: None,
            reported_by: Some(id(C)),
            peer_hint: None,
            explored_at: 0,
        }],
        edges: Vec::new(),
    };
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(B), Message::Result(forged), local, out, now)
    });
    assert!(net.result(A, &rid).node(&id(D)).is_none());

    // From B's link, a final done in the origin's name: A is the origin, so no link can
    // end its exploration.
    let forged_final = tasty_net_topology::Done {
        request_id: rid.clone(),
        branch: "x".into(),
        from: id(A),
        return_path: vec![id(A)],
        timed_out: vec![id(D)],
        is_final: true,
        more: false,
        part: 0,
        incomplete: Vec::new(),
    };
    net.with_agent(id(A), |agent, local, out, now| {
        agent.on_message(&id(B), Message::Done(forged_final), local, out, now)
    });
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Running);
    assert!(x.node(&id(D)).is_none());

    // The control: the real answers still finish it.
    net.run();
    let x = net.result(A, &rid);
    assert_eq!(x.state(), ExplorationState::Finished);
    assert_eq!(node_ids(x), sorted(&[A, B, C]));
}
