//! Transport abstraction, logical channels, handshake v2, roles and the SSH transport of the
//! Tasty network library.
//!
//! The design is `docs/design/transport.md` and `docs/design/handshake.md`. Every consumer
//! API is synchronous: blocking calls block the calling thread.
//!
//! # Public contract
//!
//! | Item | Consumer |
//! |------|----------|
//! | [`Transport`] · [`Connection`] · [`Endpoint`] · [`EndpointHandle`] · [`ConnectError`] | consumers that open channels, and transport implementations |
//! | [`Listener`] · [`AcceptError`] | the receiving side (a loopback [`std::net::TcpListener`] is one) |
//! | [`RawChannel`] · [`ChannelControl`] · [`TransportKind`] | transport implementations, and the receiving side that wraps an accepted socket ([`RawChannel::from_tcp`]) |
//! | [`ChannelReader`] · [`ChannelWriter`] · [`ReadError`] · [`Framed`] · [`Finish`] | the reading and writing threads of a channel |
//! | [`ChannelConfig`] · [`HeartbeatPolicy`] · [`DEFAULT_DRAIN_CAP`] | per-channel timeouts and Ping policy |
//! | [`LinkCloser`] · [`EndReason`] | whoever ends a link (forced close, the single settle point) |
//! | [`AttemptCancel`] · [`Cancellable`] · [`Registration`] · [`Cancelled`] | cancelling one connection attempt from another thread |
//! | [`memory_channel_pair`] · [`memory_transport`] · [`MemoryTransport`] · [`MemoryListener`] | tests that run channels without sockets |
#![deny(unsafe_code)]

mod cancel;
mod channel;
mod end;
mod heartbeat;
mod lock;
mod memory;
mod transport;
mod wake;

pub use cancel::{AttemptCancel, Cancellable, Cancelled, Registration};
pub use channel::{
    ChannelConfig, ChannelControl, ChannelReader, ChannelWriter, DEFAULT_DRAIN_CAP, Finish, Framed,
    RawChannel, ReadError,
};
pub use end::{EndReason, LinkCloser};
pub use heartbeat::HeartbeatPolicy;
pub use memory::{MemoryListener, MemoryTransport, memory_channel_pair, memory_transport};
pub use transport::{
    AcceptError, ConnectError, Connection, Endpoint, EndpointHandle, Listener, Transport,
    TransportKind,
};
