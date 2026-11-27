//! Settings of this crate (docs/design/transport.md "SSH 크레이트 설정값", docs/design/crates.md
//! "설정값 규칙").
//!
//! [`DEFAULTS`] is the one source of every default: [`SshSettings`] gives the values in effect
//! from it, and the tests compare it with the table of the design document. Every time, count,
//! interval and limit of the crate is here; the rest of the crate takes them from the
//! [`SshSettings`] of its [`crate::SshLauncher`] ([`crate::SshLauncher::with_settings`]).
//!
//! The ssh options this crate passes come from three of them. Which value ssh then uses for a
//! target, the setting's or the target's own `extra_options`, is
//! [`crate::SshLauncher::applied_options`].

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Default of `ssh.connect_timeout`: the ssh `ConnectTimeout`. It bounds connection setup, not
/// keepalive after it. A `ConnectTimeout` in the target's extra options comes first and wins.
pub const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default of `discovery.step_timeout`: the bound of one discovery step. The spawn hook, the wait
/// for the child, and output collection share it. When it expires the child is killed and
/// reaped. Spawning itself and the reap after a kill have no bound of their own.
pub const PORT_DISCOVERY_STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// Default of `discovery.total_timeout`: the time budget the steps of one discovery call share.
/// Each step gets the smaller of the step bound and what is left. When the budget is used up no
/// new child is started. Spawning itself and the reap after a kill are not guaranteed to fit in
/// it.
pub const PORT_DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(45);

/// Where a value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The default in [`DEFAULTS`].
    Default,
    /// Given with [`SshSettings::set`].
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
    /// A time in whole seconds (it becomes an ssh option, which takes seconds).
    Seconds,
    /// A count.
    Count,
    /// A size in bytes.
    Bytes,
}

/// A setting value. A [`Kind::Seconds`] setting takes a `Duration` of whole seconds.
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
const MILLIS_PER_MIN: u128 = 60 * MILLIS_PER_SEC;
const MILLIS_PER_HOUR: u128 = 60 * MILLIS_PER_MIN;

