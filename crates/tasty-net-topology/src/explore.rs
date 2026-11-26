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
    Suppressed, split_result, validate_route,
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
    /// The final `done` arrived.
    Finished,
    /// The requester's deadline passed first.
    TimedOut,
}

/// The merged, progressive result of one exploration.
#[derive(Debug, Clone)]
pub struct Exploration {
    request_id: String,
    origin: DeviceId,
    me: DeviceId,
    deadline: Instant,
    state: ExplorationState,
    nodes: Vec<Node>,
    node_index: HashMap<NodeKey, (usize, bool)>,
    edges: Vec<Edge>,
    edge_keys: HashSet<EdgeKey>,
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

    fn merge_part(&mut self, part: ResultPart, unix_ms: u64) {
        for node in part.nodes {
            self.merge_node(node, &part.from, unix_ms);
        }
        for edge in part.edges {
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
}

/// The topology state machine of one device.
pub struct Agent {
    me: SelfInfo,
    config: AgentConfig,
    answered: HashMap<String, Answered>,
    waits: HashMap<String, Wait>,
    /// Child branch → the branch of ours that forwarded it.
    children: HashMap<String, String>,
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
            explorations: HashMap::new(),
        }
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.me.device_id
    }

    /// Starts an exploration from this device and returns its request ID. The route is
    /// checked first ([`TopologyError::InvalidRoute`]); a first hop that cannot be sent to
    /// is [`TopologyError::FirstHop`]. With this device as the origin, its own part is the
    /// local view.
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
        self.explorations.insert(
            request_id.clone(),
            Exploration {
                request_id: request_id.clone(),
                origin: request.origin,
                me,
                deadline: now.mono + timeout,
                state: ExplorationState::Running,
                nodes: Vec::new(),
                node_index: HashMap::new(),
                edges: Vec::new(),
                edge_keys: HashSet::new(),
            },
        );
        if request.origin == me {
            self.answer_query(None, query, local, out, now);
        } else {
            let first = request.route.first().copied().unwrap_or(request.origin);
            if let Err(e) = out.send(&first, &Message::Query(query)) {
                self.explorations.remove(&request_id);
                return Err(TopologyError::FirstHop(e));
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
    /// `from`.
    pub fn on_message(
        &mut self,
        from: &DeviceId,
        message: Message,
        local: Local<'_>,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        match message {
            Message::Query(query) => self.answer_query(Some(from), query, local, out, now),
            reply => self.receive_reply(reply, out, now),
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
        let deadline = now.mono + Duration::from_millis(query.remaining_ms.min(MAX_QUERY_MS));
        if query.path.is_empty() && query.origin != me {
            self.relay_route(from, query, local, out, now, deadline);
            return;
        }
        let is_origin = query.path.is_empty();
        let return_path: Vec<DeviceId> = query
            .path
            .iter()
            .rev()
            .chain(query.route.iter().rev())
            .copied()
            .chain(std::iter::once(query.requester))
            .collect();

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
        self.answered.insert(
            query.request_id.clone(),
            Answered {
                depth: query.depth,
                expires,
            },
        );

        let is_local = query.requester == me;
        let header = ResultHeader {
            request_id: query.request_id.clone(),
            branch: query.branch.clone(),
            from: me,
            return_path: return_path.clone(),
        };
        if !is_local && !local.sharing.enabled {
            let view = remote_answer(
                &self.me,
                local.links,
                local.failures,
                local.sharing,
                now.unix_ms,
            );
            self.send_view(&header, view, out, now);
            self.send_done(&header, Vec::new(), is_origin, out, now);
            return;
        }
        let mut view = if is_local {
            local_view(&self.me, local.links, local.failures, now.unix_ms)
        } else {
            remote_answer(
                &self.me,
                local.links,
                local.failures,
                local.sharing,
                now.unix_ms,
            )
        };

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
                                answer_by: (now.mono + self.config.hop_timeout).min(deadline),
                                answered: false,
                            },
                        );
                    }
                    Err(SendError::NoCapability) => view.nodes.push(Node {
                        device_id: Some(peer),
                        display_name: None,
                        status: NodeStatus::NoCapability,
                        reason: None,
                        reported_by: Some(me),
                        peer_hint: None,
                        explored_at: now.unix_ms,
                    }),
                    Err(SendError::NoLink | SendError::Failed) => timed_out.push(peer),
                }
            }
        }
        self.send_view(&header, view, out, now);
        if pending.is_empty() {
            self.send_done(&header, timed_out, is_origin, out, now);
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
            },
        );
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
        let valid = validate_route(&query.requester, &query.origin, &query.route).is_ok()
            && match at {
                Some(0) => from == Some(&query.requester),
                Some(_) => true,
                None => false,
            };
        let Some(at) = at.filter(|_| valid) else {
            self.stop_route(
                &header,
                &query,
                Some(NodeReason::RouteInvalid),
                None,
                out,
                now,
            );
            return;
        };
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
            Ok(()) => {}
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
        self.send_view(
            header,
            View {
                nodes,
                edges: Vec::new(),
            },
            out,
            now,
        );
        self.send_done(header, Vec::new(), true, out, now);
    }

    fn send_view(&mut self, header: &ResultHeader, view: View, out: &mut dyn Outbox, now: Now) {
        let parts = match split_result(
            header,
            view.nodes.clone(),
            view.edges.clone(),
            MAX_MESSAGE_JSON,
        ) {
            Ok(parts) => parts,
            Err(MessageError::EntryTooLarge) => {
                // Drop the entries that cannot fit alone; keep the rest of the answer.
                let fits_node = |n: &Node| {
                    split_result(header, vec![n.clone()], Vec::new(), MAX_MESSAGE_JSON).is_ok()
                };
                let fits_edge = |e: &Edge| {
                    split_result(header, Vec::new(), vec![e.clone()], MAX_MESSAGE_JSON).is_ok()
                };
                let nodes: Vec<Node> = view.nodes.into_iter().filter(fits_node).collect();
                let edges: Vec<Edge> = view.edges.into_iter().filter(fits_edge).collect();
                tracing::warn!("dropped topology entries too large for one message");
                match split_result(header, nodes, edges, MAX_MESSAGE_JSON) {
                    Ok(parts) => parts,
                    Err(e) => {
                        tracing::warn!(error = %e, "topology answer not sent");
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "topology answer not sent");
                return;
            }
        };
        for part in parts {
            self.send_reply(Message::Result(part), out, now);
        }
    }

    fn send_done(
        &mut self,
        header: &ResultHeader,
        timed_out: Vec<DeviceId>,
        is_final: bool,
        out: &mut dyn Outbox,
        now: Now,
    ) {
        let done = Message::Done(Done {
            request_id: header.request_id.clone(),
            branch: header.branch.clone(),
            from: self.me.device_id,
            return_path: header.return_path.clone(),
            timed_out,
            is_final,
        });
        self.send_reply(done, out, now);
    }

    /// Sends a reply to the first entry of its return path, or handles it here when that
    /// entry is this device.
    fn send_reply(&mut self, reply: Message, out: &mut dyn Outbox, now: Now) {
        let Some(next) = return_path(&reply).first().copied() else {
            return;
        };
        if next == self.me.device_id {
            self.receive_reply(reply, out, now);
        } else if let Err(e) = out.send(&next, &reply) {
            tracing::debug!(error = %e, "topology reply not relayed");
        }
    }

    /// A result, suppression or done addressed to this device: relay it to the next entry
    /// of its return path (or merge it into the exploration when this device is the
    /// last), then update the branches this device waits for.
    fn receive_reply(&mut self, mut reply: Message, out: &mut dyn Outbox, now: Now) {
        let me = self.me.device_id;
        let path = return_path_mut(&mut reply);
        if path.first() != Some(&me) {
            tracing::debug!("topology reply not addressed to this device dropped");
            return;
        }
        path.remove(0);
        let (branch, from) = branch_and_from(&reply);
        let tracked = self.children.get(&branch).cloned();
        match return_path(&reply).first().copied() {
            None => self.deliver(reply.clone(), now),
            Some(next) if next == me => {
                tracing::debug!("topology reply that returns to this device twice dropped");
                return;
            }
            Some(next) => {
                if let Err(e) = out.send(&next, &reply) {
                    tracing::debug!(error = %e, "topology reply not relayed");
                }
            }
        }
        let Some(parent) = tracked else {
            return;
        };
        let Some(wait) = self.waits.get_mut(&parent) else {
            return;
        };
        let Some(child) = wait.pending.get_mut(&branch) else {
            return;
        };
        if child.peer != from {
            return;
        }
        match reply {
            Message::Result(_) => child.answered = true,
            Message::Suppressed(_) | Message::Done(_) => {
                wait.pending.remove(&branch);
                self.children.remove(&branch);
                self.finish_wait_if_empty(&parent, out, now);
            }
            Message::Query(_) => {}
        }
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
                for device in done.timed_out {
                    exploration.report_timed_out(device, &done.from, now.unix_ms);
                }
                if done.is_final {
                    exploration.state = ExplorationState::Finished;
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
        self.send_done(&header, wait.timed_out, wait.is_final, out, now);
    }

    /// Applies every deadline that passed by `now`: gives up neighbors that did not answer
    /// in [`AgentConfig::hop_timeout`], ends branches whose deadline passed, forgets
    /// expired answer records, and ends explorations past the requester's deadline.
    pub fn poll(&mut self, out: &mut dyn Outbox, now: Now) {
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
        for exploration in self.explorations.values_mut() {
            if exploration.state == ExplorationState::Running && now.mono >= exploration.deadline {
                exploration.state = ExplorationState::TimedOut;
                let origin = exploration.origin;
                if exploration.node(&origin).is_none() {
                    let me = exploration.me;
                    exploration.report_timed_out(origin, &me, now.unix_ms);
                }
            }
        }
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
        waits.chain(explorations).min()
    }
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
