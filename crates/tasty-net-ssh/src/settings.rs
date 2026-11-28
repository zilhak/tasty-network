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
use std::path::PathBuf;
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
    /// Given with [`SshSettings::set`] (or `set_text`, `set_list`).
    Configured,
    /// Read from the OS when it is queried (a text setting whose default the OS gives).
    OsDefault,
}

impl Source {
    /// `default` or `configured`.
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Configured => "configured",
            Source::OsDefault => "os_default",
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
const GIB: u64 = MIB * KIB;
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
            Value::Bytes(b) if b != 0 && b % GIB == 0 => write!(f, "{} GiB", b / GIB),
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
const CHANNEL_OPEN_TIMEOUT: &str = "embedded.channel_open_timeout";
const PROMPT_TIMEOUT: &str = "embedded.prompt_timeout";
const AUTH_TIMEOUT: &str = "embedded.auth_timeout";
const WORKER_THREADS: &str = "embedded.runtime.worker_threads";
const WINDOW_SIZE: &str = "embedded.window_size";
const MAX_PACKET_SIZE: &str = "embedded.max_packet_size";
const CHANNEL_BUFFER: &str = "embedded.channel_buffer";
const REKEY_BYTES: &str = "embedded.rekey.bytes";
const REKEY_TIME: &str = "embedded.rekey.time";
const GEX_MIN_BITS: &str = "embedded.gex.min_bits";
const GEX_PREFERRED_BITS: &str = "embedded.gex.preferred_bits";
const GEX_MAX_BITS: &str = "embedded.gex.max_bits";

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
    entry(
        "embedded.channel_open_timeout",
        Kind::Duration,
        secs(10),
        millis(100),
        secs(600),
    ),
    entry(
        "embedded.prompt_timeout",
        Kind::Duration,
        secs(300),
        secs(1),
        secs(3600),
    ),
    entry(
        "embedded.auth_timeout",
        Kind::Duration,
        secs(120),
        secs(1),
        secs(3600),
    ),
    entry(
        "embedded.runtime.worker_threads",
        Kind::Count,
        Value::Count(1),
        Value::Count(1),
        Value::Count(16),
    ),
    entry(
        "embedded.window_size",
        Kind::Bytes,
        Value::Bytes(2 * MIB),
        Value::Bytes(32 * KIB),
        Value::Bytes(64 * MIB),
    ),
    entry(
        "embedded.max_packet_size",
        Kind::Bytes,
        Value::Bytes(32 * KIB),
        Value::Bytes(KIB),
        Value::Bytes(65_535),
    ),
    entry(
        "embedded.channel_buffer",
        Kind::Count,
        Value::Count(100),
        Value::Count(1),
        Value::Count(10_000),
    ),
    entry(
        "embedded.rekey.bytes",
        Kind::Bytes,
        Value::Bytes(GIB),
        Value::Bytes(MIB),
        Value::Bytes(GIB),
    ),
    entry(
        "embedded.rekey.time",
        Kind::Duration,
        secs(3600),
        secs(60),
        secs(86_400),
    ),
    entry(
        "embedded.gex.min_bits",
        Kind::Count,
        Value::Count(3072),
        Value::Count(2048),
        Value::Count(8192),
    ),
    entry(
        "embedded.gex.preferred_bits",
        Kind::Count,
        Value::Count(8192),
        Value::Count(2048),
        Value::Count(8192),
    ),
    entry(
        "embedded.gex.max_bits",
        Kind::Count,
        Value::Count(8192),
        Value::Count(2048),
        Value::Count(8192),
    ),
];

/// The setting with `key`.
pub fn setting(key: &str) -> Option<&'static Setting> {
    DEFAULTS.iter().find(|s| s.key == key)
}

/// The backends `ssh.backend` chooses between (docs/design/ssh-backends.md "선택 규칙").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendChoice {
    /// The system `ssh` when its file is found, otherwise the embedded backend (when it is
    /// built).
    Auto,
    /// Always the system `ssh` (`system-ssh` builds).
    System,
    /// Always the embedded backend (`embedded-ssh` builds).
    Embedded,
}

