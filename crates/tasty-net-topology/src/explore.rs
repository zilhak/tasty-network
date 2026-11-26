//! The exploration state machine (docs/design/topology.md "탐색 규칙").
//!
//! One [`Agent`] per device answers queries ([`Agent::answer_query`]), relays results
//! back along their return path, tracks the neighbors it forwarded to, and runs the
//! explorations this device started ([`Agent::explore`]). It never touches a transport:
//! the consumer passes an [`Outbox`] that sends one message to one neighbor, feeds every
//! received topology message to [`Agent::on_message`], and calls [`Agent::poll`] at
//! [`Agent::next_deadline`]. Time comes in as [`Now`], so tests drive it with fake
//! neighbors and a fake clock.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

use tasty_net_identity::DeviceId;

use crate::TopologyError;
use crate::ids::random_id;
use crate::local::{
    ConnectFailure, LinkKind, LocalLink, SelfInfo, Sharing, View, local_view, remote_answer,
    self_node,
};
use crate::message::{
    Done, MAX_MESSAGE_JSON, MAX_QUERY_MS, Message, MessageError, Query, ResultHeader, ResultPart,
    Suppressed, split_done, split_result, validate_route,
};
use crate::model::{Edge, EdgeKey, Node, NodeReason, NodeStatus};

/// Sends one message to one neighbor over its authenticated attach link.
pub trait Outbox {
    fn send(&mut self, to: &DeviceId, message: &Message) -> Result<(), SendError>;
}

/// Why [`Outbox::send`] did not send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendError {
    /// No authenticated attach link to that device.
    NoLink,
    /// The link is up but the peer did not announce [`crate::CAPABILITY`]; nothing was
    /// sent (tasty drops unknown events, so no answer would come).
    NoCapability,
    /// Writing to the link failed.
    Failed,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoLink => "no authenticated attach link to the device",
            Self::NoCapability => "the device does not announce the topology capability",
            Self::Failed => "writing the topology message to the link failed",
        })
    }
}

impl std::error::Error for SendError {}

/// The local input of one call: this device's links, failures and sharing setting.
#[derive(Debug, Clone, Copy)]
pub struct Local<'a> {
    pub links: &'a [LocalLink],
    pub failures: &'a [ConnectFailure],
    pub sharing: Sharing,
}

/// The time of one call. `mono` judges every deadline; `unix_ms` only stamps nodes.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub mono: Instant,
    pub unix_ms: u64,
}

/// Timing of an [`Agent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConfig {
    /// How long a forwarding device waits for the first message of a neighbor (a result
    /// part or a suppression) before it gives the neighbor up as timed out.
    pub hop_timeout: Duration,
    /// Time a device keeps back when it forwards, so its neighbors finish before its own
    /// deadline and its `done` still reaches the requester in time.
    pub hop_reserve: Duration,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            hop_timeout: Duration::from_secs(5),
            hop_reserve: Duration::from_millis(250),
        }
    }
}

/// What the requester asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreRequest {
    /// The device the exploration starts from.
    pub origin: DeviceId,
    /// The devices between this device and the origin (see [`Query::route`]).
    pub route: Vec<DeviceId>,
    pub depth: u32,
    /// The whole time limit, capped at [`MAX_QUERY_MS`].
    pub timeout: Duration,
}

/// Where an exploration is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorationState {
    /// Results may still arrive.
    Running,
    /// The final `done` arrived and every answer arrived whole.
    Finished,
    /// The final `done` arrived or the requester's deadline passed, and at least one
    /// answer is incomplete (its node is [`NodeStatus::TimedOut`] with
    /// [`NodeReason::Incomplete`]).
    Incomplete,
    /// The requester's deadline passed first, with no answer known to be incomplete.
    TimedOut,
}

/// The merged, progressive result of one exploration.
#[derive(Debug, Clone)]
pub struct Exploration {
    request_id: String,
    origin: DeviceId,
    route: Vec<DeviceId>,
    /// The link the query left on; `None` when this device is the origin.
    first_hop: Option<DeviceId>,
    me: DeviceId,
    deadline: Instant,
    state: ExplorationState,
    nodes: Vec<Node>,
    node_index: HashMap<NodeKey, (usize, bool)>,
    edges: Vec<Edge>,
    edge_keys: HashSet<EdgeKey>,
    /// The parts received per `(branch, from)`, to notice a missing one.
    parts: HashMap<(String, DeviceId), PartTrack>,
    /// The dones received per `(branch, from)`, counted like the parts.
    done_parts: HashMap<(String, DeviceId), PartTrack>,
    /// This device's local view was merged: its own edges are the local view's.
    local_merged: bool,
    /// Devices known to have an incomplete answer → who reported it (`None` when this
    /// device found it). Kept apart from the nodes so a later direct answer of the same
    /// device does not erase it: it is applied again after each merge and at the end.
    incomplete: HashMap<DeviceId, Option<DeviceId>>,
}

#[derive(Debug, Clone, Default)]
struct PartTrack {
    seen: HashSet<u32>,
    last: Option<u32>,
}

