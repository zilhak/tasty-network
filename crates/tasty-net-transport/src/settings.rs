//! The settings of the connection table and the redialer: their keys, their defaults and how
//! they are shown (docs/design/crates.md "설정값 규칙", docs/design/transport.md "보관 상한과
//! 재연결 도우미").
//!
//! [`DEFAULTS`] is the one source of the default values: the `Default` of
//! [`ConnectionTableConfig`](crate::ConnectionTableConfig) and
//! [`RedialerConfig`](crate::RedialerConfig) and every `settings()` read it, and a test
//! compares it with the table of the design document.

use std::fmt;
use std::time::{Duration, Instant};

/// `connections.history_limit`: the transitions kept per entry.
pub const HISTORY_LIMIT: &str = "connections.history_limit";
/// `connections.keep_closed`: the `disconnected` entries kept.
pub const KEEP_CLOSED: &str = "connections.keep_closed";
/// `connections.event_queue_limit`: the events kept for a consumer that does not read them.
pub const EVENT_QUEUE_LIMIT: &str = "connections.event_queue_limit";
/// `redial.initial`: the first wait.
pub const REDIAL_INITIAL: &str = "redial.initial";
/// `redial.multiplier`: the factor between two waits.
pub const REDIAL_MULTIPLIER: &str = "redial.multiplier";
/// `redial.max_delay`: the longest wait.
pub const REDIAL_MAX_DELAY: &str = "redial.max_delay";
/// `redial.jitter`: the random width of a wait.
pub const REDIAL_JITTER: &str = "redial.jitter";
/// `redial.give_up_after`: how long the redialer tries after the loss (0: it does not redial).
pub const REDIAL_GIVE_UP_AFTER: &str = "redial.give_up_after";
/// `redial.max_attempts`: the most attempts (0: no bound).
pub const REDIAL_MAX_ATTEMPTS: &str = "redial.max_attempts";

/// The keys of [`LinkTimeouts`](crate::LinkTimeouts) as an entry's `applied` shows them.
pub const LINK_READ_TIMEOUT: &str = "link.read_timeout";
/// See [`LINK_READ_TIMEOUT`].
pub const LINK_WRITE_TIMEOUT: &str = "link.write_timeout";
/// See [`LINK_READ_TIMEOUT`].
pub const LINK_HANDSHAKE_TIMEOUT: &str = "link.handshake_timeout";
/// See [`LINK_READ_TIMEOUT`].
pub const LINK_ROLE_PROPOSAL_TIMEOUT: &str = "link.role_proposal_timeout";
/// See [`LINK_READ_TIMEOUT`].
pub const LINK_DRAIN_CAP: &str = "link.drain_cap";

/// A setting's value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingValue {
    /// A count or a bound.
    Count(u64),
    /// A time.
    Duration(Duration),
    /// A factor or a fraction.
    Ratio(f64),
    /// A value a P2P connection was given (`tasty-net-p2p`'s own form, shown as it is).
    P2p(tasty_net_p2p::settings::Value),
}

impl fmt::Display for SettingValue {
    /// The form of the design tables: `32`, `500 ms`, `30 s`, `0.2`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::Duration(d) if d.subsec_nanos() == 0 => write!(f, "{} s", d.as_secs()),
            Self::Duration(d) if d.subsec_nanos() % NANOS_PER_MILLI == 0 => {
                write!(f, "{} ms", d.as_millis())
            }
            Self::Duration(d) => write!(f, "{} ns", d.as_nanos()),
            Self::Ratio(r) => write!(f, "{r}"),
            Self::P2p(value) => write!(f, "{value}"),
        }
    }
}

const NANOS_PER_MILLI: u32 = 1_000_000;

/// Where a setting's value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SettingSource {
    /// The default of [`DEFAULTS`].
    Default,
    /// Given by the consumer.
    Configured,
    /// The source a P2P connection names for a value it was given (`tasty-net-p2p`'s own,
    /// `server` included).
    P2p(tasty_net_p2p::settings::Source),
}

impl SettingSource {
    /// `default` or `configured`, or the name of the P2P source.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Configured => "configured",
            Self::P2p(source) => source.name(),
        }
    }
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One setting as `settings()` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    /// The key of the design table.
    pub key: &'static str,
    /// The value in use.
    pub value: SettingValue,
    /// Where the value came from.
    pub source: SettingSource,
}

/// One row of [`DEFAULTS`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DefaultSetting {
    /// The key.
    pub key: &'static str,
    /// The default value.
    pub value: SettingValue,
}

/// The defaults of the connection table and the redialer, the rows of the design table in
/// its order. The keys are written out here (not as the key constants) so that the
/// `no-hidden-constants` check can compare them with the design table; the
/// `settings_list_matches_query` test keeps them equal to the constants.
pub const DEFAULTS: &[DefaultSetting] = &[
    entry("connections.history_limit", SettingValue::Count(32)),
    entry("connections.keep_closed", SettingValue::Count(16)),
    entry("connections.event_queue_limit", SettingValue::Count(256)),
    entry(
        "redial.initial",
        SettingValue::Duration(Duration::from_millis(500)),
    ),
    entry("redial.multiplier", SettingValue::Ratio(2.0)),
    entry(
        "redial.max_delay",
        SettingValue::Duration(Duration::from_secs(30)),
    ),
    entry("redial.jitter", SettingValue::Ratio(0.2)),
    entry(
        "redial.give_up_after",
        SettingValue::Duration(Duration::from_secs(300)),
    ),
    entry("redial.max_attempts", SettingValue::Count(0)),
];

