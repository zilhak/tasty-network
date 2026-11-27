//! What the caller can see: the endpoint status and the information of every connection and
//! attempt in progress (docs/design/p2p.md "연결 정보").

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use tasty_net_identity::DeviceId;
use tasty_net_rendezvous::wire::message::{Limits, NatMapping, PunchMode};

use crate::error::StopCause;
use crate::events::{AttemptPhase, Cause, EndpointPhase, Events, P2pEvent, Phase, Subject};
use crate::nat::StunAnswer;
use crate::settings::{Source, Value};
use crate::socket::RemoteCandidate;

/// A rendezvous server: its address and its public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RendezvousServer {
    /// `host:port` as a socket address.
    pub addr: SocketAddr,
    /// The server's ed25519 public key, compared with the key the TLS handshake shows.
    pub public_key: DeviceId,
}

/// Which side of a connection this device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// A: asked for the connection and opened QUIC.
    Dialer,
    /// B: received the offer and accepted QUIC.
    Acceptor,
}

impl Side {
    /// `dialer` or `acceptor`.
    pub fn name(self) -> &'static str {
        match self {
            Side::Dialer => "dialer",
            Side::Acceptor => "acceptor",
        }
    }
}

/// The NAT values of one punch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NatInfo {
    /// This device's judgement.
    pub local: NatMapping,
    /// The peer's, from `punch_go.peer_nat`.
    pub peer: NatMapping,
    /// `all` or `host_only`.
    pub mode: PunchMode,
}

/// The two addresses of a QUIC handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressPair {
    /// This side: the address the socket is bound to (may be a wildcard).
    pub local: Option<SocketAddr>,
    /// The peer: for A the address whose probe succeeded, for B the address the Initial came
    /// from.
    pub remote: SocketAddr,
}

/// Where the attempt is, with the values of its state (docs/design/p2p.md "연결 시도 상태").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttemptInfo {
    /// The punch attempt number, from 1 (0 before the first `punch_go` on B).
    pub number: u32,
    /// In `RetryWait`: the time to the next attempt.
    pub retry_in: Option<Duration>,
    /// The time left of the attempt's whole bound (A).
    pub bound_in: Option<Duration>,
    /// In `Punching`: the peer's candidates now, peer reflexive ones included.
    pub candidates: Vec<RemoteCandidate>,
    /// In `Punching`: the time to the end of the probe window.
    pub window_end_in: Option<Duration>,
    /// In `Handshaking`: the addresses of the handshake.
    pub handshake_pair: Option<AddressPair>,
    /// In `Handshaking`: the time to the handshake deadline.
    pub handshake_deadline_in: Option<Duration>,
}

/// A value applied to one connection, with its source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppliedSetting {
    /// The setting key, or for a server value the message field (`punch_go.window_ms`).
    pub key: &'static str,
    /// The value.
    pub value: Value,
    /// Where it comes from.
    pub source: Source,
}

/// One entry of a connection's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The state before (`None` at the start).
    pub from: Option<AttemptPhase>,
    /// The state after.
    pub to: AttemptPhase,
    /// Why.
    pub cause: Cause,
    /// When.
    pub at: SystemTime,
}

/// One connection or attempt (docs/design/p2p.md "연결 정보" table).
#[derive(Clone, Debug, PartialEq)]
pub struct P2pConnectionInfo {
    /// A number unique in this endpoint that does not change: one attempt (with its punch
    /// retries) and the connection it makes share it.
    pub id: u64,
    /// The peer's device ID.
    pub peer: DeviceId,
    /// Dialer (A) or acceptor (B).
    pub side: Side,
    /// The state.
    pub state: AttemptPhase,
    /// The address the socket is bound to (may be a wildcard).
    pub local_bind: Option<SocketAddr>,
    /// The local IP this connection uses. B fills it when quinn knows it; A (QUIC client) always
    /// leaves it unknown with quinn 0.11.12.
    pub local_ip: Option<IpAddr>,
    /// This device's reflexive address as the server told it.
    pub local_reflexive: Option<SocketAddr>,
    /// The peer address quinn uses now. Empty before the connection.
    pub remote_addr: Option<SocketAddr>,
    /// The candidate that succeeded and its kind.
    pub remote_candidate: Option<RemoteCandidate>,
    /// The server that introduced the two devices.
    pub rendezvous: Option<RendezvousServer>,
    /// Both mappings and the mode.
    pub nat: Option<NatInfo>,
    /// The attempt number and the times left.
    pub attempt: AttemptInfo,
    /// quinn's RTT estimate.
    pub rtt: Option<Duration>,
    /// The cause of the last transition.
    pub last_cause: Option<Cause>,
    /// Transitions and observed remote address changes, at most `history.limit`.
    pub history: Vec<HistoryEntry>,
    /// History entries dropped because of `history.limit`.
    pub history_dropped: u64,
    /// The values applied to this connection.
    pub applied: Vec<AppliedSetting>,
}

