//! Settings of the bindings (docs/design/mobile.md "FFI 설정값", docs/design/crates.md
//! "설정값 규칙").
//!
//! [`DEFAULTS`] is the one source of every default of this crate; the tests compare it with
//! the design table. The settings of the other crates (`connections.*`, `link.*`, P2P) are
//! theirs and only shown here.

use std::fmt;
use std::time::Duration;

/// One setting of this crate: key, default and the accepted range (both ends included).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Setting {
    pub(crate) key: &'static str,
    pub(crate) default: Duration,
    pub(crate) min: Duration,
    pub(crate) max: Duration,
}

const fn entry(key: &'static str, default: Duration, min: Duration, max: Duration) -> Setting {
    Setting {
        key,
        default,
        min,
        max,
    }
}

const fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A time value as the design tables write it, without losing precision: whole milliseconds
/// as `50 ms`, else whole microseconds as `1500 µs`, else nanoseconds as `1000000001 ns`.
pub(crate) fn show(value: Duration) -> String {
    let nanos = value.as_nanos();
    let ms = Duration::from_millis(1).as_nanos();
    let us = Duration::from_micros(1).as_nanos();
    if nanos.is_multiple_of(ms) {
        format!("{} ms", nanos / ms)
    } else if nanos.is_multiple_of(us) {
        format!("{} µs", nanos / us)
    } else {
        format!("{nanos} ns")
    }
}

/// The key of the wait between two looks at the connection table in `EventStream::next`.
pub(crate) const EVENTS_POLL_INTERVAL: &str = "events.poll_interval";

/// Every setting with its default and range, in the order of the design table.
pub(crate) static DEFAULTS: &[Setting] = &[entry(
    "events.poll_interval",
    millis(50),
    millis(1),
    millis(1000),
)];

/// The values in effect: the defaults with what the app gave.
#[derive(Clone, Debug)]
pub(crate) struct FfiSettings {
    poll_interval: Duration,
    configured: bool,
}

impl Default for FfiSettings {
    fn default() -> Self {
        Self {
            poll_interval: default_of(EVENTS_POLL_INTERVAL),
            configured: false,
        }
    }
}

fn default_of(key: &str) -> Duration {
    DEFAULTS
        .iter()
        .find(|s| s.key == key)
        .map(|s| s.default)
        .expect("every key of this module is in DEFAULTS")
}

/// A refused setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SettingRefused {
    UnknownKey(String),
    OutOfRange {
        key: &'static str,
        value: Duration,
        min: Duration,
        max: Duration,
    },
}

impl fmt::Display for SettingRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(key) => write!(f, "unknown setting key {key}"),
            Self::OutOfRange {
                key,
                value,
                min,
                max,
            } => write!(
                f,
                "setting {key} = {} is out of range ({} to {})",
                show(*value),
                show(*min),
                show(*max)
            ),
        }
    }
}

impl FfiSettings {
    /// Sets `key` to `value`; an unknown key or a value out of range changes nothing.
    pub(crate) fn set_duration(
        &mut self,
        key: &str,
        value: Duration,
    ) -> Result<(), SettingRefused> {
        let setting = DEFAULTS
            .iter()
            .find(|s| s.key == key)
            .ok_or_else(|| SettingRefused::UnknownKey(key.to_string()))?;
        if value < setting.min || value > setting.max {
            return Err(SettingRefused::OutOfRange {
                key: setting.key,
                value,
                min: setting.min,
                max: setting.max,
            });
        }
        self.poll_interval = value;
        self.configured = true;
        Ok(())
    }

    /// The wait between two looks at the connection table.
    pub(crate) fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Every setting of this crate with its value in effect and source.
    pub(crate) fn entries(&self) -> Vec<(&'static str, Duration, &'static str)> {
        let source = if self.configured {
            "configured"
        } else {
            "default"
        };
        vec![(EVENTS_POLL_INTERVAL, self.poll_interval, source)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys, defaults and ranges equal the design table (docs/design/mobile.md "FFI
    /// 설정값").
    #[test]
    fn defaults_match_design_tables() {
        let doc = include_str!("../../../docs/design/mobile.md");
        let section = doc
            .split("### FFI 설정값")
            .nth(1)
            .expect("the design has the FFI settings table");
        let rows: Vec<Vec<String>> = section
            .lines()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'))
            .skip(2)
            .map(|l| l.split('|').map(|c| c.trim().to_string()).collect())
            .collect();
        assert_eq!(rows.len(), DEFAULTS.len(), "one row per setting");
        let ms = |d: Duration| match d.as_millis() {
            n if n != 0 && n % 1000 == 0 => format!("{} s", n / 1000),
            n => format!("{n} ms"),
        };
        for (row, setting) in rows.iter().zip(DEFAULTS) {
            assert_eq!(row[1], format!("`{}`", setting.key));
            assert_eq!(row[2], ms(setting.default), "{}", setting.key);
            assert_eq!(
                row[3],
                format!("{} ~ {}", ms(setting.min), ms(setting.max)),
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn a_value_out_of_range_or_an_unknown_key_changes_nothing() {
        let mut s = FfiSettings::default();
        assert!(s.set_duration("events.nothing", millis(5)).is_err());
        assert!(
            s.set_duration(EVENTS_POLL_INTERVAL, Duration::ZERO)
                .is_err()
        );
        assert_eq!(s.poll_interval(), millis(50));
        assert_eq!(s.entries()[0].2, "default");
        s.set_duration(EVENTS_POLL_INTERVAL, millis(5))
            .expect("set");
        assert_eq!(s.poll_interval(), millis(5));
        assert_eq!(s.entries()[0].2, "configured");
    }

    /// A value shows at the precision it was given, and a refusal shows the value given.
    #[test]
    fn values_and_refusals_keep_their_precision() {
        assert_eq!(show(millis(50)), "50 ms");
        assert_eq!(show(Duration::from_micros(1500)), "1500 µs");
        assert_eq!(show(Duration::from_nanos(1_000_000_001)), "1000000001 ns");
        let mut s = FfiSettings::default();
        s.set_duration(EVENTS_POLL_INTERVAL, Duration::from_micros(1500))
            .expect("set");
        assert_eq!(show(s.entries()[0].1), "1500 µs");
        let refused = s
            .set_duration(
                EVENTS_POLL_INTERVAL,
                Duration::from_secs(1) + Duration::from_nanos(1),
            )
            .expect_err("over the maximum");
        assert_eq!(
            refused.to_string(),
            "setting events.poll_interval = 1000000001 ns is out of range (1 ms to 1000 ms)"
        );
    }
}
