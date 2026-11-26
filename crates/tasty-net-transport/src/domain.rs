//! The consumer's domain check (docs/design/handshake.md "도메인 확장", check 9).
//!
//! The handshake calls these only after every earlier check passed: the request types can
//! only be built inside this crate, so a consumer sees them only for an authenticated peer.

use std::any::Any;
use std::fmt;

use crate::link::PeerInfo;

/// The consumer's check of what an authenticated peer asks for. Occupying a workspace and
/// registering with a hub happen here, so they never happen before authentication.
pub trait DomainPolicy: Send + Sync {
    /// An attach link. The request `ext` holds the consumer's JSON text (tasty: `target`,
    /// `target_workspace`, `bulk_workspace`, which are mutually exclusive).
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject>;

    /// A bulk channel under an attach link this end accepted. It gets the parent's peer and
    /// the grant the attach acceptance returned, and decides whether the grant still holds.
    fn accept_bulk(&self, request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject>;

    /// A request channel. Accepted unless the consumer says otherwise.
    fn accept_request(&self, peer: &PeerInfo) -> Result<(), DomainReject> {
        let _accepted = peer;
        Ok(())
    }
}

/// An authenticated attach request.
#[derive(Debug)]
pub struct AttachRequest<'a> {
    pub(crate) peer: &'a PeerInfo,
    pub(crate) ext: Option<&'a str>,
}

impl AttachRequest<'_> {
    /// The peer, as the handshake proved it.
    pub fn peer(&self) -> &PeerInfo {
        self.peer
    }

    /// The consumer's JSON text the dialer sent, if any.
    pub fn ext(&self) -> Option<&str> {
        self.ext
    }
}

/// A bulk request whose credential names a live parent link.
#[derive(Debug)]
pub struct BulkRequest<'a> {
    pub(crate) parent: &'a PeerInfo,
    pub(crate) grant: &'a [u8],
    pub(crate) ext: Option<&'a str>,
}

impl BulkRequest<'_> {
    /// The peer of the parent link.
    pub fn parent(&self) -> &PeerInfo {
        self.parent
    }

    /// The opaque grant the parent's acceptance returned.
    pub fn grant(&self) -> &[u8] {
        self.grant
    }

    /// The consumer's JSON text the dialer sent, if any.
    pub fn ext(&self) -> Option<&str> {
        self.ext
    }
}

/// The consumer accepts.
#[derive(Default)]
pub struct DomainAccept {
    /// JSON text for the success ACK `ext`.
    pub ext: Option<String>,
    /// The connection number for the ACK `client_id`.
    pub client_id: Option<u32>,
    /// An attach link's opaque grant, given back to [`DomainPolicy::accept_bulk`] for the bulk
    /// channels under it. Ignored for a bulk channel.
    pub grant: Vec<u8>,
    /// The consumer's registration. Dropped by this crate if the success ACK cannot be written
    /// (a deadline, a cancel or a write failure wins), so the consumer undoes the
    /// registration; otherwise it lives as long as the link or bulk channel.
    pub guard: Option<Box<dyn Any + Send>>,
}

impl fmt::Debug for DomainAccept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainAccept")
            .field("ext", &self.ext)
            .field("client_id", &self.client_id)
            .field("grant_len", &self.grant.len())
            .field("guard", &self.guard.is_some())
            .finish()
    }
}

/// The consumer refuses. The refusing ACK has the reason `domain` and the `ext`
/// `{"code": "<code>"}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainReject {
    code: String,
}

impl DomainReject {
    /// A refusal with the consumer's code, such as `conflicting_targets`.
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }

    /// The consumer's code.
    pub fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn ext(&self) -> String {
        serde_json::json!({ "code": self.code }).to_string()
    }
}

/// The consumer code of a `domain` refusal `ext` (`{"code": "<code>"}`).
pub fn domain_code(ext: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(ext).ok()?;
    value.get("code")?.as_str().map(str::to_string)
}
