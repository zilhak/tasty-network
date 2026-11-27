//! The punching measurement of docs/design/p2p.md "펀칭 실험": repeated connections between two
//! devices through a rendezvous server, one JSON line per attempt.
//!
//! ```text
//! punch_probe id --key-dir <dir>
//! punch_probe --listen --key-dir <dir> --server <addr> --server-key <id> --peer <id> [options]
//! punch_probe --dial <id> --key-dir <dir> --server <addr> --server-key <id> [options]
//!
//! options:
//!   --count <n>             attempts (dialer) or connections to take (listener), default 10
//!   --pause <ms>            the wait between two attempts, default 1000
//!   --probe-interval <ms>   the device setting punch.probe_interval (default: the setting's)
//!   --timeout <s>           the bound of the whole run, default 600
//!   --host-ip <ip>          a host address to report besides the ones the endpoint finds
//!                           (P2pConfig::host_ips), may be given more than once
//! ```
//!
//! Each line has the attempt number, the result, the time from the request to the punch
//! success (`Handshaking`), the time from there to the QUIC connection, the candidate that
//! worked, both NAT mappings and the failure reason. The last line is a summary. The output
//! carries addresses: when writing it up in a public document, keep only the network kinds
//! (docs/design/p2p.md "펀칭 실험").

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value as Json, json};
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{
    AttemptPhase, NatInfo, P2pCloseReason, P2pConfig, P2pConnectionInfo, P2pEndpoint, P2pEvent,
    PairingList, Phase, RendezvousServer, Subject,
};

const USAGE: &str = "usage: punch_probe id --key-dir <dir>
       punch_probe --listen --key-dir <dir> --server <addr> --server-key <id> --peer <id> [options]
       punch_probe --dial <id> --key-dir <dir> --server <addr> --server-key <id> [options]
options: --count <n> --pause <ms> --probe-interval <ms> --timeout <s> --host-ip <ip>";

/// The default number of attempts.
const DEFAULT_COUNT: u64 = 10;
/// The default wait between two attempts.
const DEFAULT_PAUSE: Duration = Duration::from_secs(1);
/// The default bound of a run.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the server registration before the first attempt.
const REGISTER_LIMIT: Duration = Duration::from_secs(30);
/// The poll interval while waiting for the registration.
const REGISTER_POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Args {
    id: bool,
    listen: bool,
    dial: Option<DeviceId>,
    key_dir: Option<PathBuf>,
    server: Option<SocketAddr>,
    server_key: Option<DeviceId>,
    peer: Option<DeviceId>,
    host_ips: Vec<IpAddr>,
    count: Option<u64>,
    pause: Option<Duration>,
    probe_interval: Option<Duration>,
    timeout: Option<Duration>,
}

