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

fn line(
    n: u64,
    events: &[P2pEvent],
    info: Option<P2pConnectionInfo>,
    error: Option<String>,
) -> Json {
    let (requested, _) = entered(events, AttemptPhase::Requesting);
    let (offered, _) = entered(events, AttemptPhase::Offered);
    let (_, punched) = entered(events, AttemptPhase::Handshaking);
    let (connected, _) = entered(events, AttemptPhase::Connected);
    let (retries, cause) = retries_and_cause(events);
    let cause = if error.is_none() { Json::Null } else { cause };
    // An attempt that failed left the connection list; its end record carries the NAT values.
    let nat = info
        .as_ref()
        .and_then(|i| i.nat)
        .or_else(|| ended_nat(events));
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

/// The NAT combination in the end record of the attempt last started in `events`: the
/// `Closed` transition of that attempt's number. `None` when it ended before `punch_go` or did
/// not end.
fn ended_nat(events: &[P2pEvent]) -> Option<NatInfo> {
    let id = events.iter().rev().find_map(|e| match e {
        P2pEvent::Transition {
            subject: Subject::Connection(id),
            from: None,
            ..
        } => Some(*id),
        _ => None,
    })?;
    events.iter().find_map(|e| match e {
        P2pEvent::Transition {
            subject: Subject::Connection(n),
            to: Phase::Attempt(AttemptPhase::Closed),
            nat,
            ..
        } if *n == id => *nat,
        _ => None,
    })
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
    for n in 1..=count {
        if Instant::now() >= end {
            break;
        }
        let _old = ep.events();
        let r = ep.connect(peer);
        let events = ep.events();
        let out = match &r {
            Ok(c) => line(n, &events, c.info(), None),
            Err(e) => line(n, &events, None, Some(e.to_string())),
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
    for n in 1..=count {
        let left = end.saturating_duration_since(Instant::now());
        let _old = ep.events();
        let conn = match ep.accept_timeout(Some(left)) {
            Ok(c) => c,
            Err(e) => {
                // The measurement counts the failure too, with the NAT values of the last
                // punch that reached this side.
                println!(
                    "{}",
                    line(n, &ep.events(), None, Some(format!("accept: {e}")))
                );
                break;
            }
        };
        // Read the information while the connection is up; the dialer closes it soon.
        let info = conn.info();
        let left = end.saturating_duration_since(Instant::now());
        let _closed = conn.wait_closed(left);
        println!("{}", line(n, &ep.events(), info, None));
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

    const NAT: NatInfo = NatInfo {
        local: NatMapping::Independent,
        peer: NatMapping::Dependent,
        mode: PunchMode::All,
    };

    /// A failed attempt's line carries the NAT combination of its end record, found by the
    /// attempt's number without watching the connection list.
    #[test]
    fn a_failed_attempt_reports_its_nat_combination() {
        let events = [
            moved(1, None, AttemptPhase::Requesting, None),
            moved(
                1,
                Some(AttemptPhase::Requesting),
                AttemptPhase::Closed,
                Some(NAT),
            ),
        ];
        let out = line(
            1,
            &events,
            None,
            Some(P2pConnectError::PunchFailed.to_string()),
        );
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["local_nat"], json!("Independent"));
        assert_eq!(out["peer_nat"], json!("Dependent"));
        assert_eq!(out["mode"], json!("All"));
        // An attempt that ended before punch_go has none.
        let before = [
            moved(2, None, AttemptPhase::Requesting, None),
            moved(
                2,
                Some(AttemptPhase::Requesting),
                AttemptPhase::Closed,
                None,
            ),
        ];
        assert_eq!(
            line(1, &before, None, Some(String::new()))["mode"],
            Json::Null
        );
        // The record of the attempt last started is taken, not one of an earlier attempt.
        let later = [
            moved(3, None, AttemptPhase::Offered, None),
            moved(
                3,
                Some(AttemptPhase::Offered),
                AttemptPhase::Closed,
                Some(NAT),
            ),
            moved(4, None, AttemptPhase::Offered, None),
            moved(4, Some(AttemptPhase::Offered), AttemptPhase::Closed, None),
        ];
        assert_eq!(ended_nat(&later), None);
        assert_eq!(ended_nat(&later[..2]), Some(NAT));
    }
}
