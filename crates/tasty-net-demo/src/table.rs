//! The connection table as event lines (docs/design/transport.md "연결 목록"), and their
//! reading back for the tests.
//!
//! On the stdin command `table` both binaries print a snapshot of their
//! [`ConnectionTable`](tasty_net_transport::ConnectionTable) as one block of lines, written at
//! once so no other event line falls inside it:
//!
//! | Line | Says |
//! |------|------|
//! | `<who> table device=<own device ID> entries=<n>` | the snapshot starts; `n` entries follow |
//! | `<who> conn id=<n> ...` | one entry, with the keys of [`ENTRY_KEYS`] |
//! | `<who> link conn=<id> ...` | one attach link of the entry `conn`, with the keys of [`LINK_KEYS`] |
//! | `<who> table-end entries=<n>` | the snapshot ends |
//!
//! Every value is one word: a value that is not there is `-`, and any byte outside
//! `A-Z a-z 0-9 . _ : / , @ + -` (a space, `=`, `%`, a quote) is written as `%XX`
//! ([`encode_value`]); a value that is `-` itself is written `%2D`. So [`crate::field`] reads
//! every key, and [`decode_value`] gives the value back.
//!
//! Segments come as `segments=<names, comma separated>` and, per segment,
//! `seg.<name>.local=` and `seg.<name>.remote=`. The history is
//! `history=<state>/<phase>/<cause>,...`, oldest first, with `-` for a missing phase or
//! cause.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tasty_net_transport::{ConnectionInfo, LinkInfo, Side, Via};

/// The keys of a `conn` line, in the order they are printed.
pub const ENTRY_KEYS: &[&str] = &[
    "id",
    "previous",
    "previous_dropped",
    "parent",
    "peer_device",
    "peer_instance",
    "instance_verified",
    "path",
    "direction",
    "local_side",
    "state",
    "phase",
    "last_cause",
    "cause_text",
    "via",
    "via_target",
    "via_remote_port",
    "segments",
    "history",
    "links",
    "bulk",
    "requests",
    "rtt_ms",
    "redial_attempt",
];

/// The keys of a `link` line, in the order they are printed.
pub const LINK_KEYS: &[&str] = &[
    "conn",
    "link_id",
    "holder",
    "local_side",
    "transport",
    "opened_at_ms",
    "rtt_ms",
];

/// The word of a value that is not there.
const NONE: &str = "-";

/// Whether `byte` stays as it is in a value.
fn plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._:/,@+-".contains(&byte)
}