fn number(name: &str, v: &str) -> Result<u64, String> {
    v.parse().map_err(|e| format!("{name} {v}: {e}"))
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "id" => a.id = true,
            "--listen" => a.listen = true,
            "--dial" => a.dial = Some(id(&value("--dial")?)?),
            "--key-dir" => a.key_dir = Some(PathBuf::from(value("--key-dir")?)),
            "--server" => {
                let v = value("--server")?;
                a.server = Some(v.parse().map_err(|e| format!("--server {v}: {e}"))?);
            }
            "--server-key" => a.server_key = Some(id(&value("--server-key")?)?),
            "--peer" => a.peer = Some(id(&value("--peer")?)?),
            "--host-ip" => {
                let v = value("--host-ip")?;
                a.host_ips
                    .push(v.parse().map_err(|e| format!("--host-ip {v}: {e}"))?);
            }
            "--count" => a.count = Some(number("--count", &value("--count")?)?),
            "--pause" => {
                let ms = number("--pause", &value("--pause")?)?;
                a.pause = Some(Duration::from_millis(ms));
            }
            "--probe-interval" => {
                let ms = number("--probe-interval", &value("--probe-interval")?)?;
                a.probe_interval = Some(Duration::from_millis(ms));
            }
            "--timeout" => {
                let s = number("--timeout", &value("--timeout")?)?;
                a.timeout = Some(Duration::from_secs(s));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn id(s: &str) -> Result<DeviceId, String> {
    s.parse().map_err(|e| format!("device ID {s}: {e}"))
}

fn need<T>(v: Option<T>, name: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("{name} is missing"))
}

fn endpoint(a: &Args, peer: DeviceId) -> Result<P2pEndpoint, String> {
    let dir = need(a.key_dir.clone(), "--key-dir")?;
    let key = DeviceKey::load_or_create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(key, pairing);
    config.enabled = true;
    config.key_dir = Some(dir);
    config.server = Some(RendezvousServer {
        addr: need(a.server, "--server")?,
        public_key: need(a.server_key, "--server-key")?,
    });
    config.host_ips = a.host_ips.clone();
    if let Some(p) = a.probe_interval {
        config
            .settings
            .set("punch.probe_interval", Value::Duration(p));
    }
    let ep = P2pEndpoint::bind(config).map_err(|e| format!("bind: {e}"))?;
    let end = Instant::now() + REGISTER_LIMIT;
    while ep.status().reflexive.is_none() && Instant::now() < end {
        std::thread::sleep(REGISTER_POLL);
    }
    let status = ep.status();
    eprintln!(
        "punch_probe: {} state={} nat={:?} reflexive={:?}",
        ep.id(),
        status.state.name(),
        status.nat,
        status.reflexive
    );
    Ok(ep)
}

/// When an attempt state was first and last entered, from the events of one attempt.
fn entered(events: &[P2pEvent], phase: AttemptPhase) -> (Option<SystemTime>, Option<SystemTime>) {
    let times: Vec<SystemTime> = events
        .iter()
        .filter_map(|e| match e {
            P2pEvent::Transition {
                to: Phase::Attempt(p),
                at,
                ..
            } if *p == phase => Some(*at),
            _ => None,
        })
        .collect();
    (times.first().copied(), times.last().copied())
}

fn millis(from: Option<SystemTime>, to: Option<SystemTime>) -> Json {
    match (from, to) {
        (Some(a), Some(b)) => b
            .duration_since(a)
            .map_or(Json::Null, |d| json!(d.as_secs_f64() * 1e3)),
        _ => Json::Null,
    }
}

/// The punch retries and the closing cause, from the events of one attempt.
fn retries_and_cause(events: &[P2pEvent]) -> (usize, Json) {
    let retries = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                P2pEvent::Transition {
                    to: Phase::Attempt(AttemptPhase::RetryWait),
                    ..
                }
            )
        })
        .count();
    let cause = events.iter().rev().find_map(|e| match e {
        P2pEvent::Transition {
            to: Phase::Attempt(AttemptPhase::Closed),
            cause,
            ..
        } => Some(json!(cause.to_string())),
        _ => None,
    });
    (retries, cause.unwrap_or(Json::Null))
}

/// One line: `info` of a connection that stands, or `ended` (the NAT combination of the end
/// record of the attempt measured) of one that failed.
fn line(
    n: u64,
    events: &[P2pEvent],
    info: Option<P2pConnectionInfo>,
    ended: Option<NatInfo>,
    error: Option<String>,
) -> Json {
    let (requested, _) = entered(events, AttemptPhase::Requesting);
    let (offered, _) = entered(events, AttemptPhase::Offered);
    let (_, punched) = entered(events, AttemptPhase::Handshaking);
    let (connected, _) = entered(events, AttemptPhase::Connected);
    let (retries, cause) = retries_and_cause(events);
    let cause = if error.is_none() { Json::Null } else { cause };
    // An attempt that failed left the connection list; its end record carries the NAT values.
    let nat = info.as_ref().and_then(|i| i.nat).or(ended);
    let candidate = info.as_ref().and_then(|i| i.remote_candidate);
    json!({
        "n": n,
        "ok": error.is_none(),
        "request_to_punched_ms": millis(requested.or(offered), punched),
        "punched_to_connected_ms": millis(punched, connected),
        "candidate": candidate.map(|c| c.kind.name()),
        "candidate_addr": candidate.map(|c| c.addr.to_string()),
        "local_nat": nat.map(|n| format!("{:?}", n.local)),
        "peer_nat": nat.map(|n| format!("{:?}", n.peer)),
        "mode": nat.map(|n| format!("{:?}", n.mode)),
        "punch_retries": retries,
        "error": error,
        "closed_cause": cause,
    })
}

