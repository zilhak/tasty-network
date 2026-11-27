//! Server settings (docs/design/rendezvous.md "서버 설정값", docs/design/crates.md "설정값
//! 규칙").
//!
//! [`DEFAULTS`] is the one source of every default: [`ServerSettings::entries`] and the values
//! the server runs with come from it, and the tests compare it with the tables in the design
//! documents. Every time, count, interval and limit of the server is here; the rest of the crate
//! takes them from [`ServerParams`]. Fixed values are listed too (source `fixed`) but have no
//! key a settings file may set.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

/// Where a value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The default in [`DEFAULTS`].
    Default,
    /// Given in the settings.
    Configured,
    /// Not settable (docs/design/p2p.md "고정값").
    Fixed,
}

impl Source {
    /// `default`, `configured` or `fixed`.
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Configured => "configured",
            Source::Fixed => "fixed",
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
    /// A count.
    Count,
    /// A size in bytes.
    Bytes,
    /// A size in bytes or nothing.
    OptionalBytes(Absent),
    /// A ratio `num/den`.
    Ratio,
    /// A socket address.
    Address,
    /// A UDP port.
    Port,
    /// A UDP port, or by default the value of another key.
    PortOr(&'static str),
    /// An IP address or nothing.
    OptionalIp,
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
    /// A socket address.
    Address(SocketAddr),
    /// A UDP port.
    Port(u16),
    /// An IP address.
    Ip(IpAddr),
    /// On or off.
    Switch(bool),
    /// An optional value that is not there.
    Absent(Absent),
    /// The value of another key.
    SameAs(&'static str),
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
            Value::Address(a) => write!(f, "{a}"),
            Value::Port(p) => write!(f, "{p}"),
            Value::Ip(ip) => write!(f, "{ip}"),
            Value::Switch(true) => f.write_str("on"),
            Value::Switch(false) | Value::Absent(Absent::Off) => f.write_str("off"),
            Value::Absent(Absent::None) => f.write_str("none"),
            Value::SameAs(k) => write!(f, "same as {k}"),
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
                    Value::SameAs(k) => write!(f, "{k}와 같음"),
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

/// Every server setting with its default, in the order of the design tables: the server table
/// of docs/design/rendezvous.md, then the dependency values and the fixed values of
/// docs/design/p2p.md that the server carries too.
pub static DEFAULTS: &[Setting] = &[
    entry(
        "listen.primary",
        Kind::Address,
        Value::Address(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            3478,
        ))),
    ),
    entry("listen.alternate_port", Kind::Port, Value::Port(3479)),
    entry(
        "listen.alternate_ip",
        Kind::OptionalIp,
        Value::Absent(Absent::None),
    ),
    entry(
        "advertise.primary_ip",
        Kind::OptionalIp,
        Value::Absent(Absent::None),
    ),
    entry(
        "advertise.alternate_port",
        Kind::PortOr("listen.alternate_port"),
        Value::SameAs("listen.alternate_port"),
    ),
    entry(
        "advertise.alternate_ip",
        Kind::OptionalIp,
        Value::Absent(Absent::None),
    ),
    entry("hello_timeout", Kind::Duration, secs(5)),
    entry("frame_timeout", Kind::Duration, secs(5)),
    entry("keepalive_interval", Kind::Duration, secs(15)),
    entry("idle_timeout", Kind::Duration, secs(45)),
    entry("reflexive_poll_interval", Kind::Duration, secs(1)),
    entry("offer_timeout", Kind::Duration, secs(2)),
    entry("start_delay_margin", Kind::Duration, millis(100)),
    entry("start_delay_max", Kind::Duration, secs(1)),
    entry("punch_window", Kind::Duration, secs(5)),
    entry("quic_handshake_timeout", Kind::Duration, secs(5)),
    entry("punch_session_grace", Kind::Duration, secs(5)),
    entry("max_devices", Kind::Count, Value::Count(10_000)),
    entry("max_punches", Kind::Count, Value::Count(1000)),
    entry("max_punches_per_device", Kind::Count, Value::Count(4)),
    entry("punch_requests_per_minute", Kind::Count, Value::Count(20)),
    entry("max_host_candidates", Kind::Count, Value::Count(4)),
    entry("max_reported_reflexive", Kind::Count, Value::Count(3)),
    entry("max_message_bytes", Kind::Bytes, Value::Bytes(16 * KIB)),
    entry("stun_per_source_per_second", Kind::Count, Value::Count(50)),
    entry("status_log_interval", Kind::Duration, secs(60)),
    entry("quic.max_bidi_streams", Kind::Count, Value::Count(2)),
    entry("quic.max_uni_streams", Kind::Count, Value::Count(0)),
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
    fixed("tls.alpn", "tasty-rendezvous/1", "tasty-rendezvous/1"),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// A settings problem. The server does not start with it.
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
    /// The settings file is not valid TOML.
    Toml(String),
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
            SettingsError::Toml(e) => write!(f, "settings file is not valid TOML: {e}"),
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

/// Configured server settings. Keys that are not given take their default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerSettings {
    configured: BTreeMap<&'static str, Value>,
}

fn parse_duration_text(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number.parse().ok()?;
    let unit_secs = match unit.trim() {
        "ms" => return Some(Duration::from_millis(n)),
        "s" => 1,
        "min" => 60,
        "h" => 3600,
        "d" => SECS_PER_DAY,
        _ => return None,
    };
    n.checked_mul(unit_secs).map(Duration::from_secs)
}

fn parse_bytes_text(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number.parse().ok()?;
    let unit = match unit.trim() {
        "B" => 1,
        "KiB" => KIB,
        "MiB" => MIB,
        "GiB" => MIB * KIB,
        _ => return None,
    };
    n.checked_mul(unit)
}

fn is_absent_text(text: &str) -> bool {
    matches!(text.trim(), "none" | "off")
}

impl ServerSettings {
    /// No configured values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one value. The key must be a settable key and the value must have its type.
    pub fn set(&mut self, key: &str, value: Value) -> Result<&mut Self, SettingsError> {
        let s = setting(key)
            .filter(|s| s.kind != Kind::Fixed)
            .ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        let ok = matches!(
            (s.kind, value),
            (Kind::Duration, Value::Duration(_))
                | (
                    Kind::OptionalDuration,
                    Value::Duration(_) | Value::Absent(_)
                )
                | (Kind::Count, Value::Count(_))
                | (Kind::Bytes, Value::Bytes(_))
                | (Kind::OptionalBytes(_), Value::Bytes(_) | Value::Absent(_))
                | (Kind::Ratio, Value::Ratio(_, _))
                | (Kind::Address, Value::Address(_))
                | (Kind::Port, Value::Port(_))
                | (Kind::PortOr(_), Value::Port(_))
                | (Kind::OptionalIp, Value::Ip(_) | Value::Absent(_))
                | (Kind::Switch, Value::Switch(_))
        );
        if !ok {
            return Err(SettingsError::WrongType {
                key: key.to_owned(),
                given: value.to_string(),
            });
        }
        let value = match (s.kind, value) {
            (Kind::OptionalBytes(word), Value::Absent(_)) => Value::Absent(word),
            (_, Value::Absent(_)) => Value::Absent(Absent::None),
            (_, v) => v,
        };
        self.configured.insert(s.key, value);
        Ok(self)
    }

