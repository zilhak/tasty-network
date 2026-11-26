//! Query and result messages (docs/design/topology.md "질의와 결과 메시지").
//!
//! Every message is one JSON object with an `event` tag, carried on the ordered channel of
//! an authenticated attach link in the raw JSON `event` extension slot of tasty.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use tasty_net_identity::DeviceId;

use crate::ids;
use crate::model::{Edge, Node};

/// The capability name a device announces in the handshake `caps` when it answers
/// topology queries.
pub const CAPABILITY: &str = "net.topology";

/// `event` value of [`Query`].
pub const EVENT_QUERY: &str = "net.topology.query";
/// `event` value of [`ResultPart`].
pub const EVENT_RESULT: &str = "net.topology.result";
/// `event` value of [`Suppressed`].
pub const EVENT_SUPPRESSED: &str = "net.topology.suppressed";
/// `event` value of [`Done`].
pub const EVENT_DONE: &str = "net.topology.done";

/// Upper bound of the `route` length.
pub const MAX_ROUTE_HOPS: usize = 16;

/// Upper bound of `remaining_ms`: no device waits longer than this for one query.
pub const MAX_QUERY_MS: u64 = 30_000;

/// Upper bound of the JSON of one message: half of `MAX_FRAME_LEN`, the rule of tasty's
/// remote read results.
pub const MAX_MESSAGE_JSON: usize = tasty_net_frame::MAX_FRAME_LEN as usize / 2;

/// A topology query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub request_id: String,
    /// The device that first sent the query.
    #[serde(with = "ids::device_id")]
    pub requester: DeviceId,
    /// The device the exploration starts from.
    #[serde(with = "ids::device_id")]
    pub origin: DeviceId,
    /// The devices between the requester and the origin, in order. Empty when the
    /// requester is the origin or links to it directly.
    #[serde(with = "ids::device_id_list")]
    pub route: Vec<DeviceId>,
    /// The devices from the origin up to the sender of this query, in order. Empty until
    /// the origin forwards.
    #[serde(with = "ids::device_id_list")]
    pub path: Vec<DeviceId>,
    /// The remaining depth.
    pub depth: u32,
    /// The time left when the query was sent, at most [`MAX_QUERY_MS`].
    pub remaining_ms: u64,
    /// The ID of this one forward (128-bit random hex).
    pub branch: String,
}

/// One part of the answer of `from` to the query of `branch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultPart {
    pub request_id: String,
    pub branch: String,
    #[serde(with = "ids::device_id")]
    pub from: DeviceId,
    /// Where the result still has to go; the first entry is the next receiver.
    #[serde(with = "ids::device_id_list")]
    pub return_path: Vec<DeviceId>,
    pub part: u32,
    /// The answer of `from` to this branch is complete.
    pub last: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// `from` already answered the same request with an equal or larger remaining depth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suppressed {
    pub request_id: String,
    pub branch: String,
    #[serde(with = "ids::device_id")]
    pub from: DeviceId,
    #[serde(with = "ids::device_id_list")]
    pub return_path: Vec<DeviceId>,
}

/// `from` finished `branch`: it sent its own parts and every neighbor it forwarded to
/// finished or was given up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub request_id: String,
    pub branch: String,
    #[serde(with = "ids::device_id")]
    pub from: DeviceId,
    #[serde(with = "ids::device_id_list")]
    pub return_path: Vec<DeviceId>,
    /// Neighbors `from` gave up waiting for in this branch.
    #[serde(with = "ids::device_id_list")]
    pub timed_out: Vec<DeviceId>,
    /// This ends the whole query (the origin, or a route hop that stopped the query).
    #[serde(rename = "final")]
    pub is_final: bool,
    /// More `done` messages of the same `branch` and `from` follow with the rest of
    /// `timed_out` (a list too long for one message, [`split_done`]). Only the one without
    /// `more` finishes the branch. Absent when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub more: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Any topology message, tagged by `event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Message {
    #[serde(rename = "net.topology.query")]
    Query(Query),
    #[serde(rename = "net.topology.result")]
    Result(ResultPart),
    #[serde(rename = "net.topology.suppressed")]
    Suppressed(Suppressed),
    #[serde(rename = "net.topology.done")]
    Done(Done),
}

impl Message {
    /// Whether `event` is one of the topology events, so a consumer can route a raw JSON
    /// event to [`Message::from_json`] before parsing it.
    pub fn is_topology_event(event: &str) -> bool {
        matches!(
            event,
            EVENT_QUERY | EVENT_RESULT | EVENT_SUPPRESSED | EVENT_DONE
        )
    }