impl PartTrack {
    /// Every part from 0 to the last arrived. Counted in `u64`, so no part number
    /// overflows on any target.
    fn complete(&self) -> bool {
        self.last.is_some_and(|last| {
            u64::try_from(self.seen.len()).is_ok_and(|n| n == u64::from(last) + 1)
                && self.seen.iter().all(|p| *p <= last)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NodeKey {
    Id(DeviceId),
    Hint(Option<DeviceId>, Option<String>),
}

impl Exploration {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn origin(&self) -> &DeviceId {
        &self.origin
    }

    pub fn state(&self) -> ExplorationState {
        self.state
    }

    /// The nodes merged so far, in order of first arrival.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The edges merged so far, in order of first arrival.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// The node of `device`, if any.
    pub fn node(&self, device: &DeviceId) -> Option<&Node> {
        self.node_index
            .get(&NodeKey::Id(*device))
            .map(|&(at, _)| &self.nodes[at])
    }

    /// This device's own view, merged without crossing a link.
    /// Merges this device's local view. Its own edges come from here only: remote copies
    /// of them that arrived before are dropped and later ones are skipped (`merge_part`),
    /// so its screen shows its own edges as the local view has them whatever the order.
    fn merge_view(&mut self, view: View, unix_ms: u64) {
        let me = self.me;
        for node in view.nodes {
            self.merge_node(node, &me, unix_ms);
        }
        if !self.local_merged {
            self.local_merged = true;
            self.edges.retain(|edge| !edge.touches(&me));
            self.edge_keys = self.edges.iter().map(Edge::key).collect();
        }
        for edge in view.edges {
            if self.edge_keys.insert(edge.key()) {
                self.edges.push(edge);
            }
        }
    }

    fn merge_part(&mut self, part: ResultPart, unix_ms: u64) {
        // This device's own parts were merged as its local view already.
        if part.from == self.me {
            return;
        }
        let track = self
            .parts
            .entry((part.branch.clone(), part.from))
            .or_default();
        track.seen.insert(part.part);
        if part.last {
            track.last = Some(part.part);
        }
        for node in part.nodes {
            self.merge_node(node, &part.from, unix_ms);
        }
        for edge in part.edges {
            if self.local_merged && edge.touches(&self.me) {
                continue;
            }
            if self.edge_keys.insert(edge.key()) {
                self.edges.push(edge);
            }
        }
    }

    /// Merges one node (docs/design/topology.md "같은 노드에 대한 합치기"): a node's own
    /// answer wins over any other report; among reports, the more specific state wins.
    fn merge_node(&mut self, mut node: Node, from: &DeviceId, unix_ms: u64) {
        let direct = node.device_id == Some(*from) && node.reported_by.is_none();
        if !direct && node.reported_by.is_none() {
            node.reported_by = Some(*from);
        }
        node.explored_at = unix_ms;
        let device = node.device_id;
        let key = match node.device_id {
            Some(id) => NodeKey::Id(id),
            None => NodeKey::Hint(node.reported_by, node.peer_hint.clone()),
        };
        match self.node_index.get(&key).copied() {
            None => {
                self.node_index.insert(key, (self.nodes.len(), direct));
                self.nodes.push(node);
            }
            Some((at, was_direct)) => {
                let replace = if direct {
                    true
                } else {
                    !was_direct && report_rank(node.status) > report_rank(self.nodes[at].status)
                };
                if replace {
                    self.nodes[at] = node;
                    self.node_index.insert(key, (at, was_direct || direct));
                }
            }
        }
        if let Some(device) = device {
            self.apply_incomplete(device, unix_ms);
        }
    }

    /// Counts one done of `branch` by `from`. This device's own dones are not counted (its
    /// own answer was merged as its local view).
    fn track_done(&mut self, done: &Done) {
        if done.from == self.me {
            return;
        }
        let track = self
            .done_parts
            .entry((done.branch.clone(), done.from))
            .or_default();
        track.seen.insert(done.part);
        if !done.more {
            track.last = Some(done.part);
        }
    }

    /// The answer of `branch` by `from` ended (its last done arrived): missing result
    /// parts or dones mark `from` incomplete.
    /// A done with no part at all before it is incomplete too: the answer's first part
    /// was lost. This device's own done carries no tracked parts and is skipped.
    fn check_parts(&mut self, branch: &str, from: &DeviceId, unix_ms: u64) {
        if *from == self.me {
            return;
        }
        let key = (branch.to_owned(), *from);
        let parts_broken = self.parts.get(&key).is_none_or(|t| !t.complete());
        let dones_broken = self.done_parts.get(&key).is_some_and(|t| !t.complete());
        if parts_broken || dones_broken {
            self.mark_incomplete(*from, None, unix_ms);
        }
    }

    fn has_incomplete(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.reason == Some(NodeReason::Incomplete))
    }

    /// Every answer with a missing result part or done, or with results but no done at
    /// all, marks its device incomplete, and every device known incomplete is marked
    /// again.
    fn check_all_parts(&mut self, unix_ms: u64) {
        let no_done = self
            .parts
            .keys()
            .filter(|key| !self.done_parts.contains_key(*key));
        let broken: Vec<DeviceId> = self
            .parts
            .iter()
            .chain(&self.done_parts)
            .filter(|(_, t)| !t.complete())
            .map(|(key, _)| key)
            .chain(no_done)
            .map(|(_, from)| *from)
            .collect();
        for device in broken {
            self.mark_incomplete(device, None, unix_ms);
        }
        let known: Vec<DeviceId> = self.incomplete.keys().copied().collect();
        for device in known {
            self.apply_incomplete(device, unix_ms);
        }
    }

    /// Records `device` as incomplete and marks its node timed out with reason incomplete,
    /// over any answer of its own, now and after any later merge.
    fn mark_incomplete(&mut self, device: DeviceId, reporter: Option<DeviceId>, unix_ms: u64) {
        let entry = self.incomplete.entry(device).or_insert(reporter);
        if reporter.is_some() {
            *entry = reporter;
        }
        self.apply_incomplete(device, unix_ms);
    }

    /// Marks the node of `device` incomplete if `device` is recorded so. A new node
    /// records the reporter.
    fn apply_incomplete(&mut self, device: DeviceId, unix_ms: u64) {
        let Some(&reporter) = self.incomplete.get(&device) else {
            return;
        };
        let key = NodeKey::Id(device);
        match self.node_index.get(&key).copied() {
            Some((at, _)) => {
                let node = &mut self.nodes[at];
                node.status = NodeStatus::TimedOut;
                node.reason = Some(NodeReason::Incomplete);
                node.explored_at = unix_ms;
            }
            None => {
                self.node_index.insert(key, (self.nodes.len(), true));
                self.nodes.push(Node {
                    device_id: Some(device),
                    display_name: None,
                    status: NodeStatus::TimedOut,
                    reason: Some(NodeReason::Incomplete),
                    reported_by: reporter,
                    peer_hint: None,
                    explored_at: unix_ms,
                });
            }
        }
    }

    /// `reporter` could not relay the answer of `device` whole, or its done never came:
    /// part of that answer is lost even when other parts of it arrived.
    fn report_incomplete(&mut self, device: DeviceId, reporter: &DeviceId, unix_ms: u64) {
        self.mark_incomplete(device, Some(*reporter), unix_ms);
    }

    /// `reporter` sent `device` the query and gave it up before its first part: timed
    /// out with reason incomplete, so the exploration does not end `Finished`. A direct
    /// answer of `device` on another path still wins (as for any report).
    fn report_no_answer(&mut self, device: DeviceId, reporter: &DeviceId, unix_ms: u64) {
        self.merge_node(
            Node {
                device_id: Some(device),
                display_name: None,
                status: NodeStatus::TimedOut,
                reason: Some(NodeReason::Incomplete),
                reported_by: Some(*reporter),
                peer_hint: None,
                explored_at: unix_ms,
            },
            reporter,
            unix_ms,
        );
    }

    fn report_timed_out(&mut self, device: DeviceId, reporter: &DeviceId, unix_ms: u64) {
        self.merge_node(
            Node {
                device_id: Some(device),
                display_name: None,
                status: NodeStatus::TimedOut,
                reason: None,
                reported_by: Some(*reporter),
                peer_hint: None,
                explored_at: unix_ms,
            },
            reporter,
            unix_ms,
        );
    }
}

fn report_rank(status: NodeStatus) -> u8 {
    match status {
        NodeStatus::AuthRejected => 3,
        NodeStatus::NoCapability => 2,
        NodeStatus::TimedOut => 1,
        NodeStatus::Online | NodeStatus::Offline | NodeStatus::SharingOff => 0,
    }
}

struct Answered {
    depth: u32,
    expires: Instant,
}

/// A route hop's record of where it forwarded a query, so it relays replies only from
/// that link.
struct RouteForward {
    next: DeviceId,
    expires: Instant,
}

struct Child {
    peer: DeviceId,
    answer_by: Instant,
    answered: bool,
}

/// One branch this device answered and forwarded, waiting for its neighbors.
struct Wait {
    request_id: String,
    branch: String,
    return_path: Vec<DeviceId>,
    is_final: bool,
    deadline: Instant,
    pending: HashMap<String, Child>,
    timed_out: Vec<DeviceId>,
    /// Devices whose answer this device could not relay whole.
    incomplete: Vec<DeviceId>,
}

/// The topology state machine of one device.
pub struct Agent {
    me: SelfInfo,
    config: AgentConfig,
    answered: HashMap<String, Answered>,
    waits: HashMap<String, Wait>,
    /// Child branch → the branch of ours that forwarded it.
    children: HashMap<String, String>,
    /// Request ID → the next hop this device forwarded it to as a route hop.
    routes: HashMap<String, RouteForward>,
    /// `(request_id, branch, from)` of answers whose relay failed → when to forget it.
    failed_relays: HashMap<(String, String, DeviceId), Instant>,
    explorations: HashMap<String, Exploration>,
}

impl Agent {
    pub fn new(me: SelfInfo, config: AgentConfig) -> Self {
        Self {
            me,
            config,
            answered: HashMap::new(),
            waits: HashMap::new(),
            children: HashMap::new(),
            routes: HashMap::new(),
            failed_relays: HashMap::new(),
            explorations: HashMap::new(),
        }
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.me.device_id
    }

    /// Starts an exploration from this device and returns its request ID. The route is
    /// checked first ([`TopologyError::InvalidRoute`]); a first hop with no link or a
    /// failed send is [`TopologyError::FirstHop`]. A first hop without the capability ends
    /// the exploration at once with that node as [`NodeStatus::NoCapability`] (and, when it
    /// is a route hop, the origin as timed out with [`NodeReason::RouteNoCapability`]). This device's own part of the exploration is its
    /// local view, merged directly; what it sends on links is its remote answer.
    pub fn explore(
        &mut self,
        request: ExploreRequest,
        local: Local<'_>,
        out: &mut dyn Outbox,
        now: Now,
    ) -> Result<String, TopologyError> {
        let me = self.me.device_id;
        validate_route(&me, &request.origin, &request.route)
            .map_err(TopologyError::InvalidRoute)?;
        let timeout = request.timeout.min(Duration::from_millis(MAX_QUERY_MS));
        let request_id = random_id()?;
        let branch = random_id()?;
        let query = Query {
            request_id: request_id.clone(),
            requester: me,
            origin: request.origin,
            route: request.route.clone(),
            path: Vec::new(),
            depth: request.depth,
            remaining_ms: millis(timeout.saturating_sub(self.config.hop_reserve)),
            branch,
        };
        let mut exploration = Exploration {
            request_id: request_id.clone(),
            origin: request.origin,
            route: request.route.clone(),
            first_hop: (request.origin != me)
                .then(|| request.route.first().copied().unwrap_or(request.origin)),
            me,
            deadline: now.mono + timeout,
            state: ExplorationState::Running,
            nodes: Vec::new(),
            node_index: HashMap::new(),
            edges: Vec::new(),
            edge_keys: HashSet::new(),
            parts: HashMap::new(),
            done_parts: HashMap::new(),
            local_merged: false,
            incomplete: HashMap::new(),
        };
        if request.origin == me {
            self.explorations.insert(request_id.clone(), exploration);
            self.answer_query(None, query, local, out, now);
        } else {
            let first = request.route.first().copied().unwrap_or(request.origin);
            match out.send(&first, &Message::Query(query)) {
                Ok(()) => {
                    self.explorations.insert(request_id.clone(), exploration);
                }
                Err(SendError::NoCapability) => {
                    // Nothing was sent: the first hop is a no_capability node, and a
                    // route that cannot start leaves the origin timed out with that reason.
                    let reported = |device, status, reason| Node {
                        device_id: Some(device),
                        display_name: None,
                        status,
                        reason,
                        reported_by: Some(me),
                        peer_hint: None,
                        explored_at: now.unix_ms,
                    };
                    exploration.merge_node(
                        reported(first, NodeStatus::NoCapability, None),
                        &me,
                        now.unix_ms,
                    );
                    if first != request.origin {
                        exploration.merge_node(
                            reported(
                                request.origin,
                                NodeStatus::TimedOut,
                                Some(NodeReason::RouteNoCapability),
                            ),
                            &me,
                            now.unix_ms,
                        );
                    }
                    exploration.state = ExplorationState::Finished;
                    self.explorations.insert(request_id.clone(), exploration);
                }
                Err(e) => return Err(TopologyError::FirstHop(e)),
            }
        }
        Ok(request_id)
    }

    /// The exploration `request_id`, while this agent keeps it.
    pub fn exploration(&self, request_id: &str) -> Option<&Exploration> {
        self.explorations.get(request_id)
    }

    /// Removes the exploration `request_id` and gives it to the caller. The agent keeps
    /// every exploration until it is taken.
    pub fn take_exploration(&mut self, request_id: &str) -> Option<Exploration> {
        self.explorations.remove(request_id)
    }

    /// Handles one topology message received from the authenticated attach link with
    /// `from`. Every deadline that passed by `now` is applied first ([`Agent::poll`]), so a
    /// reply that arrives after the requester's deadline, its branch's deadline or its
    /// neighbor's first-answer deadline is dropped instead of merged or relayed.
    pub fn on_message(
        &mut self,
        from: &DeviceId,
        message: Message,
        local: Local<'_>,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        self.poll(out, now);
        match message {
            Message::Query(query) => self.answer_query(Some(from), query, local, out, now),
            reply => self.receive_reply(Some(from), reply, out, now),
        }
    }

    /// The answering side: answers `query` with this device's view and forwards it to the
    /// next route hop or to the neighbors. `from` is the peer of the link it came on
    /// (`None` only for the requester's own query to itself).
    pub fn answer_query(
        &mut self,
        from: Option<&DeviceId>,
        query: Query,
        local: Local<'_>,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let me = self.me.device_id;
        self.answered.retain(|_, seen| seen.expires > now.mono);
        let deadline = now.mono + Duration::from_millis(query.remaining_ms.min(MAX_QUERY_MS));
        if query.path.is_empty() && query.origin != me {
            self.relay_route(from, query, local, out, now, deadline);
            return;
        }
        if !query_path_ok(&me, from, &query) {
            tracing::debug!("topology query whose path does not match its link dropped");
            return;
        }
        let is_origin = query.path.is_empty();
        // Back along `path`, then `route`, then to the requester. When the requester is
        // the origin, `path` already ends there.
        let mut return_path: Vec<DeviceId> = query
            .path
            .iter()
            .rev()
            .chain(query.route.iter().rev())
            .copied()
            .collect();
        if return_path.last() != Some(&query.requester) {
            return_path.push(query.requester);
        }

        if let Some(seen) = self.answered.get(&query.request_id)
            && seen.depth >= query.depth
            && seen.expires > now.mono
        {
            let message = Message::Suppressed(Suppressed {
                request_id: query.request_id,
                branch: query.branch,
                from: me,
                return_path,
            });
            self.send_reply(message, out, now);
            return;
        }
        let expires = self
            .answered
            .get(&query.request_id)
            .map_or(deadline, |seen| seen.expires.max(deadline));
        // Kept to undo the record if the answer cannot be sent, so the same query can be
        // answered again instead of suppressed.
        let previous = self
            .answered
            .get(&query.request_id)
            .map(|seen| (seen.depth, seen.expires));
        self.answered.insert(
            query.request_id.clone(),
            Answered {
                depth: query.depth,
                expires,
            },
        );

        // What goes on a link is always the remote answer. The requester's own screen gets
        // the local view merged directly; its parts on the link are for the waiting parent
        // only and are not merged again (`deliver`). An answer that does not leave this
        // device (the requester is the origin) is not a remote answer, so sharing off does
        // not stop the requester's own exploration.
        let is_local = query.requester == me;
        let on_link = return_path.as_slice() != [me];
        if is_local {
            let view = local_view(&self.me, local.links, local.failures, now.unix_ms);
            if let Some(exploration) = self.explorations.get_mut(&query.request_id)
                && exploration.state == ExplorationState::Running
            {
                exploration.merge_view(view, now.unix_ms);
            }
        }
        let header = ResultHeader {
            request_id: query.request_id.clone(),
            branch: query.branch.clone(),
            from: me,
            return_path: return_path.clone(),
        };
        let mut view = remote_answer(
            &self.me,
            local.links,
            local.failures,
            local.sharing,
            now.unix_ms,
        );
        if on_link && !local.sharing.enabled {
            if self.send_view(&header, view, out, now) {
                self.send_done(&header, Vec::new(), Vec::new(), is_origin, out, now);
            } else {
                self.undo_answered(&query.request_id, previous);
            }
            return;
        }

        let mut pending = HashMap::new();
        let mut timed_out = Vec::new();
        let child_ms = millis(
            deadline
                .saturating_duration_since(now.mono)
                .saturating_sub(self.config.hop_reserve),
        );
        if query.depth > 0 && child_ms > 0 {
            let mut path = query.path.clone();
            path.push(me);
            for peer in neighbors(&me, local.links, &path) {
                let Ok(branch) = random_id() else {
                    timed_out.push(peer);
                    continue;
                };
                let child = Query {
                    request_id: query.request_id.clone(),
                    requester: query.requester,
                    origin: query.origin,
                    route: query.route.clone(),
                    path: path.clone(),
                    depth: query.depth - 1,
                    remaining_ms: child_ms,
                    branch: branch.clone(),
                };
                match out.send(&peer, &Message::Query(child)) {
                    Ok(()) => {
                        pending.insert(
                            branch,
                            Child {
                                peer,
                                answer_by: answer_by(now.mono, self.config.hop_timeout, deadline),
                                answered: false,
                            },
                        );
                    }
                    Err(SendError::NoCapability) => {
                        let node = Node {
                            device_id: Some(peer),
                            display_name: None,
                            status: NodeStatus::NoCapability,
                            reason: None,
                            reported_by: Some(me),
                            peer_hint: None,
                            explored_at: now.unix_ms,
                        };
                        if is_local
                            && let Some(exploration) = self.explorations.get_mut(&query.request_id)
                            && exploration.state == ExplorationState::Running
                        {
                            exploration.merge_node(node.clone(), &me, now.unix_ms);
                        }
                        view.nodes.push(node);
                    }
                    Err(SendError::NoLink | SendError::Failed) => timed_out.push(peer),
                }
            }
        }
        if !self.send_view(&header, view, out, now) {
            // The branch failed: no done follows and the neighbors' answers are not
            // waited for (their replies are dropped as unexpected). The record is undone
            // only when no neighbor got the query: a neighbor that did would answer a
            // retry with a suppression, hiding the answer this device dropped.
            if pending.is_empty() {
                self.undo_answered(&query.request_id, previous);
            }
            return;
        }
        if pending.is_empty() {
            self.send_done(&header, timed_out, Vec::new(), is_origin, out, now);
            return;
        }
        for branch in pending.keys() {
            self.children.insert(branch.clone(), query.branch.clone());
        }
        self.waits.insert(
            query.branch.clone(),
            Wait {
                request_id: query.request_id,
                branch: query.branch,
                return_path,
                is_final: is_origin,
                deadline,
                pending,
                timed_out,
                incomplete: Vec::new(),
            },
        );
    }

    fn undo_answered(&mut self, request_id: &str, previous: Option<(u32, Instant)>) {
        match previous {
            Some((depth, expires)) => {
                self.answered
                    .insert(request_id.to_owned(), Answered { depth, expires });
            }
            None => {
                self.answered.remove(request_id);
            }
        }
    }

    /// A query on its way to the origin: forward it to the next hop, or stop it with a
    /// reason (docs/design/topology.md "탐색 규칙", "전달 hop 상한").
    fn relay_route(
        &mut self,
        from: Option<&DeviceId>,
        query: Query,
        local: Local<'_>,
        out: &mut dyn Outbox,
        now: Now,
        deadline: Instant,
    ) {
        let me = self.me.device_id;
        let at = query.route.iter().position(|hop| *hop == me);
        let back: Vec<DeviceId> = query.route[..at.unwrap_or(0)]
            .iter()
            .rev()
            .copied()
            .chain(std::iter::once(query.requester))
            .collect();
        let header = ResultHeader {
            request_id: query.request_id.clone(),
            branch: query.branch.clone(),
            from: me,
            return_path: back,
        };
        // The link first: a query that did not come from the previous device (or that does
        // not name this device on its route) is dropped, since its return path cannot be
        // trusted. Only a route that breaks the rules of `validate_route` is answered with
        // `route_invalid`, back along the checked previous hop.
        let link_ok = match at {
            Some(0) => from == Some(&query.requester),
            Some(k) => from == query.route.get(k - 1),
            None => false,
        };
        let Some(at) = at.filter(|_| link_ok) else {
            tracing::debug!("topology route query that does not match its link dropped");
            return;
        };
        if validate_route(&query.requester, &query.origin, &query.route).is_err() {
            self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteInvalid),
                None,
                out,
                now,
            );
            return;
        }
        if !local.sharing.enabled {
            let own = self_node(&self.me, NodeStatus::SharingOff, now.unix_ms);
            self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteSharingOff),
                Some(own),
                out,
                now,
            );
            return;
        }
        let child_ms = millis(
            deadline
                .saturating_duration_since(now.mono)
                .saturating_sub(self.config.hop_reserve),
        );
        if child_ms == 0 {
            self.stop_route(&header, &query, None, None, out, now);
            return;
        }
        let Ok(branch) = random_id() else {
            self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteBroken),
                None,
                out,
                now,
            );
            return;
        };
        let next = query.route.get(at + 1).copied().unwrap_or(query.origin);
        let forward = Query {
            remaining_ms: child_ms,
            branch,
            ..query.clone()
        };
        match out.send(&next, &Message::Query(forward)) {
            Ok(()) => {
                self.routes.insert(
                    query.request_id.clone(),
                    RouteForward {
                        next,
                        expires: deadline,
                    },
                );
            }
            Err(SendError::NoCapability) => self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteNoCapability),
                None,
                out,
                now,
            ),
            Err(SendError::NoLink | SendError::Failed) => self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteBroken),
                None,
                out,
                now,
            ),
        }
    }

    /// Ends a query at a route hop: the origin as timed out with `reason`, then a final
    /// `done`.
    fn stop_route(
        &mut self,
        header: &ResultHeader,
        query: &Query,
        reason: Option<NodeReason>,
        own: Option<Node>,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let origin = Node {
            device_id: Some(query.origin),
            display_name: None,
            status: NodeStatus::TimedOut,
            reason,
            reported_by: Some(self.me.device_id),
            peer_hint: None,
            explored_at: now.unix_ms,
        };
        let nodes = own.into_iter().chain(std::iter::once(origin)).collect();
        let view = View {
            nodes,
            edges: Vec::new(),
        };
        if self.send_view(header, view, out, now) {
            self.send_done(header, Vec::new(), Vec::new(), true, out, now);
        }
    }

    /// Sends the parts of `view`. `false` when the answer could not be sent whole: a part
    /// failed (the later parts are not sent) or the header alone does not fit. The caller
    /// then sends no done for the branch.
    fn send_view(
        &mut self,
        header: &ResultHeader,
        view: View,
        out: &mut dyn Outbox,
        now: Now,
    ) -> bool {
        let parts = match split_result(
            header,
            view.nodes.clone(),
            view.edges.clone(),
            MAX_MESSAGE_JSON,
        ) {
            Ok(parts) => parts,
            Err(MessageError::EntryTooLarge) => {
                // Leave out the entries that cannot fit alone and say so: this device's
                // own node becomes timed out with reason `incomplete`.
                let fits_node = |n: &Node| {
                    split_result(header, vec![n.clone()], Vec::new(), MAX_MESSAGE_JSON).is_ok()
                };
                let fits_edge = |e: &Edge| {
                    split_result(header, Vec::new(), vec![e.clone()], MAX_MESSAGE_JSON).is_ok()
                };
                let me = self.me.device_id;
                let own = |n: &Node| n.device_id == Some(me) && n.reported_by.is_none();
                let marker = Node {
                    device_id: Some(me),
                    display_name: None,
                    status: NodeStatus::TimedOut,
                    reason: Some(NodeReason::Incomplete),
                    reported_by: None,
                    peer_hint: None,
                    explored_at: now.unix_ms,
                };
                let nodes: Vec<Node> = std::iter::once(marker)
                    .chain(view.nodes.into_iter().filter(|n| !own(n) && fits_node(n)))
                    .collect();
                let edges: Vec<Edge> = view.edges.into_iter().filter(fits_edge).collect();
                tracing::warn!("topology entries too large for one message left out");
                match split_result(header, nodes, edges, MAX_MESSAGE_JSON) {
                    Ok(parts) => parts,
                    Err(e) => {
                        tracing::warn!(error = %e, "topology answer not sent");
                        return false;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "topology answer not sent");
                return false;
            }
        };
        for part in parts {
            if !self.send_reply(Message::Result(part), out, now) {
                tracing::debug!("topology answer stopped at a part that failed");
                return false;
            }
        }
        true
    }

    fn send_done(
        &mut self,
        header: &ResultHeader,
        timed_out: Vec<DeviceId>,
        incomplete: Vec<DeviceId>,
        is_final: bool,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let header = ResultHeader {
            from: self.me.device_id,
            ..header.clone()
        };
        let dones = match split_done(&header, timed_out, incomplete, is_final, MAX_MESSAGE_JSON) {
            Ok(dones) => dones,
            Err(e) => {
                tracing::warn!(error = %e, "topology done not sent");
                return;
            }
        };
        for done in dones {
            if !self.send_reply(Message::Done(done), out, now) {
                return;
            }
        }
    }

    /// Sends a reply to the first entry of its return path, or handles it here when that
    /// entry is this device. `false` when sending failed.
    fn send_reply(&mut self, reply: Message, out: &mut dyn Outbox, now: Now) -> bool {
        let Some(next) = return_path(&reply).first().copied() else {
            return false;
        };
        if next == self.me.device_id {
            self.receive_reply(None, reply, out, now);
            true
        } else if let Err(e) = out.send(&next, &reply) {
            tracing::debug!(error = %e, "topology reply not sent");
            false
        } else {
            true
        }
    }

    /// A result, suppression or done addressed to this device: relay it to the next entry
    /// of its return path (or merge it into the exploration when this device is the
    /// last), then update the branches this device waits for. `link` is the peer of the
    /// link it came on (`None` for a reply this device made for itself); a reply from a
    /// link this device expects no reply from for that request is dropped.
    fn receive_reply(
        &mut self,
        link: Option<&DeviceId>,
        mut reply: Message,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let me = self.me.device_id;
        if return_path(&reply).first() != Some(&me) {
            tracing::debug!("topology reply not addressed to this device dropped");
            return;
        }
        if let Some(link) = link
            && !self.expects_reply_from(link, reply.request_id())
        {
            tracing::debug!("topology reply from a link not expected for its request dropped");
            return;
        }
        let (branch, from) = branch_and_from(&reply);
        // A reply of a branch this device forwarded must be from that branch's neighbor,
        // on that neighbor's link, for the same request.
        if let Some(parent) = self.children.get(&branch) {
            let request_id = reply.request_id();
            let matches = self.waits.get(parent).is_some_and(|wait| {
                wait.request_id == request_id
                    && wait.pending.get(&branch).is_some_and(|child| {
                        child.peer == from && link.is_none_or(|l| *l == child.peer)
                    })
            });
            if !matches {
                tracing::debug!("topology reply that does not match its branch dropped");
                return;
            }
        }
        // A final done for this device's own exploration counts only from the link the
        // end can come on: the first hop, or this device itself when it is the origin.
        if return_path(&reply).len() == 1
            && let Message::Done(done) = &reply
            && done.is_final
            && let Some(exploration) = self.explorations.get(&done.request_id)
            && link != exploration.first_hop.as_ref()
        {
            tracing::debug!("topology final done from a link that cannot end it dropped");
            return;
        }
        let relay_key = (reply.request_id().to_owned(), branch.clone(), from);
        if self.failed_relays.contains_key(&relay_key) {
            tracing::debug!("topology reply of a branch whose relay failed dropped");
            return;
        }
        return_path_mut(&mut reply).remove(0);
        let tracked = self.children.get(&branch).cloned();
        match return_path(&reply).first().copied() {
            None => self.deliver(reply.clone(), now),
            Some(next) if next == me => {
                tracing::debug!("topology reply that returns to this device twice dropped");
                return;
            }
            Some(next) => {
                if let Err(e) = out.send(&next, &reply) {
                    // Relay nothing more of this answer: the receiver sees the missing part
                    // (or the missing done) and marks the answer incomplete.
                    tracing::debug!(error = %e, "topology reply not relayed");
                    self.failed_relays
                        .insert(relay_key, now.mono + Duration::from_millis(MAX_QUERY_MS));
                    if let Some(link) = link {
                        self.note_failed_relay(link, &reply, &branch, from, out, now);
                    }
                    return;
                }
                if let Message::Done(done) = &reply
                    && done.is_final
                    && self
                        .routes
                        .get(&done.request_id)
                        .is_some_and(|r| link == Some(&r.next))
                {
                    self.routes.remove(&done.request_id);
                }
            }
        }
        let Some(parent) = tracked else {
            return;
        };
        let Some(wait) = self.waits.get_mut(&parent) else {
            return;
        };
        if wait.request_id != reply.request_id() {
            return;
        }
        let Some(child) = wait.pending.get_mut(&branch) else {
            return;
        };
        if child.peer != from || link.is_some_and(|l| *l != from) {
            return;
        }
        match reply {
            Message::Result(_) => child.answered = true,
            Message::Done(done) if done.more => child.answered = true,
            Message::Suppressed(_) | Message::Done(_) => {
                wait.pending.remove(&branch);
                self.children.remove(&branch);
                self.finish_wait_if_empty(&parent, out, now);
            }
            Message::Query(_) => {}
        }
    }

    /// A reply that came on `link` could not be relayed: every branch of this device that
    /// waits on `link` for that request reports `from` as incomplete in its done. When the
    /// reply was the neighbor's own answer to that branch, the branch stops waiting for it.
    fn note_failed_relay(
        &mut self,
        link: &DeviceId,
        reply: &Message,
        branch: &str,
        from: DeviceId,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let request_id = reply.request_id();
        let mut emptied = Vec::new();
        for (key, wait) in &mut self.waits {
            if wait.request_id != request_id || !wait.pending.values().any(|c| c.peer == *link) {
                continue;
            }
            if !wait.incomplete.contains(&from) {
                wait.incomplete.push(from);
            }
            if wait.pending.get(branch).is_some_and(|c| c.peer == from) {
                wait.pending.remove(branch);
                self.children.remove(branch);
                if wait.pending.is_empty() {
                    emptied.push(key.clone());
                }
            }
        }
        for key in emptied {
            self.finish_wait_if_empty(&key, out, now);
        }
    }

    /// Whether `link` is a peer this device waits on for `request_id`: a neighbor it
    /// forwarded the request to, the next hop it relayed it to on the route, or the first
    /// hop of its own exploration.
    fn expects_reply_from(&self, link: &DeviceId, request_id: &str) -> bool {
        self.waits.values().any(|wait| {
            wait.request_id == request_id && wait.pending.values().any(|c| c.peer == *link)
        }) || self.routes.get(request_id).is_some_and(|r| r.next == *link)
            || self
                .explorations
                .get(request_id)
                .is_some_and(|x| x.first_hop == Some(*link))
    }

    fn deliver(&mut self, reply: Message, now: Now) {
        let Some(exploration) = self.explorations.get_mut(reply.request_id()) else {
            return;
        };
        if exploration.state != ExplorationState::Running {
            return;
        }
        match reply {
            Message::Result(part) => exploration.merge_part(part, now.unix_ms),
            Message::Done(done) => {
                exploration.track_done(&done);
                for device in done.timed_out {
                    exploration.report_no_answer(device, &done.from, now.unix_ms);
                }
                for device in done.incomplete {
                    exploration.report_incomplete(device, &done.from, now.unix_ms);
                }
                if !done.more {
                    exploration.check_parts(&done.branch, &done.from, now.unix_ms);
                }
                // Only the origin or a route hop that stopped the query ends it.
                if done.is_final
                    && !done.more
                    && (done.from == exploration.origin || exploration.route.contains(&done.from))
                {
                    exploration.check_all_parts(now.unix_ms);
                    exploration.state = if exploration.has_incomplete() {
                        ExplorationState::Incomplete
                    } else {
                        ExplorationState::Finished
                    };
                }
            }
            Message::Suppressed(_) | Message::Query(_) => {}
        }
    }

    fn finish_wait_if_empty(&mut self, branch: &str, out: &mut dyn Outbox, now: Now) {
        if !self.waits.get(branch).is_some_and(|w| w.pending.is_empty()) {
            return;
        }
        let Some(wait) = self.waits.remove(branch) else {
            return;
        };
        let header = ResultHeader {
            request_id: wait.request_id,
            branch: wait.branch,
            from: self.me.device_id,
            return_path: wait.return_path,
        };
        self.send_done(
            &header,
            wait.timed_out,
            wait.incomplete,
            wait.is_final,
            out,
            now,
        );
    }

    /// Applies every deadline that passed by `now`: gives up neighbors that did not answer
    /// in [`AgentConfig::hop_timeout`], ends branches whose deadline passed, forgets
    /// expired answer records, and ends explorations past the requester's deadline.
    pub fn poll(&mut self, out: &mut dyn Outbox, now: Now) {
        // The requester's whole deadline first: a late poll must not let this device's own
        // branch finish an exploration whose deadline already passed.
        for exploration in self.explorations.values_mut() {
            if exploration.state == ExplorationState::Running && now.mono >= exploration.deadline {
                // A missing part found now (or a node already incomplete) says more than
                // the deadline: part of an answer is known to be lost.
                exploration.check_all_parts(now.unix_ms);
                exploration.state = if exploration.has_incomplete() {
                    ExplorationState::Incomplete
                } else {
                    ExplorationState::TimedOut
                };
                let origin = exploration.origin;
                if exploration.node(&origin).is_none() {
                    let me = exploration.me;
                    exploration.report_timed_out(origin, &me, now.unix_ms);
                }
            }
        }
        let mut ended = Vec::new();
        for (key, wait) in &mut self.waits {
            let past_deadline = now.mono >= wait.deadline;
            let mut given_up = Vec::new();
            for (branch, child) in &wait.pending {
                if past_deadline || (!child.answered && now.mono >= child.answer_by) {
                    given_up.push(branch.clone());
                }
            }
            for branch in given_up {
                if let Some(child) = wait.pending.remove(&branch) {
                    self.children.remove(&branch);
                    if !child.answered {
                        wait.timed_out.push(child.peer);
                    } else if !wait.incomplete.contains(&child.peer) {
                        // Its answer began but its last done never came.
                        wait.incomplete.push(child.peer);
                    }
                }
            }
            if wait.pending.is_empty() {
                ended.push(key.clone());
            }
        }
        for key in ended {
            self.finish_wait_if_empty(&key, out, now);
        }
        self.answered.retain(|_, seen| seen.expires > now.mono);
        self.routes.retain(|_, r| r.expires > now.mono);
        self.failed_relays.retain(|_, expires| *expires > now.mono);
    }

    /// The earliest deadline [`Agent::poll`] has to see, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let waits = self.waits.values().flat_map(|wait| {
            std::iter::once(wait.deadline).chain(
                wait.pending
                    .values()
                    .filter(|child| !child.answered)
                    .map(|child| child.answer_by),
            )
        });
        let explorations = self
            .explorations
            .values()
            .filter(|e| e.state == ExplorationState::Running)
            .map(|e| e.deadline);
        let routes = self.routes.values().map(|r| r.expires);
        let answered = self.answered.values().map(|seen| seen.expires);
        let failed = self.failed_relays.values().copied();
        waits
            .chain(explorations)
            .chain(routes)
            .chain(answered)
            .chain(failed)
            .min()
    }
}