/// One value as one word (the module text).
pub fn encode_value(text: &str) -> String {
    if text == NONE {
        return "%2D".into();
    }
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if plain(byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The value of a word [`encode_value`] wrote, `None` for `-`. A broken `%XX` or bytes that
/// are not UTF-8 give `None` too.
pub fn decode_value(word: &str) -> Option<String> {
    if word == NONE {
        return None;
    }
    let bytes = word.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = word.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn optional(value: Option<String>) -> String {
    value.map_or_else(|| NONE.to_string(), |v| encode_value(&v))
}

/// `dialer` or `acceptor`.
pub fn side_name(side: Side) -> &'static str {
    match side {
        Side::Dialer => "dialer",
        Side::Acceptor => "acceptor",
    }
}

fn millis(duration: Option<Duration>) -> String {
    duration.map_or_else(|| NONE.to_string(), |d| d.as_millis().to_string())
}

fn unix_millis(at: SystemTime) -> String {
    at.duration_since(UNIX_EPOCH)
        .map_or_else(|_| NONE.to_string(), |d| d.as_millis().to_string())
}

/// The lines of one snapshot of `entries` (the module text). `device` is this device's ID.
pub fn table_lines(who: &str, device: &str, entries: &[ConnectionInfo]) -> Vec<String> {
    let mut lines = vec![format!(
        "{who} table device={} entries={}",
        encode_value(device),
        entries.len()
    )];
    for entry in entries {
        lines.push(entry_line(who, entry));
        lines.extend(entry.links.iter().map(|link| link_line(who, entry, link)));
    }
    lines.push(format!("{who} table-end entries={}", entries.len()));
    lines
}

fn entry_line(who: &str, entry: &ConnectionInfo) -> String {
    let (via, target, port) = match &entry.via {
        Via::Ssh {
            target,
            remote_port,
        } => (
            "ssh",
            Some(target.clone()),
            remote_port.map(|p| p.to_string()),
        ),
        Via::Loopback => ("loopback", None, None),
        _ => ("other", None, None),
    };
    let segments: Vec<&str> = entry.segments.iter().map(|s| s.name).collect();
    let history: Vec<String> = entry
        .history
        .iter()
        .map(|h| {
            format!(
                "{}/{}/{}",
                h.state.as_str(),
                h.phase.map_or(NONE, |p| p.as_str()),
                h.cause.as_ref().map_or(NONE, |c| c.as_str())
            )
        })
        .collect();
    let mut words = vec![
        format!("{who} conn"),
        format!("id={}", entry.id),
        format!(
            "previous={}",
            optional(entry.previous.map(|p| p.to_string()))
        ),
        format!("previous_dropped={}", entry.previous_dropped),
        format!("parent={}", optional(entry.parent.map(|p| p.to_string()))),
        format!(
            "peer_device={}",
            optional(entry.peer.device_id.map(|d| d.to_string()))
        ),
        format!("peer_instance={}", optional(entry.peer.instance_id.clone())),
        format!("instance_verified={}", entry.peer.instance_verified),
        format!("path={}", entry.path.as_str()),
        format!("direction={}", entry.direction.as_str()),
        format!("local_side={}", side_name(entry.local_side)),
        format!("state={}", entry.state.as_str()),
        format!("phase={}", entry.phase.map_or(NONE, |p| p.as_str())),
        format!(
            "last_cause={}",
            entry.last_cause.as_ref().map_or(NONE, |c| c.as_str())
        ),
        format!(
            "cause_text={}",
            optional(entry.last_cause.as_ref().map(ToString::to_string))
        ),
        format!("via={via}"),
        format!("via_target={}", optional(target)),
        format!("via_remote_port={}", optional(port)),
        format!(
            "segments={}",
            optional((!segments.is_empty()).then(|| segments.join(",")))
        ),
    ];
    for segment in &entry.segments {
        words.push(format!(
            "seg.{}.local={}",
            segment.name,
            optional(segment.local.clone())
        ));
        words.push(format!(
            "seg.{}.remote={}",
            segment.name,
            optional(segment.remote.clone())
        ));
    }
    words.extend([
        format!(
            "history={}",
            optional((!history.is_empty()).then(|| history.join(",")))
        ),
        format!("links={}", entry.links.len()),
        format!("bulk={}", entry.bulk_channels),
        format!("requests={}", entry.request_channels),
        format!("rtt_ms={}", millis(entry.rtt)),
        format!(
            "redial_attempt={}",
            optional(entry.redial.as_ref().map(|r| r.attempt.to_string()))
        ),
    ]);
    words.join(" ")
}

fn link_line(who: &str, entry: &ConnectionInfo, link: &LinkInfo) -> String {
    format!(
        "{who} link conn={} link_id={} holder={} local_side={} transport={} opened_at_ms={} \
         rtt_ms={}",
        entry.id,
        link.link_id,
        link.holder.as_str(),
        side_name(link.local_side),
        link.transport.as_str(),
        unix_millis(link.opened_at),
        millis(link.rtt)
    )
}

/// One line of a snapshot read back: its decoded values by key. A key with `-` is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    values: HashMap<String, String>,
}

impl Row {
    fn parse(words: &str) -> Result<Self, String> {
        let mut values = HashMap::new();
        for word in words.split(' ').filter(|w| !w.is_empty()) {
            let (key, value) = word
                .split_once('=')
                .ok_or_else(|| format!("{word:?} is not key=value"))?;
            if value != NONE {
                let value =
                    decode_value(value).ok_or_else(|| format!("{word:?} is not a value"))?;
                values.insert(key.to_string(), value);
            }
        }
        Ok(Self { values })
    }

    /// The value of `key`, if it is there.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The value of `key`; panics when it is not there.
    pub fn value(&self, key: &str) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("no {key} in {self:?}"))
    }

    /// The value of `key` as a number; panics when it is not one.
    pub fn number(&self, key: &str) -> u64 {
        self.value(key)
            .parse()
            .unwrap_or_else(|_| panic!("{key} is not a number in {self:?}"))
    }
}

/// An entry read back, with its attach links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryView {
    pub row: Row,
    pub links: Vec<Row>,
}

impl EntryView {
    /// The entry's `id`.
    pub fn id(&self) -> u64 {
        self.row.number("id")
    }