    /// Reads one message. Longer than [`MAX_MESSAGE_JSON`] is [`MessageError::TooLarge`]
    /// before parsing.
    pub fn from_json(bytes: &[u8]) -> Result<Self, MessageError> {
        if bytes.len() > MAX_MESSAGE_JSON {
            return Err(MessageError::TooLarge);
        }
        serde_json::from_slice(bytes).map_err(|_| MessageError::Malformed)
    }

    /// The JSON of this message. [`MessageError::TooLarge`] when it is longer than
    /// [`MAX_MESSAGE_JSON`].
    pub fn to_json(&self) -> Result<Vec<u8>, MessageError> {
        let bytes = serde_json::to_vec(self).map_err(|_| MessageError::Malformed)?;
        if bytes.len() > MAX_MESSAGE_JSON {
            return Err(MessageError::TooLarge);
        }
        Ok(bytes)
    }

    /// The request this message belongs to.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Query(m) => &m.request_id,
            Self::Result(m) => &m.request_id,
            Self::Suppressed(m) => &m.request_id,
            Self::Done(m) => &m.request_id,
        }
    }
}

/// Why a message could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageError {
    /// The bytes are not a topology message.
    Malformed,
    /// The JSON is longer than [`MAX_MESSAGE_JSON`].
    TooLarge,
    /// One node or edge alone does not fit in a message.
    EntryTooLarge,
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the bytes are not a topology message",
            Self::TooLarge => "the topology message is longer than its frame limit",
            Self::EntryTooLarge => "a topology node or edge does not fit in one message",
        })
    }
}

impl std::error::Error for MessageError {}

/// The fields of a result that every part repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultHeader {
    pub request_id: String,
    pub branch: String,
    pub from: DeviceId,
    pub return_path: Vec<DeviceId>,
}

/// Splits an answer into parts whose JSON (as [`Message::Result`]) is at most `limit`
/// bytes. Parts are numbered from 0 and only the final one has `last: true`. An empty
/// answer is one empty part. Concatenating the `nodes` and `edges` of the parts in order
/// gives the input back. [`MessageError::TooLarge`] when the header alone does not fit,
/// [`MessageError::EntryTooLarge`] when one entry does not fit with it.
pub fn split_result(
    header: &ResultHeader,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    limit: usize,
) -> Result<Vec<ResultPart>, MessageError> {
    let empty = |part: u32, last: bool| ResultPart {
        request_id: header.request_id.clone(),
        branch: header.branch.clone(),
        from: header.from,
        return_path: header.return_path.clone(),
        part,
        last,
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    // The whole answer as the one part it would be (`part: 0, last: true`), measured as
    // sent: this is exact, so an answer that fits is never refused.
    let mut whole = empty(0, true);
    let tag = json_len(&Message::Result(whole.clone()))? - json_len(&whole)?;
    whole.nodes = nodes;
    whole.edges = edges;
    if json_len(&whole)? + tag <= limit {
        return Ok(vec![whole]);
    }
    let ResultPart { nodes, edges, .. } = whole;
    // Several parts: estimate with the longest empty part, the largest part number and
    // `false` (longer than `true`).
    let base = json_len(&Message::Result(empty(u32::MAX, false)))?;
    if base > limit {
        return Err(MessageError::TooLarge);
    }
    let mut parts = Vec::new();
    let mut current = empty(0, false);
    let mut size = base;
    // Each entry adds its JSON and at most one separating comma.
    for node in nodes {
        let len = json_len(&node)? + 1;
        if base + len > limit {
            return Err(MessageError::EntryTooLarge);
        }
        if size + len > limit {
            let next = empty(next_part(&current)?, false);
            parts.push(std::mem::replace(&mut current, next));
            size = base;
        }
        size += len;
        current.nodes.push(node);
    }
    for edge in edges {
        let len = json_len(&edge)? + 1;
        if base + len > limit {
            return Err(MessageError::EntryTooLarge);
        }
        if size + len > limit {
            let next = empty(next_part(&current)?, false);
            parts.push(std::mem::replace(&mut current, next));
            size = base;
        }
        size += len;
        current.edges.push(edge);
    }
    current.last = true;
    parts.push(current);
    Ok(parts)
}

/// Splits a branch's `done` so that each message's JSON is at most `limit` bytes: the
/// `timed_out` list is spread over several messages, all but the last with `more: true`
/// and `final: false`; the last carries `is_final`. Concatenating the `timed_out` lists in
/// order gives the input back. [`MessageError::TooLarge`] when the header alone does not
/// fit.
pub fn split_done(
    header: &ResultHeader,
    timed_out: Vec<DeviceId>,
    is_final: bool,
    limit: usize,
) -> Result<Vec<Done>, MessageError> {
    let empty = || Done {
        request_id: header.request_id.clone(),
        branch: header.branch.clone(),
        from: header.from,
        return_path: header.return_path.clone(),
        timed_out: Vec::new(),
        is_final: false,
        more: true,
    };
    // The whole list as the one done it would be (`more` absent), measured as sent.
    let mut whole = Done {
        is_final,
        more: false,
        ..empty()
    };
    let tag = json_len(&Message::Done(whole.clone()))? - json_len(&whole)?;
    whole.timed_out = timed_out;
    if json_len(&whole)? + tag <= limit {
        return Ok(vec![whole]);
    }
    let Done { timed_out, .. } = whole;
    // Several dones: estimate with the longest empty one, `more` present and `false`.
    let base = json_len(&Message::Done(empty()))?;
    if base > limit {
        return Err(MessageError::TooLarge);
    }
    let mut dones = Vec::new();
    let mut current = empty();
    let mut size = base;
    for device in timed_out {
        // The quoted ID and at most one comma.
        let len = device.to_string().len() + 3;
        if base + len > limit {
            return Err(MessageError::TooLarge);
        }
        if size + len > limit {
            dones.push(std::mem::replace(&mut current, empty()));
            size = base;
        }
        size += len;
        current.timed_out.push(device);
    }
    current.more = false;
    current.is_final = is_final;
    dones.push(current);
    Ok(dones)
}

fn next_part(current: &ResultPart) -> Result<u32, MessageError> {
    current.part.checked_add(1).ok_or(MessageError::TooLarge)
}

fn json_len<T: Serialize>(value: &T) -> Result<usize, MessageError> {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .map_err(|_| MessageError::Malformed)
}

/// Why a route was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteError {
    /// Longer than [`MAX_ROUTE_HOPS`].
    TooLong,
    /// A device appears twice.
    Repeated,
    /// The route contains the requester or the origin.
    ContainsEnd,
    /// The requester is the origin but the route is not empty.
    NotEmptyForSelf,
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "the route is longer than MAX_ROUTE_HOPS",
            Self::Repeated => "a device appears twice in the route",
            Self::ContainsEnd => "the route contains the requester or the origin",
            Self::NotEmptyForSelf => "the route is not empty although the requester is the origin",
        })
    }
}