/// The endpoint's state and registration (docs/design/p2p.md "endpoint" table).
#[derive(Clone, Debug, PartialEq)]
pub struct P2pStatus {
    /// The state.
    pub state: EndpointPhase,
    /// The server.
    pub server: Option<RendezvousServer>,
    /// The local socket address.
    pub local_addr: Option<SocketAddr>,
    /// The reflexive address the server saw.
    pub reflexive: Option<SocketAddr>,
    /// The NAT judgement.
    pub nat: NatMapping,
    /// The answers of the last judgement.
    pub nat_answers: Vec<StunAnswer>,
    /// The server values (`welcome.limits`).
    pub limits: Option<Limits>,
    /// The server value that does not fit this device's settings, if any.
    pub limits_mismatch: Option<&'static str>,
    /// When the registration began.
    pub registered_at: Option<SystemTime>,
    /// The reconnect attempt number.
    pub reconnect_attempt: u64,
    /// In `Backoff`: the time to the next attempt.
    pub next_attempt_in: Option<Duration>,
    /// The cause of the last transition.
    pub last_cause: Option<Cause>,
    /// In `Stopped`: why.
    pub stopped: Option<StopCause>,
    /// The bound of one connection attempt with the server's values: per punch attempt
    /// `punch.request_timeout + start_delay_max + window + handshake`, between attempts
    /// `max(punch.retry_delay, nat.stun_rto × nat.stun_attempts)`.
    pub attempt_bound: Option<Duration>,
}

/// One tracked attempt.
#[derive(Debug)]
pub(crate) struct Rec {
    pub(crate) info: P2pConnectionInfo,
    pub(crate) retry_at: Option<Instant>,
    pub(crate) bound_at: Option<Instant>,
    /// The punch's probe session, for the candidates while `Punching`.
    pub(crate) probe: Option<Arc<crate::socket::ProbeSession>>,
    /// The end of the probe window.
    pub(crate) window_end_at: Option<Instant>,
    /// The handshake's addresses and deadline.
    pub(crate) handshake: Option<(AddressPair, Instant)>,
    pub(crate) conn: Option<quinn::Connection>,
    history: VecDeque<HistoryEntry>,
}

/// The table of attempts and connections.
#[derive(Debug)]
pub(crate) struct Tracker {
    records: Mutex<BTreeMap<u64, Arc<Mutex<Rec>>>>,
    next: AtomicU64,
    events: Arc<Events>,
    history_limit: usize,
}

/// A tracked attempt.
#[derive(Clone, Debug)]
pub(crate) struct Tracked {
    pub(crate) id: u64,
    rec: Arc<Mutex<Rec>>,
    tracker: Arc<Tracker>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Tracker {
    pub(crate) fn new(events: Arc<Events>, history_limit: usize) -> Self {
        Tracker {
            records: Mutex::default(),
            next: AtomicU64::new(u64::MIN),
            events,
            history_limit,
        }
    }

    /// Start tracking an attempt in `phase`.
    pub(crate) fn start(
        self: &Arc<Self>,
        side: Side,
        peer: DeviceId,
        phase: AttemptPhase,
        cause: Cause,
        fill: impl FnOnce(&mut P2pConnectionInfo),
    ) -> Tracked {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut info = P2pConnectionInfo {
            id,
            peer,
            side,
            state: phase,
            local_bind: None,
            local_ip: None,
            local_reflexive: None,
            remote_addr: None,
            remote_candidate: None,
            rendezvous: None,
            nat: None,
            attempt: AttemptInfo::default(),
            rtt: None,
            last_cause: None,
            history: Vec::new(),
            history_dropped: u64::MIN,
            applied: Vec::new(),
        };
        fill(&mut info);
        let rec = Arc::new(Mutex::new(Rec {
            info,
            retry_at: None,
            bound_at: None,
            probe: None,
            window_end_at: None,
            handshake: None,
            conn: None,
            history: VecDeque::new(),
        }));
        let t = Tracked {
            id,
            rec,
            tracker: self.clone(),
        };
        t.record(&mut lock(&t.rec), None, phase, cause);
        lock(&self.records).insert(id, t.rec.clone());
        t
    }

    /// Every tracked attempt and connection.
    pub(crate) fn snapshot(&self) -> Vec<P2pConnectionInfo> {
        let records: Vec<_> = lock(&self.records).values().cloned().collect();
        let now = Instant::now();
        records
            .iter()
            .map(|r| {
                let r = lock(r);
                let mut info = r.info.clone();
                info.history = r.history.iter().cloned().collect();
                info.attempt.retry_in = r.retry_at.map(|t| t.saturating_duration_since(now));
                info.attempt.bound_in = r.bound_at.map(|t| t.saturating_duration_since(now));
                match info.state {
                    AttemptPhase::Punching => {
                        info.attempt.candidates =
                            r.probe.as_ref().map(|p| p.candidates()).unwrap_or_default();
                        info.attempt.window_end_in =
                            r.window_end_at.map(|t| t.saturating_duration_since(now));
                    }
                    AttemptPhase::Handshaking => {
                        if let Some((pair, at)) = r.handshake {
                            info.attempt.handshake_pair = Some(pair);
                            info.attempt.handshake_deadline_in =
                                Some(at.saturating_duration_since(now));
                        }
                    }
                    _ => {}
                }
                if let Some(c) = &r.conn {
                    info.rtt = Some(c.rtt());
                    info.remote_addr = Some(crate::socket::canonical(c.remote_address()));
                    info.local_ip = info
                        .local_ip
                        .or_else(|| c.local_ip().map(|ip| ip.to_canonical()));
                }
                info
            })
            .collect()
    }
}

impl Tracked {
    /// Record the move to `to` in `r` and queue its event, under the record's lock: the events
    /// of one attempt come in the order of its transitions (the lock order is the record, then
    /// the queue).
    fn record(&self, r: &mut Rec, from: Option<AttemptPhase>, to: AttemptPhase, cause: Cause) {
        let at = SystemTime::now();
        r.info.state = to;
        r.info.last_cause = Some(cause.clone());
        if r.history.len() >= self.tracker.history_limit {
            r.history.pop_front();
            r.info.history_dropped += 1;
        }
        r.history.push_back(HistoryEntry {
            from,
            to,
            cause: cause.clone(),
            at,
        });
        self.tracker.events.push(P2pEvent::Transition {
            subject: Subject::Connection(self.id),
            from: from.map(Phase::Attempt),
            to: Phase::Attempt(to),
            cause,
            at,
        });
    }

