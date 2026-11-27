//! Example binaries and two-process integration tests of the Tasty network library
//! (docs/design/crates.md "tasty-net-demo"). Nothing here is a product API; the crate is not
//! published.
//!
//! The binaries print one event per line on stdout, `<who> <event> key=value ...`, so a person
//! can follow them and the integration tests can wait for them. How to run them by hand is in
//! `docs/examples.md`.
//!
//! | Binary | Does |
//! |--------|------|
//! | `demo-server` | the accepting end: publishes a connection record and accepts handshake v2 on a loopback port. On a forward link it provides the workspace (one Control snapshot, Data echoed); on an accepted role reversal it holds (waits for the dialer's descriptor, sends its stdin lines, prints the echo) |
//! | `demo-dialer` | reaches the server over SSH (record discovery, `-L` tunnel) as the holder (forward) or, with `--holder acceptor`, as the provider of the role reversal; connects again with backoff after a loss |
#![forbid(unsafe_code)]

use std::error::Error;
use std::ffi::OsString;
use std::io::Write;
use std::time::Duration;

use tasty_net_frame::ReadFailure;
use tasty_net_transport::LinkTimeouts;

/// What the workspace descriptor of the demo names: the providing instance and the client ID
/// the accepting end gave the link. The provider sends it as the link's first Control frame,
/// `{"snapshot":{"instance_id":"<id>","client_id":<n>}}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub instance_id: String,
    pub client_id: u32,
}

impl Snapshot {
    /// The descriptor as JSON. The instance ID is any string the handshake took, so it is
    /// serialized, never pasted between quotes.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "snapshot": {
                "instance_id": self.instance_id,
                "client_id": self.client_id,
            }
        })
        .to_string()
    }

    /// Reads a descriptor, or says why it is not one.
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_slice(payload).map_err(|e| format!("not JSON: {e}"))?;
        let snapshot = value
            .get("snapshot")
            .ok_or_else(|| "no snapshot".to_string())?;
        let instance_id = snapshot
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "no string instance_id".to_string())?;
        let client_id = snapshot
            .get("client_id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "no u32 client_id".to_string())?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            client_id,
        })
    }
}

/// Prints one event line on stdout and flushes it, so a reader of the pipe sees it at once.
pub fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    // A closed stdout (the reader went away) is not a reason to stop the demo.
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        eprintln!("{line}");
    }
}

/// The timeouts both binaries use for one link. Both ends are expected to use the same
/// values ([`LinkTimeouts`]); the read timeout is the one a test shortens.
pub fn link_timeouts(read_timeout: Duration) -> LinkTimeouts {
    LinkTimeouts {
        read_timeout,
        write_timeout: read_timeout,
        handshake_timeout: Duration::from_secs(10),
        role_proposal_timeout: Duration::from_secs(10),
        drain_cap: Duration::from_secs(1),
    }
}

/// The event name of a read failure (`dialer lost failure=<name>`).
pub fn failure_name(failure: ReadFailure) -> &'static str {
    match failure {
        ReadFailure::HeartbeatTimeout => "heartbeat_timeout",
        ReadFailure::PeerClosed => "peer_closed",
        ReadFailure::Protocol => "protocol",
        ReadFailure::Other => "other",
    }
}