impl BackendChoice {
    /// `auto`, `system` or `embedded`.
    pub fn name(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::System => "system",
            BackendChoice::Embedded => "embedded",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(BackendChoice::Auto),
            "system" => Some(BackendChoice::System),
            "embedded" => Some(BackendChoice::Embedded),
            _ => None,
        }
    }

    /// Whether this build has the backend the value names.
    fn built(self) -> bool {
        match self {
            BackendChoice::Auto => true,
            BackendChoice::System => cfg!(feature = "system-ssh"),
            BackendChoice::Embedded => cfg!(feature = "embedded-ssh"),
        }
    }
}

impl fmt::Display for BackendChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The type of a text setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// `auto`, `system` or `embedded` ([`BackendChoice`]).
    Backend,
    /// A list of `Keyword=Value` ssh options ([`SshSettings::set_list`]).
    Options,
    /// A path, or `none` for no file.
    PathOrNone,
    /// A path.
    Path,
    /// A name.
    Name,
}

/// One text setting: key, type, and the default and range as the design table writes them.
/// A default the OS gives (`embedded.home`, for example) is read when it is queried, with the
/// source [`Source::OsDefault`].
#[derive(Clone, Copy, Debug)]
pub struct TextSetting {
    /// The key.
    pub key: &'static str,
    /// The type.
    pub kind: TextKind,
    /// The default, as the design table writes it.
    pub default_text: &'static str,
    /// The accepted values, as the design table writes them.
    pub range_text: &'static str,
}

mod text {
    use super::{TextKind, TextSetting};

    pub(super) const fn entry(
        key: &'static str,
        kind: TextKind,
        default_text: &'static str,
        range_text: &'static str,
    ) -> TextSetting {
        TextSetting {
            key,
            kind,
            default_text,
            range_text,
        }
    }
}

pub(crate) const BACKEND: &str = "ssh.backend";
pub(crate) const OVERRIDES: &str = "ssh.overrides";
const CONFIG_FILE: &str = "embedded.config_file";
const SYSTEM_CONFIG_FILE: &str = "embedded.system_config_file";
const HOME: &str = "embedded.home";
const SSH_DIR: &str = "embedded.ssh_dir";
const LOCAL_USER: &str = "embedded.local_user";

/// The most entries `ssh.overrides` takes.
const OVERRIDES_MAX: usize = 64;

/// Every text setting, in the order of the design table.
pub static TEXT_DEFAULTS: &[TextSetting] = &[
    text::entry(
        "ssh.backend",
        TextKind::Backend,
        "auto",
        "auto·system·embedded",
    ),
    text::entry(
        "ssh.overrides",
        TextKind::Options,
        "빈 목록",
        "지시어=값 목록, 64개까지",
    ),
    text::entry(
        "embedded.config_file",
        TextKind::PathOrNone,
        "없음",
        "경로 하나 또는 none",
    ),
    text::entry(
        "embedded.system_config_file",
        TextKind::PathOrNone,
        "OS의 시스템 설정 경로",
        "경로 또는 none",
    ),
    text::entry("embedded.home", TextKind::Path, "OS 홈", "경로"),
    text::entry(
        "embedded.ssh_dir",
        TextKind::Path,
        "<embedded.home>/.ssh",
        "경로",
    ),
    text::entry(
        "embedded.local_user",
        TextKind::Name,
        "OS 사용자 이름",
        "문자열",
    ),
];

/// The text setting with `key`.
pub fn text_setting(key: &str) -> Option<&'static TextSetting> {
    TEXT_DEFAULTS.iter().find(|s| s.key == key)
}

/// A text setting value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextValue {
    /// `ssh.backend`.
    Backend(BackendChoice),
    /// `ssh.overrides`, in order.
    Options(Vec<String>),
    /// A path.
    Path(PathBuf),
    /// `none`: no file.
    NoFile,
    /// A name.
    Name(String),
}

impl fmt::Display for TextValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextValue::Backend(b) => f.write_str(b.name()),
            TextValue::Options(list) => f.write_str(&list.join(", ")),
            TextValue::Path(p) => write!(f, "{}", p.display()),
            TextValue::NoFile => f.write_str("none"),
            TextValue::Name(n) => f.write_str(n),
        }
    }
}

/// One line of the text settings query. `value` is `None` when there is no value (no OS home,
/// for example).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEntry {
    /// The key.
    pub key: &'static str,
    /// The value in effect.
    pub value: Option<TextValue>,
    /// Where it comes from.
    pub source: Source,
}

