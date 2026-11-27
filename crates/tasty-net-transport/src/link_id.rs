//! The link ID: the identifier both ends of an attach link know (docs/design/handshake.md
//! "bulk 자격").

use std::fmt;

use sha2::{Digest, Sha256};

/// The domain string hashed in front of the credential, so a link ID never equals another
/// hash made from the same bytes.
const DOMAIN: &[u8] = b"tasty-net link id v1";

/// The length of a link ID: the first 16 bytes of the digest (topology's link ID length).
const LINK_ID_LEN: usize = 16;

/// The length of a bulk credential (docs/design/handshake.md "bulk 자격").
const CREDENTIAL_LEN: usize = 32;

/// The identifier of one attach link, the same on both ends: the first 16 bytes of
/// SHA-256(`tasty-net link id v1` ‖ the link's 32-byte bulk credential). Each end computes it
/// after the success ACK; it is never sent on the wire. Its text form is 32 lowercase hex
/// digits. topology takes the bytes with `LinkId::from_bytes`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkId([u8; LINK_ID_LEN]);

impl LinkId {
    /// The ID of the link whose bulk credential is `credential`.
    pub(crate) fn of_credential(credential: &[u8; CREDENTIAL_LEN]) -> Self {
        let digest = Sha256::new()
            .chain_update(DOMAIN)
            .chain_update(credential)
            .finalize();
        let mut id = [0u8; LINK_ID_LEN];
        let len = id.len();
        id.copy_from_slice(&digest[..len]);
        Self(id)
    }

    /// The 16 bytes.
    pub fn as_bytes(&self) -> &[u8; LINK_ID_LEN] {
        &self.0
    }

    /// The 32 lowercase hex digits (the same as `Display`).
    pub fn to_hex(&self) -> String {
        crate::hex::encode(&self.0)
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkId({})", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed vectors: the credential bytes 0, 1, ..., 31 and 32 bytes of 0xff (computed
    /// independently with Python's hashlib).
    #[test]
    fn the_link_id_follows_the_fixed_vectors() {
        let mut counting = [0u8; 32];
        for (i, b) in counting.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("small");
        }
        let id = LinkId::of_credential(&counting);
        assert_eq!(id.to_hex(), "db1ae81afc7b9597a0c2ad6a53adbe6f");
        assert_eq!(id.to_string(), id.to_hex());
        assert_eq!(id.as_bytes().len(), 16);
        assert_eq!(
            LinkId::of_credential(&[0xff; 32]).to_hex(),
            "fc44199bb8aaca8dd3fc535ad19c920b"
        );
    }
}