/// The attempts of one side, read from the events as they come, so the attempt a line
/// measures keeps its number across reads: its start may be read before the wait and its end
/// after, and attempts of the other side, of other devices or later ones may come in between.
/// A line reads its values only from the events of the attempt it measures.
struct Attempts {
    /// The phase the measured attempts start in: `Requesting` for the dialer, `Offered` for
    /// the listener.
    start: AttemptPhase,
    /// The attempts that started in that phase, in order.
    started: Vec<u64>,
    /// Offers of other devices: the listener pairs only `--peer`, so an offer it refuses with
    /// `not_paired` came from another device and is not measured.
    others: std::collections::HashSet<u64>,
    /// The events of each attempt without a line yet, in order.
    log: std::collections::HashMap<u64, Vec<P2pEvent>>,
    /// Every attempt up to this number has its line.
    reported: Option<u64>,
}

impl Attempts {
    fn new(start: AttemptPhase) -> Self {
        Attempts {
            start,
            started: Vec::new(),
            others: std::collections::HashSet::new(),
            log: std::collections::HashMap::new(),
            reported: None,
        }
    }

    /// Take in `events`.
    fn take(&mut self, events: &[P2pEvent]) {
        for e in events {
            let P2pEvent::Transition {
                subject: Subject::Connection(id),
                from,
                to: Phase::Attempt(to),
                cause,
                ..
            } = e
            else {
                continue;
            };
            if self.reported.is_some_and(|r| *id <= r) {
                continue;
            }
            if from.is_none() && *to == self.start {
                self.started.push(*id);
            }
            if *to == AttemptPhase::Closed
                && self.start == AttemptPhase::Offered
                && cause.name == "not_paired"
            {
                self.others.insert(*id);
            }
            self.log.entry(*id).or_default().push(e.clone());
        }
    }

    /// The attempt a line measures: the first one that started, is not another device's and
    /// has no line yet.
    fn measured(&self) -> Option<u64> {
        self.started
            .iter()
            .copied()
            .find(|id| !self.others.contains(id) && self.reported.is_none_or(|r| *id > r))
    }

    /// The line of the connection `id` that stood, with its information if its record is
    /// still on the list. The dial and the listen call this. The attempt is marked by the
    /// number from `P2pConnection::id`, which stays after the record left the list, so the
    /// next failure does not measure it again.
    fn stood(&mut self, n: u64, id: u64, info: Option<P2pConnectionInfo>) -> Json {
        let events = self.close(Some(id));
        line(n, &events, info, None, None)
    }

    /// The line of a failure (`error`): the attempt measured, its events and the NAT
    /// combination of its end record.
    fn failed(&mut self, n: u64, error: String) -> Json {
        let events = self.close(self.measured());
        let nat = events.iter().find_map(|e| match e {
            P2pEvent::Transition {
                to: Phase::Attempt(AttemptPhase::Closed),
                nat,
                ..
            } => *nat,
            _ => None,
        });
        line(n, &events, None, nat, Some(error))
    }

    /// Attempt `id` and every one before it have their line; the events of `id` for it.
    fn close(&mut self, id: Option<u64>) -> Vec<P2pEvent> {
        let Some(id) = id else {
            return Vec::new();
        };
        let events = self.log.remove(&id).unwrap_or_default();
        let r = self.reported.map_or(id, |r| r.max(id));
        self.reported = Some(r);
        self.log.retain(|k, _| *k > r);
        self.started.retain(|k| *k > r);
        events
    }
}

/// The middle and the largest value.
fn median_max(mut v: Vec<f64>) -> (Json, Json) {
    if v.is_empty() {
        return (Json::Null, Json::Null);
    }
    v.sort_by(f64::total_cmp);
    (json!(v[v.len() / 2]), json!(v[v.len() - 1]))
}

