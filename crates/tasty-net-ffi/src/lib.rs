//! UniFFI bindings of the Tasty network library for Kotlin and Swift apps.
//!
//! The design is `docs/design/mobile.md` "바인딩 층". The bindings copy the core types into
//! FFI records, error enums and objects; no other crate of this repository depends on UniFFI.
//!
//! - [`TastyNet`]: the entry object. It makes or reads the device key in a directory the app
//!   gives, owns the connection table and the P2P endpoint, and opens attach links and
//!   request channels.
//! - [`EventStream`]: the connection table's events, pulled with a blocking
//!   [`EventStream::next`] on an app thread. The library never calls app code from its own
//!   threads.
//! - [`AttachLink`], [`FrameChannel`], [`BulkChannel`], [`RequestChannel`]: the links and
//!   channels after handshake v2. Frame payloads are the app's.
//! - [`PeerKeyStore`]: the app's trust store of peer device IDs (TOFU), called only inside a
//!   connection attempt, on the attempt's thread.
//!
//! Every call that waits blocks the calling thread; call it off the UI thread. Connection
//! tokens never cross this boundary: no record, error, event or `Debug` output carries one.
//!
//! # Linking into an app's library
//!
//! The crate is a `lib` too, so an app links it as an rlib into its own cdylib and generates
//! the bindings of both from that one library. The scaffolding uses the namespace
//! `tasty_net` and its symbols carry this crate's name, so they do not collide with the app's.
//! The crate sets up no process-wide state (no global logger, runtime or hand-written
//! `#[no_mangle]` entry).
#![deny(unsafe_code)]

uniffi::setup_scaffolding!("tasty_net");

mod channel;
mod error;
mod net;
mod records;
mod secrets;
mod settings;
mod store;

pub use channel::{
    AttachLink, BulkChannel, CancelHandle, FinishResult, Frame, FrameChannel, FrameTag, PeerRecord,
    RequestChannel,
};
pub use error::{
    ChannelFailure, HandshakeFailure, IdentityFailureKind, P2pClose, P2pConnectFailure,
    P2pFailureKind, P2pStop, PunchCancelReason, ReadEnd, Rejection, RequestFailure, SetupFailure,
    TooOld,
};
pub use net::{
    AttachOptions, DurationSetting, EventStream, P2pSetup, RequestOptions, TastyNet, TastyNetConfig,
};
pub use records::{
    AddressSegment, AttemptRecord, CandidateRecord, CauseKind, ChannelTransport, ConnectionCause,
    ConnectionPath, ConnectionPeerInfo, ConnectionRecord, ConnectionStatus, ConnectionVia,
    LinkDirection, LinkHolder, LinkRecord, LocalSide, NatMappingKind, NatRecord, NetEvent,
    PunchModeKind, RedialRecord, SettingEntry, Transition,
};
pub use store::{PeerKeyStore, PeerStoreError, PinResult};
#[cfg(test)]
mod p2p_tests;
#[cfg(test)]
mod tests;
