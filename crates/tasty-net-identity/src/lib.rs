//! Device key, device ID, connection token, private secret files and TOFU of the Tasty network.
//!
//! The design is `docs/design/identity.md`. The device key lives in a directory the consumer
//! gives ([`DeviceKey::load_or_create`]) and every instance using that directory shares it.
//! The connection token belongs to one instance ([`ConnectionToken::issue`]) and is published
//! together with the port in a [`ConnectionRecord`].
#![deny(unsafe_code)]

#[cfg(not(any(unix, windows)))]
compile_error!("tasty-net-identity supports Unix and Windows only");

mod device_key;
mod error;
mod hex;
mod record;
mod secret_file;
mod token;

pub use device_key::{
    DEVICE_KEY_FILE, DeviceId, DeviceIdParseError, DeviceKey, Signature, SignatureParseError,
};
pub use error::{CorruptKind, FileOp, IdentityError, UnsafeReason};
pub use record::{
    ConnectionRecord, ParsedRecord, RecordField, RecordParseError, RecordProblem,
    RecordPublication, RetireOutcome,
};
pub use token::{ConnectionToken, TokenParseError, constant_time_eq};