fn dial(a: &Args, peer: DeviceId, end: Instant) -> Result<(u64, u64), String> {
    let ep = endpoint(a, peer)?;
    let count = a.count.unwrap_or(DEFAULT_COUNT);
    let pause = a.pause.unwrap_or(DEFAULT_PAUSE);
    let mut ok = 0;
    let mut times = Vec::new();
    let mut attempts = Attempts::new(AttemptPhase::Requesting);
    for n in 1..=count {
        if Instant::now() >= end {
            break;
        }
        attempts.take(&ep.events());
        let r = ep.connect(peer);
        let events = ep.events();
        attempts.take(&events);
        let out = match &r {
            Ok(c) => attempts.stood(n, c.id(), c.info()),
            // The attempt this connect started: the first Requesting without a line.
            Err(e) => attempts.failed(n, e.to_string()),
        };
        if let (Some(ms), true) = (out["request_to_punched_ms"].as_f64(), r.is_ok()) {
            times.push(ms);
        }
        println!("{out}");
        if let Ok(c) = r {
            ok += 1;
            c.close(P2pCloseReason::Closed);
        }
        if n < count {
            std::thread::sleep(pause);
        }
    }
    let (median, max) = median_max(times);
    println!(
        "{}",
        json!({"summary": true, "ok": ok, "attempts": count, "request_to_punched_ms_median": median, "request_to_punched_ms_max": max})
    );
    ep.close();
    Ok((ok, count))
}

fn listen(a: &Args, end: Instant) -> Result<(u64, u64), String> {
    let peer = need(a.peer, "--peer")?;
    let ep = endpoint(a, peer)?;
    let count = a.count.unwrap_or(DEFAULT_COUNT);
    let mut taken = 0;
    let mut attempts = Attempts::new(AttemptPhase::Offered);
    for n in 1..=count {
        let left = end.saturating_duration_since(Instant::now());
        // The events before the wait still count: an offer may have started already.
        attempts.take(&ep.events());
        let conn = match ep.accept_timeout(Some(left)) {
            Ok(c) => c,
            Err(e) => {
                // The measurement counts the failure too, with the values of the offer it
                // measures: the first one of the peer that started and has no line.
                attempts.take(&ep.events());
                println!("{}", attempts.failed(n, format!("accept: {e}")));
                break;
            }
        };
        // Read the information while the connection is up; the dialer closes it soon.
        let info = conn.info();
        let left = end.saturating_duration_since(Instant::now());
        let _closed = conn.wait_closed(left);
        attempts.take(&ep.events());
        println!("{}", attempts.stood(n, conn.id(), info));
        taken += 1;
    }
    ep.close();
    Ok((taken, count))
}