/// `error` and its sources on one line, joined by `: `.
pub fn error_chain(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Command line options of the form `--name value` and `--flag`.
#[derive(Debug)]
pub struct Args {
    items: Vec<(String, Option<String>)>,
}

/// Why the command line was not understood.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for UsageError {}

impl Args {
    /// Reads `args` (without the program name). `flags` are the options that take no value;
    /// every other `--name` takes the next argument.
    pub fn parse(
        args: impl IntoIterator<Item = OsString>,
        flags: &[&str],
    ) -> Result<Self, UsageError> {
        let mut items = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg
                .into_string()
                .map_err(|a| UsageError(format!("argument {a:?} is not UTF-8")))?;
            let Some(name) = arg.strip_prefix("--") else {
                return Err(UsageError(format!("unexpected argument {arg:?}")));
            };
            if flags.contains(&name) {
                items.push((name.to_string(), None));
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| UsageError(format!("--{name} needs a value")))?
                .into_string()
                .map_err(|a| UsageError(format!("value {a:?} of --{name} is not UTF-8")))?;
            items.push((name.to_string(), Some(value)));
        }
        Ok(Self { items })
    }

    /// The last value of `--name`.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.items
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Every value of `--name`, in order.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.items
            .iter()
            .filter(|(n, _)| n == name)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }

    /// Whether the flag `--name` was given.
    pub fn flag(&self, name: &str) -> bool {
        self.items.iter().any(|(n, v)| n == name && v.is_none())
    }

    /// The value of `--name`, which must be given.
    pub fn required(&self, name: &str) -> Result<&str, UsageError> {
        self.value(name)
            .ok_or_else(|| UsageError(format!("--{name} is required")))
    }

    /// Options this program does not know, so a typo is not silently ignored.
    pub fn check_known(&self, known: &[&str]) -> Result<(), UsageError> {
        match self
            .items
            .iter()
            .find(|(n, _)| !known.contains(&n.as_str()))
        {
            Some((name, _)) => Err(UsageError(format!("unknown option --{name}"))),
            None => Ok(()),
        }
    }

    /// `--name` in milliseconds, or `default`.
    pub fn millis(&self, name: &str, default: Duration) -> Result<Duration, UsageError> {
        match self.value(name) {
            None => Ok(default),
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|&ms| ms > 0)
                .map(Duration::from_millis)
                .ok_or_else(|| {
                    UsageError(format!("--{name} needs milliseconds above 0, got {v:?}"))
                }),
        }
    }
}

/// The value of `key=` in an event line, up to the next space.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ')
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::Snapshot;

    /// An instance ID with a quote and a backslash comes back unchanged.
    #[test]
    fn a_descriptor_keeps_any_instance_id() {
        for id in ["demo-dialer-7", "desk\"east", "a\\b\\\"c", "\\u0041", ""] {
            let snapshot = Snapshot {
                instance_id: id.to_string(),
                client_id: 3,
            };
            let json = snapshot.to_json();
            assert_eq!(Snapshot::parse(json.as_bytes()), Ok(snapshot), "{json}");
        }
    }

    #[test]
    fn a_broken_descriptor_is_refused() {
        for payload in [
            &b"{\"snapshot\":{\"instance_id\":\"desk\"east\",\"client_id\":1}}"[..],
            b"{}",
            b"{\"snapshot\":{\"instance_id\":7,\"client_id\":1}}",
            b"{\"snapshot\":{\"instance_id\":\"x\",\"client_id\":-1}}",
            b"not json",
        ] {
            assert!(
                Snapshot::parse(payload).is_err(),
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn options_flags_and_repeated_values() {
        let a =
            Args::parse(args(&["--a", "1", "--f", "--o", "x", "--o", "y"]), &["f"]).expect("parse");
        assert_eq!(a.value("a"), Some("1"));
        assert!(a.flag("f"));
        assert_eq!(a.values("o"), ["x", "y"]);
        assert!(a.check_known(&["a", "f", "o"]).is_ok());
        assert!(a.check_known(&["a", "f"]).is_err());
        assert!(Args::parse(args(&["--a"]), &[]).is_err());
        assert!(Args::parse(args(&["a"]), &[]).is_err());
    }

    #[test]
    fn millis_refuses_zero_and_text() {
        let a = Args::parse(args(&["--t", "0", "--u", "x", "--v", "250"]), &[]).expect("parse");
        assert!(a.millis("t", Duration::ZERO).is_err());
        assert!(a.millis("u", Duration::ZERO).is_err());
        assert_eq!(
            a.millis("v", Duration::ZERO).expect("v"),
            Duration::from_millis(250)
        );
        assert_eq!(
            a.millis("w", Duration::from_secs(1)).expect("w"),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn field_reads_one_word() {
        let line = "dialer connected attempt=2 instance=srv-1 tunnel_pid=77";
        assert_eq!(field(line, "instance"), Some("srv-1"));
        assert_eq!(field(line, "tunnel_pid"), Some("77"));
        assert_eq!(field(line, "pid"), None);
    }
}
