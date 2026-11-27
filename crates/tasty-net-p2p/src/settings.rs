//! Device settings (docs/design/p2p.md "설정값", docs/design/crates.md "설정값 규칙").
//!
//! [`DEFAULTS`] is the one source of every default: [`P2pSettings::entries`], the values the
//! endpoint runs with ([`P2pParams`]) and the tests that compare it with the tables of
//! docs/design/p2p.md all come from it. Every time, count, interval, timeout and limit of this
//! crate is here; the rest of the crate takes them from [`P2pParams`]. Fixed values are listed
//! too (source `fixed`) but have no key the settings may set.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use tasty_net_rendezvous::settings::QuicParams;

/// Where a value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The default in [`DEFAULTS`].
    Default,
    /// Given in [`P2pSettings`].
    Configured,
    /// Read from the OS (the socket buffers when they are not configured).
    OsDefault,
    /// Not settable (docs/design/p2p.md "고정값").
    Fixed,
    /// Told by the rendezvous server (`welcome.limits`, `punch_go`). Only connection
    /// information shows it.
    Server,
}

impl Source {
    /// `default`, `configured`, `os_default`, `fixed` or `server`.
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Configured => "configured",
            Source::OsDefault => "os_default",
            Source::Fixed => "fixed",
            Source::Server => "server",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The word an absent optional value is written with in the design tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absent {
    /// "없음": there is no such value.
    None,
    /// "끔": the feature is off.
    Off,
}

/// The type of a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A time.
    Duration,
    /// A time or nothing.
    OptionalDuration,
    /// A count. For a cap, 0 may mean "no cap" where the design says so.
    Count,
    /// A size in bytes.
    Bytes,
    /// A size in bytes or nothing.
    OptionalBytes(Absent),
    /// A size in bytes, by default the OS value.
    OsBytes,
    /// A ratio `num/den`.
    Ratio,
    /// A decimal factor.
    Factor,
    /// On or off.
    Switch,
    /// Not settable.
    Fixed,
}

/// A setting value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// A time.
    Duration(Duration),
    /// A count.
    Count(u64),
    /// A size in bytes.
    Bytes(u64),
    /// A ratio.
    Ratio(u32, u32),
    /// A decimal factor.
    Factor(f64),
    /// On or off.
    Switch(bool),
    /// An optional value that is not there.
    Absent(Absent),
    /// The OS value, not known (no OS socket, as with injected packet I/O).
    Os,
    /// A fixed value: English text and the design table's text.
    Fixed(&'static str, &'static str),
}

const KIB: u64 = 1024;
const MIB: u64 = KIB * KIB;
const SECS_PER_DAY: u64 = 86_400;
const MILLIS_PER_SEC: u128 = 1000;
/// The largest QUIC variable-length integer, 2^62 − 1 (RFC 9000 §16).
const VAR_INT_MAX: u64 = (1 << 62) - 1;

fn write_duration(f: &mut fmt::Formatter<'_>, d: Duration, day_word: &str) -> fmt::Result {
    let ms = d.as_millis();
    if !ms.is_multiple_of(MILLIS_PER_SEC) {
        return write!(f, "{ms} ms");
    }
    let secs = d.as_secs();
    if secs >= SECS_PER_DAY && secs.is_multiple_of(SECS_PER_DAY) {
        return write!(f, "{}{day_word}", secs / SECS_PER_DAY);
    }
    write!(f, "{secs} s")
}

fn write_bytes(f: &mut fmt::Formatter<'_>, b: u64) -> fmt::Result {
    if b == VAR_INT_MAX {
        write!(f, "2^62 − 1 B")
    } else if b >= MIB && b.is_multiple_of(MIB) {
        write!(f, "{} MiB", b / MIB)
    } else if b >= KIB && b.is_multiple_of(KIB) {
        write!(f, "{} KiB", b / KIB)
    } else {
        write!(f, "{b} B")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Duration(d) => write_duration(f, *d, " d"),
            Value::Count(n) => write!(f, "{n}"),
            Value::Bytes(b) => write_bytes(f, *b),
            Value::Ratio(n, d) => write!(f, "{n}/{d}"),
            Value::Factor(x) => write!(f, "{x}"),
            Value::Switch(true) => f.write_str("on"),
            Value::Switch(false) | Value::Absent(Absent::Off) => f.write_str("off"),
            Value::Absent(Absent::None) => f.write_str("none"),
            Value::Os => f.write_str("OS value"),
            Value::Fixed(text, _) => f.write_str(text),
        }
    }
}

impl Value {
    /// The value in the form the design tables write it.
    pub fn doc_form(&self) -> String {
        struct Doc<'a>(&'a Value);
        impl fmt::Display for Doc<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    Value::Duration(d) => write_duration(f, *d, "일"),
                    Value::Switch(true) => f.write_str("켬"),
                    Value::Switch(false) | Value::Absent(Absent::Off) => f.write_str("끔"),
                    Value::Absent(Absent::None) => f.write_str("없음"),
                    Value::Os => f.write_str("OS 값"),
                    Value::Fixed(_, doc) => f.write_str(doc),
                    other => write!(f, "{other}"),
                }
            }
        }
        Doc(self).to_string()
    }
}

/// One setting: key, type and default.
#[derive(Clone, Copy, Debug)]
pub struct Setting {
    /// The key.
    pub key: &'static str,
    /// The type.
    pub kind: Kind,
    /// The default (for a fixed setting, its value).
    pub default: Value,
}

const fn entry(key: &'static str, kind: Kind, default: Value) -> Setting {
    Setting { key, kind, default }
}

const fn secs(n: u64) -> Value {
    Value::Duration(Duration::from_secs(n))
}