    /// Settings from a TOML document. Tables nest keys (`[listen] primary = ...` is
    /// `listen.primary`); a quoted key may hold dots. Times are strings with a unit (`"5 s"`,
    /// `"100 ms"`, `"1 min"`, `"1 h"`, `"14 d"`), sizes are integers (bytes) or strings with a
    /// unit (`"16 KiB"`), absent optional values are `"none"`.
    pub fn from_toml(table: &toml::Table) -> Result<Self, SettingsError> {
        let mut out = Self::new();
        let mut flat = Vec::new();
        flatten("", table, &mut flat);
        for (key, value) in flat {
            out.set_toml(&key, value)?;
        }
        Ok(out)
    }

    /// Settings from TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self, SettingsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| SettingsError::Toml(e.to_string()))?;
        Self::from_toml(&table)
    }

    fn set_toml(&mut self, key: &str, value: &toml::Value) -> Result<(), SettingsError> {
        let s = setting(key)
            .filter(|s| s.kind != Kind::Fixed)
            .ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        let wrong = || SettingsError::WrongType {
            key: key.to_owned(),
            given: format!("{value:?}"),
        };
        let int = || {
            value
                .as_integer()
                .and_then(|i| u64::try_from(i).ok())
                .ok_or_else(wrong)
        };
        let text = value.as_str();
        let parsed = match s.kind {
            Kind::Duration => {
                Value::Duration(text.and_then(parse_duration_text).ok_or_else(wrong)?)
            }
            Kind::OptionalDuration => match text {
                Some(t) if is_absent_text(t) => Value::Absent(Absent::None),
                _ => Value::Duration(text.and_then(parse_duration_text).ok_or_else(wrong)?),
            },
            Kind::Count => Value::Count(int()?),
            Kind::Bytes => Value::Bytes(match text {
                Some(t) => parse_bytes_text(t).ok_or_else(wrong)?,
                None => int()?,
            }),
            Kind::OptionalBytes(word) => match text {
                Some(t) if is_absent_text(t) => Value::Absent(word),
                Some(t) => Value::Bytes(parse_bytes_text(t).ok_or_else(wrong)?),
                None => Value::Bytes(int()?),
            },
            Kind::Ratio => {
                let (n, d) = text.and_then(|t| t.split_once('/')).ok_or_else(wrong)?;
                Value::Ratio(
                    n.trim().parse().map_err(|_| wrong())?,
                    d.trim().parse().map_err(|_| wrong())?,
                )
            }
            Kind::Address => Value::Address(text.and_then(|t| t.parse().ok()).ok_or_else(wrong)?),
            Kind::Port | Kind::PortOr(_) => {
                Value::Port(u16::try_from(int()?).map_err(|_| wrong())?)
            }
            Kind::OptionalIp => match text {
                Some(t) if is_absent_text(t) => Value::Absent(Absent::None),
                _ => Value::Ip(text.and_then(|t| t.parse().ok()).ok_or_else(wrong)?),
            },
            Kind::Switch => Value::Switch(value.as_bool().ok_or_else(wrong)?),
            Kind::Fixed => return Err(SettingsError::UnknownKey(key.to_owned())),
        };
        self.set(key, parsed).map(|_| ())
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
                source: if s.kind == Kind::Fixed {
                    Source::Fixed
                } else {
                    Source::Default
                },
            },
        })
    }

    /// Every setting with the value in effect and its source, in [`DEFAULTS`] order.
    pub fn entries(&self) -> Vec<SettingEntry> {
        DEFAULTS.iter().filter_map(|s| self.get(s.key)).collect()
    }

    /// Check ranges and relations and give the values the server runs with.
    pub fn resolve(&self) -> Result<ServerParams, SettingsError> {
        Resolver(self).params()
    }
}

