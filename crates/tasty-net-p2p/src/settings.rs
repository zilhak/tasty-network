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