impl std::error::Error for RouteError {}

/// Checks the route rules: at most [`MAX_ROUTE_HOPS`] hops, no device twice, neither the
/// requester nor the origin in it, and empty when the requester is the origin.
pub fn validate_route(
    requester: &DeviceId,
    origin: &DeviceId,
    route: &[DeviceId],
) -> Result<(), RouteError> {
    if route.len() > MAX_ROUTE_HOPS {
        return Err(RouteError::TooLong);
    }
    if requester == origin && !route.is_empty() {
        return Err(RouteError::NotEmptyForSelf);
    }
    let mut seen = HashSet::new();
    for hop in route {
        if hop == requester || hop == origin {
            return Err(RouteError::ContainsEnd);
        }
        if !seen.insert(hop) {
            return Err(RouteError::Repeated);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeStatus, Side, TransportKind};
    use tasty_net_identity::DeviceKey;

    fn id(n: u32) -> DeviceId {
        let mut seed = [0u8; 32];
        seed[..4].copy_from_slice(&n.to_be_bytes());
        DeviceKey::from_seed(seed).id()
    }

    fn header() -> ResultHeader {
        ResultHeader {
            request_id: "0123456789abcdef0123456789abcdef".into(),
            branch: "fedcba9876543210fedcba9876543210".into(),
            from: id(1),
            return_path: (2..=17).map(id).collect(),
        }
    }

    fn node(n: u32) -> Node {
        Node {
            device_id: Some(id(n)),
            display_name: Some(format!("device number {n}")),
            status: NodeStatus::Online,
            reason: None,
            reported_by: None,
            peer_hint: None,
            explored_at: 1_700_000_000_000,
        }
    }

    fn edge(n: u32) -> Edge {
        Edge {
            dialer: id(n),
            acceptor: id(n + 1),
            holder: Side::Dialer,
            transport: TransportKind::P2pDirect,
            rtt_ms: Some(3),
            connected_at: 1_700_000_000_000,
            workspace: None,
        }
    }

    #[test]
    fn a_large_result_splits_into_parts_within_half_a_frame() {
        let nodes: Vec<Node> = (0..4_000).map(node).collect();
        let edges: Vec<Edge> = (0..4_000).map(edge).collect();
        let parts =
            split_result(&header(), nodes.clone(), edges.clone(), MAX_MESSAGE_JSON).unwrap();
        assert!(parts.len() > 1, "{} parts", parts.len());
        for (i, part) in parts.iter().enumerate() {
            assert_eq!(part.part, u32::try_from(i).unwrap());
            assert_eq!(part.last, i + 1 == parts.len());
            let json = Message::Result(part.clone()).to_json().unwrap();
            assert!(json.len() <= MAX_FRAME_HALF, "{} bytes", json.len());
        }
        let joined_nodes: Vec<Node> = parts.iter().flat_map(|p| p.nodes.clone()).collect();
        let joined_edges: Vec<Edge> = parts.iter().flat_map(|p| p.edges.clone()).collect();
        assert_eq!(joined_nodes, nodes);
        assert_eq!(joined_edges, edges);
    }

    const MAX_FRAME_HALF: usize = tasty_net_frame::MAX_FRAME_LEN as usize / 2;

    #[test]
    fn parts_fill_up_to_the_limit() {
        // A small limit: every part stays under it, and the next entry would bring it within
        // the estimate slack (the widest part number, `false`, one comma per entry) of it.
        let limit = 2_000;
        let nodes: Vec<Node> = (0..50).map(node).collect();
        let parts = split_result(&header(), nodes, Vec::new(), limit).unwrap();
        for pair in parts.windows(2) {
            let json = Message::Result(pair[0].clone()).to_json().unwrap();
            assert!(json.len() <= limit);
            let mut grown = pair[0].clone();
            grown.nodes.push(pair[1].nodes[0].clone());
            assert!(serde_json::to_vec(&Message::Result(grown)).unwrap().len() > limit - 16);
        }
    }

    #[test]
    fn an_empty_answer_is_one_last_part() {
        let parts = split_result(&header(), Vec::new(), Vec::new(), MAX_MESSAGE_JSON).unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].last);
    }

    #[test]
    fn an_entry_larger_than_a_part_is_refused() {
        let mut big = node(1);
        big.display_name = Some("x".repeat(3_000));
        assert_eq!(
            split_result(&header(), vec![big], Vec::new(), 2_000),
            Err(MessageError::EntryTooLarge)
        );
    }

    #[test]
    fn messages_carry_their_event_tag_and_final_field() {
        let done = Message::Done(Done {
            request_id: "r".into(),
            branch: "b".into(),
            from: id(1),
            return_path: vec![id(2)],
            timed_out: vec![id(3)],
            is_final: true,
            more: false,
        });
        let value: serde_json::Value = serde_json::from_slice(&done.to_json().unwrap()).unwrap();
        assert_eq!(value["event"], EVENT_DONE);
        assert_eq!(value["final"], true);
        assert_eq!(value["timed_out"][0], id(3).to_string());
        assert!(value.get("more").is_none());

        let query = Message::Query(Query {
            request_id: "r".into(),
            requester: id(1),
            origin: id(2),
            route: Vec::new(),
            path: vec![id(2)],
            depth: 3,
            remaining_ms: 100,
            branch: "b".into(),
        });
        let json = query.to_json().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["event"], EVENT_QUERY);
        assert_eq!(Message::from_json(&json).unwrap(), query);
        for event in [EVENT_QUERY, EVENT_RESULT, EVENT_SUPPRESSED, EVENT_DONE] {
            assert!(Message::is_topology_event(event));
        }
        assert!(!Message::is_topology_event("net.topology.v2"));
        assert_eq!(
            Message::from_json(br#"{"event":"net.topology.other"}"#),
            Err(MessageError::Malformed)
        );
    }

    #[test]
    fn the_route_rules() {
        let (r, o) = (id(1), id(2));
        assert_eq!(validate_route(&r, &o, &[]), Ok(()));
        assert_eq!(validate_route(&r, &o, &[id(3), id(4)]), Ok(()));
        assert_eq!(
            validate_route(&r, &o, &[id(3), r]),
            Err(RouteError::ContainsEnd)
        );
        assert_eq!(validate_route(&r, &o, &[o]), Err(RouteError::ContainsEnd));
        assert_eq!(
            validate_route(&r, &r, &[id(3)]),
            Err(RouteError::NotEmptyForSelf)
        );
        let limit: Vec<DeviceId> = (10..10 + MAX_ROUTE_HOPS as u32).map(id).collect();
        assert_eq!(validate_route(&r, &o, &limit), Ok(()));
        let over: Vec<DeviceId> = (10..11 + MAX_ROUTE_HOPS as u32).map(id).collect();
        assert_eq!(validate_route(&r, &o, &over), Err(RouteError::TooLong));
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    fn id(n: u32) -> DeviceId {
        let mut seed = [7u8; 32];
        seed[..4].copy_from_slice(&n.to_be_bytes());
        DeviceKey::from_seed(seed).id()
    }

    fn header(hops: u32) -> ResultHeader {
        ResultHeader {
            request_id: "r".into(),
            branch: "b".into(),
            from: id(0),
            return_path: (1..=hops).map(id).collect(),
        }
    }

    #[test]
    fn reading_refuses_more_than_half_a_frame() {
        let done = Message::Done(Done {
            request_id: "r".into(),
            branch: "b".into(),
            from: id(0),
            return_path: vec![id(1)],
            timed_out: Vec::new(),
            is_final: false,
            more: false,
        });
        let mut json = done.to_json().unwrap();
        assert!(Message::from_json(&json).is_ok());
        // Valid JSON padded with spaces past the limit.
        json.resize(MAX_MESSAGE_JSON + 1, b' ');
        assert_eq!(Message::from_json(&json), Err(MessageError::TooLarge));
    }

    #[test]
    fn a_header_too_large_alone_is_refused() {
        // About 67 bytes per return path entry: 8,000 entries are past half a frame.
        let big = header(8_000);
        assert_eq!(
            split_result(&big, Vec::new(), Vec::new(), MAX_MESSAGE_JSON),
            Err(MessageError::TooLarge)
        );
        assert_eq!(
            split_done(&big, Vec::new(), true, MAX_MESSAGE_JSON),
            Err(MessageError::TooLarge)
        );
    }

    #[test]
    fn a_long_timed_out_list_is_spread_over_several_dones() {
        let ids: Vec<DeviceId> = (1..=9_000).map(id).collect();
        let dones = split_done(&header(3), ids.clone(), true, MAX_MESSAGE_JSON).unwrap();
        assert!(dones.len() > 1, "{} dones", dones.len());
        for (i, done) in dones.iter().enumerate() {
            let is_last = i + 1 == dones.len();
            assert_eq!(done.more, !is_last);
            assert_eq!(done.is_final, is_last);
            let json = Message::Done(done.clone()).to_json().unwrap();
            assert!(json.len() <= MAX_MESSAGE_JSON);
        }
        let joined: Vec<DeviceId> = dones.iter().flat_map(|d| d.timed_out.clone()).collect();
        assert_eq!(joined, ids);
    }
}