fn flatten<'a>(prefix: &str, table: &'a toml::Table, out: &mut Vec<(String, &'a toml::Value)>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => flatten(&key, t, out),
            other => out.push((key, other)),
        }
    }
}

/// The values the server runs with, checked.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerParams {
    /// `listen.primary`.
    pub listen_primary: SocketAddr,
    /// `listen.alternate_port`.
    pub listen_alternate_port: u16,
    /// `listen.alternate_ip`.
    pub listen_alternate_ip: Option<IpAddr>,
    /// `advertise.primary_ip`.
    pub advertise_primary_ip: Option<IpAddr>,
    /// `advertise.alternate_port`; `None` is the port `listen.alternate_port` bound.
    pub advertise_alternate_port: Option<u16>,
    /// `advertise.alternate_ip`.
    pub advertise_alternate_ip: Option<IpAddr>,
    /// `hello_timeout`.
    pub hello_timeout: Duration,
    /// `frame_timeout`.
    pub frame_timeout: Duration,
    /// `keepalive_interval`.
    pub keepalive_interval: Duration,
    /// `idle_timeout`.
    pub idle_timeout: Duration,
    /// `reflexive_poll_interval`.
    pub reflexive_poll_interval: Duration,
    /// `offer_timeout`.
    pub offer_timeout: Duration,
    /// `start_delay_margin`.
    pub start_delay_margin: Duration,
    /// `start_delay_max`.
    pub start_delay_max: Duration,
    /// `punch_window`.
    pub punch_window: Duration,
    /// `quic_handshake_timeout`.
    pub quic_handshake_timeout: Duration,
    /// `punch_session_grace`.
    pub punch_session_grace: Duration,
    /// `max_devices`.
    pub max_devices: usize,
    /// `max_punches`.
    pub max_punches: usize,
    /// `max_punches_per_device`.
    pub max_punches_per_device: usize,
    /// `punch_requests_per_minute`.
    pub punch_requests_per_minute: usize,
    /// `max_host_candidates`.
    pub max_host_candidates: usize,
    /// `max_reported_reflexive`.
    pub max_reported_reflexive: usize,
    /// `max_message_bytes`.
    pub max_message_bytes: u32,
    /// `stun_per_source_per_second`.
    pub stun_per_source_per_second: u64,
    /// `status_log_interval`.
    pub status_log_interval: Duration,
    /// The window of `punch_requests_per_minute`.
    pub punch_request_window: Duration,
    /// The window of `stun_per_source_per_second`.
    pub stun_rate_window: Duration,
    /// QUIC and TLS values.
    pub quic: QuicParams,
}

