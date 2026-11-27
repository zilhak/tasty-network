//! Rendezvous protocol and server of the Tasty network.
//!
//! The design is `docs/design/rendezvous.md`. The server does two things only: it tells a
//! device its address as seen from outside (STUN Binding) and it coordinates simultaneous
//! punching between two devices registered on it. It never relays data: there is no code path
//! that sends bytes received from one device to another.
//!
//! - [`wire`]: the format shared with `tasty-net-p2p` (frames, messages, STUN subset, punch
//!   probe, first-byte packet classes, candidate rule).
//! - [`tls`]: RFC 7250 raw public key TLS 1.3 with device keys.
//! - [`settings`]: every server time, count and limit, with defaults and sources.
//! - [`packet_io`]: the packet I/O trait and the first-byte split socket under quinn.
//! - [`server`]: the server ([`server::Server`]) with its [`access`] policy.
#![deny(unsafe_code)]

pub mod access;
pub mod packet_io;
pub mod quic;
pub mod server;
pub mod settings;
pub mod tls;
pub mod wire;