/// Whether an exploring query (at the origin or past it) matches the link it came on:
/// the route rules hold; at the origin the sender is the last route hop (or the requester
/// with no route); past it the sender is the last device of `path`, `path` starts at the
/// origin, has no device twice and does not contain this device.
fn query_path_ok(me: &DeviceId, from: Option<&DeviceId>, query: &Query) -> bool {
    if validate_route(&query.requester, &query.origin, &query.route).is_err() {
        return false;
    }
    let Some(last) = query.path.last() else {
        return match from {
            None => query.requester == *me,
            Some(from) => *from == query.route.last().copied().unwrap_or(query.requester),
        };
    };
    let mut seen = HashSet::new();
    from == Some(last)
        && query.path.first() == Some(&query.origin)
        && !query.path.contains(me)
        && query.path.iter().all(|d| seen.insert(*d))
}

/// The distinct peers of this device's attach links, minus `skip`, in link order. Both
/// directions count.
fn neighbors(me: &DeviceId, links: &[LocalLink], skip: &[DeviceId]) -> Vec<DeviceId> {
    let mut seen = HashSet::new();
    links
        .iter()
        .filter(|link| link.kind == LinkKind::Attach)
        .map(|link| link.peer)
        .filter(|peer| peer != me && !skip.contains(peer) && seen.insert(*peer))
        .collect()
}

