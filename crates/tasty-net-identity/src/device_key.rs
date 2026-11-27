//! The device key, the device ID and signatures (docs/design/identity.md).

use std::{ffi::OsStr, fmt, io, path::Path, str::FromStr};

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

use crate::{
    error::{CorruptKind, FileOp, IdentityError, RandomError},
    hex, secret_file,
};

/// The file name of the device key inside the directory given to [`DeviceKey::load_or_create`].
pub const DEVICE_KEY_FILE: &str = "device_key";

const HEADER: &[u8] = b"tasty-net-device-key-v1";

/// The ed25519 secret key of this device. Its canonical form is the 32-byte seed, which
/// `ed25519_dalek::SigningKey::from_bytes` takes as is, so SSH and P2P see the same device ID.
#[derive(Clone)]
pub struct DeviceKey {
    secret: SigningKey,
}

impl DeviceKey {
    /// Load the key from `dir/device_key`, or create it when the file does not exist.
    ///
    /// Every instance that passes the same directory gets the same key, also when several
    /// processes or threads create it at the same time: the new key is published with
    /// `hard_link`, which never replaces an existing file, and every caller then reads the file
    /// that won. A file in the wrong format is [`IdentityError::Corrupt`] and is left as it is;
    /// a file that is not private is [`IdentityError::UnsafeFile`]. A failure of the OS random
    /// source is [`IdentityError::Random`].
    pub fn load_or_create(dir: &Path) -> Result<Self, IdentityError> {
        let dir = &secret_file::prepare_dir(dir)?;
        let path = dir.join(DEVICE_KEY_FILE);
        if let Some(key) = read(&path)? {
            return Ok(key);
        }
        let mut text = Vec::with_capacity(HEADER.len() + 66);
        text.extend_from_slice(HEADER);
        text.push(b'\n');
        text.extend_from_slice(
            hex::encode(&os_random_32().map_err(IdentityError::Random)?).as_bytes(),
        );
        text.push(b'\n');
        let temp = secret_file::write_temp(dir, OsStr::new(DEVICE_KEY_FILE), &text)?.path;
        match std::fs::hard_link(&temp, &path) {
            // AlreadyExists: another creator won; its file is complete because it was linked
            // after sync_all.
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                let cause = IdentityError::io(FileOp::Publish, &path, e);
                return Err(secret_file::fail_with_cleanup(&temp, cause));
            }
            _ => {}
        }
        secret_file::discard(&temp).map_err(|cleanup| IdentityError::Leftover {
            leftover: temp.clone(),
            cause: None,
            cleanup,
        })?;
        read(&path)?.ok_or_else(|| {
            IdentityError::io(
                FileOp::Read,
                &path,
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "device key vanished after publication",
                ),
            )
        })
    }

    /// A key from a 32-byte seed, without any file.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            secret: SigningKey::from_bytes(&seed),
        }
    }

    /// A new random key, without any file.
    pub fn generate() -> Result<Self, RandomError> {
        os_random_32().map(Self::from_seed)
    }

    /// The 32-byte seed, for `ed25519_dalek::SigningKey::from_bytes` (the P2P TLS key).
    pub fn to_seed(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// The device ID (the public key).
    pub fn id(&self) -> DeviceId {
        DeviceId(self.secret.verifying_key().to_bytes())
    }

    /// Sign `message` with this key.
    pub fn sign(&self, message: &[u8]) -> Signature {
        Signature(self.secret.sign(message).to_bytes())
    }
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKey")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

fn read(path: &Path) -> Result<Option<DeviceKey>, IdentityError> {
    let Some(file) = secret_file::open_private(path)? else {
        return Ok(None);
    };
    let seed = parse(&file.contents).map_err(|kind| IdentityError::Corrupt {
        path: path.to_path_buf(),
        kind,
    })?;
    Ok(Some(DeviceKey::from_seed(seed)))
}

fn parse(bytes: &[u8]) -> Result<[u8; 32], CorruptKind> {
    if bytes.len() > secret_file::MAX_SECRET_LEN {
        return Err(CorruptKind::TooLarge);
    }
    let Some(rest) = bytes
        .strip_prefix(HEADER)
        .and_then(|rest| rest.strip_prefix(b"\n"))
    else {
        return Err(CorruptKind::Header);
    };
    let seed_len = rest.len().min(64);
    let seed = hex::decode::<32>(&rest[..seed_len]).ok_or(CorruptKind::Seed)?;
    match &rest[seed_len..] {
        b"" | b"\n" => Ok(seed),
        _ => Err(CorruptKind::TrailingData),
    }
}

/// 32 bytes from the OS random source.
pub(crate) fn os_random_32() -> Result<[u8; 32], RandomError> {
    #[cfg(test)]
    if random_fault::fails() {
        return Err(RandomError::new(io::Error::other(
            "injected random failure",
        )));
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| RandomError::new(io::Error::other(e)))?;
    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod random_fault {
    use std::cell::Cell;

    thread_local! {
        static FAILS: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn fails() -> bool {
        FAILS.with(Cell::get)
    }

    /// Make the OS random source fail on this thread while the guard lives.
    pub(crate) fn fail_random() -> impl Drop {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                FAILS.with(|f| f.set(false));
            }
        }
        FAILS.with(|f| f.set(true));
        Guard
    }
}

/// The device ID: the ed25519 public key of a [`DeviceKey`]. Its text form is 64 lowercase hex
/// digits. IDs are ordered by their 32 bytes, which is also the order of their text.
///
/// It holds the 32 compressed bytes, checked to be a valid point when the ID is made, and not an
/// `ed25519_dalek::VerifyingKey`, which also carries the decompressed point (192 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    /// The ID from the 32 raw public key bytes. Bytes that are not a valid ed25519 point are
    /// refused.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, DeviceIdParseError> {
        VerifyingKey::from_bytes(bytes)
            .map(|key| Self(key.to_bytes()))
            .map_err(|_| DeviceIdParseError::InvalidKey)
    }

    /// The 32 raw public key bytes (the form signed into the handshake and taken by
    /// [`DeviceId::from_bytes`]).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Whether `signature` is a valid signature of `message` by this device. Verification is
    /// strict (`verify_strict`).
    pub fn verify(&self, message: &[u8], signature: &Signature) -> bool {
        // The bytes were checked when the ID was made, so decompressing them again succeeds.
        VerifyingKey::from_bytes(&self.0).is_ok_and(|key| {
            key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&signature.0))
                .is_ok()
        })
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.as_bytes()))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

