//! Settings of this crate (docs/design/identity.md "설정값", docs/design/crates.md "설정값
//! 규칙").
//!
//! [`DEFAULTS`] is the one source of every default: [`IdentitySettings`] gives the values in
//! effect from it, and the tests compare it with the table of the design document. Every time,
//! count and limit of the crate is here; the rest of the crate takes them from an
//! [`IdentitySettings`].

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Where a value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The default in [`DEFAULTS`].
    Default,
    /// Given with [`IdentitySettings::set`].
    Configured,
}

impl Source {
    /// `default` or `configured`.
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Configured => "configured",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The type of a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A time.
    Duration,
    /// A count.
    Count,
    /// A size in bytes.
    Bytes,
}

/// A setting value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// A time.
    Duration(Duration),
    /// A count.
    Count(u64),
    /// A size in bytes.
    Bytes(u64),
}

const KIB: u64 = 1024;
const MIB: u64 = KIB * KIB;
const NANOS_PER_MILLI: u32 = 1_000_000;
const MILLIS_PER_SEC: u128 = 1000;

impl Value {
    fn kind(self) -> Kind {
        match self {
            Value::Duration(_) => Kind::Duration,
            Value::Count(_) => Kind::Count,
            Value::Bytes(_) => Kind::Bytes,
        }
    }
}

impl fmt::Display for Value {
    /// The form the design table writes: `10 ms`, `1 s`, `50`, `4 KiB`. A time that is not a
    /// whole number of milliseconds is written in the `Debug` form of `Duration`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::Duration(d) if d.subsec_nanos() % NANOS_PER_MILLI != 0 => write!(f, "{d:?}"),
            Value::Duration(d) => {
                let ms = d.as_millis();
                if ms != 0 && ms % MILLIS_PER_SEC == 0 {
                    write!(f, "{} s", ms / MILLIS_PER_SEC)
                } else {
                    write!(f, "{ms} ms")
                }
            }
            Value::Count(n) => write!(f, "{n}"),
            Value::Bytes(b) if b != 0 && b % MIB == 0 => write!(f, "{} MiB", b / MIB),
            Value::Bytes(b) if b != 0 && b % KIB == 0 => write!(f, "{} KiB", b / KIB),
            Value::Bytes(b) => write!(f, "{b} B"),
        }
    }
}

/// One setting: key, type, default and the accepted range (both ends included).
#[derive(Clone, Copy, Debug)]
pub struct Setting {
    /// The key.
    pub key: &'static str,
    /// The type.
    pub kind: Kind,
    /// The default.
    pub default: Value,
    /// The smallest accepted value.
    pub min: Value,
    /// The largest accepted value.
    pub max: Value,
}

const fn entry(key: &'static str, kind: Kind, default: Value, min: Value, max: Value) -> Setting {
    Setting {
        key,
        kind,
        default,
        min,
        max,
    }
}

const fn millis(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

const NEW_PARENT_RETRIES: &str = "new_parent_retries";
const NEW_PARENT_WAIT: &str = "new_parent_wait";
const MAX_SECRET_BYTES: &str = "max_secret_bytes";

/// Every setting with its default and range, in the order of the design table.
pub static DEFAULTS: &[Setting] = &[
    entry(
        "new_parent_retries",
        Kind::Count,
        Value::Count(50),
        Value::Count(0),
        Value::Count(1000),
    ),
    entry(
        "new_parent_wait",
        Kind::Duration,
        millis(10),
        millis(0),
        millis(1000),
    ),
    entry(
        "max_secret_bytes",
        Kind::Bytes,
        Value::Bytes(4 * KIB),
        Value::Bytes(KIB),
        Value::Bytes(64 * KIB),
    ),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// A value [`IdentitySettings::set`] refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingsError {
    /// The key is not a setting.
    UnknownKey(String),
    /// The value does not have the type of the setting.
    WrongType {
        /// The key.
        key: &'static str,
        /// What was given.
        given: Value,
    },
    /// The value is outside the accepted range.
    OutOfRange {
        /// The key.
        key: &'static str,
        /// The value.
        value: Value,
        /// The smallest accepted value.
        min: Value,
        /// The largest accepted value.
        max: Value,
    },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::UnknownKey(k) => write!(f, "unknown setting key {k}"),
            SettingsError::WrongType { key, given } => {
                write!(f, "setting {key} does not take the value {given}")
            }
            SettingsError::OutOfRange {
                key,
                value,
                min,
                max,
            } => write!(
                f,
                "setting {key} = {value} is out of range ({min} to {max})"
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

/// One line of the settings query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingEntry {
    /// The key.
    pub key: &'static str,
    /// The value in effect.
    pub value: Value,
    /// Where it comes from.
    pub source: Source,
}

impl fmt::Display for SettingEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {} ({})", self.key, self.value, self.source)
    }
}