    /// The entry's value of `key`, if it is there.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.row.get(key)
    }

    /// The entry's value of `key`; panics when it is not there.
    pub fn value(&self, key: &str) -> &str {
        self.row.value(key)
    }

    /// The `(state, phase, cause)` steps of the history, oldest first.
    pub fn history(&self) -> Vec<(String, Option<String>, Option<String>)> {
        self.get("history")
            .map(|text| {
                text.split(',')
                    .map(|step| {
                        let mut parts = step.split('/').map(|p| p.to_string());
                        let mut next = || parts.next().filter(|p| p != NONE);
                        let state = next().unwrap_or_default();
                        (state, next(), next())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A snapshot read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableView {
    /// The device ID of the end that printed it.
    pub device: String,
    pub entries: Vec<EntryView>,
}

impl TableView {
    /// Reads the lines of one snapshot of `who`, from its `table` line to its `table-end`
    /// line. Lines of another kind among them are an error.
    pub fn parse(who: &str, lines: &[String]) -> Result<Self, String> {
        let begin = format!("{who} table ");
        let conn = format!("{who} conn ");
        let link = format!("{who} link ");
        let end = format!("{who} table-end ");
        let mut lines = lines.iter();
        let first = lines.next().ok_or("no lines")?;
        let head = Row::parse(
            first
                .strip_prefix(&begin)
                .ok_or_else(|| format!("{first:?} does not start a snapshot"))?,
        )?;
        let device = head.get("device").ok_or("no device")?.to_string();
        let count: usize = head
            .get("entries")
            .and_then(|n| n.parse().ok())
            .ok_or("no entry count")?;
        let mut entries: Vec<EntryView> = Vec::new();
        for line in lines {
            if let Some(words) = line.strip_prefix(&conn) {
                entries.push(EntryView {
                    row: Row::parse(words)?,
                    links: Vec::new(),
                });
            } else if let Some(words) = line.strip_prefix(&link) {
                let row = Row::parse(words)?;
                let owner = entries
                    .last_mut()
                    .filter(|e| e.get("id") == row.get("conn"))
                    .ok_or_else(|| format!("{line:?} follows no entry of its own"))?;
                owner.links.push(row);
            } else if line.starts_with(&end) {
                if entries.len() != count {
                    return Err(format!("{count} entries announced, {} read", entries.len()));
                }
                return Ok(Self { device, entries });
            } else {
                return Err(format!("{line:?} is not a snapshot line"));
            }
        }
        Err("no table-end line".into())
    }

    /// The entry `id`, if the snapshot has it.
    pub fn entry(&self, id: u64) -> Option<&EntryView> {
        self.entries.iter().find(|e| e.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_with_any_text_stay_one_word() {
        for text in [
            "plain",
            "with space",
            "a=b",
            "100%",
            "-",
            "--",
            "127.0.0.1:22, 127.0.0.1:23",
            "quote\"and\\",
            "한글",
            "",
        ] {
            let word = encode_value(text);
            assert!(!word.contains(' ') && !word.contains('='), "{word}");
            assert_eq!(decode_value(&word).as_deref(), Some(text), "{word}");
        }
        assert_eq!(decode_value("-"), None);
        assert_eq!(decode_value("%2"), None);
        assert_eq!(decode_value("%zz"), None);
    }

    #[test]
    fn a_snapshot_reads_back() {
        let lines: Vec<String> = [
            "dialer table device=ab entries=2",
            "dialer conn id=1 previous=- cause_text=tunnel_failed:%20no history=connecting/tunnel/-,disconnected/-/tunnel_failed",
            "dialer conn id=2 previous=1 links=1",
            "dialer link conn=2 link_id=ff holder=dialer",
            "dialer table-end entries=2",
        ]
        .map(String::from)
        .to_vec();
        let view = TableView::parse("dialer", &lines).expect("parse");
        assert_eq!(view.device, "ab");
        assert_eq!(view.entries.len(), 2);
        let first = view.entry(1).expect("1");
        assert_eq!(first.get("previous"), None);
        assert_eq!(first.get("cause_text"), Some("tunnel_failed: no"));
        assert_eq!(
            first.history(),
            [
                ("connecting".into(), Some("tunnel".into()), None),
                ("disconnected".into(), None, Some("tunnel_failed".into())),
            ]
        );
        let second = view.entry(2).expect("2");
        assert_eq!(second.value("previous"), "1");
        assert_eq!(second.links.len(), 1);
        assert_eq!(second.links[0].value("holder"), "dialer");

        let mut short = lines.clone();
        short.remove(2);
        assert!(
            TableView::parse("dialer", &short).is_err(),
            "a link without its entry"
        );
        let mut unended = lines;
        unended.pop();
        assert!(TableView::parse("dialer", &unended).is_err());
    }
}
