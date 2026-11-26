//! Trust on first use of a peer's device ID (docs/design/identity.md "TOFU").
//!
//! The decision is here; the consumer stores the records through [`PeerKeyStore`].

use std::{collections::HashMap, error::Error, fmt, sync::Mutex};

use crate::device_key::DeviceId;

/// The consumer's name for a peer (tasty uses the remote profile name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerRef(String);

impl PeerRef {
    /// A peer name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A failure of the consumer's store, passed through unchanged.
#[derive(Debug)]
pub struct StoreError(Box<dyn Error + Send + Sync>);

impl StoreError {
    /// Wrap the store's own error.
    pub fn new(error: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self(error.into())
    }

    /// The store's own error.
    pub fn into_inner(self) -> Box<dyn Error + Send + Sync> {
        self.0
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the peer key store failed")
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&*self.0)
    }
}

/// The result of [`PeerKeyStore::pin_if_absent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// There was no record; this call recorded the given ID.
    Inserted,
    /// A record exists; it was left unchanged. The value is the recorded ID.
    Existing(DeviceId),
}

/// The record store of peer device IDs. `pin_if_absent` must be atomic per peer: of two
/// concurrent first calls only one returns [`PinOutcome::Inserted`].
pub trait PeerKeyStore {
    /// The recorded ID, for display. The decision does not use it.
    fn get(&self, peer: &PeerRef) -> Result<Option<DeviceId>, StoreError>;
    /// Record `id` when the peer has no record; otherwise return the record unchanged.
    fn pin_if_absent(&self, peer: &PeerRef, id: &DeviceId) -> Result<PinOutcome, StoreError>;
    /// Replace the record. Only [`reregister`] calls this.
    fn replace(&self, peer: &PeerRef, id: &DeviceId) -> Result<(), StoreError>;
}

/// The TOFU decision for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuVerdict {
    /// First sight; the ID was recorded. Accept.
    New,
    /// The ID equals the record. Accept.
    Match,
    /// The ID differs from the record, which was not changed. Refuse (`peer_key_mismatch`).
    Mismatch {
        /// The recorded ID.
        expected: DeviceId,
        /// The ID the peer proved.
        got: DeviceId,
    },
}

impl TofuVerdict {
    /// Whether the connection may continue.
    pub fn is_accepted(&self) -> bool {
        !matches!(self, Self::Mismatch { .. })
    }
}

/// Decide on `id`, the device ID the peer proved, with one `pin_if_absent` call.
pub fn check_peer<S: PeerKeyStore + ?Sized>(
    store: &S,
    peer: &PeerRef,
    id: &DeviceId,
) -> Result<TofuVerdict, StoreError> {
    Ok(match store.pin_if_absent(peer, id)? {
        PinOutcome::Inserted => TofuVerdict::New,
        PinOutcome::Existing(expected) if expected == *id => TofuVerdict::Match,
        PinOutcome::Existing(expected) => TofuVerdict::Mismatch { expected, got: *id },
    })
}

/// The user's explicit re-registration of a peer with a new device ID. The handshake never
/// calls this.
pub fn reregister<S: PeerKeyStore + ?Sized>(
    store: &S,
    peer: &PeerRef,
    id: &DeviceId,
) -> Result<(), StoreError> {
    store.replace(peer, id)
}

/// A [`PeerKeyStore`] in memory, for tests and consumers that keep no records on disk.
#[derive(Debug, Default)]
pub struct MemoryPeerKeyStore {
    records: Mutex<HashMap<PeerRef, DeviceId>>,
}

impl MemoryPeerKeyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> std::sync::MutexGuard<'_, HashMap<PeerRef, DeviceId>> {
        // A panic while holding the lock cannot leave a half-written map entry.
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PeerKeyStore for MemoryPeerKeyStore {
    fn get(&self, peer: &PeerRef) -> Result<Option<DeviceId>, StoreError> {
        Ok(self.records().get(peer).copied())
    }

    fn pin_if_absent(&self, peer: &PeerRef, id: &DeviceId) -> Result<PinOutcome, StoreError> {
        Ok(match self.records().entry(peer.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => PinOutcome::Existing(*e.get()),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(*id);
                PinOutcome::Inserted
            }
        })
    }

    fn replace(&self, peer: &PeerRef, id: &DeviceId) -> Result<(), StoreError> {
        self.records().insert(peer.clone(), *id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceKey;

    #[test]
    fn new_match_mismatch_and_reregister() {
        let store = MemoryPeerKeyStore::new();
        let peer = PeerRef::new("work-laptop");
        let first = DeviceKey::generate().unwrap().id();
        let second = DeviceKey::generate().unwrap().id();

        assert_eq!(check_peer(&store, &peer, &first).unwrap(), TofuVerdict::New);
        assert_eq!(store.get(&peer).unwrap(), Some(first));
        assert_eq!(
            check_peer(&store, &peer, &first).unwrap(),
            TofuVerdict::Match
        );

        let verdict = check_peer(&store, &peer, &second).unwrap();
        assert_eq!(
            verdict,
            TofuVerdict::Mismatch {
                expected: first,
                got: second
            }
        );
        assert!(!verdict.is_accepted());
        assert_eq!(
            store.get(&peer).unwrap(),
            Some(first),
            "mismatch changes nothing"
        );

        reregister(&store, &peer, &second).unwrap();
        assert_eq!(
            check_peer(&store, &peer, &second).unwrap(),
            TofuVerdict::Match
        );
        assert!(matches!(
            check_peer(&store, &peer, &first).unwrap(),
            TofuVerdict::Mismatch { .. }
        ));
    }

    #[test]
    fn peers_are_independent() {
        let store = MemoryPeerKeyStore::new();
        let id = DeviceKey::generate().unwrap().id();
        let other = DeviceKey::generate().unwrap().id();
        assert_eq!(
            check_peer(&store, &PeerRef::new("a"), &id).unwrap(),
            TofuVerdict::New
        );
        assert_eq!(
            check_peer(&store, &PeerRef::new("b"), &other).unwrap(),
            TofuVerdict::New
        );
    }

    #[test]
    fn concurrent_first_connections_insert_once() {
        let store = MemoryPeerKeyStore::new();
        let peer = PeerRef::new("peer");
        let ids: Vec<DeviceId> = (0..8)
            .map(|_| DeviceKey::generate().unwrap().id())
            .collect();
        let verdicts: Vec<TofuVerdict> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .iter()
                .map(|id| s.spawn(|| check_peer(&store, &peer, id).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            verdicts.iter().filter(|v| **v == TofuVerdict::New).count(),
            1
        );
    }

    #[test]
    fn store_errors_pass_through() {
        struct Failing;
        impl PeerKeyStore for Failing {
            fn get(&self, _: &PeerRef) -> Result<Option<DeviceId>, StoreError> {
                Err(StoreError::new("disk"))
            }
            fn pin_if_absent(&self, _: &PeerRef, _: &DeviceId) -> Result<PinOutcome, StoreError> {
                Err(StoreError::new("disk"))
            }
            fn replace(&self, _: &PeerRef, _: &DeviceId) -> Result<(), StoreError> {
                Err(StoreError::new("disk"))
            }
        }
        let id = DeviceKey::generate().unwrap().id();
        let error = check_peer(&Failing, &PeerRef::new("p"), &id).unwrap_err();
        assert_eq!(error.into_inner().to_string(), "disk");
        assert!(reregister(&Failing as &dyn PeerKeyStore, &PeerRef::new("p"), &id).is_err());
    }
}
