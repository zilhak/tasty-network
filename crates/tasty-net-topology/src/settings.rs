//! Settings of this crate (docs/design/topology.md "설정값", docs/design/crates.md "설정값
//! 규칙").
//!
//! [`DEFAULTS`] is the one source of every default: [`TopologySettings`] gives the values in
//! effect from it, and the tests compare it with the table of the design document. Every time,
//! count and limit of the exploration is here; the rest of the crate takes them from a
//! [`TopologySettings`]. The size limit of one message ([`crate::MAX_MESSAGE_JSON`]) is half
//! the frame limit of the wire format and not a setting.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Where a value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The default in [`DEFAULTS`].
    Default,
    /// Given with [`TopologySettings::set`].
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
}

/// A setting value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// A time.
    Duration(Duration),
    /// A count.
    Count(u64),
}

const NANOS_PER_MILLI: u32 = 1_000_000;
const MILLIS_PER_SEC: u128 = 1000;
const MILLIS_PER_MIN: u128 = 60 * MILLIS_PER_SEC;
const MILLIS_PER_HOUR: u128 = 60 * MILLIS_PER_MIN;

impl Value {
    fn kind(self) -> Kind {
        match self {
            Value::Duration(_) => Kind::Duration,
            Value::Count(_) => Kind::Count,
        }
    }
}

impl fmt::Display for Value {
    /// The form the design table writes: `250 ms`, `5 s`, `1 h`, `16`. A time that is not a
    /// whole number of milliseconds is written in the `Debug` form of `Duration`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::Duration(d) if d.subsec_nanos() % NANOS_PER_MILLI != 0 => write!(f, "{d:?}"),
            Value::Duration(d) => {
                let ms = d.as_millis();
                if ms != 0 && ms % MILLIS_PER_HOUR == 0 {
                    write!(f, "{} h", ms / MILLIS_PER_HOUR)
                } else if ms != 0 && ms % MILLIS_PER_MIN == 0 {
                    write!(f, "{} min", ms / MILLIS_PER_MIN)
                } else if ms != 0 && ms % MILLIS_PER_SEC == 0 {
                    write!(f, "{} s", ms / MILLIS_PER_SEC)
                } else {
                    write!(f, "{ms} ms")
                }
            }
            Value::Count(n) => write!(f, "{n}"),
        }
    }
}

/// One setting: key, type, default and the accepted range (both ends included; no `max` means
/// no upper end).
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
    /// The largest accepted value, if there is one.
    pub max: Option<Value>,
}

impl Setting {
    /// The range in the form of the design table: `1 ~ 1024`, or `0 ms 이상` without an
    /// upper end.
    pub fn range_text(&self) -> String {
        match self.max {
            Some(max) => format!("{} ~ {max}", self.min),
            None => format!("{} 이상", self.min),
        }
    }
}

const fn entry(
    key: &'static str,
    kind: Kind,
    default: Value,
    min: Value,
    max: Option<Value>,
) -> Setting {
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

const HOP_TIMEOUT: &str = "hop_timeout";
const HOP_RESERVE: &str = "hop_reserve";
const MAX_QUERY_TIME: &str = "max_query_time";
const MAX_ROUTE_HOPS: &str = "max_route_hops";
const MAX_PARTS: &str = "max_parts";

/// Every setting with its default and range, in the order of the design table.
pub static DEFAULTS: &[Setting] = &[
    entry("hop_timeout", Kind::Duration, millis(5000), millis(0), None),
    entry("hop_reserve", Kind::Duration, millis(250), millis(0), None),
    entry(
        "max_query_time",
        Kind::Duration,
        millis(30_000),
        millis(1),
        Some(millis(3_600_000)),
    ),
    entry(
        "max_route_hops",
        Kind::Count,
        Value::Count(16),
        Value::Count(0),
        Some(Value::Count(1024)),
    ),
    entry(
        "max_parts",
        Kind::Count,
        Value::Count(1024),
        Value::Count(1),
        Some(Value::Count(65_536)),
    ),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// A value [`TopologySettings::set`] refuses.
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
        /// The accepted range, as [`Setting::range_text`] writes it.
        range: String,
    },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::UnknownKey(k) => write!(f, "unknown setting key {k}"),
            SettingsError::WrongType { key, given } => {
                write!(f, "setting {key} does not take the value {given}")
            }
            SettingsError::OutOfRange { key, value, range } => {
                write!(f, "setting {key} = {value} is out of range ({range})")
            }
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

/// The settings an [`crate::Agent`] runs with, and that the message functions
/// ([`crate::Message::from_json`], [`crate::split_result`], [`crate::split_done`],
/// [`crate::validate_route`]) take. Keys that are not given take their default.
/// [`crate::Agent::settings`] shows the values in effect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopologySettings {
    configured: BTreeMap<&'static str, Value>,
}

impl TopologySettings {
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
        if value < s.min || s.max.is_some_and(|max| value > max) {
            return Err(SettingsError::OutOfRange {
                key: s.key,
                value,
                range: s.range_text(),
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

    fn duration(&self, key: &'static str) -> Duration {
        match self.get(key).map(|e| e.value) {
            Some(Value::Duration(d)) => d,
            other => panic!("{key} is a time of DEFAULTS, got {other:?}"),
        }
    }

    fn count(&self, key: &'static str) -> u64 {
        match self.get(key).map(|e| e.value) {
            Some(Value::Count(n)) => n,
            other => panic!("{key} is a count of DEFAULTS, got {other:?}"),
        }
    }

    /// `hop_timeout`: how long a device waits for the first answer of a neighbor it forwarded
    /// to.
    pub fn hop_timeout(&self) -> Duration {
        self.duration(HOP_TIMEOUT)
    }

    /// `hop_reserve`: what a device keeps of its own time when it forwards, so the next
    /// device ends first and its `done` comes back in time.
    pub fn hop_reserve(&self) -> Duration {
        self.duration(HOP_RESERVE)
    }

    /// `max_query_time`: the longest a device waits for one query, and the cap of
    /// `remaining_ms` it accepts and of the time an exploration asks for.
    pub fn max_query_time(&self) -> Duration {
        self.duration(MAX_QUERY_TIME)
    }

    /// [`Self::max_query_time`] in whole milliseconds, the unit of `remaining_ms`.
    pub fn max_query_ms(&self) -> u64 {
        // The range (at most 1 h) fits u64.
        u64::try_from(self.max_query_time().as_millis()).unwrap_or(u64::MAX)
    }

    /// `max_route_hops`: the longest `route` a device sends or accepts.
    pub fn max_route_hops(&self) -> usize {
        // The range (at most 1024) fits usize on every platform.
        usize::try_from(self.count(MAX_ROUTE_HOPS)).unwrap_or(usize::MAX)
    }

    /// `max_parts`: the most result parts, and separately dones, of one answer; a `part` of
    /// this or more is malformed.
    pub fn max_parts(&self) -> u32 {
        // The range (at most 65536) fits u32.
        u32::try_from(self.count(MAX_PARTS)).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests;