impl fmt::Display for TextEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            Some(v) => write!(f, "{} = {v} ({})", self.key, self.source),
            None => write!(f, "{} = - ({})", self.key, self.source),
        }
    }
}

/// Why an `ssh.overrides` entry is refused. A library setting never weakens host key checking
/// and never starts another program (docs/design/ssh-backends.md "옵션 적용 순서").
///
/// The value is judged the way `ssh` reads it: split like OpenSSH `argv_split` (quotes and
/// `\\` escapes removed, `#` starts a comment), so `StrictHostKeyChecking="no"` is `no`.
/// `ProxyCommand` and `KnownHostsCommand` take the rest of the line as written, so they are
/// judged on the written text. A keyword with a quote or a backslash, and a value whose quote
/// does not close, are refused: `ssh` would read them in a way this check does not.
fn override_refusal(option: &str) -> Option<&'static str> {
    let (keyword, raw) = split_option(option)?;
    if keyword.contains(['"', '\'', '\\']) {
        return Some("a keyword is written without quotes or escapes");
    }
    let keyword = keyword.to_ascii_lowercase();
    let raw = raw.trim().to_ascii_lowercase();
    let Some(args) = argv_split(&raw) else {
        return Some("a quote in the value does not close");
    };
    let value = args.first().map_or("", String::as_str);
    match keyword.as_str() {
        "stricthostkeychecking" if matches!(value, "no" | "off" | "false") => {
            Some("a setting does not turn host key checking off")
        }
        "nohostauthenticationforlocalhost" if matches!(value, "yes" | "true") => {
            Some("a setting does not turn host key checking off")
        }
        "proxycommand" if raw != "none" => Some("a setting does not run another program"),
        "knownhostscommand" => Some("a setting does not run another program"),
        "verifyhostkeydns" if matches!(value, "yes" | "ask" | "true") => {
            Some("the embedded backend does not implement it and refuses it")
        }
        "canonicalizehostname" if matches!(value, "yes" | "always" | "true") => {
            Some("the embedded backend does not implement it and refuses it")
        }
        "pubkeyauthentication" if value == "host-bound" => {
            Some("the embedded backend does not implement it and refuses it")
        }
        "bindinterface" => Some("the embedded backend does not implement it and refuses it"),
        "match" | "host" | "include" => Some("only options, not blocks or includes"),
        _ => unknown_keyword(&keyword),
    }
}

/// The arguments of a value as OpenSSH `argv_split` (`misc.c`) makes them with
/// `terminate_on_comment`: blanks separate, `"` and `'` quote, `\\` escapes a quote, a
/// backslash or (outside quotes) a space, and an unquoted `#` at the start of an argument ends
/// the line. `None` when a quote does not close (`SSH_ERR_INVALID_FORMAT`).
fn argv_split(text: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut args = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' || chars[i] == '\t' {
            i += 1;
            continue;
        }
        if chars[i] == '#' {
            break;
        }
        let mut arg = String::new();
        let mut quote: Option<char> = None;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if c == '\\' {
                if matches!(next, Some('\'' | '"' | '\\')) || (quote.is_none() && next == Some(' '))
                {
                    i += 1;
                    arg.push(chars[i]);
                } else {
                    arg.push(c);
                }
            } else if quote.is_none() && (c == ' ' || c == '\t') {
                break;
            } else if quote.is_none() && (c == '"' || c == '\'') {
                quote = Some(c);
            } else if quote == Some(c) {
                quote = None;
            } else {
                arg.push(c);
            }
            i += 1;
        }
        if quote.is_some() {
            return None;
        }
        args.push(arg);
    }
    Some(args)
}

/// `Keyword=Value` or `Keyword Value`, as `ssh -o` takes it.
fn split_option(option: &str) -> Option<(&str, &str)> {
    let option = option.trim();
    let end = option.find(|c: char| c == '=' || c.is_whitespace())?;
    let (keyword, rest) = option.split_at(end);
    let value = rest.trim_start().strip_prefix('=').unwrap_or(rest).trim();
    (!keyword.is_empty() && !value.is_empty()).then_some((keyword, value))
}