/// When a neighbor's first answer is due: `hop_timeout` from `now`, but never past the
/// branch `deadline`. The timeout is cut to the time left first, so no value of it can
/// overflow an `Instant`.
fn answer_by(now: Instant, hop_timeout: Duration, deadline: Instant) -> Instant {
    let left = deadline.saturating_duration_since(now);
    now.checked_add(hop_timeout.min(left)).unwrap_or(deadline)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn return_path(reply: &Message) -> &[DeviceId] {
    match reply {
        Message::Result(m) => &m.return_path,
        Message::Suppressed(m) => &m.return_path,
        Message::Done(m) => &m.return_path,
        Message::Query(_) => &[],
    }
}

fn return_path_mut(reply: &mut Message) -> &mut Vec<DeviceId> {
    match reply {
        Message::Result(m) => &mut m.return_path,
        Message::Suppressed(m) => &mut m.return_path,
        Message::Done(m) => &mut m.return_path,
        Message::Query(m) => &mut m.path,
    }
}

fn branch_and_from(reply: &Message) -> (String, DeviceId) {
    match reply {
        Message::Result(m) => (m.branch.clone(), m.from),
        Message::Suppressed(m) => (m.branch.clone(), m.from),
        Message::Done(m) => (m.branch.clone(), m.from),
        Message::Query(m) => (m.branch.clone(), m.requester),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Side, TransportKind};
    use tasty_net_identity::DeviceKey;

    fn id(n: u8) -> DeviceId {
        DeviceKey::from_seed([n; 32]).id()
    }

    struct Sink;

    impl Outbox for Sink {
        fn send(&mut self, _: &DeviceId, _: &Message) -> Result<(), SendError> {
            Ok(())
        }
    }

    fn query(request_id: &str) -> Query {
        Query {
            request_id: request_id.into(),
            requester: id(1),
            origin: id(1),
            route: Vec::new(),
            path: vec![id(1)],
            depth: 0,
            remaining_ms: 1_000,
            branch: "b".into(),
        }
    }

    #[test]
    fn the_largest_hop_timeout_is_cut_to_the_branch_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(3);
        assert_eq!(answer_by(now, Duration::MAX, deadline), deadline);
        assert_eq!(
            answer_by(now, Duration::from_secs(1), deadline),
            now + Duration::from_secs(1)
        );
        assert_eq!(answer_by(deadline, Duration::MAX, deadline), deadline);
    }

    #[test]
    fn answer_records_of_a_device_that_does_not_forward_expire_on_its_timer() {
        let me = SelfInfo {
            device_id: id(2),
            display_name: None,
        };
        let mut agent = Agent::new(me, AgentConfig::default());
        let links = [LocalLink {
            peer: id(1),
            local_side: Side::Acceptor,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: None,
            connected_at: 0,
            workspace: None,
            kind: LinkKind::Attach,
        }];
        let local = Local {
            links: &links,
            failures: &[],
            sharing: Sharing::default(),
        };
        let start = Instant::now();
        let at = |ms: u64| Now {
            mono: start + Duration::from_millis(ms),
            unix_ms: ms,
        };
        // Depth 0: nothing to wait for, only the answer record.
        agent.on_message(&id(1), Message::Query(query("r1")), local, &mut Sink, at(0));
        assert!(agent.waits.is_empty());
        assert_eq!(agent.answered.len(), 1);
        let due = agent.next_deadline().expect("the record has a timer");
        assert_eq!(due, start + Duration::from_millis(1_000));

        agent.poll(&mut Sink, at(1_000));
        assert_eq!(agent.answered.len(), 0);
        assert_eq!(agent.next_deadline(), None);

        // A new query past an old record's expiry clears it too, without a poll.
        agent.answer_query(Some(&id(1)), query("r2"), local, &mut Sink, at(2_000));
        agent.answer_query(Some(&id(1)), query("r3"), local, &mut Sink, at(3_500));
        assert_eq!(agent.answered.len(), 1);
        assert!(agent.answered.contains_key("r3"));
    }

    #[test]
    fn the_largest_part_number_does_not_overflow_the_count() {
        let mut track = PartTrack::default();
        track.seen.insert(u32::MAX);
        track.last = Some(u32::MAX);
        assert!(!track.complete());
        let whole = PartTrack {
            seen: [0].into_iter().collect(),
            last: Some(0),
        };
        assert!(whole.complete());
    }

    /// Fails its first send, then records what it sends.
    struct FailOnce {
        failed: bool,
        sent: Vec<Message>,
    }

    impl Outbox for FailOnce {
        fn send(&mut self, _: &DeviceId, message: &Message) -> Result<(), SendError> {
            if !self.failed {
                self.failed = true;
                return Err(SendError::Failed);
            }
            self.sent.push(message.clone());
            Ok(())
        }
    }

    #[test]
    fn an_answer_that_could_not_be_sent_is_sent_again_for_the_same_query() {
        let me = SelfInfo {
            device_id: id(2),
            display_name: None,
        };
        let mut agent = Agent::new(me, AgentConfig::default());
        let links = [LocalLink {
            peer: id(1),
            local_side: Side::Acceptor,
            holder: Side::Dialer,
            transport: TransportKind::SshForward,
            rtt: None,
            connected_at: 0,
            workspace: None,
            kind: LinkKind::Attach,
        }];
        let local = Local {
            links: &links,
            failures: &[],
            sharing: Sharing::default(),
        };
        let now = Now {
            mono: Instant::now(),
            unix_ms: 0,
        };
        let mut out = FailOnce {
            failed: false,
            sent: Vec::new(),
        };
        agent.on_message(&id(1), Message::Query(query("r1")), local, &mut out, now);
        assert!(out.sent.is_empty());
        assert!(agent.answered.is_empty());

        agent.on_message(&id(1), Message::Query(query("r1")), local, &mut out, now);
        assert!(matches!(out.sent.first(), Some(Message::Result(p)) if p.part == 0));
        assert!(matches!(out.sent.last(), Some(Message::Done(_))));
        assert!(!out.sent.iter().any(|m| matches!(m, Message::Suppressed(_))));
    }
}