    /// The state now.
    pub(crate) fn phase(&self) -> AttemptPhase {
        lock(&self.rec).info.state
    }

    /// Move to `to`. `Closed` also stops tracking.
    pub(crate) fn to(&self, to: AttemptPhase, cause: Cause) {
        {
            let mut r = lock(&self.rec);
            if r.info.state == AttemptPhase::Closed {
                return;
            }
            let from = r.info.state;
            self.record(&mut r, Some(from), to, cause);
            if to != AttemptPhase::Closed {
                return;
            }
            r.conn = None;
        }
        lock(&self.tracker.records).remove(&self.id);
    }

    /// Change the values.
    pub(crate) fn update(&self, f: impl FnOnce(&mut Rec)) {
        f(&mut lock(&self.rec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Threads moving one attempt at once: each event starts where the one before it ended,
    /// and the record's history and state agree with the events.
    #[test]
    fn attempt_events_follow_the_order_of_the_transitions() {
        let events = Arc::new(Events::new(100_000));
        let tracker = Arc::new(Tracker::new(events.clone(), 100_000));
        let peer = tasty_net_identity::DeviceKey::from_seed([7; 32]).id();
        let t = tracker.start(
            Side::Dialer,
            peer,
            AttemptPhase::Requesting,
            Cause::named("connect_called"),
            |_| {},
        );
        let phases = [
            AttemptPhase::Scheduled,
            AttemptPhase::Punching,
            AttemptPhase::RetryWait,
        ];
        std::thread::scope(|s| {
            for to in phases {
                let t = &t;
                s.spawn(move || {
                    for _ in 0..2_000 {
                        t.to(to, Cause::named("punch_go"));
                    }
                });
            }
        });
        let got: Vec<(Option<Phase>, Phase)> = events
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                P2pEvent::Transition { from, to, .. } => Some((from, to)),
                P2pEvent::Lagged { .. } => None,
            })
            .collect();
        assert_eq!(got.len(), 6_001);
        assert_eq!(got[0], (None, Phase::Attempt(AttemptPhase::Requesting)));
        for w in got.windows(2) {
            assert_eq!(w[1].0, Some(w[0].1), "{w:?}");
        }
        let info = &tracker.snapshot()[0];
        assert_eq!(got.last().map(|e| e.1), Some(Phase::Attempt(info.state)));
        let history: Vec<_> = info.history.iter().map(|h| (h.from, h.to)).collect();
        let events: Vec<_> = got
            .iter()
            .map(|(from, to)| {
                let attempt = |p: &Phase| match p {
                    Phase::Attempt(a) => *a,
                    Phase::Endpoint(_) => panic!("an endpoint phase"),
                };
                (from.as_ref().map(attempt), attempt(to))
            })
            .collect();
        assert_eq!(history, events);
        t.to(AttemptPhase::Closed, Cause::named("cancelled"));
        assert!(tracker.snapshot().is_empty());
    }
}