/// A keyword OpenSSH does not know, or one the embedded backend refuses whatever its value
/// ("보안 관련 미구현"), from the OpenSSH port's keyword table. Builds of either backend judge
/// it the same way, so a setting a system-only build takes is one every build takes.
fn unknown_keyword(keyword: &str) -> Option<&'static str> {
    use tasty_net_ssh_config::keywords::{Treatment, known_keywords};
    match known_keywords().find(|(name, _, _)| *name == keyword) {
        None => Some("OpenSSH does not know this keyword"),
        Some((_, _, Treatment::SecurityRefused)) => {
            Some("the embedded backend does not implement it and refuses it")
        }
        Some(_) => None,
    }
}

/// The OS value of a setting whose default the OS gives.
fn os_default(key: &str) -> Option<TextValue> {
    match key {
        SYSTEM_CONFIG_FILE => system_config_path().map(TextValue::Path),
        HOME => env_text(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|h| TextValue::Path(PathBuf::from(h))),
        LOCAL_USER => {
            env_text(if cfg!(windows) { "USERNAME" } else { "USER" }).map(TextValue::Name)
        }
        _ => None,
    }
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// OpenSSH's system configuration file: `/etc/ssh/ssh_config`, Windows
/// `%PROGRAMDATA%\ssh\ssh_config`. Mobile systems have none.
fn system_config_path() -> Option<PathBuf> {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        None
    } else if cfg!(windows) {
        env_text("PROGRAMDATA").map(|d| PathBuf::from(d).join("ssh").join("ssh_config"))
    } else {
        Some(PathBuf::from("/etc/ssh/ssh_config"))
    }
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
    /// The values of `embedded.gex.min_bits`, `preferred_bits` and `max_bits` would not keep
    /// `min ≤ preferred ≤ max`.
    GexOrder {
        /// The three values the change would give.
        min: u64,
        preferred: u64,
        max: u64,
    },
    /// A text value outside the accepted values (a backend this build does not have, an
    /// empty path, a refused or malformed `ssh.overrides` entry, too many entries).
    TextOutOfRange {
        /// The key.
        key: &'static str,
        /// The value (for `ssh.overrides`, the refused entry).
        value: String,
        /// Why.
        reason: String,
    },
    /// The setting takes a list ([`SshSettings::set_list`]) or a single text
    /// ([`SshSettings::set_text`]), not the other.
    WrongTextType {
        /// The key.
        key: &'static str,
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
            SettingsError::GexOrder {
                min,
                preferred,
                max,
            } => write!(
                f,
                "embedded.gex.* must keep min <= preferred <= max (would be {min}, {preferred}, {max})"
            ),
            SettingsError::TextOutOfRange { key, value, reason } => {
                write!(f, "setting {key} does not take {value:?}: {reason}")
            }
            SettingsError::WrongTextType { key } => {
                write!(f, "setting {key} is not set this way")
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
    text: BTreeMap<&'static str, TextValue>,
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
        let previous = self.configured.insert(s.key, value);
        let (min, preferred, max) = self.gex_bits_raw();
        if !(min <= preferred && preferred <= max) {
            match previous {
                Some(p) => self.configured.insert(s.key, p),
                None => self.configured.remove(s.key),
            };
            return Err(SettingsError::GexOrder {
                min,
                preferred,
                max,
            });
        }
        Ok(self)
    }

    /// Set one text setting (`ssh.backend`, `embedded.*` paths and names). A path setting
    /// that allows it takes `none` for no file.
    pub fn set_text(&mut self, key: &str, value: &str) -> Result<&mut Self, SettingsError> {
        let s = text_setting(key).ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        let refuse = |reason: &str| SettingsError::TextOutOfRange {
            key: s.key,
            value: value.to_owned(),
            reason: reason.to_owned(),
        };
        let parsed = match s.kind {
            TextKind::Backend => match BackendChoice::parse(value) {
                Some(b) if b.built() => TextValue::Backend(b),
                Some(_) => return Err(refuse("this build does not have that backend")),
                None => return Err(refuse("not auto, system or embedded")),
            },
            TextKind::Options => return Err(SettingsError::WrongTextType { key: s.key }),
            TextKind::PathOrNone if value == "none" => TextValue::NoFile,
            TextKind::PathOrNone | TextKind::Path if value.is_empty() => {
                return Err(refuse("an empty path"));
            }
            TextKind::PathOrNone | TextKind::Path => TextValue::Path(PathBuf::from(value)),
            TextKind::Name if value.is_empty() => return Err(refuse("an empty name")),
            TextKind::Name => TextValue::Name(value.to_owned()),
        };
        self.text.insert(s.key, parsed);
        Ok(self)
    }

    /// Set a list setting (`ssh.overrides`: `Keyword=Value` entries, as `ssh -o` takes them).
    /// It refuses, whichever backend runs, entries that turn host key checking off
    /// (`StrictHostKeyChecking=no`/`off`, `NoHostAuthenticationForLocalhost=yes`), that run
    /// another program (`ProxyCommand`, `KnownHostsCommand`), the security directives the
    /// embedded backend does not implement, and keywords OpenSSH does not know. Builds of
    /// either backend, a system-only build included, refuse the same entries: the keyword table
    /// of the OpenSSH port is in every build.
    pub fn set_list(&mut self, key: &str, values: &[&str]) -> Result<&mut Self, SettingsError> {
        let s = text_setting(key).ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        if s.kind != TextKind::Options {
            return Err(SettingsError::WrongTextType { key: s.key });
        }
        if values.len() > OVERRIDES_MAX {
            return Err(SettingsError::TextOutOfRange {
                key: s.key,
                value: format!("{} entries", values.len()),
                reason: format!("at most {OVERRIDES_MAX} entries"),
            });
        }
        for v in values {
            let reason = match split_option(v) {
                None => Some("not Keyword=Value"),
                Some(_) => override_refusal(v),
            };
            if let Some(reason) = reason {
                return Err(SettingsError::TextOutOfRange {
                    key: s.key,
                    value: (*v).to_owned(),
                    reason: reason.to_owned(),
                });
            }
        }
        self.text.insert(
            s.key,
            TextValue::Options(values.iter().map(|v| (*v).to_owned()).collect()),
        );
        Ok(self)
    }

    /// The text value in effect for `key` and its source.
    pub fn get_text(&self, key: &str) -> Option<TextEntry> {
        let s = text_setting(key)?;
        if let Some(v) = self.text.get(s.key) {
            return Some(TextEntry {
                key: s.key,
                value: Some(v.clone()),
                source: Source::Configured,
            });
        }
        let (value, source) = match s.key {
            BACKEND => (
                Some(TextValue::Backend(BackendChoice::Auto)),
                Source::Default,
            ),
            OVERRIDES => (Some(TextValue::Options(Vec::new())), Source::Default),
            CONFIG_FILE => (None, Source::Default),
            SSH_DIR => (
                self.embedded_home()
                    .map(|h| TextValue::Path(h.join(".ssh"))),
                Source::Default,
            ),
            other => match os_default(other) {
                Some(v) => (Some(v), Source::OsDefault),
                None => (None, Source::Default),
            },
        };
        Some(TextEntry {
            key: s.key,
            value,
            source,
        })
    }

    /// Every text setting with the value in effect and its source, in [`TEXT_DEFAULTS`] order.
    pub fn text_entries(&self) -> Vec<TextEntry> {
        TEXT_DEFAULTS
            .iter()
            .filter_map(|s| self.get_text(s.key))
            .collect()
    }

    /// `ssh.backend`.
    pub fn backend(&self) -> BackendChoice {
        match self.text.get(BACKEND) {
            Some(TextValue::Backend(b)) => *b,
            _ => BackendChoice::Auto,
        }
    }

    /// `ssh.overrides`, in order.
    pub fn overrides(&self) -> &[String] {
        match self.text.get(OVERRIDES) {
            Some(TextValue::Options(list)) => list,
            _ => &[],
        }
    }

    fn text_path(&self, key: &str) -> Option<TextValue> {
        self.get_text(key).and_then(|e| e.value)
    }

    /// `embedded.config_file`: `None` reads the user and the system file, `Some(None)` reads
    /// none (`none`), `Some(Some(path))` reads only that file (`ssh -F`).
    pub fn embedded_config_file(&self) -> Option<Option<PathBuf>> {
        match self.text_path(CONFIG_FILE) {
            Some(TextValue::Path(p)) => Some(Some(p)),
            Some(TextValue::NoFile) => Some(None),
            _ => None,
        }
    }

    /// `embedded.system_config_file` (`None`: no system file).
    pub fn embedded_system_config_file(&self) -> Option<PathBuf> {
        match self.text_path(SYSTEM_CONFIG_FILE) {
            Some(TextValue::Path(p)) => Some(p),
            _ => None,
        }
    }

    /// `embedded.home` (`None`: not known).
    pub fn embedded_home(&self) -> Option<PathBuf> {
        match self.text.get(HOME).cloned().or_else(|| os_default(HOME)) {
            Some(TextValue::Path(p)) => Some(p),
            _ => None,
        }
    }

    /// `embedded.ssh_dir` (`None`: neither it nor the home is known).
    pub fn embedded_ssh_dir(&self) -> Option<PathBuf> {
        match self.text_path(SSH_DIR) {
            Some(TextValue::Path(p)) => Some(p),
            _ => None,
        }
    }

    /// `embedded.local_user` (`None`: not known).
    pub fn embedded_local_user(&self) -> Option<String> {
        match self.text_path(LOCAL_USER) {
            Some(TextValue::Name(n)) => Some(n),
            _ => None,
        }
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

    fn gex_bits_raw(&self) -> (u64, u64, u64) {
        (
            self.count(GEX_MIN_BITS),
            self.count(GEX_PREFERRED_BITS),
            self.count(GEX_MAX_BITS),
        )
    }

    fn bytes(&self, key: &'static str) -> usize {
        match self.get(key).map(|e| e.value) {
            // Every size range ends at 1 GiB or below, which fits usize on every platform.
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

    /// `embedded.channel_open_timeout`: how long the embedded backend waits for the answer to
    /// a channel open.
    pub fn channel_open_timeout(&self) -> Duration {
        self.duration(CHANNEL_OPEN_TIMEOUT)
    }

    /// `embedded.prompt_timeout`: how long the embedded backend waits for an `SshPrompt`
    /// answer.
    pub fn prompt_timeout(&self) -> Duration {
        self.duration(PROMPT_TIMEOUT)
    }

    /// `embedded.auth_timeout`: how long the embedded backend gives the key exchange and the
    /// authentication of one host, from the end of the banner exchange (`SshPrompt` time does
    /// not count). `ssh.connect_timeout` bounds only the TCP connection and the banner
    /// exchange, as the ssh `ConnectTimeout` does; OpenSSH leaves the rest to the server's
    /// `LoginGraceTime`, whose default this one has.
    pub fn auth_timeout(&self) -> Duration {
        self.duration(AUTH_TIMEOUT)
    }

    /// `embedded.runtime.worker_threads`: the worker threads of the embedded backend's runtime.
    pub fn worker_threads(&self) -> usize {
        // The range (at most 16) fits usize.
        usize::try_from(self.count(WORKER_THREADS)).unwrap_or(usize::MAX)
    }

    /// `embedded.window_size`: the russh `Config::window_size`.
    pub fn window_size(&self) -> u32 {
        // The range (at most 64 MiB) fits u32.
        u32::try_from(self.bytes(WINDOW_SIZE)).unwrap_or(u32::MAX)
    }

    /// `embedded.max_packet_size`: the russh `Config::maximum_packet_size`.
    pub fn max_packet_size(&self) -> u32 {
        // The range (at most 65535) fits u32.
        u32::try_from(self.bytes(MAX_PACKET_SIZE)).unwrap_or(u32::MAX)
    }

    /// `embedded.channel_buffer`: the russh `Config::channel_buffer_size`.
    pub fn channel_buffer(&self) -> usize {
        // The range (at most 10 000) fits usize.
        usize::try_from(self.count(CHANNEL_BUFFER)).unwrap_or(usize::MAX)
    }

    /// `embedded.rekey.bytes`: the russh `Limits` read and write limits.
    pub fn rekey_bytes(&self) -> usize {
        self.bytes(REKEY_BYTES)
    }

    /// `embedded.rekey.time`: the russh `Limits::rekey_time_limit`.
    pub fn rekey_time(&self) -> Duration {
        self.duration(REKEY_TIME)
    }

    /// `embedded.gex.min_bits`, `preferred_bits`, `max_bits` (`min ≤ preferred ≤ max`).
    pub fn gex_bits(&self) -> (usize, usize, usize) {
        let (min, preferred, max) = self.gex_bits_raw();
        // The ranges (at most 8192) fit usize.
        let to = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        (to(min), to(preferred), to(max))
    }
}

// These tests drive the system backend's helpers; the default build runs them.
#[cfg(all(test, feature = "system-ssh"))]
mod tests;
