//! Transport abstraction, logical channels, handshake v2, roles and the SSH transport of the
//! Tasty network library.
//!
//! The design is `docs/design/transport.md` and `docs/design/handshake.md`. Every consumer
//! API is synchronous: blocking calls block the calling thread.
//!
//! A consumer opens a channel with a [`Transport`], runs [`dial_handshake`] on it, takes the
//! ordered channel's halves from the [`Link`], opens bulk channels with
//! [`Link::open_bulk`], and ends with [`ChannelWriter::finish`] or [`LinkCloser::force_close`].
//! The accepting end reads the first line ([`read_first_line`]) and runs
//! [`accept_handshake`], which calls the consumer's [`DomainPolicy`] only after the token,
//! signature, self, TOFU, role and bulk credential checks passed.
//!
//! # Public contract
//!
//! | Item | Consumer |
//! |------|----------|
//! | [`Transport`] · [`Connection`] · [`Endpoint`] · [`EndpointHandle`] · [`DialAuth`] · [`ConnectError`] | consumers that open channels, and transport implementations |
//! | [`Listener`] · [`AcceptError`] | the receiving side (a loopback [`std::net::TcpListener`] is one) |
//! | [`RawChannel`] · [`ChannelControl`] · [`TransportKind`] | transport implementations, and the receiving side that wraps an accepted socket ([`RawChannel::from_tcp`]) |
//! | [`ChannelReader`] · [`ChannelWriter`] · [`ReadError`] · [`Framed`] · [`Finish`] | the reading and writing threads of a channel |
//! | [`ChannelConfig`] · [`HeartbeatPolicy`] · [`DEFAULT_DRAIN_CAP`] · [`LinkTimeouts`] | per-channel and per-link timeouts and Ping policy |
//! | [`LinkCloser`] · [`EndReason`] | whoever ends a link (forced close, the single settle point) |
//! | [`AttemptCancel`] · [`Cancellable`] · [`Registration`] · [`Cancelled`] | cancelling one connection attempt from another thread |
//! | [`dial_handshake`] · [`dial_request`] · [`DialConfig`] · [`Link`] · [`BulkChannel`] · [`PeerInfo`] | the dialing end |
//! | [`read_first_line`] · [`FirstLineError`] · [`accept_handshake`] · [`accept_p2p_handshake`] · [`AcceptConfig`] · [`Accepted`] · [`RoleProposal`] · [`BulkRegistry`] · [`DEFAULT_MAX_HANDSHAKES`] | the accepting end |
//! | [`DomainPolicy`] · [`AttachRequest`] · [`BulkRequest`] · [`DomainAccept`] · [`DomainReject`] · [`domain_code`] | the consumer's domain check (occupation and registration happen in it) |
//! | [`RequestClient`] · [`RequestServer`] · [`RequestHandler`] · [`RequestError`] | request channels (port-based lookup, creation, status) |
//! | [`RejectReason`] · [`HandshakeError`] · [`TooOldEvidence`] · [`P2pCloseReason`] | consumers that branch on why a handshake ended (`is_retryable`) |
//! | [`Holder`] · [`Side`] · [`Roles`] · [`roles`] | consumers that act on the roles of a link |
//! | [`Purpose`] · [`PROTO`] · [`STREAM_OPEN_METHOD`] · [`MAX_HANDSHAKE_LINE`] · [`LEGACY_PROTOCOL_ERROR`] · [`is_valid_capability`] · [`InvalidCapabilities`] | the wire constants |
//! | [`SshTransport`] · [`LOCAL_CONNECT_TIMEOUT`] | consumers that reach a peer over SSH (port discovery, shared `-L` tunnel, loopback TCP channels) |
//! | [`memory_channel_pair`] · [`memory_transport`] · [`MemoryTransport`] · [`MemoryListener`] | tests that run channels without sockets |
#![deny(unsafe_code)]

mod bulk;
mod cancel;
mod channel;
mod domain;
mod end;
mod handshake;
mod heartbeat;
mod hex;
mod link;
mod lock;
mod memory;
mod reject;
mod request;
mod role;
mod ssh;
mod transport;
mod wake;
mod wire;

pub use bulk::BulkRegistry;
pub use cancel::{AttemptCancel, Cancellable, Cancelled, Registration};
pub use channel::{
    ChannelConfig, ChannelControl, ChannelReader, ChannelWriter, DEFAULT_DRAIN_CAP, Finish, Framed,
    RawChannel, ReadError,
};
pub use domain::{
    AttachRequest, BulkRequest, DomainAccept, DomainPolicy, DomainReject, domain_code,
};
pub use end::{EndReason, LinkCloser};
pub use handshake::{
    AcceptConfig, Accepted, DEFAULT_MAX_HANDSHAKES, DialConfig, FirstLineError,
    InvalidCapabilities, RoleProposal, accept_handshake, accept_p2p_handshake, dial_handshake,
    dial_request, read_first_line,
};
pub use heartbeat::HeartbeatPolicy;
pub use link::{BulkChannel, Link, LinkTimeouts, PeerInfo};
pub use memory::{MemoryListener, MemoryTransport, memory_channel_pair, memory_transport};
pub use reject::{HandshakeError, P2pCloseReason, RejectReason, TooOldEvidence};
pub use request::{RequestClient, RequestError, RequestHandler, RequestServer};
pub use role::{Holder, Roles, Side, roles};
pub use ssh::{LOCAL_CONNECT_TIMEOUT, SshTransport};
pub use transport::{
    AcceptError, ConnectError, Connection, DialAuth, Endpoint, EndpointHandle, Listener, Transport,
    TransportKind,
};
pub use wire::{
    LEGACY_PROTOCOL_ERROR, MAX_HANDSHAKE_LINE, PROTO, Purpose, STREAM_OPEN_METHOD,
    is_valid_capability,
};