/// The QUIC and TLS values (docs/design/p2p.md "의존성이 노출하는 값").
#[derive(Clone, Debug, PartialEq)]
pub struct QuicParams {
    /// `quic.max_bidi_streams`.
    pub max_bidi_streams: u64,
    /// `quic.max_uni_streams`.
    pub max_uni_streams: u64,
    /// `quic.initial_rtt`.
    pub initial_rtt: Duration,
    /// `quic.packet_threshold`.
    pub packet_threshold: u32,
    /// `quic.time_threshold` as a float.
    pub time_threshold: f32,
    /// `quic.persistent_congestion_threshold`.
    pub persistent_congestion_threshold: u32,
    /// `quic.stream_receive_window`.
    pub stream_receive_window: u64,
    /// `quic.receive_window`.
    pub receive_window: u64,
    /// `quic.send_window`.
    pub send_window: u64,
    /// `quic.initial_mtu`.
    pub initial_mtu: u16,
    /// `quic.min_mtu`.
    pub min_mtu: u16,
    /// `quic.mtu_discovery.interval`.
    pub mtu_interval: Duration,
    /// `quic.mtu_discovery.upper_bound`.
    pub mtu_upper_bound: u16,
    /// `quic.mtu_discovery.black_hole_cooldown`.
    pub mtu_black_hole_cooldown: Duration,
    /// `quic.mtu_discovery.minimum_change`.
    pub mtu_minimum_change: u16,
    /// `quic.crypto_buffer_size`.
    pub crypto_buffer_size: usize,
    /// `quic.ack_frequency`.
    pub ack_frequency: bool,
    /// `quic.ack_frequency.ack_eliciting_threshold`.
    pub ack_eliciting_threshold: u64,
    /// `quic.ack_frequency.max_ack_delay`.
    pub max_ack_delay: Option<Duration>,
    /// `quic.ack_frequency.reordering_threshold`.
    pub reordering_threshold: u64,
    /// `quic.datagram_receive_buffer_size`.
    pub datagram_receive_buffer_size: Option<usize>,
    /// `quic.datagram_send_buffer_size`.
    pub datagram_send_buffer_size: usize,
    /// `quic.cubic.initial_window`.
    pub cubic_initial_window: u64,
    /// `quic.cid_lifetime`.
    pub cid_lifetime: Option<Duration>,
    /// `quic.max_udp_payload_size`.
    pub max_udp_payload_size: u16,
    /// `quic.min_reset_interval`.
    pub min_reset_interval: Duration,
    /// `quic.retry_token_lifetime`.
    pub retry_token_lifetime: Duration,
    /// `quic.validation_token.lifetime`.
    pub validation_token_lifetime: Duration,
    /// `quic.validation_token.sent`.
    pub validation_token_sent: u32,
    /// `quic.max_incoming`.
    pub max_incoming: usize,
    /// `quic.incoming_buffer_size`.
    pub incoming_buffer_size: u64,
    /// `quic.incoming_buffer_size_total`.
    pub incoming_buffer_size_total: u64,
    /// `tls.max_fragment_size`.
    pub tls_max_fragment_size: Option<usize>,
    /// The `keepalive_interval` the QUIC connections use.
    pub keepalive_interval: Duration,
    /// The `idle_timeout` the QUIC connections use.
    pub idle_timeout: Duration,
    /// `tls.tickets` (fixed).
    pub tls_tickets: usize,
}

/// Bounds the server checks at start (docs/design/rendezvous.md "서버 상태" "설정 관계 검사"
/// and "프레임", docs/design/p2p.md "잘못된 설정의 거절").
const MIN_MESSAGE_BYTES: u64 = KIB;
const MAX_MESSAGE_BYTES: u64 = MIB;
const MIN_SESSION_GRACE: Duration = Duration::from_secs(1);
const MIN_OFFER_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_PUNCH_CANDIDATES: u64 = 64;
const MIN_QUIC_MTU: u64 = 1200;
const MAX_UDP_PAYLOAD: u64 = 65_527;
const MIN_TLS_FRAGMENT: u64 = 32;
const MAX_TLS_FRAGMENT: u64 = 16_384 + 5;
const ONE_MINUTE: Duration = Duration::from_secs(60);
const ONE_SECOND: Duration = Duration::from_secs(1);
/// The longest time a setting takes, 2^32 − 1 s (about 136 years): a deadline of the current
/// time plus such a time is in range on every supported system, and the sums the server makes
/// of these values stay in range of [`Duration`].
const MAX_DURATION: Duration = Duration::from_secs(u32::MAX as u64);
/// The most streams a QUIC peer may allow, 2^60 (RFC 9000 §4.6).
const MAX_STREAMS: u64 = 1 << 60;
/// The coordination stream needs one bidirectional stream.
const MIN_BIDI_STREAMS: u64 = 1;

struct Resolver<'a>(&'a ServerSettings);

