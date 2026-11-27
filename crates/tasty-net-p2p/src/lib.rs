//! P2P transport of the Tasty network: QUIC (quinn) over one UDP socket, punched through NAT
//! with the help of a rendezvous server.
//!
//! The design is `docs/design/p2p.md`. One UDP socket carries STUN, punch probes and QUIC,
//! split by the first byte. The endpoint registers on a rendezvous server, learns its address
//! as seen from outside, judges its NAT, and punches with a paired device when one side calls
//! [`P2pEndpoint::connect`]. Both sides authenticate with their device keys (RFC 7250 raw
//! public keys); only devices in the pairing store are accepted. The server never relays data.
//!
//! - [`P2pEndpoint`], [`P2pConfig`]: binding, the owned runtime and the blocking API.
//! - [`P2pConnection`], [`P2pStream`]: a connection and its bidirectional streams.
//! - [`settings`]: every time, count and limit, with defaults and sources.
//! - [`P2pStatus`], [`P2pConnectionInfo`], [`P2pEvent`]: state, connection info and events.
//! - [`P2pCloseReason`]: the QUIC application close codes.
#![deny(unsafe_code)]

mod attempt;
mod close;
mod conn;
mod endpoint;
mod error;
mod events;
mod info;
mod nat;
mod pairing;
mod probe;
mod runtime;
mod server;
pub mod settings;
mod socket;

pub use close::P2pCloseReason;
pub use conn::{P2pCancel, P2pConnection, P2pRecvStream, P2pSendStream, P2pStream};
pub use endpoint::{P2pConfig, P2pEndpoint};
pub use error::{P2pConnectError, P2pError, StopCause};
pub use events::{AttemptPhase, Cause, CauseDetail, EndpointPhase, P2pEvent, Phase, Subject};
pub use info::{
    AddressPair, AppliedSetting, AttemptInfo, HistoryEntry, NatInfo, P2pConnectionInfo, P2pStatus,
    RendezvousServer, Side,
};
pub use nat::{NatJudgement, StunAnswer, judge};
pub use pairing::{PairingAccept, PairingError, PairingList, PairingOffer, PairingStore};
pub use socket::{RemoteCandidate, RemoteKind, StunCounters};