fn run() -> Result<bool, String> {
    let a = parse(std::env::args().skip(1))?;
    if a.id {
        let dir = need(a.key_dir, "--key-dir")?;
        let key = DeviceKey::load_or_create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        println!("{}", key.id());
        return Ok(true);
    }
    let end = Instant::now() + a.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let (ok, count) = match (a.listen, a.dial) {
        (true, None) => listen(&a, end)?,
        (false, Some(peer)) => dial(&a, peer, end)?,
        _ => return Err("give one of id, --listen or --dial".to_owned()),
    };
    Ok(ok == count)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("punch_probe: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_p2p::{Cause, P2pConnectError};
    use tasty_net_rendezvous::wire::message::{NatMapping, PunchMode};

    fn moved(
        id: u64,
        from: Option<AttemptPhase>,
        to: AttemptPhase,
        nat: Option<NatInfo>,
    ) -> P2pEvent {
        P2pEvent::Transition {
            subject: Subject::Connection(id),
            from: from.map(Phase::Attempt),
            to: Phase::Attempt(to),
            cause: Cause::named("punch_window_elapsed"),
            at: SystemTime::UNIX_EPOCH,
            nat,
        }
    }

    /// The listener's refusal of an offer from a device it does not pair.
    fn refused(id: u64) -> P2pEvent {
        P2pEvent::Transition {
            subject: Subject::Connection(id),
            from: Some(Phase::Attempt(AttemptPhase::Offered)),
            to: Phase::Attempt(AttemptPhase::Closed),
            cause: Cause::named("not_paired"),
            at: SystemTime::UNIX_EPOCH,
            nat: None,
        }
    }

    const NAT: NatInfo = NatInfo {
        local: NatMapping::Independent,
        peer: NatMapping::Dependent,
        mode: PunchMode::All,
    };

    /// A transition of attempt `id` at `ms` milliseconds, caused by `cause`.
    fn at(
        id: u64,
        from: Option<AttemptPhase>,
        to: AttemptPhase,
        ms: u64,
        cause: &'static str,
        nat: Option<NatInfo>,
    ) -> P2pEvent {
        P2pEvent::Transition {
            subject: Subject::Connection(id),
            from: from.map(Phase::Attempt),
            to: Phase::Attempt(to),
            cause: Cause::named(cause),
            at: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(ms),
            nat,
        }
    }

    /// A failed dial's line carries the NAT combination of the end record of the attempt it
    /// started, also when an offer of another device starts and ends in between with none.
    #[test]
    fn a_failed_dial_reports_the_nat_combination_of_its_own_attempt() {
        let mut attempts = Attempts::new(AttemptPhase::Requesting);
        attempts.take(&[
            moved(1, None, AttemptPhase::Requesting, None),
            moved(2, None, AttemptPhase::Offered, None),
            moved(2, Some(AttemptPhase::Offered), AttemptPhase::Closed, None),
            moved(
                1,
                Some(AttemptPhase::Requesting),
                AttemptPhase::Closed,
                Some(NAT),
            ),
        ]);
        let out = attempts.failed(1, P2pConnectError::PunchFailed.to_string());
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["local_nat"], json!("Independent"));
        assert_eq!(out["peer_nat"], json!("Dependent"));
        assert_eq!(out["mode"], json!("All"));
        // The next dial measures its own attempt, not the reported one.
        attempts.take(&[
            moved(3, None, AttemptPhase::Requesting, None),
            moved(
                3,
                Some(AttemptPhase::Requesting),
                AttemptPhase::Closed,
                None,
            ),
        ]);
        assert_eq!(attempts.measured(), Some(3));
        assert_eq!(attempts.failed(2, String::new())["mode"], Json::Null);
    }

    /// A failed listen's line carries the NAT combination of the offer it measures, when the
    /// offer's start was read before the wait and its end after it, and when a later offer
    /// starts in between.
    #[test]
    fn a_failed_listen_reports_the_nat_combination_of_the_offer_it_measures() {
        let mut attempts = Attempts::new(AttemptPhase::Offered);
        // Read before the wait: the offer started.
        attempts.take(&[moved(4, None, AttemptPhase::Offered, None)]);
        // Read after the wait: a later offer started, then the measured one ended.
        attempts.take(&[
            moved(5, None, AttemptPhase::Offered, None),
            moved(
                4,
                Some(AttemptPhase::Offered),
                AttemptPhase::Closed,
                Some(NAT),
            ),
        ]);
        assert_eq!(attempts.measured(), Some(4));
        let out = attempts.failed(1, String::new());
        assert_eq!(out["mode"], json!("All"));
        assert_eq!(out["local_nat"], json!("Independent"));
        // An offer that ended before punch_go has none.
        attempts.take(&[moved(
            5,
            Some(AttemptPhase::Offered),
            AttemptPhase::Closed,
            None,
        )]);
        assert_eq!(attempts.measured(), Some(5));
        assert_eq!(attempts.failed(2, String::new())["mode"], Json::Null);
    }

    /// A failed listen measures only offers of `--peer`: an offer of another device refused
    /// before the wait is not the one measured, and the offer of the peer that ends with NAT
    /// values during the wait gives its line those values.
    #[test]
    fn a_failed_listen_skips_the_offers_of_other_devices() {
        let mut attempts = Attempts::new(AttemptPhase::Offered);
        // Before the wait: another device's offer, refused.
        attempts.take(&[moved(6, None, AttemptPhase::Offered, None), refused(6)]);
        // During the wait: the peer's offer starts and ends with its NAT values.
        attempts.take(&[
            moved(7, None, AttemptPhase::Offered, None),
            moved(
                7,
                Some(AttemptPhase::Punching),
                AttemptPhase::Closed,
                Some(NAT),
            ),
        ]);
        assert_eq!(attempts.measured(), Some(7));
        let out = attempts.failed(1, String::new());
        assert_eq!(out["local_nat"], json!("Independent"));
        assert_eq!(out["peer_nat"], json!("Dependent"));
        assert_eq!(out["mode"], json!("All"));
    }

    /// Every value of a failed listen's line comes from the peer's offer, when offers of other
    /// devices come in the same batch during the wait: one before the peer's (its `Offered`
    /// first in the batch) and one refused after the peer's offer ended.
    #[test]
    fn a_failed_listen_line_takes_every_value_from_the_peer_offer() {
        use AttemptPhase::{Closed, Handshaking, Offered, Punching};
        let mut attempts = Attempts::new(Offered);
        attempts.take(&[
            at(10, None, Offered, 1_000, "punch_offer", None),
            at(10, Some(Offered), Closed, 1_000, "not_paired", None),
            at(11, None, Offered, 2_000, "punch_offer", None),
            at(11, Some(Offered), Punching, 2_500, "start_elapsed", None),
            at(11, Some(Punching), Handshaking, 3_000, "punched", None),
            at(
                11,
                Some(Handshaking),
                Closed,
                4_000,
                "handshake_timeout",
                Some(NAT),
            ),
            at(12, None, Offered, 4_500, "punch_offer", None),
            at(12, Some(Offered), Closed, 4_500, "not_paired", None),
        ]);
        let out = attempts.failed(1, "accept: timed out".into());
        assert_eq!(out["request_to_punched_ms"], json!(1_000.0));
        assert_eq!(
            out["closed_cause"],
            json!(Cause::named("handshake_timeout").to_string())
        );
        assert_eq!(out["local_nat"], json!("Independent"));
        assert_eq!(out["peer_nat"], json!("Dependent"));
        assert_eq!(out["mode"], json!("All"));
        assert_eq!(out["punch_retries"], json!(0));
    }

    /// A connection that stood marks its attempt by its number through the path the dial and
    /// the listen use, also when its record has left the list (`info()` is `None`): its line
    /// reads its own events, and the next failure does not take that connection's NAT values.
    #[test]
    fn a_failure_after_a_connection_does_not_take_its_nat_combination() {
        use AttemptPhase::{Closed, Connected, Handshaking, Requesting};
        let mut attempts = Attempts::new(Requesting);
        attempts.take(&[
            at(1, None, Requesting, 1_000, "connect_called", None),
            at(1, Some(Requesting), Handshaking, 1_400, "punched", None),
            at(
                1,
                Some(Handshaking),
                Connected,
                1_500,
                "quic_established",
                None,
            ),
        ]);
        // The connection's number, as P2pConnection::id gives it, and no record left.
        let out = attempts.stood(1, 1, None);
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["request_to_punched_ms"], json!(400.0));
        assert_eq!(out["punched_to_connected_ms"], json!(100.0));
        // Its end record comes with the next dial's, which fails before punch_go.
        attempts.take(&[
            at(1, Some(Connected), Closed, 2_000, "closed", Some(NAT)),
            at(2, None, Requesting, 3_000, "connect_called", None),
            at(2, Some(Requesting), Closed, 3_500, "request_timeout", None),
        ]);
        assert_eq!(attempts.measured(), Some(2));
        let out = attempts.failed(2, P2pConnectError::PunchFailed.to_string());
        assert_eq!(out["local_nat"], Json::Null);
        assert_eq!(out["peer_nat"], Json::Null);
        assert_eq!(out["mode"], Json::Null);
        assert_eq!(
            out["closed_cause"],
            json!(Cause::named("request_timeout").to_string())
        );
    }
}
