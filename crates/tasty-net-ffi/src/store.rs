//! The peer key trust store the app implements (android #7b, docs/design/identity.md "TOFU").
//!
//! The handshake calls [`PeerKeyStore::pin_if_absent`] once per connection, on the thread
//! that opened it, while that thread waits. The first record must be atomic in the app's
//! store: of two concurrent first calls for one peer only one may answer
//! [`PinResult::Inserted`]. When the key changed, the connection ends with
//! `HandshakeFailure::PeerKeyMismatch` and this library never calls
//! [`PeerKeyStore::replace`] by itself; only [`TastyNet::reregister_peer`]
//! (crate::TastyNet::reregister_peer), which the app calls after the user confirmed, does.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use tasty_net_identity::{
    DeviceId, PeerKeyStore as CoreStore, PeerRef, PinOutcome, StoreError, reregister,
};

/// Why the app's store failed. The text is the app's own and comes back inside
/// `HandshakeFailure::Store`.
#[derive(Debug, uniffi::Error)]
pub enum PeerStoreError {
    /// The store failed.
    Failed {
        /// The app's error text.
        reason: String,
    },
}

impl fmt::Display for PeerStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed { reason } => f.write_str(reason),
        }
    }
}

impl std::error::Error for PeerStoreError {}

impl From<uniffi::UnexpectedUniFFICallbackError> for PeerStoreError {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed {
            reason: error.reason,
        }
    }
}

/// The answer of [`PeerKeyStore::pin_if_absent`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PinResult {
    /// There was no record; this call recorded the given device ID.
    Inserted,
    /// A record exists and was left unchanged.
    Existing {
        /// The recorded device ID.
        device_id: String,
    },
}

/// The record store of peer device IDs, by peer name (the app's name for the remote, such as
/// a profile name). Device IDs are the text form `DeviceId` shows.
#[uniffi::export(with_foreign)]
pub trait PeerKeyStore: Send + Sync {
    /// The recorded device ID of `peer`, for display.
    fn get(&self, peer: String) -> Result<Option<String>, PeerStoreError>;
    /// Records `device_id` when `peer` has no record; otherwise returns the record unchanged.
    /// Must be atomic per peer.
    fn pin_if_absent(&self, peer: String, device_id: String) -> Result<PinResult, PeerStoreError>;
    /// Replaces the record. Only the user's re-registration calls this.
    fn replace(&self, peer: String, device_id: String) -> Result<(), PeerStoreError>;
}

/// The core store over the app's.
pub(crate) struct AppStore(pub(crate) Arc<dyn PeerKeyStore>);

impl fmt::Debug for AppStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppStore").finish_non_exhaustive()
    }
}

fn parse_id(text: &str) -> Result<DeviceId, StoreError> {
    DeviceId::from_str(text).map_err(StoreError::new)
}

impl CoreStore for AppStore {
    fn get(&self, peer: &PeerRef) -> Result<Option<DeviceId>, StoreError> {
        match self.0.get(peer.as_str().to_string()) {
            Ok(Some(text)) => parse_id(&text).map(Some),
            Ok(None) => Ok(None),
            Err(e) => Err(StoreError::new(e)),
        }
    }

    fn pin_if_absent(&self, peer: &PeerRef, id: &DeviceId) -> Result<PinOutcome, StoreError> {
        match self
            .0
            .pin_if_absent(peer.as_str().to_string(), id.to_string())
        {
            Ok(PinResult::Inserted) => Ok(PinOutcome::Inserted),
            Ok(PinResult::Existing { device_id }) => parse_id(&device_id).map(PinOutcome::Existing),
            Err(e) => Err(StoreError::new(e)),
        }
    }

    fn replace(&self, peer: &PeerRef, id: &DeviceId) -> Result<(), StoreError> {
        self.0
            .replace(peer.as_str().to_string(), id.to_string())
            .map_err(StoreError::new)
    }
}

/// The user's re-registration of `peer` with `device_id`, through the core's `reregister`.
pub(crate) fn reregister_with(
    store: &Arc<dyn PeerKeyStore>,
    peer: &str,
    device_id: &str,
) -> Result<(), PeerStoreError> {
    let id = DeviceId::from_str(device_id).map_err(|e| PeerStoreError::Failed {
        reason: e.to_string(),
    })?;
    let core = AppStore(Arc::clone(store));
    reregister(&core, &PeerRef::new(peer), &id).map_err(|e| PeerStoreError::Failed {
        reason: crate::error::store_text(&e),
    })
}