impl Value {
    fn fits(self, kind: Kind) -> bool {
        match (self, kind) {
            (Value::Duration(_), Kind::Duration) | (Value::Count(_), Kind::Count) => true,
            (Value::Duration(d), Kind::Seconds) => d.subsec_nanos() == 0,
            (Value::Bytes(_), Kind::Bytes) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    /// The form the design table writes: `50 ms`, `10 s`, `1 min`, `1 h`, `3`, `64 KiB`. A time
    /// that is not a whole number of milliseconds is written in the `Debug` form of `Duration`.
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

impl Setting {
    /// The range in the form of the design table: `1 s ~ 1 h`.
    pub fn range_text(&self) -> String {
        format!("{} ~ {}", self.min, self.max)
    }
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

const fn secs(n: u64) -> Value {
    Value::Duration(Duration::from_secs(n))
}

const fn time(d: Duration) -> Value {
    Value::Duration(d)
}

pub(crate) const CONNECT_TIMEOUT: &str = "ssh.connect_timeout";
pub(crate) const SERVER_ALIVE_INTERVAL: &str = "ssh.server_alive_interval";
pub(crate) const SERVER_ALIVE_COUNT_MAX: &str = "ssh.server_alive_count_max";
const STEP_TIMEOUT: &str = "discovery.step_timeout";
const TOTAL_TIMEOUT: &str = "discovery.total_timeout";
const OUTPUT_LIMIT: &str = "discovery.output_limit";
const STDERR_LIMIT: &str = "discovery.stderr_limit";
const STDERR_DETAIL_LIMIT: &str = "discovery.stderr_detail_limit";
const OVERFLOW_EXIT_GRACE: &str = "discovery.overflow_exit_grace";
const STDERR_GRACE: &str = "discovery.stderr_grace";
const POLL_MIN: &str = "discovery.poll_min";
const POLL_MAX: &str = "discovery.poll_max";
const READY_TIMEOUT: &str = "tunnel.ready_timeout";
const READY_POLL: &str = "tunnel.ready_poll";
const STDERR_DRAIN_LIMIT: &str = "tunnel.stderr_drain_limit";
const STDERR_DRAIN_POLL: &str = "tunnel.stderr_drain_poll";
const STDERR_MAX_LINE: &str = "tunnel.stderr_max_line";
const VERDICT_GRACE: &str = "tunnel.verdict_grace";
const VERDICT_POLL: &str = "tunnel.verdict_poll";
const BACKOFF_INITIAL: &str = "backoff.initial";
const BACKOFF_MULTIPLIER: &str = "backoff.multiplier";
const BACKOFF_MAX_DELAY: &str = "backoff.max_delay";

/// Every setting with its default and range, in the order of the design table.
pub static DEFAULTS: &[Setting] = &[
    entry(
        "ssh.connect_timeout",
        Kind::Seconds,
        time(SSH_CONNECT_TIMEOUT),
        secs(1),
        secs(3600),
    ),
    entry(
        "ssh.server_alive_interval",
        Kind::Seconds,
        secs(15),
        secs(1),
        secs(3600),
    ),
    entry(
        "ssh.server_alive_count_max",
        Kind::Count,
        Value::Count(3),
        Value::Count(1),
        Value::Count(100),
    ),
    entry(
        "discovery.step_timeout",
        Kind::Duration,
        time(PORT_DISCOVERY_STEP_TIMEOUT),
        secs(1),
        secs(3600),
    ),
    entry(
        "discovery.total_timeout",
        Kind::Duration,
        time(PORT_DISCOVERY_TOTAL_TIMEOUT),
        secs(1),
        secs(3600),
    ),
    entry(
        "discovery.output_limit",
        Kind::Bytes,
        Value::Bytes(64 * KIB),
        Value::Bytes(KIB),
        Value::Bytes(MIB),
    ),
    entry(
        "discovery.stderr_limit",
        Kind::Bytes,
        Value::Bytes(16 * KIB),
        Value::Bytes(KIB),
        Value::Bytes(MIB),
    ),
    entry(
        "discovery.stderr_detail_limit",
        Kind::Count,
        Value::Count(512),
        Value::Count(64),
        Value::Count(65_536),
    ),
    entry(
        "discovery.overflow_exit_grace",
        Kind::Duration,
        millis(100),
        millis(0),
        secs(10),
    ),
    entry(
        "discovery.stderr_grace",
        Kind::Duration,
        millis(500),
        millis(0),
        secs(10),
    ),
    entry(
        "discovery.poll_min",
        Kind::Duration,
        millis(5),
        millis(1),
        secs(1),
    ),
    entry(
        "discovery.poll_max",
        Kind::Duration,
        millis(50),
        millis(1),
        secs(1),
    ),
    entry(
        "tunnel.ready_timeout",
        Kind::Duration,
        secs(5),
        millis(100),
        secs(600),
    ),
    entry(
        "tunnel.ready_poll",
        Kind::Duration,
        millis(50),
        millis(1),
        secs(1),
    ),
    entry(
        "tunnel.stderr_drain_limit",
        Kind::Duration,
        millis(500),
        millis(0),
        secs(10),
    ),
    entry(
        "tunnel.stderr_drain_poll",
        Kind::Duration,
        millis(10),
        millis(1),
        secs(1),
    ),
    entry(
        "tunnel.stderr_max_line",
        Kind::Bytes,
        Value::Bytes(KIB),
        Value::Bytes(256),
        Value::Bytes(64 * KIB),
    ),
    entry(
        "tunnel.verdict_grace",
        Kind::Duration,
        millis(500),
        millis(0),
        secs(10),
    ),
    entry(
        "tunnel.verdict_poll",
        Kind::Duration,
        millis(20),
        millis(1),
        secs(1),
    ),
    entry(
        "backoff.initial",
        Kind::Duration,
        millis(500),
        millis(1),
        secs(60),
    ),
    entry(
        "backoff.multiplier",
        Kind::Count,
        Value::Count(2),
        Value::Count(1),
        Value::Count(10),
    ),
    entry(
        "backoff.max_delay",
        Kind::Duration,
        secs(30),
        millis(1),
        secs(3600),
    ),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// A value [`SshSettings::set`] refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingsError {
    /// The key is not a setting.
    UnknownKey(String),
    /// The value does not have the type of the setting (a [`Kind::Seconds`] setting also refuses
    /// a time that is not whole seconds).
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

/// The settings a [`crate::SshLauncher`] runs with: its ssh options, port discovery, tunnels and
/// [`crate::Backoff::from_settings`]. Keys that are not given take their default.
/// [`crate::SshLauncher::settings`] shows the values in effect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshSettings {
    configured: BTreeMap<&'static str, Value>,
}

impl SshSettings {
    /// No configured values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one value. The key must be a setting, and the value must have its type and lie in
    /// its range.
    pub fn set(&mut self, key: &str, value: Value) -> Result<&mut Self, SettingsError> {
        let s = setting(key).ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        if !value.fits(s.kind) {
            return Err(SettingsError::WrongType {
                key: s.key,
                given: value,
            });
        }
        if value < s.min || value > s.max {
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

    pub(crate) fn source(&self, key: &'static str) -> Source {
        match self.get(key) {
            Some(e) => e.source,
            None => panic!("{key} is a setting of DEFAULTS"),
        }
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

    fn bytes(&self, key: &'static str) -> usize {
        match self.get(key).map(|e| e.value) {
            // Every size range ends at 1 MiB or below, which fits usize on every platform.
            Some(Value::Bytes(b)) => usize::try_from(b).unwrap_or(usize::MAX),
            other => panic!("{key} is a size of DEFAULTS, got {other:?}"),
        }
    }

    /// `ssh.connect_timeout`: the ssh `ConnectTimeout` this crate passes last, so a target's own
    /// `ConnectTimeout` wins. Port discovery reports a connection failure that took at least the
    /// value in effect as a timeout.
    pub fn connect_timeout(&self) -> Duration {
        self.duration(CONNECT_TIMEOUT)
    }

    /// `ssh.server_alive_interval`: the ssh `ServerAliveInterval`, passed before the target's
    /// options, so it wins over them.
    pub fn server_alive_interval(&self) -> Duration {
        self.duration(SERVER_ALIVE_INTERVAL)
    }

    /// `ssh.server_alive_count_max`: the ssh `ServerAliveCountMax`, passed before the target's
    /// options, so it wins over them.
    pub fn server_alive_count_max(&self) -> u64 {
        self.count(SERVER_ALIVE_COUNT_MAX)
    }

    /// `discovery.step_timeout`: the bound of one discovery step.
    pub fn step_timeout(&self) -> Duration {
        self.duration(STEP_TIMEOUT)
    }

    /// `discovery.total_timeout`: the budget the steps of one discovery call share.
    pub fn total_timeout(&self) -> Duration {
        self.duration(TOTAL_TIMEOUT)
    }

    /// `discovery.output_limit`: a discovery command's stdout beyond this is refused.
    pub fn output_limit(&self) -> usize {
        self.bytes(OUTPUT_LIMIT)
    }

    /// `discovery.stderr_limit`: the part of a discovery command's stderr kept for the detail.
    pub fn stderr_limit(&self) -> usize {
        self.bytes(STDERR_LIMIT)
    }

    /// `discovery.stderr_detail_limit`: the most characters of stderr a failure detail keeps.
    pub fn stderr_detail_limit(&self) -> usize {
        // The range (at most 65536) fits usize on every platform.
        usize::try_from(self.count(STDERR_DETAIL_LIMIT)).unwrap_or(usize::MAX)
    }

    /// `discovery.overflow_exit_grace`: how long a child whose stdout passed the limit may take
    /// to exit on its own before it is killed.
    pub fn overflow_exit_grace(&self) -> Duration {
        self.duration(OVERFLOW_EXIT_GRACE)
    }

    /// `discovery.stderr_grace`: how long a failed command's stderr is waited for after it
    /// exited.
    pub fn stderr_grace(&self) -> Duration {
        self.duration(STDERR_GRACE)
    }

    /// `discovery.poll_min`: the first interval of the poll of a discovery child; it doubles up
    /// to `discovery.poll_max`.
    pub fn poll_min(&self) -> Duration {
        self.duration(POLL_MIN)
    }

    /// `discovery.poll_max`: the longest interval of the poll of a discovery child and of the
    /// wait for its output.
    pub fn poll_max(&self) -> Duration {
        self.duration(POLL_MAX)
    }

    /// `tunnel.ready_timeout`: how long a tunnel waits for its local end to accept connections.
    pub fn ready_timeout(&self) -> Duration {
        self.duration(READY_TIMEOUT)
    }

    /// `tunnel.ready_poll`: the interval of that check.
    pub fn ready_poll(&self) -> Duration {
        self.duration(READY_POLL)
    }

    /// `tunnel.stderr_drain_limit`: how long an ssh that exited before ready may take to pass
    /// its last stderr on.
    pub fn stderr_drain_limit(&self) -> Duration {
        self.duration(STDERR_DRAIN_LIMIT)
    }

    /// `tunnel.stderr_drain_poll`: the interval of that wait.
    pub fn stderr_drain_poll(&self) -> Duration {
        self.duration(STDERR_DRAIN_POLL)
    }

    /// `tunnel.stderr_max_line`: a tunnel stderr line longer than this loses its tail before it
    /// is judged.
    pub fn stderr_max_line(&self) -> usize {
        self.bytes(STDERR_MAX_LINE)
    }

    /// `tunnel.verdict_grace`: how long a listener verdict waits after a failure for the ssh
    /// refusal line.
    pub fn verdict_grace(&self) -> Duration {
        self.duration(VERDICT_GRACE)
    }

    /// `tunnel.verdict_poll`: the interval of that wait.
    pub fn verdict_poll(&self) -> Duration {
        self.duration(VERDICT_POLL)
    }

    /// `backoff.initial`: the first wait of a [`crate::Backoff`] and the one it resets to.
    pub fn backoff_initial(&self) -> Duration {
        self.duration(BACKOFF_INITIAL)
    }

    /// `backoff.multiplier`: what each step of a [`crate::Backoff`] multiplies its wait by.
    pub fn backoff_multiplier(&self) -> u32 {
        // The range (at most 10) fits u32.
        u32::try_from(self.count(BACKOFF_MULTIPLIER)).unwrap_or(u32::MAX)
    }

    /// `backoff.max_delay`: the longest wait of a [`crate::Backoff`].
    pub fn backoff_max_delay(&self) -> Duration {
        self.duration(BACKOFF_MAX_DELAY)
    }
}

#[cfg(test)]
mod tests;
