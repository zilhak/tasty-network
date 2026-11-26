//! Serde forms of [`DeviceId`] and the random IDs of a query.
//!
//! A device ID travels as its text form (64 lowercase hex digits). An unknown device ID
//! travels as the empty string (docs/design/topology.md "모델").

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};
use tasty_net_identity::DeviceId;

use crate::TopologyError;

pub(crate) mod device_id {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(id: &DeviceId, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(id)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DeviceId, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// `Option<DeviceId>` as the ID text, or `""` when unknown.
pub(crate) mod device_id_or_empty {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(id: &Option<DeviceId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.collect_str(id),
            None => s.serialize_str(""),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DeviceId>, D::Error> {
        let text = String::deserialize(d)?;
        if text.is_empty() {
            return Ok(None);
        }
        text.parse().map(Some).map_err(D::Error::custom)
    }
}

/// `Option<DeviceId>` as the ID text, absent when `None`.
pub(crate) mod device_id_opt {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(id: &Option<DeviceId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.collect_str(id),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DeviceId>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(None),
            Some(text) => text.parse().map(Some).map_err(D::Error::custom),
        }
    }
}

pub(crate) mod device_id_list {
    use super::*;
    use serde::ser::SerializeSeq;

    pub(crate) fn serialize<S: Serializer>(ids: &[DeviceId], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(ids.len()))?;
        for id in ids {
            seq.serialize_element(&id.to_string())?;
        }
        seq.end()
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<DeviceId>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|text| text.parse().map_err(D::Error::custom))
            .collect()
    }
}

/// A fresh 128-bit random ID as 32 lowercase hex digits (request and branch IDs).
pub(crate) fn random_id() -> Result<String, TopologyError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| TopologyError::Random)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