/// One row of [`DEFAULTS`].
const fn entry(key: &'static str, value: SettingValue) -> DefaultSetting {
    DefaultSetting { key, value }
}

/// The default of `key`. Every key this crate reads is in [`DEFAULTS`] with the type its
/// reader asks for (the `defaults_*` tests read every one).
fn default_of(key: &str) -> SettingValue {
    match DEFAULTS.iter().find(|row| row.key == key) {
        Some(row) => row.value,
        None => panic!("{key} is not in settings::DEFAULTS"),
    }
}

pub(crate) fn default_count(key: &str) -> u64 {
    match default_of(key) {
        SettingValue::Count(n) => n,
        other => panic!("the default of {key} is {other:?}, not a count"),
    }
}

pub(crate) fn default_duration(key: &str) -> Duration {
    match default_of(key) {
        SettingValue::Duration(d) => d,
        other => panic!("the default of {key} is {other:?}, not a time"),
    }
}

pub(crate) fn default_ratio(key: &str) -> f64 {
    match default_of(key) {
        SettingValue::Ratio(r) => r,
        other => panic!("the default of {key} is {other:?}, not a ratio"),
    }
}

/// The setting `key` with `configured` if given, else its default.
pub(crate) fn shown(key: &'static str, configured: Option<SettingValue>) -> Setting {
    match configured {
        Some(value) => Setting {
            key,
            value,
            source: SettingSource::Configured,
        },
        None => Setting {
            key,
            value: default_of(key),
            source: SettingSource::Default,
        },
    }
}

/// The timeouts of a link with their sources: a value that differs from
/// [`LinkTimeouts::default`](crate::LinkTimeouts) is `configured`. (The link timeouts have
/// no table of their own yet: their defaults are `LinkTimeouts::default`.)
pub(crate) fn link_timeouts(timeouts: &crate::LinkTimeouts) -> Vec<Setting> {
    let default = crate::LinkTimeouts::default();
    [
        (
            LINK_READ_TIMEOUT,
            timeouts.read_timeout,
            default.read_timeout,
        ),
        (
            LINK_WRITE_TIMEOUT,
            timeouts.write_timeout,
            default.write_timeout,
        ),
        (
            LINK_HANDSHAKE_TIMEOUT,
            timeouts.handshake_timeout,
            default.handshake_timeout,
        ),
        (
            LINK_ROLE_PROPOSAL_TIMEOUT,
            timeouts.role_proposal_timeout,
            default.role_proposal_timeout,
        ),
        (LINK_DRAIN_CAP, timeouts.drain_cap, default.drain_cap),
    ]
    .into_iter()
    .map(|(key, value, default)| Setting {
        key,
        value: SettingValue::Duration(value),
        source: if value == default {
            SettingSource::Default
        } else {
            SettingSource::Configured
        },
    })
    .collect()
}

/// A count as a `usize` bound; a count past `usize` is the largest bound.
pub(crate) fn as_bound(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// A setting that cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidSetting {
    /// The key.
    pub key: &'static str,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for InvalidSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.reason)
    }
}

impl std::error::Error for InvalidSetting {}

/// The smallest multiplier: the waits never shrink.
pub(crate) const MIN_MULTIPLIER: f64 = 1.0;
/// The jitter is a fraction of the wait.
pub(crate) const MAX_JITTER: f64 = 1.0;

/// Checks the redial values: a finite multiplier of at least 1, a jitter in [0, 1], and
/// times (`redial.initial`, `redial.max_delay`, `redial.give_up_after`) that can be added to
/// the present instant.
pub(crate) fn check_redial(
    multiplier: f64,
    jitter: f64,
    times: [(&'static str, Duration); 3],
) -> Result<(), InvalidSetting> {
    let now = Instant::now();
    if let Some((key, _)) = times.iter().find(|(_, d)| now.checked_add(*d).is_none()) {
        return Err(InvalidSetting {
            key,
            reason: "is too long to count from now",
        });
    }
    if !(multiplier.is_finite() && multiplier >= MIN_MULTIPLIER) {
        return Err(InvalidSetting {
            key: REDIAL_MULTIPLIER,
            reason: "must be a finite number of at least 1",
        });
    }
    if !(jitter.is_finite() && (0.0..=MAX_JITTER).contains(&jitter)) {
        return Err(InvalidSetting {
            key: REDIAL_JITTER,
            reason: "must be between 0 and 1",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_print_as_the_design_tables_write_them() {
        assert_eq!(SettingValue::Count(32).to_string(), "32");
        assert_eq!(
            SettingValue::Duration(Duration::from_millis(500)).to_string(),
            "500 ms"
        );
        assert_eq!(
            SettingValue::Duration(Duration::from_secs(30)).to_string(),
            "30 s"
        );
        assert_eq!(SettingValue::Ratio(0.2).to_string(), "0.2");
        assert_eq!(SettingValue::Ratio(2.0).to_string(), "2");
    }
}