impl FromStr for DeviceId {
    type Err = DeviceIdParseError;

    /// Only 64 lowercase hex digits: any other text is [`DeviceIdParseError::Format`], and hex
    /// that is not a valid ed25519 public key is [`DeviceIdParseError::InvalidKey`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode::<32>(s.as_bytes()).ok_or(DeviceIdParseError::Format)?;
        Self::from_bytes(&bytes)
    }
}

/// Why a device ID was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceIdParseError {
    /// The text is not 64 lowercase hex digits.
    Format,
    /// The bytes are not a valid ed25519 public key.
    InvalidKey,
}

impl fmt::Display for DeviceIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "device ID is not 64 lowercase hex digits",
            Self::InvalidKey => "device ID is not a valid ed25519 public key",
        })
    }
}

impl std::error::Error for DeviceIdParseError {}

/// An ed25519 signature (64 bytes). Its text form is 128 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    /// The signature from its 64 bytes.
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// The 64 signature bytes.
    pub fn to_bytes(&self) -> [u8; 64] {
        self.0
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(&self.0))
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({self})")
    }
}

impl FromStr for Signature {
    type Err = SignatureParseError;

    /// Only 128 lowercase hex digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode::<64>(s.as_bytes())
            .map(Self)
            .ok_or(SignatureParseError)
    }
}

/// The text is not 128 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureParseError;

impl fmt::Display for SignatureParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("signature is not 128 lowercase hex digits")
    }
}

impl std::error::Error for SignatureParseError {}

#[cfg(test)]
#[path = "device_key/tests.rs"]
mod tests;
