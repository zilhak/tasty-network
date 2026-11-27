//! Who may register (docs/design/rendezvous.md "기기 인증" "접속 허용").
//!
//! The server asks the policy right after TLS with the device ID TLS verified. A refusal
//! closes the connection with `access_denied`.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use tasty_net_identity::DeviceId;

/// The access policy.
pub trait AccessPolicy: Send + Sync + fmt::Debug + 'static {
    /// Whether `device` may register.
    fn allows(&self, device: &DeviceId) -> bool;
}

/// Every device may register.
#[derive(Clone, Copy, Debug, Default)]
pub struct Everyone;

impl AccessPolicy for Everyone {
    fn allows(&self, _device: &DeviceId) -> bool {
        true
    }
}

/// Only the listed devices may register.
#[derive(Clone, Debug, Default)]
pub struct Allowlist(HashSet<DeviceId>);

/// Why an allowlist file could not be read.
#[derive(Debug)]
pub enum AllowlistError {
    /// The file could not be read.
    Io(std::io::Error),
    /// A line is not a device ID.
    BadLine {
        /// The 1-based line number.
        line: usize,
    },
    /// A line is a weak (small order) key ([`crate::tls::is_weak_key`]).
    WeakKey {
        /// The 1-based line number.
        line: usize,
    },
}

impl fmt::Display for AllowlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllowlistError::Io(e) => write!(f, "allowlist file could not be read: {e}"),
            AllowlistError::BadLine { line } => {
                write!(f, "allowlist line {line} is not a device ID")
            }
            AllowlistError::WeakKey { line } => {
                write!(f, "allowlist line {line} is a weak (small order) key")
            }
        }
    }
}

impl std::error::Error for AllowlistError {}

impl Allowlist {
    /// A list of devices.
    pub fn new(devices: impl IntoIterator<Item = DeviceId>) -> Self {
        Allowlist(devices.into_iter().collect())
    }

    /// Parse a list: one device ID (64 lowercase hex digits) per line. Empty lines and lines
    /// starting with `#` are skipped. A weak (small order) key is refused.
    pub fn parse(text: &str) -> Result<Self, AllowlistError> {
        let mut out = HashSet::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let id = line
                .parse()
                .map_err(|_| AllowlistError::BadLine { line: index + 1 })?;
            if crate::tls::is_weak_key(&id) {
                return Err(AllowlistError::WeakKey { line: index + 1 });
            }
            out.insert(id);
        }
        Ok(Allowlist(out))
    }

    /// Read a list file.
    pub fn from_file(path: &Path) -> Result<Self, AllowlistError> {
        Self::parse(&std::fs::read_to_string(path).map_err(AllowlistError::Io)?)
    }

    /// The number of devices.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AccessPolicy for Allowlist {
    fn allows(&self, device: &DeviceId) -> bool {
        self.0.contains(device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_refuses_weak_keys() {
        let normal = tasty_net_identity::DeviceKey::from_seed([3; 32]).id();
        let list = Allowlist::parse(&format!("# devices\n\n{normal}\n")).unwrap();
        assert!(list.allows(&normal));
        // The neutral point (order 1) is a valid point but a weak key.
        let weak = format!("01{}", "00".repeat(31));
        assert!(weak.parse::<DeviceId>().is_ok());
        assert!(matches!(
            Allowlist::parse(&format!("{normal}\n{weak}\n")),
            Err(AllowlistError::WeakKey { line: 2 })
        ));
    }
}
