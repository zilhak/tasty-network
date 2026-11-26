//! Bulk credentials: a bulk channel is bound to the attach link it was issued on and to the
//! grant the consumer gave that link (docs/design/handshake.md "bulk 자격").

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use crate::end::LinkCloser;
use crate::hex::random_32;
use crate::link::PeerInfo;
use crate::lock::lock;

/// What a credential names.
#[derive(Clone)]
pub(crate) struct BulkParent {
    /// The parent link's closer. Adopting a bulk channel into it is the single point that
    /// decides between "registered as a child" and "the parent already ended".
    pub closer: LinkCloser,
    pub grant: Arc<[u8]>,
    pub peer: PeerInfo,
    /// The QUIC connection of a P2P parent; `None` on the SSH path.
    pub scope: Option<u64>,
}

/// The credentials the accepting end issued, shared by every handshake of one acceptor.
/// A credential lives while its parent link lives and is removed when the link ends.
#[derive(Clone, Default)]
pub struct BulkRegistry {
    inner: Arc<Mutex<HashMap<[u8; 32], BulkParent>>>,
}

impl fmt::Debug for BulkRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BulkRegistry")
            .field("live", &self.live())
            .finish()
    }
}

impl BulkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many credentials are live.
    pub fn live(&self) -> usize {
        lock(&self.inner).len()
    }

    /// Issues a credential for `parent`: 32 OS random bytes.
    pub(crate) fn issue(&self, parent: BulkParent) -> io::Result<[u8; 32]> {
        let credential = random_32()?;
        lock(&self.inner).insert(credential, parent);
        Ok(credential)
    }

    pub(crate) fn revoke(&self, credential: &[u8; 32]) {
        lock(&self.inner).remove(credential);
    }

    pub(crate) fn find(&self, credential: &[u8; 32]) -> Option<BulkParent> {
        lock(&self.inner).get(credential).cloned()
    }
}
