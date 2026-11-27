//! Pairing: the allow list a device checks, and the messages two people exchange to pair
//! (docs/design/p2p.md "페어링").
//!
//! The key that finds a device is its device ID alone; no address is stored, since the server
//! tells it at every connection. The list is checked twice: when a `punch_offer` arrives (an
//! unpaired device gets no address) and right after a QUIC connection comes in (an unpaired
//! device is closed with `not_paired`). The path that carries the messages and the approval
//! screen are the consumer's.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tasty_net_identity::DeviceId;

/// The allow list of paired devices. Calls come from the endpoint's runtime and must return
/// quickly.
pub trait PairingStore: Send + Sync + fmt::Debug + 'static {
    /// Whether `device` is paired with this device.
    fn is_paired(&self, device: &DeviceId) -> bool;
}

/// An in-memory allow list.
#[derive(Debug, Default)]
pub struct PairingList {
    devices: Mutex<HashSet<DeviceId>>,
}

impl PairingList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<DeviceId>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add a device.
    pub fn add(&self, device: DeviceId) {
        self.lock().insert(device);
    }

    /// Remove a device; whether it was there.
    pub fn remove(&self, device: &DeviceId) -> bool {
        self.lock().remove(device)
    }
}

impl PairingStore for PairingList {
    fn is_paired(&self, device: &DeviceId) -> bool {
        self.lock().contains(device)
    }
}

/// Why a pairing message could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingError(&'static str);

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the pairing message is malformed: {}", self.0)
    }
}

impl std::error::Error for PairingError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePairing {
    device_id: String,
    display_name: String,
}

macro_rules! pairing_message {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            /// The sender's device ID.
            pub device_id: DeviceId,
            /// A name for the approval screen.
            pub display_name: String,
        }

        impl $name {
            /// The message as one JSON text: `{"device_id": <hex>, "display_name": <text>}`.
            pub fn to_json(&self) -> String {
                serde_json::to_string(&WirePairing {
                    device_id: self.device_id.to_string(),
                    display_name: self.display_name.clone(),
                })
                .expect("a pairing message serializes")
            }

            /// Read [`Self::to_json`]'s text. Unknown fields are refused.
            pub fn from_json(text: &str) -> Result<Self, PairingError> {
                let wire: WirePairing =
                    serde_json::from_str(text).map_err(|_| PairingError("json"))?;
                Ok(Self {
                    device_id: DeviceId::from_str(&wire.device_id)
                        .map_err(|_| PairingError("device_id"))?,
                    display_name: wire.display_name,
                })
            }
        }
    };
}

pairing_message!(
    PairingOffer,
    "The first pairing message: the device that asks to pair."
);
pairing_message!(
    PairingAccept,
    "The answer of the device that approved the pairing."
);

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    #[test]
    fn pairing_messages_round_trip_and_the_list_answers() {
        let id = DeviceKey::from_seed([7; 32]).id();
        let offer = PairingOffer {
            device_id: id,
            display_name: "laptop".into(),
        };
        let text = offer.to_json();
        assert_eq!(PairingOffer::from_json(&text), Ok(offer));
        assert!(PairingAccept::from_json(r#"{"device_id":"zz","display_name":"x"}"#).is_err());
        assert!(
            PairingAccept::from_json(&format!(
                r#"{{"device_id":"{id}","display_name":"x","addr":"1.2.3.4:5"}}"#
            ))
            .is_err()
        );
        let list = PairingList::new();
        assert!(!list.is_paired(&id));
        list.add(id);
        assert!(list.is_paired(&id));
        assert!(list.remove(&id));
        assert!(!list.is_paired(&id));
    }
}