#[cfg(test)]
mod exact_fit_tests {
    use super::*;
    use crate::model::NodeStatus;
    use tasty_net_identity::DeviceKey;

    fn id(n: u8) -> DeviceId {
        DeviceKey::from_seed([n; 32]).id()
    }

    fn header() -> ResultHeader {
        ResultHeader {
            request_id: "r".into(),
            branch: "b".into(),
            from: id(1),
            return_path: vec![id(2), id(3)],
        }
    }

    #[test]
    fn an_answer_that_fits_exactly_is_one_part() {
        for nodes in [
            Vec::new(),
            vec![Node {
                device_id: Some(id(4)),
                display_name: Some("x".into()),
                status: NodeStatus::Online,
                reason: None,
                reported_by: None,
                peer_hint: None,
                explored_at: 1,
            }],
        ] {
            let parts = split_result(&header(), nodes.clone(), Vec::new(), usize::MAX).unwrap();
            let exact = Message::Result(parts[0].clone()).to_json().unwrap().len();
            let parts = split_result(&header(), nodes.clone(), Vec::new(), exact).unwrap();
            assert_eq!(parts.len(), 1);
            assert!(parts[0].last);
            assert_eq!(parts[0].part, 0);
            if nodes.is_empty() {
                assert_eq!(
                    split_result(&header(), nodes, Vec::new(), exact - 1),
                    Err(MessageError::TooLarge)
                );
            }
        }
    }

    #[test]
    fn a_done_that_fits_exactly_is_one_message() {
        for timed_out in [Vec::new(), vec![id(5), id(6)]] {
            let dones = split_done(&header(), timed_out.clone(), true, usize::MAX).unwrap();
            let exact = Message::Done(dones[0].clone()).to_json().unwrap().len();
            let dones = split_done(&header(), timed_out.clone(), true, exact).unwrap();
            assert_eq!(dones.len(), 1);
            assert!(!dones[0].more);
            assert!(dones[0].is_final);
            if timed_out.is_empty() {
                assert_eq!(
                    split_done(&header(), timed_out, true, exact - 1),
                    Err(MessageError::TooLarge)
                );
            }
        }
    }
}