impl Resolver<'_> {
    fn value(&self, key: &'static str) -> Value {
        self.0
            .get(key)
            .map(|e| e.value)
            .unwrap_or_else(|| panic!("{key} is in DEFAULTS"))
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
            Value::Absent(_) => None,
            other => panic!("{key} is optional bytes, got {other:?}"),
        }
    }

    fn opt_ip(&self, key: &'static str) -> Option<IpAddr> {
        match self.value(key) {
            Value::Ip(ip) => Some(ip),
            Value::Absent(_) => None,
            other => panic!("{key} is an optional IP, got {other:?}"),
        }
    }

    fn port(&self, key: &'static str) -> Option<u16> {
        match self.value(key) {
            Value::Port(p) => Some(p),
            Value::SameAs(_) => None,
            other => panic!("{key} is a port, got {other:?}"),
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
            return Err(SettingsError::OutOfRange {
                key,
                value: self.value(key),
                expected,
            });
        }
        Ok(n)
    }

    fn fits<T: TryFrom<u64>>(&self, key: &'static str) -> Result<T, SettingsError> {
        T::try_from(self.number(key)).map_err(|_| SettingsError::OutOfRange {
            key,
            value: self.value(key),
            expected: "fits the dependency's integer type",
        })
    }

    fn nonzero(&self, key: &'static str) -> Result<Duration, SettingsError> {
        let d = self.duration(key);
        if d.is_zero() {
            return Err(SettingsError::OutOfRange {
                key,
                value: self.value(key),
                expected: "greater than 0",
            });
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
    fn durations(&self, now: std::time::Instant) -> Result<(), SettingsError> {
        for s in DEFAULTS {
            let d = match (s.kind, self.value(s.key)) {
                (Kind::Duration | Kind::OptionalDuration, Value::Duration(d)) => d,
                _ => continue,
            };
            if d > MAX_DURATION || now.checked_add(d).is_none() {
                return Err(SettingsError::OutOfRange {
                    key: s.key,
                    value: Value::Duration(d),
                    expected: "at most 2^32 - 1 s",
                });
            }
        }
        Ok(())
    }

    fn params(&self) -> Result<ServerParams, SettingsError> {
        let now = std::time::Instant::now();
        self.durations(now)?;
        let listen_primary = match self.value("listen.primary") {
            Value::Address(a) => a,
            other => panic!("listen.primary is an address, got {other:?}"),
        };
        let max_message_bytes = self.range(
            "max_message_bytes",
            MIN_MESSAGE_BYTES,
            MAX_MESSAGE_BYTES,
            "1 KiB to 1 MiB",
        )?;
        let start_delay_margin = self.duration("start_delay_margin");
        let start_delay_max = self.duration("start_delay_max");
        self.relation(
            start_delay_margin <= start_delay_max,
            "start_delay_margin <= start_delay_max",
            &["start_delay_margin", "start_delay_max"],
        )?;
        let punch_session_grace = self.duration("punch_session_grace");
        self.relation(
            punch_session_grace >= MIN_SESSION_GRACE,
            "punch_session_grace >= 1 s",
            &["punch_session_grace"],
        )?;
        let offer_timeout = self.duration("offer_timeout");
        self.relation(
            offer_timeout >= MIN_OFFER_TIMEOUT,
            "offer_timeout >= 100 ms",
            &["offer_timeout"],
        )?;
        let frame_timeout = self.nonzero("frame_timeout")?;
        let keepalive_interval = self.nonzero("keepalive_interval")?;
        let idle_timeout = self.duration("idle_timeout");
        self.relation(
            frame_timeout < idle_timeout,
            "frame_timeout < idle_timeout",
            &["frame_timeout", "idle_timeout"],
        )?;
        self.relation(
            keepalive_interval < idle_timeout,
            "keepalive_interval < idle_timeout",
            &["keepalive_interval", "idle_timeout"],
        )?;
        let max_reported_reflexive = self.number("max_reported_reflexive");
        let max_host_candidates = self.number("max_host_candidates");
        self.relation(
            max_reported_reflexive
                .checked_add(max_host_candidates)
                .and_then(|n| n.checked_add(1))
                .is_some_and(|n| n <= MAX_PUNCH_CANDIDATES),
            "1 + max_reported_reflexive + max_host_candidates <= 64",
            &["max_reported_reflexive", "max_host_candidates"],
        )?;
        let advertise_primary_ip = self.opt_ip("advertise.primary_ip");
        self.relation(
            !listen_primary.ip().is_unspecified() || advertise_primary_ip.is_some(),
            "a wildcard listen.primary needs advertise.primary_ip for the alternate STUN address",
            &["listen.primary", "advertise.primary_ip"],
        )?;
        let quic = self.quic(keepalive_interval, idle_timeout)?;
        let params = ServerParams {
            listen_primary,
            listen_alternate_port: self.port("listen.alternate_port").unwrap_or_default(),
            listen_alternate_ip: self.opt_ip("listen.alternate_ip"),
            advertise_primary_ip,
            advertise_alternate_port: self.port("advertise.alternate_port"),
            advertise_alternate_ip: self.opt_ip("advertise.alternate_ip"),
            hello_timeout: self.duration("hello_timeout"),
            frame_timeout,
            keepalive_interval,
            idle_timeout,
            reflexive_poll_interval: self.nonzero("reflexive_poll_interval")?,
            offer_timeout,
            start_delay_margin,
            start_delay_max,
            punch_window: self.duration("punch_window"),
            quic_handshake_timeout: self.duration("quic_handshake_timeout"),
            punch_session_grace,
            max_devices: self.fits("max_devices")?,
            max_punches: self.fits("max_punches")?,
            max_punches_per_device: self.fits("max_punches_per_device")?,
            punch_requests_per_minute: self.fits("punch_requests_per_minute")?,
            max_host_candidates: self.fits("max_host_candidates")?,
            max_reported_reflexive: self.fits("max_reported_reflexive")?,
            max_message_bytes: u32::try_from(max_message_bytes).expect("checked against 1 MiB"),
            stun_per_source_per_second: self.number("stun_per_source_per_second"),
            status_log_interval: self.nonzero("status_log_interval")?,
            punch_request_window: ONE_MINUTE,
            stun_rate_window: ONE_SECOND,
            quic,
        };
        self.relation(
            now.checked_add(params.punch_session_ttl()).is_some(),
            "the current time + start_delay_max + punch_window + 2 × quic_handshake_timeout + \
             punch_session_grace is in range of the clock",
            &[
                "start_delay_max",
                "punch_window",
                "quic_handshake_timeout",
                "punch_session_grace",
            ],
        )?;
        Ok(params)
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
        self.range(
            "quic.max_bidi_streams",
            MIN_BIDI_STREAMS,
            MAX_STREAMS,
            "1 to 2^60",
        )?;
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
            return Err(SettingsError::OutOfRange {
                key: "idle_timeout",
                value: self.value("idle_timeout"),
                expected: "at most 2^62 - 1 ms",
            });
        }
        let (num, den) = match self.value("quic.time_threshold") {
            Value::Ratio(n, d) => (n, d),
            other => panic!("quic.time_threshold is a ratio, got {other:?}"),
        };
        if den == u32::MIN || num == u32::MIN {
            return Err(SettingsError::OutOfRange {
                key: "quic.time_threshold",
                value: self.value("quic.time_threshold"),
                expected: "a positive ratio",
            });
        }
        let tls_max_fragment_size = match self.opt_bytes("tls.max_fragment_size") {
            None => None,
            Some(n) if (MIN_TLS_FRAGMENT..=MAX_TLS_FRAGMENT).contains(&n) => {
                Some(usize::try_from(n).expect("small"))
            }
            Some(_) => {
                return Err(SettingsError::OutOfRange {
                    key: "tls.max_fragment_size",
                    value: self.value("tls.max_fragment_size"),
                    expected: "32 to 16389",
                });
            }
        };
        let datagram_receive_buffer_size = match self.opt_bytes("quic.datagram_receive_buffer_size")
        {
            None => None,
            Some(n) => Some(usize::try_from(n).map_err(|_| SettingsError::OutOfRange {
                key: "quic.datagram_receive_buffer_size",
                value: self.value("quic.datagram_receive_buffer_size"),
                expected: "fits usize",
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
            ack_frequency: matches!(self.value("quic.ack_frequency"), Value::Switch(true)),
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
            tls_tickets: 0,
        })
    }
}

impl ServerParams {
    /// The session TTL of the `go_sent` stage: `start_delay_max + punch_window +
    /// 2 × quic_handshake_timeout + punch_session_grace` (docs/design/rendezvous.md "서버 상태").
    pub fn punch_session_ttl(&self) -> Duration {
        self.start_delay_max
            .saturating_add(self.punch_window)
            .saturating_add(self.quic_handshake_timeout.saturating_mul(2))
            .saturating_add(self.punch_session_grace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The rows of the first table after the heading `heading` of a design document, as
    /// (keys, value text) with the value taken from the column named `column`.
    fn table_rows(doc: &str, heading: &str, column: &str) -> Vec<(Vec<String>, String)> {
        let path = manifest_dir().join("../../docs/design").join(doc);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let mut lines = text
            .lines()
            .skip_while(|l| l.trim_start_matches('#').trim() != heading);
        assert!(lines.next().is_some(), "{doc}: no heading {heading}");
        let mut table = lines
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
            .unwrap_or_else(|| panic!("{doc} {heading}: no column {column} in {header:?}"));
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

    fn design_rows() -> Vec<(String, String)> {
        let mut rows = Vec::new();
        for (doc, heading, column) in [
            ("rendezvous.md", "서버 설정값", "기본"),
            ("p2p.md", "의존성이 노출하는 값", "기본"),
            ("p2p.md", "고정값 (설정 불가, 조회에는 보임)", "고정값"),
        ] {
            for (keys, cell) in table_rows(doc, heading, column) {
                let value = normalize(&cell);
                if keys.len() == 1 {
                    rows.push((keys[0].clone(), value));
                } else {
                    // Several keys in one row: either one value for all, or one value each.
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
            // A fixed row may list the values of both endpoints (tls.alpn); the server's value
            // must be one of them.
            let ok = if s.kind == Kind::Fixed && doc.contains(',') {
                doc.split(',').any(|alt| alt.trim() == code)
            } else {
                normalize(&code) == *doc
            };
            assert!(ok, "{key}: code {code:?}, design {doc:?}");
        }
    }

    #[test]
    fn settings_list_matches_query() {
        let mut settings = ServerSettings::new();
        settings
            .set("hello_timeout", Value::Duration(Duration::from_secs(7)))
            .unwrap();
        // The same value as the default given explicitly is still `configured`.
        settings.set("max_devices", Value::Count(10_000)).unwrap();
        settings
            .set(
                "quic.datagram_receive_buffer_size",
                Value::Absent(Absent::None),
            )
            .unwrap();
        let entries = settings.entries();
        let keys: Vec<&str> = entries.iter().map(|e| e.key).collect();
        let listed: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
        assert_eq!(keys, listed);
        for e in &entries {
            let want = match e.key {
                "hello_timeout" | "max_devices" | "quic.datagram_receive_buffer_size" => {
                    Source::Configured
                }
                k if setting(k).unwrap().kind == Kind::Fixed => Source::Fixed,
                _ => Source::Default,
            };
            assert_eq!(e.source, want, "{e}");
            assert_eq!(settings.get(e.key), Some(*e));
        }
        assert_eq!(
            settings.get("hello_timeout").unwrap().value,
            Value::Duration(Duration::from_secs(7))
        );
        // An absent value takes the table's word for that key.
        assert_eq!(
            settings
                .get("quic.datagram_receive_buffer_size")
                .unwrap()
                .value,
            Value::Absent(Absent::Off)
        );
        // Fixed keys and unknown keys are refused, in code and in a file.
        for key in [
            "tls.resumption",
            "tls.alpn",
            "quic.grease_quic_bit",
            "relay.enabled",
        ] {
            assert!(matches!(
                ServerSettings::new().set(key, Value::Count(0)),
                Err(SettingsError::UnknownKey(_))
            ));
        }
        assert!(matches!(
            ServerSettings::from_toml_str("[tls]\nresumption = true\n"),
            Err(SettingsError::UnknownKey(k)) if k == "tls.resumption"
        ));
        // A wrong type is refused.
        assert!(matches!(
            ServerSettings::new().set("hello_timeout", Value::Count(5)),
            Err(SettingsError::WrongType { .. })
        ));
    }

    #[test]
    fn settings_file_values() {
        let s = ServerSettings::from_toml_str(
            r#"
            hello_timeout = "7 s"
            max_message_bytes = "32 KiB"
            [listen]
            primary = "127.0.0.1:0"
            alternate_port = 0
            [quic]
            time_threshold = "5/4"
            ack_frequency = true
            "ack_frequency.max_ack_delay" = "25 ms"
            [quic.validation_token]
            lifetime = "1 d"
            "#,
        )
        .unwrap();
        let p = s.resolve().unwrap();
        assert_eq!(p.hello_timeout, Duration::from_secs(7));
        assert_eq!(p.max_message_bytes, 32 * 1024);
        assert_eq!(p.listen_primary, "127.0.0.1:0".parse().unwrap());
        assert_eq!(p.listen_alternate_port, 0);
        assert_eq!(p.quic.time_threshold, 1.25);
        assert!(p.quic.ack_frequency);
        assert_eq!(p.quic.max_ack_delay, Some(Duration::from_millis(25)));
        assert_eq!(
            p.quic.validation_token_lifetime,
            Duration::from_secs(86_400)
        );
        assert!(ServerSettings::from_toml_str("hello_timeout = 5").is_err());
        assert!(ServerSettings::from_toml_str("hello_timeout = \"5 parsecs\"").is_err());
    }

    #[test]
    fn server_start_rejects_inconsistent_settings() {
        let local = || {
            let mut s = ServerSettings::new();
            s.set(
                "listen.primary",
                Value::Address("127.0.0.1:0".parse().unwrap()),
            )
            .unwrap();
            s
        };
        let defaults = local().resolve().unwrap();
        assert_eq!(defaults.punch_session_ttl(), Duration::from_secs(21));
        let d = |ms| Value::Duration(Duration::from_millis(ms));
        let bad: Vec<(&str, Value, &str)> = vec![
            ("start_delay_margin", d(1001), "start_delay_margin"),
            ("punch_session_grace", d(999), "punch_session_grace"),
            ("offer_timeout", d(99), "offer_timeout"),
            ("frame_timeout", d(45_000), "frame_timeout"),
            ("keepalive_interval", d(45_000), "keepalive_interval"),
            (
                "max_host_candidates",
                Value::Count(61),
                "max_host_candidates",
            ),
            ("max_message_bytes", Value::Bytes(1023), "max_message_bytes"),
            (
                "max_message_bytes",
                Value::Bytes(1024 * 1024 + 1),
                "max_message_bytes",
            ),
            ("quic.initial_mtu", Value::Bytes(1199), "quic.initial_mtu"),
            ("quic.min_mtu", Value::Bytes(1300), "quic.min_mtu"),
            (
                "quic.mtu_discovery.upper_bound",
                Value::Bytes(1201),
                "quic.mtu_discovery.upper_bound",
            ),
            (
                "quic.receive_window",
                Value::Bytes(1 << 62),
                "quic.receive_window",
            ),
            ("status_log_interval", d(0), "status_log_interval"),
            (
                "tls.max_fragment_size",
                Value::Bytes(31),
                "tls.max_fragment_size",
            ),
        ];
        for (key, value, named) in bad {
            let mut s = local();
            if key == "quic.mtu_discovery.upper_bound" {
                s.set("quic.initial_mtu", Value::Bytes(1300)).unwrap();
                s.set("quic.min_mtu", Value::Bytes(1200)).unwrap();
            }
            s.set(key, value).unwrap();
            let err = s.resolve().expect_err(key);
            let text = err.to_string();
            assert!(text.contains(named), "{key}: {text}");
        }
        // A wildcard primary address needs a public address to advertise.
        let mut s = ServerSettings::new();
        let err = s
            .resolve()
            .expect_err("wildcard without advertise.primary_ip");
        assert!(err.to_string().contains("advertise.primary_ip"), "{err}");
        s.set(
            "advertise.primary_ip",
            Value::Ip("203.0.113.5".parse().unwrap()),
        )
        .unwrap();
        assert!(s.resolve().is_ok());
        // Boundaries that are accepted.
        let mut s = local();
        s.set("start_delay_margin", d(1000)).unwrap();
        s.set("punch_session_grace", d(1000)).unwrap();
        s.set("offer_timeout", d(100)).unwrap();
        s.set("max_host_candidates", Value::Count(60)).unwrap();
        s.set("max_message_bytes", Value::Bytes(1024)).unwrap();
        assert!(s.resolve().is_ok());
    }

    #[test]
    fn times_and_stream_counts_stay_in_range() {
        let local = || {
            let mut s = ServerSettings::new();
            s.set(
                "listen.primary",
                Value::Address("127.0.0.1:0".parse().unwrap()),
            )
            .unwrap();
            s
        };
        let secs = |n: u64| Value::Duration(Duration::from_secs(n));
        let longest = u64::from(u32::MAX);
        // A time over 2^32 - 1 s is refused; the longest time is accepted.
        for key in ["hello_timeout", "status_log_interval", "quic.cid_lifetime"] {
            let mut s = local();
            s.set(key, secs(longest + 1)).unwrap();
            let err = s.resolve().expect_err(key);
            assert!(err.to_string().contains(key), "{err}");
            let mut s = local();
            s.set(key, secs(longest)).unwrap();
            assert!(s.resolve().is_ok(), "{key}");
        }
        assert!(matches!(
            ServerSettings::from_toml_str(&format!(
                "hello_timeout = \"{} s\"\n[listen]\nprimary = \"127.0.0.1:0\"\n",
                u64::MAX
            ))
            .unwrap()
            .resolve(),
            Err(SettingsError::OutOfRange {
                key: "hello_timeout",
                ..
            })
        ));
        // Stream counts: 1 to 2^60 bidirectional, 0 to 2^60 unidirectional.
        let streams: Vec<(&str, u64, bool)> = vec![
            ("quic.max_bidi_streams", 0, false),
            ("quic.max_bidi_streams", 1, true),
            ("quic.max_bidi_streams", 1 << 60, true),
            ("quic.max_bidi_streams", (1 << 60) + 1, false),
            ("quic.max_uni_streams", 0, true),
            ("quic.max_uni_streams", 1 << 60, true),
            ("quic.max_uni_streams", (1 << 60) + 1, false),
        ];
        for (key, n, ok) in streams {
            let mut s = local();
            s.set(key, Value::Count(n)).unwrap();
            match s.resolve() {
                Ok(_) => assert!(ok, "{key} = {n} accepted"),
                Err(e) => {
                    assert!(!ok, "{key} = {n} refused: {e}");
                    assert!(e.to_string().contains(key), "{e}");
                }
            }
        }
    }
}
