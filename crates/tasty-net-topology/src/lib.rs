//! Node and edge model, query messages and depth exploration of the Tasty network
//! topology (docs/design/topology.md).
//!
//! - [`model`]: [`Node`], [`Edge`] and their states, the wire form of both.
//! - [`LocalLink`] and [`ConnectFailure`] are what the consumer collects; [`local_view`]
//!   and [`remote_answer`] turn them into this device's answer for its own screen and for
//!   a remote query.
//! - [`Message`] and its parts are the query and result messages; [`split_result`] keeps
//!   each message at most [`MAX_MESSAGE_JSON`] bytes.
//! - [`Agent`] answers, forwards and relays queries and runs explorations. It sends through
//!   an injected [`Outbox`], so it never touches a transport.
//! - [`settings`]: the times, counts and limits of the exploration ([`TopologySettings`]),
//!   settable and visible with their source.
#![forbid(unsafe_code)]

mod explore;
mod ids;
mod local;
mod message;
pub mod model;
pub mod settings;

use std::fmt;

pub use explore::{
    Agent, Exploration, ExplorationState, ExploreRequest, Local, Now, Outbox, SendError,
};
pub use local::{
    ConnectFailure, FailureKind, LinkKind, LocalLink, SelfInfo, Sharing, View, WorkspaceExposure,
    aggregate_edges, local_view, remote_answer,
};
pub use message::{
    CAPABILITY, Done, EVENT_DONE, EVENT_QUERY, EVENT_RESULT, EVENT_SUPPRESSED, MAX_MESSAGE_JSON,
    Message, MessageError, Query, ResultHeader, ResultPart, RouteError, Suppressed, split_done,
    split_result, validate_route,
};
pub use model::{
    Edge, LinkId, LinkIdParseError, Node, NodeReason, NodeStatus, Side, TransportKind, WorkspaceRef,
};
pub use settings::TopologySettings;

/// Why a local topology call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TopologyError {
    /// The route breaks a rule of [`validate_route`]; nothing was sent.
    InvalidRoute(RouteError),
    /// The first hop could not be sent to.
    FirstHop(SendError),
    /// The OS random source failed while making a request or branch ID.
    Random,
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoute(_) => f.write_str("the topology route is invalid"),
            Self::FirstHop(_) => {
                f.write_str("the topology query could not be sent to its first hop")
            }
            Self::Random => f.write_str("the OS random source failed"),
        }
    }
}

impl std::error::Error for TopologyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRoute(e) => Some(e),
            Self::FirstHop(e) => Some(e),
            Self::Random => None,
        }
    }
}