/// The settings the file operations of this crate run with. Keys that are not given take their
/// default. The functions without settings ([`crate::DeviceKey::load_or_create`],
/// [`crate::ConnectionRecord::publish`]) use `IdentitySettings::default()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentitySettings {
    configured: BTreeMap<&'static str, Value>,
}

impl IdentitySettings {
    /// No configured values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one value. The key must be a setting, and the value must have its type and lie in
    /// its range.
    pub fn set(&mut self, key: &str, value: Value) -> Result<&mut Self, SettingsError> {
        let s = setting(key).ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        if value.kind() != s.kind {
            return Err(SettingsError::WrongType {
                key: s.key,
                given: value,
            });
        }
        if value < s.min || value > s.max {
            return Err(SettingsError::OutOfRange {
                key: s.key,
                value,
                min: s.min,
                max: s.max,
            });
        }
        self.configured.insert(s.key, value);
        Ok(self)
    }

    /// The value in effect for `key` and its source.
    pub fn get(&self, key: &str) -> Option<SettingEntry> {
        let s = setting(key)?;
        Some(match self.configured.get(s.key) {
            Some(v) => SettingEntry {
                key: s.key,
                value: *v,
                source: Source::Configured,
            },
            None => SettingEntry {
                key: s.key,
                value: s.default,
                source: Source::Default,
            },
        })
    }

    /// Every setting with the value in effect and its source, in [`DEFAULTS`] order.
    pub fn entries(&self) -> Vec<SettingEntry> {
        DEFAULTS.iter().filter_map(|s| self.get(s.key)).collect()
    }

    fn value(&self, key: &'static str) -> Value {
        match self.get(key) {
            Some(e) => e.value,
            None => panic!("{key} is a setting of DEFAULTS"),
        }
    }

    /// `new_parent_retries`: how often creating a directory element is retried while its new
    /// parent, made by another thread, is not yet 0700 (Unix).
    pub fn new_parent_retries(&self) -> u32 {
        match self.value(NEW_PARENT_RETRIES) {
            // The range keeps it far below u32::MAX.
            Value::Count(n) => u32::try_from(n).unwrap_or(u32::MAX),
            other => panic!("{NEW_PARENT_RETRIES} is a count, got {other:?}"),
        }
    }

    /// `new_parent_wait`: the wait before each of those retries.
    pub fn new_parent_wait(&self) -> Duration {
        match self.value(NEW_PARENT_WAIT) {
            Value::Duration(d) => d,
            other => panic!("{NEW_PARENT_WAIT} is a time, got {other:?}"),
        }
    }

    /// `max_secret_bytes`: the largest secret file this crate reads whole, and the largest
    /// connection record line it publishes.
    pub fn max_secret_bytes(&self) -> usize {
        match self.value(MAX_SECRET_BYTES) {
            // The range (at most 64 KiB) fits usize on every platform.
            Value::Bytes(b) => usize::try_from(b).unwrap_or(usize::MAX),
            other => panic!("{MAX_SECRET_BYTES} is a size, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests;