const fn millis(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

const fn fixed(key: &'static str, text: &'static str, doc: &'static str) -> Setting {
    entry(key, Kind::Fixed, Value::Fixed(text, doc))
}

/// Every device setting with its default, in the order of the design tables of
/// docs/design/p2p.md: "이 레포가 정하는 값", "의존성이 노출하는 값", "고정값".
pub static DEFAULTS: &[Setting] = &[
    // docs/design/p2p.md "이 레포가 정하는 값".
    entry("rendezvous.connect_timeout", Kind::Duration, secs(5)),
    entry("rendezvous.hello_timeout", Kind::Duration, secs(5)),
    entry(
        "rendezvous.max_message_bytes",
        Kind::Bytes,
        Value::Bytes(64 * KIB),
    ),
    entry("rendezvous.frame_timeout", Kind::Duration, secs(5)),
    entry("rendezvous.keepalive_interval", Kind::Duration, secs(15)),
    entry("rendezvous.idle_timeout", Kind::Duration, secs(45)),
    entry("rendezvous.reconnect.initial", Kind::Duration, secs(1)),
    entry(
        "rendezvous.reconnect.multiplier",
        Kind::Factor,
        Value::Factor(2.0),
    ),
    entry("rendezvous.reconnect.max_delay", Kind::Duration, secs(60)),
    entry(
        "rendezvous.reconnect.jitter",
        Kind::Factor,
        Value::Factor(0.2),
    ),
    entry("rendezvous.reconnect.reset_after", Kind::Duration, secs(60)),
    entry(
        "rendezvous.reconnect.max_attempts",
        Kind::Count,
        Value::Count(0),
    ),
    entry("nat.stun_rto", Kind::Duration, millis(500)),
    entry("nat.stun_attempts", Kind::Count, Value::Count(3)),
    entry("punch.request_timeout", Kind::Duration, secs(3)),
    entry("punch.go_timeout", Kind::Duration, secs(3)),
    entry("punch.probe_interval", Kind::Duration, millis(50)),
    entry("punch.window_max", Kind::Duration, secs(30)),
    entry("quic.handshake_timeout_max", Kind::Duration, secs(30)),
    entry("punch.max_candidates", Kind::Count, Value::Count(8)),
    entry("punch.attempts", Kind::Count, Value::Count(2)),
    entry("punch.retry_delay", Kind::Duration, secs(1)),
    entry("quic.keepalive_interval", Kind::Duration, secs(15)),
    entry("quic.idle_timeout", Kind::Duration, secs(45)),
    entry("quic.max_bidi_streams", Kind::Count, Value::Count(64)),
    entry("quic.max_uni_streams", Kind::Count, Value::Count(0)),
    entry("quic.allow_migration", Kind::Switch, Value::Switch(true)),
    entry("path.poll_interval", Kind::Duration, secs(1)),
    entry("history.limit", Kind::Count, Value::Count(32)),
    entry("events.queue_limit", Kind::Count, Value::Count(256)),
    entry("runtime.worker_threads", Kind::Count, Value::Count(2)),
    entry("runtime.shutdown_timeout", Kind::Duration, secs(5)),
    entry("socket.recv_buffer", Kind::OsBytes, Value::Os),
    entry("socket.send_buffer", Kind::OsBytes, Value::Os),
    // docs/design/p2p.md "의존성이 노출하는 값".
    entry("quic.initial_rtt", Kind::Duration, millis(333)),
    entry("quic.packet_threshold", Kind::Count, Value::Count(3)),
    entry("quic.time_threshold", Kind::Ratio, Value::Ratio(9, 8)),
    entry(
        "quic.persistent_congestion_threshold",
        Kind::Count,
        Value::Count(3),
    ),
    entry(
        "quic.stream_receive_window",
        Kind::Bytes,
        Value::Bytes(1_250_000),
    ),
    entry(
        "quic.receive_window",
        Kind::Bytes,
        Value::Bytes(VAR_INT_MAX),
    ),
    entry("quic.send_window", Kind::Bytes, Value::Bytes(10_000_000)),
    entry("quic.initial_mtu", Kind::Bytes, Value::Bytes(1200)),
    entry("quic.min_mtu", Kind::Bytes, Value::Bytes(1200)),
    entry("quic.mtu_discovery.interval", Kind::Duration, secs(600)),
    entry(
        "quic.mtu_discovery.upper_bound",
        Kind::Bytes,
        Value::Bytes(1452),
    ),
    entry(
        "quic.mtu_discovery.black_hole_cooldown",
        Kind::Duration,
        secs(60),
    ),
    entry(
        "quic.mtu_discovery.minimum_change",
        Kind::Bytes,
        Value::Bytes(20),
    ),
    entry(
        "quic.crypto_buffer_size",
        Kind::Bytes,
        Value::Bytes(16 * KIB),
    ),
    entry("quic.ack_frequency", Kind::Switch, Value::Switch(false)),
    entry(
        "quic.ack_frequency.ack_eliciting_threshold",
        Kind::Count,
        Value::Count(1),
    ),
    entry(
        "quic.ack_frequency.max_ack_delay",
        Kind::OptionalDuration,
        Value::Absent(Absent::None),
    ),
    entry(
        "quic.ack_frequency.reordering_threshold",
        Kind::Count,
        Value::Count(2),
    ),
    entry(
        "quic.datagram_receive_buffer_size",
        Kind::OptionalBytes(Absent::Off),
        Value::Absent(Absent::Off),
    ),
    entry(
        "quic.datagram_send_buffer_size",
        Kind::Bytes,
        Value::Bytes(MIB),
    ),
    entry(
        "quic.cubic.initial_window",
        Kind::Bytes,
        Value::Bytes(12_000),
    ),
    entry(
        "quic.cid_lifetime",
        Kind::OptionalDuration,
        Value::Absent(Absent::None),
    ),
    entry("quic.max_udp_payload_size", Kind::Bytes, Value::Bytes(1472)),
    entry("quic.min_reset_interval", Kind::Duration, millis(20)),
    entry("quic.retry_token_lifetime", Kind::Duration, secs(15)),
    entry(
        "quic.validation_token.lifetime",
        Kind::Duration,
        secs(14 * SECS_PER_DAY),
    ),
    entry("quic.validation_token.sent", Kind::Count, Value::Count(0)),
    entry("quic.max_incoming", Kind::Count, Value::Count(65_536)),
    entry(
        "quic.incoming_buffer_size",
        Kind::Bytes,
        Value::Bytes(10 * MIB),
    ),
    entry(
        "quic.incoming_buffer_size_total",
        Kind::Bytes,
        Value::Bytes(100 * MIB),
    ),
    entry(
        "tls.max_fragment_size",
        Kind::OptionalBytes(Absent::None),
        Value::Absent(Absent::None),
    ),
    // docs/design/p2p.md "고정값".
    fixed("quic.grease_quic_bit", "off", "끔"),
    fixed("quic.supported_versions", "QUIC v1 only", "QUIC v1 하나"),
    fixed("quic.preferred_address", "none", "없음"),
    fixed("quic.congestion_controller", "CUBIC", "CUBIC"),
    fixed("tls.versions", "TLS 1.3 only", "TLS 1.3만"),
    fixed("tls.provider", "ring", "ring"),
    fixed("tls.resumption", "off", "끔"),
    fixed("tls.tickets", "0", "0"),
    fixed("tls.early_data", "off", "끔"),
    fixed(
        "tls.alpn",
        "tasty-net/2, tasty-rendezvous/1",
        "tasty-net/2, tasty-rendezvous/1",
    ),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// A settings problem. [`crate::P2pEndpoint::bind`] refuses the settings with it and opens no
/// socket.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsError {
    /// The key is not a setting, or it is a fixed value.
    UnknownKey(String),
    /// The value does not have the type of the setting.
    WrongType {
        /// The key.
        key: String,
        /// What was given.
        given: String,
    },
    /// The value is outside the accepted range.
    OutOfRange {
        /// The key.
        key: &'static str,
        /// The value.
        value: Value,
        /// The accepted range.
        expected: &'static str,
    },
    /// Two values break a required relation.
    Relation {
        /// The relation.
        rule: &'static str,
        /// The keys and values involved.
        values: Vec<(&'static str, Value)>,
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
                expected,
            } => write!(f, "setting {key} = {value} is out of range ({expected})"),
            SettingsError::Relation { rule, values } => {
                write!(f, "settings break the relation {rule}:")?;
                for (k, v) in values {
                    write!(f, " {k} = {v}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SettingsError {}

/// One line of the settings query.
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// Configured device settings (`P2pConfig.settings`). Keys that are not given take their
/// default. [`P2pSettings::set`] only records; the keys, types, ranges and relations are
/// checked by [`P2pSettings::resolve`], which `bind` calls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct P2pSettings {
    given: Vec<(String, Value)>,
}

/// The OS values of the socket buffers, read after the socket is set up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OsValues {
    pub(crate) recv_buffer: Option<u64>,
    pub(crate) send_buffer: Option<u64>,
}

impl P2pSettings {
    /// No configured values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Give one value. A later value for the same key replaces an earlier one.
    pub fn set(&mut self, key: &str, value: Value) -> &mut Self {
        self.given.retain(|(k, _)| k != key);
        self.given.push((key.to_owned(), value));
        self
    }

    /// The configured values by key, with keys and types checked.
    fn configured(&self) -> Result<BTreeMap<&'static str, Value>, SettingsError> {
        let mut out = BTreeMap::new();
        for (key, value) in &self.given {
            let s = setting(key)
                .filter(|s| s.kind != Kind::Fixed)
                .ok_or_else(|| SettingsError::UnknownKey(key.clone()))?;
            let ok = matches!(
                (s.kind, value),
                (Kind::Duration, Value::Duration(_))
                    | (
                        Kind::OptionalDuration,
                        Value::Duration(_) | Value::Absent(_)
                    )
                    | (Kind::Count, Value::Count(_))
                    | (Kind::Bytes | Kind::OsBytes, Value::Bytes(_))
                    | (Kind::OptionalBytes(_), Value::Bytes(_) | Value::Absent(_))
                    | (Kind::Ratio, Value::Ratio(_, _))
                    | (Kind::Factor, Value::Factor(_))
                    | (Kind::Switch, Value::Switch(_))
            );
            if !ok {
                return Err(SettingsError::WrongType {
                    key: key.clone(),
                    given: value.to_string(),
                });
            }
            let value = match (s.kind, value) {
                (Kind::OptionalBytes(word), Value::Absent(_)) => Value::Absent(word),
                (_, Value::Absent(_)) => Value::Absent(Absent::None),
                (_, v) => *v,
            };
            out.insert(s.key, value);
        }
        Ok(out)
    }

    /// The value in effect for `key` and its source, before the socket exists (the socket
    /// buffers show [`Value::Os`]). Unknown keys and bad values give `None`; [`Self::resolve`]
    /// reports them.
    pub fn get(&self, key: &str) -> Option<SettingEntry> {
        let configured = self.configured().ok()?;
        entry_of(&configured, setting(key)?, OsValues::default())
    }

    /// Every setting with the value in effect and its source, in [`DEFAULTS`] order. Empty when
    /// the settings do not resolve.
    pub fn entries(&self) -> Vec<SettingEntry> {
        self.entries_with(OsValues::default())
    }

    pub(crate) fn entries_with(&self, os: OsValues) -> Vec<SettingEntry> {
        let Ok(configured) = self.configured() else {
            return Vec::new();
        };
        DEFAULTS
            .iter()
            .filter_map(|s| entry_of(&configured, s, os))
            .collect()
    }

    /// Check keys, types, ranges and relations and give the values the endpoint runs with.
    pub fn resolve(&self) -> Result<P2pParams, SettingsError> {
        let configured = self.configured()?;
        Resolver(&configured).params()
    }
}

fn entry_of(
    configured: &BTreeMap<&'static str, Value>,
    s: &'static Setting,
    os: OsValues,
) -> Option<SettingEntry> {
    if let Some(v) = configured.get(s.key) {
        return Some(SettingEntry {
            key: s.key,
            value: *v,
            source: Source::Configured,
        });
    }
    let (value, source) = match s.kind {
        Kind::Fixed => (s.default, Source::Fixed),
        Kind::OsBytes => {
            let read = match s.key {
                "socket.recv_buffer" => os.recv_buffer,
                _ => os.send_buffer,
            };
            (read.map_or(Value::Os, Value::Bytes), Source::OsDefault)
        }
        _ => (s.default, Source::Default),
    };
    Some(SettingEntry {
        key: s.key,
        value,
        source,
    })
}

/// The values the endpoint runs with, checked.
#[derive(Clone, Debug, PartialEq)]
pub struct P2pParams {
    /// `rendezvous.connect_timeout`.
    pub connect_timeout: Duration,
    /// `rendezvous.hello_timeout`.
    pub hello_timeout: Duration,
    /// `rendezvous.max_message_bytes`.
    pub max_message_bytes: u32,
    /// `rendezvous.frame_timeout`.
    pub frame_timeout: Duration,
    /// `rendezvous.keepalive_interval`.
    pub rendezvous_keepalive: Duration,
    /// `rendezvous.idle_timeout`.
    pub rendezvous_idle: Duration,
    /// `rendezvous.reconnect.*`.
    pub reconnect: Backoff,
    /// `nat.stun_rto`.
    pub stun_rto: Duration,
    /// `nat.stun_attempts`.
    pub stun_attempts: u32,
    /// `punch.request_timeout`.
    pub request_timeout: Duration,
    /// `punch.go_timeout`.
    pub go_timeout: Duration,
    /// `punch.probe_interval`.
    pub probe_interval: Duration,
    /// `punch.window_max`.
    pub window_max: Duration,
    /// `quic.handshake_timeout_max`.
    pub handshake_timeout_max: Duration,
    /// `punch.max_candidates`.
    pub max_candidates: usize,
    /// `punch.attempts`.
    pub punch_attempts: u32,
    /// `punch.retry_delay`.
    pub retry_delay: Duration,
    /// `quic.allow_migration`.
    pub allow_migration: bool,
    /// `path.poll_interval`.
    pub path_poll_interval: Duration,
    /// `history.limit`.
    pub history_limit: usize,
    /// `events.queue_limit`.
    pub events_queue_limit: usize,
    /// `runtime.worker_threads`.
    pub worker_threads: usize,
    /// `runtime.shutdown_timeout`.
    pub shutdown_timeout: Duration,
    /// `socket.recv_buffer`, `None` for the OS value.
    pub recv_buffer: Option<usize>,
    /// `socket.send_buffer`, `None` for the OS value.
    pub send_buffer: Option<usize>,
    /// The QUIC values of device-to-device connections (`quic.*`, `tls.*`).
    pub quic: QuicParams,
    /// The QUIC values of the rendezvous connection: [`Self::quic`] with the
    /// `rendezvous.keepalive_interval` and `rendezvous.idle_timeout`.
    pub rendezvous_quic: QuicParams,
}

/// The server reconnect backoff (`rendezvous.reconnect.*`, docs/design/p2p.md "재시도와 포기").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backoff {
    /// `initial`.
    pub initial: Duration,
    /// `multiplier`.
    pub multiplier: f64,
    /// `max_delay`.
    pub max_delay: Duration,
    /// `jitter`.
    pub jitter: f64,
    /// `reset_after`.
    pub reset_after: Duration,
    /// `max_attempts`, 0 for no cap.
    pub max_attempts: u64,
}

impl Backoff {
    /// The wait before attempt `n` (1-based): `min(max_delay, initial × multiplier^(n−1))`
    /// times `1 + u`, cut to `[0, max_delay]`. `u` is the uniform random number in
    /// `[−jitter, +jitter]` the caller drew.
    pub fn delay(&self, n: u64, u: f64) -> Duration {
        let exp = i32::try_from(n.saturating_sub(1)).unwrap_or(i32::MAX);
        let base = self.initial.as_secs_f64() * self.multiplier.powi(exp);
        let max = self.max_delay.as_secs_f64();
        let capped = if base.is_finite() { base.min(max) } else { max };
        let jittered = (capped * (1.0 + u)).clamp(0.0, max);
        Duration::try_from_secs_f64(jittered).unwrap_or(self.max_delay)
    }
}

impl Backoff {
    /// A uniform random number in `[−jitter, +jitter]` from the OS random source (0 when the
    /// source fails).
    pub(crate) fn draw_jitter(&self) -> f64 {
        let mut b = [0u8; 8];
        if getrandom::fill(&mut b).is_err() {
            return 0.0;
        }
        // The top 53 bits as a fraction in [0, 1).
        let unit = (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) * self.jitter
    }
}

/// Bounds `bind` checks (docs/design/p2p.md "잘못된 설정의 거절").
const MIN_MESSAGE_BYTES: u64 = KIB;
const MAX_MESSAGE_BYTES: u64 = MIB;
const MIN_CANDIDATES: u64 = 1;
const MAX_CANDIDATES: u64 = 64;
const MIN_QUIC_MTU: u64 = 1200;
const MAX_UDP_PAYLOAD: u64 = 65_527;
const MIN_TLS_FRAGMENT: u64 = 32;
const MAX_TLS_FRAGMENT: u64 = 16_384 + 5;
/// The longest time a setting takes, 2^32 − 1 s (about 136 years): a deadline of the current
/// time plus such a time is in range on every supported system.
const MAX_DURATION: Duration = Duration::from_secs(u32::MAX as u64);
/// The most streams a QUIC peer may allow, 2^60 (RFC 9000 §4.6).
const MAX_STREAMS: u64 = 1 << 60;
/// At least one of: attempts, STUN sends, history and event entries, runtime workers.
const AT_LEAST_ONE: u64 = 1;
/// The most runtime worker threads.
const MAX_WORKERS: u64 = 1024;
/// The largest count kept in memory (history, events), far under every platform's limit.
const MAX_KEPT: u64 = 1 << 20;
/// The reconnect multiplier is at least 1 (the wait never shrinks).
const MIN_MULTIPLIER: f64 = 1.0;
/// The jitter is in `[0, 1)`: the wait stays positive.
const JITTER_RANGE: std::ops::Range<f64> = 0.0..1.0;
/// A socket buffer size the OS takes (`c_int`).
const MAX_SOCKET_BUFFER: u64 = i32::MAX as u64;

struct Resolver<'a>(&'a BTreeMap<&'static str, Value>);

impl Resolver<'_> {
    fn value(&self, key: &'static str) -> Value {
        let s = setting(key).unwrap_or_else(|| panic!("{key} is in DEFAULTS"));
        self.0.get(key).copied().unwrap_or(s.default)
    }

    fn duration(&self, key: &'static str) -> Duration {
        match self.value(key) {
            Value::Duration(d) => d,
            other => panic!("{key} is a duration, got {other:?}"),
        }
    }

    fn opt_duration(&self, key: &'static str) -> Option<Duration> {
        match self.value(key) {
            Value::Duration(d) => Some(d),
            Value::Absent(_) => None,
            other => panic!("{key} is an optional duration, got {other:?}"),
        }
    }

    fn number(&self, key: &'static str) -> u64 {
        match self.value(key) {
            Value::Count(n) | Value::Bytes(n) => n,
            other => panic!("{key} is a number, got {other:?}"),
        }
    }

    fn opt_bytes(&self, key: &'static str) -> Option<u64> {
        match self.value(key) {
            Value::Bytes(n) => Some(n),
            Value::Absent(_) | Value::Os => None,
            other => panic!("{key} is optional bytes, got {other:?}"),
        }
    }

    fn factor(&self, key: &'static str) -> f64 {
        match self.value(key) {
            Value::Factor(x) => x,
            other => panic!("{key} is a factor, got {other:?}"),
        }
    }

    fn switch(&self, key: &'static str) -> bool {
        match self.value(key) {
            Value::Switch(b) => b,
            other => panic!("{key} is a switch, got {other:?}"),
        }
    }

    fn out_of_range(&self, key: &'static str, expected: &'static str) -> SettingsError {
        SettingsError::OutOfRange {
            key,
            value: self.value(key),
            expected,
        }
    }

    fn range(
        &self,
        key: &'static str,
        min: u64,
        max: u64,
        expected: &'static str,
    ) -> Result<u64, SettingsError> {
        let n = self.number(key);
        if n < min || n > max {
            return Err(self.out_of_range(key, expected));
        }
        Ok(n)
    }

    fn fits<T: TryFrom<u64>>(&self, key: &'static str) -> Result<T, SettingsError> {
        T::try_from(self.number(key))
            .map_err(|_| self.out_of_range(key, "fits the dependency's integer type"))
    }

    fn nonzero(&self, key: &'static str) -> Result<Duration, SettingsError> {
        let d = self.duration(key);
        if d.is_zero() {
            return Err(self.out_of_range(key, "greater than 0"));
        }
        Ok(d)
    }

    fn relation(
        &self,
        ok: bool,
        rule: &'static str,
        keys: &[&'static str],
    ) -> Result<(), SettingsError> {
        if ok {
            return Ok(());
        }
        Err(SettingsError::Relation {
            rule,
            values: keys.iter().map(|k| (*k, self.value(k))).collect(),
        })
    }

    /// Every time is at most [`MAX_DURATION`] and can be added to the current time.
    fn durations(&self) -> Result<(), SettingsError> {
        let now = Instant::now();
        for s in DEFAULTS {
            let d = match (s.kind, self.value(s.key)) {
                (Kind::Duration | Kind::OptionalDuration, Value::Duration(d)) => d,
                _ => continue,
            };
            if d > MAX_DURATION || now.checked_add(d).is_none() {
                return Err(self.out_of_range(s.key, "at most 2^32 - 1 s"));
            }
        }
        Ok(())
    }

    fn params(&self) -> Result<P2pParams, SettingsError> {
        self.durations()?;
        let max_message_bytes = self.range(
            "rendezvous.max_message_bytes",
            MIN_MESSAGE_BYTES,
            MAX_MESSAGE_BYTES,
            "1 KiB to 1 MiB",
        )?;
        let rendezvous_keepalive = self.nonzero("rendezvous.keepalive_interval")?;
        let rendezvous_idle = self.duration("rendezvous.idle_timeout");
        self.relation(
            rendezvous_idle > rendezvous_keepalive,
            "rendezvous.idle_timeout > rendezvous.keepalive_interval",
            &["rendezvous.idle_timeout", "rendezvous.keepalive_interval"],
        )?;
        let quic_keepalive = self.nonzero("quic.keepalive_interval")?;
        let quic_idle = self.duration("quic.idle_timeout");
        self.relation(
            quic_idle > quic_keepalive,
            "quic.idle_timeout > quic.keepalive_interval",
            &["quic.idle_timeout", "quic.keepalive_interval"],
        )?;
        let multiplier = self.factor("rendezvous.reconnect.multiplier");
        if !(multiplier.is_finite() && multiplier >= MIN_MULTIPLIER) {
            return Err(self.out_of_range("rendezvous.reconnect.multiplier", "at least 1"));
        }
        let jitter = self.factor("rendezvous.reconnect.jitter");
        if !JITTER_RANGE.contains(&jitter) {
            return Err(self.out_of_range("rendezvous.reconnect.jitter", "0 to less than 1"));
        }
        let reconnect = Backoff {
            initial: self.duration("rendezvous.reconnect.initial"),
            multiplier,
            max_delay: self.duration("rendezvous.reconnect.max_delay"),
            jitter,
            reset_after: self.duration("rendezvous.reconnect.reset_after"),
            max_attempts: self.number("rendezvous.reconnect.max_attempts"),
        };
        self.relation(
            reconnect.initial <= reconnect.max_delay,
            "rendezvous.reconnect.initial <= rendezvous.reconnect.max_delay",
            &[
                "rendezvous.reconnect.initial",
                "rendezvous.reconnect.max_delay",
            ],
        )?;
        let max_candidates = self.range(
            "punch.max_candidates",
            MIN_CANDIDATES,
            MAX_CANDIDATES,
            "1 to 64",
        )?;
        let at_least_one = |key| self.range(key, AT_LEAST_ONE, u64::from(u32::MAX), "1 or more");
        let stun_attempts = at_least_one("nat.stun_attempts")?;
        let punch_attempts = at_least_one("punch.attempts")?;
        let history_limit = self.range("history.limit", AT_LEAST_ONE, MAX_KEPT, "1 to 2^20")?;
        let events_queue_limit =
            self.range("events.queue_limit", AT_LEAST_ONE, MAX_KEPT, "1 to 2^20")?;
        let worker_threads = self.range(
            "runtime.worker_threads",
            AT_LEAST_ONE,
            MAX_WORKERS,
            "1 to 1024",
        )?;
        let buffer = |key| -> Result<Option<usize>, SettingsError> {
            match self.opt_bytes(key) {
                None => Ok(None),
                Some(n) if (AT_LEAST_ONE..=MAX_SOCKET_BUFFER).contains(&n) => {
                    Ok(Some(usize::try_from(n).map_err(|_| {
                        self.out_of_range(key, "1 B to 2^31 - 1 B")
                    })?))
                }
                Some(_) => Err(self.out_of_range(key, "1 B to 2^31 - 1 B")),
            }
        };
        let quic = self.quic(quic_keepalive, quic_idle)?;
        let mut rendezvous_quic = quic.clone();
        rendezvous_quic.keepalive_interval = rendezvous_keepalive;
        rendezvous_quic.idle_timeout = rendezvous_idle;
        Ok(P2pParams {
            connect_timeout: self.nonzero("rendezvous.connect_timeout")?,
            hello_timeout: self.nonzero("rendezvous.hello_timeout")?,
            max_message_bytes: u32::try_from(max_message_bytes).expect("checked against 1 MiB"),
            frame_timeout: self.nonzero("rendezvous.frame_timeout")?,
            rendezvous_keepalive,
            rendezvous_idle,
            reconnect,
            stun_rto: self.nonzero("nat.stun_rto")?,
            stun_attempts: u32::try_from(stun_attempts).expect("checked against u32"),
            request_timeout: self.nonzero("punch.request_timeout")?,
            go_timeout: self.nonzero("punch.go_timeout")?,
            probe_interval: self.nonzero("punch.probe_interval")?,
            window_max: self.duration("punch.window_max"),
            handshake_timeout_max: self.duration("quic.handshake_timeout_max"),
            max_candidates: usize::try_from(max_candidates).expect("checked against 64"),
            punch_attempts: u32::try_from(punch_attempts).expect("checked against u32"),
            retry_delay: self.duration("punch.retry_delay"),
            allow_migration: self.switch("quic.allow_migration"),
            path_poll_interval: self.nonzero("path.poll_interval")?,
            history_limit: usize::try_from(history_limit).expect("checked against 2^20"),
            events_queue_limit: usize::try_from(events_queue_limit).expect("checked against 2^20"),
            worker_threads: usize::try_from(worker_threads).expect("checked against 1024"),
            shutdown_timeout: self.duration("runtime.shutdown_timeout"),
            recv_buffer: buffer("socket.recv_buffer")?,
            send_buffer: buffer("socket.send_buffer")?,
            quic,
            rendezvous_quic,
        })
    }

    fn quic(
        &self,
        keepalive_interval: Duration,
        idle_timeout: Duration,
    ) -> Result<QuicParams, SettingsError> {
        let initial_mtu = self.range(
            "quic.initial_mtu",
            MIN_QUIC_MTU,
            u64::from(u16::MAX),
            "1200 to 65535",
        )?;
        let min_mtu = self.range(
            "quic.min_mtu",
            MIN_QUIC_MTU,
            u64::from(u16::MAX),
            "1200 to 65535",
        )?;
        self.relation(
            min_mtu <= initial_mtu,
            "quic.min_mtu <= quic.initial_mtu",
            &["quic.min_mtu", "quic.initial_mtu"],
        )?;
        let upper = self.range(
            "quic.mtu_discovery.upper_bound",
            MIN_QUIC_MTU,
            u64::from(u16::MAX),
            "1200 to 65535",
        )?;
        self.relation(
            upper >= initial_mtu,
            "quic.mtu_discovery.upper_bound >= quic.initial_mtu",
            &["quic.mtu_discovery.upper_bound", "quic.initial_mtu"],
        )?;
        self.range("quic.max_bidi_streams", u64::MIN, MAX_STREAMS, "0 to 2^60")?;
        self.range("quic.max_uni_streams", u64::MIN, MAX_STREAMS, "0 to 2^60")?;
        for key in [
            "quic.stream_receive_window",
            "quic.receive_window",
            "quic.ack_frequency.ack_eliciting_threshold",
            "quic.ack_frequency.reordering_threshold",
        ] {
            self.range(key, u64::MIN, VAR_INT_MAX, "0 to 2^62 - 1")?;
        }
        let idle_ms = u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX);
        if idle_ms > VAR_INT_MAX {
            return Err(self.out_of_range("quic.idle_timeout", "at most 2^62 - 1 ms"));
        }
        let (num, den) = match self.value("quic.time_threshold") {
            Value::Ratio(n, d) => (n, d),
            other => panic!("quic.time_threshold is a ratio, got {other:?}"),
        };
        if den == u32::MIN || num == u32::MIN {
            return Err(self.out_of_range("quic.time_threshold", "a positive ratio"));
        }
        let tls_max_fragment_size = match self.opt_bytes("tls.max_fragment_size") {
            None => None,
            Some(n) if (MIN_TLS_FRAGMENT..=MAX_TLS_FRAGMENT).contains(&n) => {
                Some(usize::try_from(n).expect("small"))
            }
            Some(_) => return Err(self.out_of_range("tls.max_fragment_size", "32 to 16389")),
        };
        let datagram_receive_buffer_size = match self.opt_bytes("quic.datagram_receive_buffer_size")
        {
            None => None,
            Some(n) => Some(usize::try_from(n).map_err(|_| {
                self.out_of_range("quic.datagram_receive_buffer_size", "fits usize")
            })?),
        };
        Ok(QuicParams {
            max_bidi_streams: self.number("quic.max_bidi_streams"),
            max_uni_streams: self.number("quic.max_uni_streams"),
            initial_rtt: self.duration("quic.initial_rtt"),
            packet_threshold: self.fits("quic.packet_threshold")?,
            time_threshold: num as f32 / den as f32,
            persistent_congestion_threshold: self.fits("quic.persistent_congestion_threshold")?,
            stream_receive_window: self.number("quic.stream_receive_window"),
            receive_window: self.number("quic.receive_window"),
            send_window: self.number("quic.send_window"),
            initial_mtu: u16::try_from(initial_mtu).expect("checked"),
            min_mtu: u16::try_from(min_mtu).expect("checked"),
            mtu_interval: self.duration("quic.mtu_discovery.interval"),
            mtu_upper_bound: u16::try_from(upper).expect("checked"),
            mtu_black_hole_cooldown: self.duration("quic.mtu_discovery.black_hole_cooldown"),
            mtu_minimum_change: self.fits("quic.mtu_discovery.minimum_change")?,
            crypto_buffer_size: self.fits("quic.crypto_buffer_size")?,
            ack_frequency: self.switch("quic.ack_frequency"),
            ack_eliciting_threshold: self.number("quic.ack_frequency.ack_eliciting_threshold"),
            max_ack_delay: self.opt_duration("quic.ack_frequency.max_ack_delay"),
            reordering_threshold: self.number("quic.ack_frequency.reordering_threshold"),
            datagram_receive_buffer_size,
            datagram_send_buffer_size: self.fits("quic.datagram_send_buffer_size")?,
            cubic_initial_window: self.number("quic.cubic.initial_window"),
            cid_lifetime: self.opt_duration("quic.cid_lifetime"),
            max_udp_payload_size: u16::try_from(self.range(
                "quic.max_udp_payload_size",
                MIN_QUIC_MTU,
                MAX_UDP_PAYLOAD,
                "1200 to 65527",
            )?)
            .expect("checked"),
            min_reset_interval: self.duration("quic.min_reset_interval"),
            retry_token_lifetime: self.duration("quic.retry_token_lifetime"),
            validation_token_lifetime: self.duration("quic.validation_token.lifetime"),
            validation_token_sent: self.fits("quic.validation_token.sent")?,
            max_incoming: self.fits("quic.max_incoming")?,
            incoming_buffer_size: self.number("quic.incoming_buffer_size"),
            incoming_buffer_size_total: self.number("quic.incoming_buffer_size_total"),
            tls_max_fragment_size,
            keepalive_interval,
            idle_timeout,
            tls_tickets: usize::MIN,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tasty_net_identity::DeviceKey;
    use tasty_net_rendezvous::quic;
    use tasty_net_rendezvous::tls::{self, TlsOptions};
    use tasty_net_rendezvous::wire::ALPN_P2P;

    fn manifest_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"))
    }

    fn design(doc: &str) -> String {
        read(manifest_dir().join("../../docs/design").join(doc))
    }

    /// The lines of the section under `heading` (up to the next heading).
    fn section(text: &str, heading: &str) -> Vec<String> {
        text.lines()
            .skip_while(|l| l.trim_start_matches('#').trim() != heading)
            .skip(1)
            .take_while(|l| !l.starts_with('#'))
            .map(str::to_owned)
            .collect()
    }

    /// The rows of the first table under `heading` of p2p.md, as (keys, value text) with the
    /// value from the column named `column`.
    fn table_rows(heading: &str, column: &str) -> Vec<(Vec<String>, String)> {
        let lines = section(&design("p2p.md"), heading);
        assert!(!lines.is_empty(), "p2p.md: no heading {heading}");
        let mut table = lines
            .iter()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'));
        let cells = |l: &str| -> Vec<String> {
            l.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_owned())
                .collect()
        };
        let header = cells(table.next().expect("table header"));
        let col = header
            .iter()
            .position(|h| h == column)
            .unwrap_or_else(|| panic!("{heading}: no column {column} in {header:?}"));
        table.next(); // the |---| line
        table
            .map(|l| {
                let row = cells(l);
                let keys = row[0]
                    .split(',')
                    .map(|k| k.trim().trim_matches('`').to_owned())
                    .collect();
                (keys, row[col].clone())
            })
            .collect()
    }

    /// The comparable form of a design cell: the text before a parenthesis, without backticks
    /// and without the spaces that group digits.
    fn normalize(cell: &str) -> String {
        let head = cell.split('(').next().unwrap_or(cell);
        let head = head.replace('`', "");
        let chars: Vec<char> = head.trim().chars().collect();
        let mut out = String::new();
        for (i, c) in chars.iter().enumerate() {
            let between_digits = *c == ' '
                && i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
            if !between_digits {
                out.push(*c);
            }
        }
        out
    }

    const TABLES: [(&str, &str); 3] = [
        ("이 레포가 정하는 값", "기본"),
        ("의존성이 노출하는 값", "기본"),
        ("고정값 (설정 불가, 조회에는 보임)", "고정값"),
    ];

    fn design_rows() -> Vec<(String, String)> {
        let mut rows = Vec::new();
        for (heading, column) in TABLES {
            for (keys, cell) in table_rows(heading, column) {
                let value = normalize(&cell);
                if keys.len() == 1 {
                    rows.push((keys[0].clone(), value));
                } else {
                    // Several keys in one row: one value for all, or one value each.
                    let values: Vec<String> = cell.split(',').map(normalize).collect();
                    for (i, k) in keys.iter().enumerate() {
                        let v = if values.len() == keys.len() {
                            values[i].clone()
                        } else {
                            value.clone()
                        };
                        rows.push((k.clone(), v));
                    }
                }
            }
        }
        rows
    }

    #[test]
    fn defaults_match_design_tables() {
        let rows = design_rows();
        let doc_keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        let code_keys: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
        assert_eq!(code_keys, doc_keys, "the keys and their order");
        for ((key, doc), s) in rows.iter().zip(DEFAULTS) {
            let code = s.default.doc_form();
            assert_eq!(normalize(&code), *doc, "{key}");
        }
    }

    #[test]
    fn settings_list_matches_query() {
        let mut s = P2pSettings::new();
        s.set("punch.attempts", Value::Count(3))
            .set(
                "quic.initial_rtt",
                Value::Duration(Duration::from_millis(100)),
            )
            // The same value as the default given explicitly is still `configured`.
            .set("history.limit", Value::Count(32))
            .set("socket.recv_buffer", Value::Bytes(MIB));
        let configured = [
            "punch.attempts",
            "quic.initial_rtt",
            "history.limit",
            "socket.recv_buffer",
        ];
        let entries = s.entries();
        let keys: Vec<&str> = entries.iter().map(|e| e.key).collect();
        let listed: Vec<&str> = DEFAULTS.iter().map(|d| d.key).collect();
        assert_eq!(keys, listed);
        for e in &entries {
            let want = match e.key {
                k if configured.contains(&k) => Source::Configured,
                "socket.send_buffer" => Source::OsDefault,
                k if setting(k).unwrap().kind == Kind::Fixed => Source::Fixed,
                _ => Source::Default,
            };
            assert_eq!(e.source, want, "{e}");
            assert_eq!(s.get(e.key), Some(*e));
        }
        // Before a socket exists, an OS value is not known.
        assert_eq!(s.get("socket.send_buffer").unwrap().value, Value::Os);
        // A bound endpoint shows what the OS gave.
        let mut config = crate::P2pConfig::new(
            DeviceKey::from_seed([1; 32]),
            Arc::new(crate::PairingList::new()),
        );
        config.enabled = true;
        config.server = Some(crate::RendezvousServer {
            addr: "127.0.0.1:9".parse().unwrap(),
            public_key: DeviceKey::from_seed([2; 32]).id(),
        });
        config.settings = s.clone();
        let ep = crate::P2pEndpoint::bind(config.clone()).expect("bind");
        let shown = ep.settings();
        assert_eq!(
            shown.iter().map(|e| e.key).collect::<Vec<_>>(),
            listed,
            "the query lists every key"
        );
        let send = shown
            .iter()
            .find(|e| e.key == "socket.send_buffer")
            .unwrap();
        assert_eq!(send.source, Source::OsDefault);
        assert!(matches!(send.value, Value::Bytes(n) if n > 0), "{send}");
        let recv = shown
            .iter()
            .find(|e| e.key == "socket.recv_buffer")
            .unwrap();
        assert_eq!(recv.source, Source::Configured);
        ep.close();
        // Fixed keys and unknown keys are refused by bind.
        for key in ["tls.resumption", "quic.grease_quic_bit", "relay.enabled"] {
            let mut bad = config.clone();
            bad.settings.set(key, Value::Count(0));
            match crate::P2pEndpoint::bind(bad) {
                Err(crate::P2pError::Settings(SettingsError::UnknownKey(k))) => assert_eq!(k, key),
                other => panic!("{key}: {:?}", other.map(|_| ())),
            }
        }
        // A wrong type and a broken relation are refused.
        let mut wrong = P2pSettings::new();
        wrong.set("punch.attempts", Value::Duration(Duration::ZERO));
        assert!(matches!(
            wrong.resolve(),
            Err(SettingsError::WrongType { .. })
        ));
        let mut relation = P2pSettings::new();
        relation.set(
            "quic.idle_timeout",
            Value::Duration(Duration::from_secs(15)),
        );
        assert!(matches!(
            relation.resolve(),
            Err(SettingsError::Relation { .. })
        ));
    }

    /// The value of `name` in a `Debug` string of a struct, without `Some(…)` and `VarInt(…)`.
    fn field(debug: &str, name: &str) -> String {
        let pat = format!("{name}: ");
        let start = debug
            .match_indices(&pat)
            .find(|(i, _)| {
                debug[..*i]
                    .chars()
                    .last()
                    .is_none_or(|c| c == ' ' || c == '{')
            })
            .map(|(i, _)| i + pat.len())
            .unwrap_or_else(|| panic!("no field {name} in {debug}"));
        let mut depth = 0i32;
        let mut end = debug.len();
        for (i, c) in debug[start..].char_indices() {
            match c {
                '(' | '{' | '[' => depth += 1,
                ')' | '}' | ']' if depth == 0 => {
                    end = start + i;
                    break;
                }
                ')' | '}' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    end = start + i;
                    break;
                }
                _ => {}
            }
        }
        unwrap(debug[start..end].trim())
    }

    fn unwrap(v: &str) -> String {
        for wrap in ["Some(", "VarInt("] {
            if let Some(inner) = v.strip_prefix(wrap).and_then(|r| r.strip_suffix(')')) {
                return unwrap(inner);
            }
        }
        v.to_owned()
    }

    /// The `Debug` form a quinn value of `v` has.
    fn quinn_form(v: Value) -> String {
        match v {
            Value::Duration(d) => format!("{d:?}"),
            Value::Count(n) | Value::Bytes(n) => n.to_string(),
            #[allow(clippy::cast_precision_loss)]
            Value::Ratio(n, d) => format!("{:?}", n as f32 / d as f32),
            Value::Absent(_) | Value::Switch(false) => "None".to_owned(),
            other => panic!("no quinn form for {other:?}"),
        }
    }

    fn default_of(key: &str) -> Value {
        setting(key).unwrap().default
    }

    fn crypto() -> quinn::crypto::rustls::QuicServerConfig {
        let tls = tls::server_config(
            &DeviceKey::from_seed([3; 32]),
            ALPN_P2P,
            TlsOptions {
                max_fragment_size: None,
            },
        )
        .unwrap();
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap()
    }

    /// (crate, version) pairs of Cargo.lock.
    fn locked(name: &str) -> String {
        let lock = read(manifest_dir().join("../../Cargo.lock"));
        let at = lock
            .find(&format!("name = \"{name}\"\n"))
            .unwrap_or_else(|| panic!("{name} not in Cargo.lock"));
        let rest = &lock[at..];
        let v = rest.lines().nth(1).unwrap();
        v.trim_start_matches("version = ")
            .trim_matches('"')
            .to_owned()
    }

    #[test]
    fn dependency_defaults_match_source() {
        let t = format!("{:?}", quinn::TransportConfig::default());
        let m = format!("{:?}", quinn::MtuDiscoveryConfig::default());
        let a = format!("{:?}", quinn::AckFrequencyConfig::default());
        let e = format!("{:?}", quinn::EndpointConfig::default());
        let s = format!("{:?}", quinn::ServerConfig::with_crypto(Arc::new(crypto())));
        let v = format!("{:?}", quinn::ValidationTokenConfig::default());
        let c = format!("{:?}", quinn::congestion::CubicConfig::default());
        // The keys whose design default is the dependency's own.
        let shown: [(&str, &str, &str); 27] = [
            ("quic.initial_rtt", &t, "initial_rtt"),
            ("quic.packet_threshold", &t, "packet_threshold"),
            ("quic.time_threshold", &t, "time_threshold"),
            (
                "quic.persistent_congestion_threshold",
                &t,
                "persistent_congestion_threshold",
            ),
            ("quic.stream_receive_window", &t, "stream_receive_window"),
            ("quic.receive_window", &t, "receive_window"),
            ("quic.send_window", &t, "send_window"),
            ("quic.initial_mtu", &t, "initial_mtu"),
            ("quic.min_mtu", &t, "min_mtu"),
            ("quic.mtu_discovery.interval", &m, "interval"),
            ("quic.mtu_discovery.upper_bound", &m, "upper_bound"),
            (
                "quic.mtu_discovery.black_hole_cooldown",
                &m,
                "black_hole_cooldown",
            ),
            ("quic.mtu_discovery.minimum_change", &m, "minimum_change"),
            ("quic.crypto_buffer_size", &t, "crypto_buffer_size"),
            ("quic.ack_frequency", &t, "ack_frequency_config"),
            (
                "quic.ack_frequency.ack_eliciting_threshold",
                &a,
                "ack_eliciting_threshold",
            ),
            ("quic.ack_frequency.max_ack_delay", &a, "max_ack_delay"),
            (
                "quic.ack_frequency.reordering_threshold",
                &a,
                "reordering_threshold",
            ),
            (
                "quic.datagram_send_buffer_size",
                &t,
                "datagram_send_buffer_size",
            ),
            ("quic.cubic.initial_window", &c, "initial_window"),
            ("quic.max_udp_payload_size", &e, "max_udp_payload_size"),
            ("quic.retry_token_lifetime", &s, "retry_token_lifetime"),
            ("quic.validation_token.lifetime", &v, "lifetime"),
            ("quic.validation_token.sent", &v, "sent"),
            ("quic.max_incoming", &s, "max_incoming"),
            ("quic.incoming_buffer_size", &s, "incoming_buffer_size"),
            (
                "quic.incoming_buffer_size_total",
                &s,
                "incoming_buffer_size_total",
            ),
        ];
        for (key, debug, name) in shown {
            assert_eq!(
                field(debug, name),
                quinn_form(default_of(key)),
                "{key} ({name})"
            );
        }
        // The design turns datagrams off; the table names quinn's own default.
        assert_eq!(field(&t, "datagram_receive_buffer_size"), "1250000");
        let row = section(&design("p2p.md"), "의존성이 노출하는 값")
            .into_iter()
            .find(|l| l.starts_with("| `quic.datagram_receive_buffer_size`"))
            .unwrap();
        assert!(row.contains("1 250 000 B"), "{row}");
        // The two values quinn's repo keys differ from (the table names quinn's).
        assert_eq!(field(&t, "max_concurrent_bidi_streams"), "100");
        assert_eq!(field(&t, "max_concurrent_uni_streams"), "100");
        // The values Debug does not show, copied from the pinned source.
        let data = read(manifest_dir().join("tests/data/dependency-defaults.txt"));
        let mut copied = 0;
        for line in data.lines().filter(|l| !l.starts_with('#')) {
            let cols: Vec<&str> = line.split('\t').collect();
            let [krate, version, key, value, _where] = cols[..] else {
                panic!("bad line {line}");
            };
            assert_eq!(locked(krate), version, "{line}: the pinned version");
            match key {
                "quic.congestion_controller" => {
                    assert_eq!(value, "CubicConfig::default()");
                    assert_eq!(default_of(key), Value::Fixed("CUBIC", "CUBIC"));
                }
                _ => assert_eq!(quinn_form(default_of(key)), value, "{key}"),
            }
            copied += 1;
        }
        assert_eq!(copied, 3);
        // rustls: the public fields.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        assert_eq!(client.max_fragment_size, None);
        assert_eq!(
            default_of("tls.max_fragment_size"),
            Value::Absent(Absent::None)
        );
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
        assert_eq!(server.max_fragment_size, None);
        // The fixed table names these rustls defaults.
        assert_eq!(server.send_tls13_tickets, 2);
        assert_eq!(server.max_tls13_tickets, 0);
        assert_eq!(server.max_early_data_size, 0);
        assert!(!client.enable_early_data);
        // quinn's default turns grease on; the fixed table says so.
        assert_eq!(field(&e, "grease_quic_bit"), "true");
    }

    #[test]
    fn dependency_setters_are_covered() {
        let data = read(manifest_dir().join("tests/data/dependency-setters.txt"));
        let text = design("p2p.md");
        let lines: Vec<String> = section(&text, "의존성이 노출하는 값")
            .into_iter()
            .chain(section(&text, "고정값 (설정 불가, 조회에는 보임)"))
            .collect();
        let mut checked = 0;
        for line in data.lines().filter(|l| !l.starts_with('#')) {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols[0] == "version" {
                assert_eq!(locked(cols[1]), cols[2], "{line}: the pinned version");
                continue;
            }
            let [_krate, kind, name, _ty] = cols[..] else {
                panic!("bad line {line}");
            };
            let (ty, member) = match kind {
                "setter" => name.split_once("::").unwrap(),
                _ => name.split_once('.').unwrap(),
            };
            let quoted = format!("`{member}`");
            let covered = lines.iter().any(|l| {
                // A row that names the type and the member (or a key ending in it), or a
                // sentence or fixed row that names the member alone.
                (l.contains(ty) && l.contains(member)) || l.contains(&quoted)
            });
            assert!(covered, "{name} ({kind}) is in no row of p2p.md");
            checked += 1;
        }
        assert!(checked >= 30, "{checked}");
    }

    /// Settings that change every value that goes into quinn or rustls.
    fn changed() -> P2pSettings {
        let d = |ms: u64| Value::Duration(Duration::from_millis(ms));
        let mut s = P2pSettings::new();
        s.set("quic.initial_rtt", d(100))
            .set("quic.packet_threshold", Value::Count(5))
            .set("quic.time_threshold", Value::Ratio(5, 4))
            .set("quic.persistent_congestion_threshold", Value::Count(4))
            .set("quic.stream_receive_window", Value::Bytes(2_000_000))
            .set("quic.receive_window", Value::Bytes(3_000_000))
            .set("quic.send_window", Value::Bytes(4_000_000))
            .set("quic.initial_mtu", Value::Bytes(1300))
            .set("quic.min_mtu", Value::Bytes(1250))
            .set("quic.mtu_discovery.interval", d(300_000))
            .set("quic.mtu_discovery.upper_bound", Value::Bytes(1400))
            .set("quic.mtu_discovery.black_hole_cooldown", d(30_000))
            .set("quic.mtu_discovery.minimum_change", Value::Bytes(30))
            .set("quic.crypto_buffer_size", Value::Bytes(32 * KIB))
            .set("quic.ack_frequency", Value::Switch(true))
            .set(
                "quic.ack_frequency.ack_eliciting_threshold",
                Value::Count(3),
            )
            .set("quic.ack_frequency.max_ack_delay", d(40))
            .set("quic.ack_frequency.reordering_threshold", Value::Count(4))
            .set("quic.datagram_receive_buffer_size", Value::Bytes(5000))
            .set("quic.datagram_send_buffer_size", Value::Bytes(6000))
            .set("quic.max_udp_payload_size", Value::Bytes(1400))
            .set("quic.retry_token_lifetime", d(20_000))
            .set("quic.validation_token.lifetime", d(86_400_000))
            .set("quic.validation_token.sent", Value::Count(2))
            .set("quic.max_incoming", Value::Count(100))
            .set("quic.incoming_buffer_size", Value::Bytes(2_000_000))
            .set("quic.incoming_buffer_size_total", Value::Bytes(3_000_000))
            .set("quic.max_bidi_streams", Value::Count(10))
            .set("quic.max_uni_streams", Value::Count(1))
            .set("quic.keepalive_interval", d(7000))
            .set("quic.idle_timeout", d(21_000))
            .set("tls.max_fragment_size", Value::Bytes(1000));
        s
    }

    #[test]
    fn dependency_settings_are_applied() {
        let p = changed().resolve().expect("valid");
        let t = format!("{:?}", quic::transport_config(&p.quic));
        for (name, want) in [
            ("initial_rtt", "100ms"),
            ("packet_threshold", "5"),
            ("time_threshold", "1.25"),
            ("persistent_congestion_threshold", "4"),
            ("stream_receive_window", "2000000"),
            ("receive_window", "3000000"),
            ("send_window", "4000000"),
            ("initial_mtu", "1300"),
            ("min_mtu", "1250"),
            ("crypto_buffer_size", "32768"),
            ("datagram_receive_buffer_size", "5000"),
            ("datagram_send_buffer_size", "6000"),
            ("keep_alive_interval", "7s"),
            ("max_concurrent_bidi_streams", "10"),
            ("max_concurrent_uni_streams", "1"),
        ] {
            assert_eq!(field(&t, name), want, "{name}");
        }
        assert!(field(&t, "max_idle_timeout").contains("21000"), "{t}");
        let mtu = field(&t, "mtu_discovery_config");
        for (name, want) in [
            ("interval", "300s"),
            ("upper_bound", "1400"),
            ("black_hole_cooldown", "30s"),
            ("minimum_change", "30"),
        ] {
            assert_eq!(field(&mtu, name), want, "{name}");
        }
        let ack = field(&t, "ack_frequency_config");
        for (name, want) in [
            ("ack_eliciting_threshold", "3"),
            ("max_ack_delay", "40ms"),
            ("reordering_threshold", "4"),
        ] {
            assert_eq!(field(&ack, name), want, "{name}");
        }
        let e = format!("{:?}", quic::endpoint_config(&p.quic));
        assert_eq!(field(&e, "max_udp_payload_size"), "1400");
        // Fixed: grease off.
        assert_eq!(field(&e, "grease_quic_bit"), "false");
        let s = format!("{:?}", quic::server_config(&p.quic, crypto()));
        for (name, want) in [
            ("retry_token_lifetime", "20s"),
            ("max_incoming", "100"),
            ("incoming_buffer_size", "2000000"),
            ("incoming_buffer_size_total", "3000000"),
        ] {
            assert_eq!(field(&s, name), want, "{name}");
        }
        let v = field(&s, "validation_token");
        assert_eq!(field(&v, "lifetime"), "86400s");
        assert_eq!(field(&v, "sent"), "2");
        // rustls: the fragment size, and the fixed values (no resumption, tickets or 0-RTT).
        let options = TlsOptions {
            max_fragment_size: p.quic.tls_max_fragment_size,
        };
        let key = DeviceKey::from_seed([4; 32]);
        let client = tls::client_config(&key, key.id(), ALPN_P2P, options).unwrap();
        assert_eq!(client.max_fragment_size, Some(1000));
        assert!(!client.enable_early_data);
        assert_eq!(
            format!("{:?}", client.resumption),
            format!("{:?}", rustls::client::Resumption::disabled())
        );
        let server = tls::server_config(&key, ALPN_P2P, options).unwrap();
        assert_eq!(server.max_fragment_size, Some(1000));
        assert_eq!(server.send_tls13_tickets, 0);
        assert_eq!(server.max_tls13_tickets, 0);
        assert_eq!(server.max_early_data_size, 0);
        assert_eq!(server.alpn_protocols, vec![ALPN_P2P.to_vec()]);
        // The stream limit reaches the peer: with the default 0, a stream the peer opens
        // waits for credit that never comes; with 1 it opens.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let opens = |uni: u64| {
            let mut s = P2pSettings::new();
            s.set("quic.max_uni_streams", Value::Count(uni));
            let p = s.resolve().unwrap();
            rt.block_on(async move {
                let server_key = DeviceKey::from_seed([5; 32]);
                let client_key = DeviceKey::from_seed([6; 32]);
                let tls = tls::server_config(&server_key, ALPN_P2P, options).unwrap();
                let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
                let server = quinn::Endpoint::server(
                    quic::server_config(&p.quic, crypto),
                    "127.0.0.1:0".parse().unwrap(),
                )
                .unwrap();
                let addr = server.local_addr().unwrap();
                let accepting = tokio::spawn(async move {
                    let c = server.accept().await.unwrap().await.unwrap();
                    c.closed().await;
                });
                let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                let tls =
                    tls::client_config(&client_key, server_key.id(), ALPN_P2P, options).unwrap();
                let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
                let mut config = quinn::ClientConfig::new(Arc::new(crypto));
                config.transport_config(Arc::new(quic::transport_config(&p.quic)));
                let conn = client
                    .connect_with(config, addr, tls::SERVER_NAME)
                    .unwrap()
                    .await
                    .unwrap();
                let opened = tokio::time::timeout(Duration::from_millis(500), conn.open_uni())
                    .await
                    .is_ok();
                conn.close(quinn::VarInt::from_u32(0), b"");
                let _done = accepting.await;
                opened
            })
        };
        assert!(
            !opens(0),
            "a uni stream opened with quic.max_uni_streams = 0"
        );
        assert!(opens(1));
    }
}
