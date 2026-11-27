//! Server state: registrations and punch sessions (docs/design/rendezvous.md "서버 상태").
//!
//! Everything is in memory. Every message the server sends to a device is a
//! [`ServerMessage`] the server builds from this state; nothing a device sent is passed on
//! except the closed set of normalised values in docs/design/rendezvous.md "중계 없음".

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use crate::settings::ServerParams;
use crate::wire::CloseCode;
use crate::wire::candidate::{
    CandidateKind, CandidateLimits, candidate_allowed, parse_address, select_candidates,
};
use crate::wire::message::{
    AnswerReason, CancelReason, NatMapping, NatReport, PunchAnswer, PunchCancel, PunchGo,
    PunchMode, PunchOffer, PunchRefused, PunchRequest, PunchResult, PunchToken, RefuseReason,
    ResultReason, Role, ServerMessage,
};

/// Counters behind [`super::ServerStatus`].
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub busy_max_devices: AtomicU64,
    pub busy_max_punches: AtomicU64,
    pub busy_per_device: AtomicU64,
    pub busy_request_rate: AtomicU64,
    pub busy_pair: AtomicU64,
    pub stun_answered: AtomicU64,
    pub stun_rate_limited: AtomicU64,
    pub stun_sources_full: AtomicU64,
    pub stun_dropped: AtomicU64,
    pub candidates_rejected: AtomicU64,
    pub candidates_truncated: AtomicU64,
    pub registrations_total: AtomicU64,
    pub superseded: AtomicU64,
    pub access_denied: AtomicU64,
    pub pending_refused: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

fn add(c: &AtomicU64, n: usize) {
    c.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// One registered device.
pub(crate) struct Registration {
    pub conn_id: u64,
    pub conn: quinn::Connection,
    /// The send queue of the connection, `max_queued_messages` long.
    pub tx: Sender<ServerMessage>,
    /// Counts connections closed because their send queue was full.
    pub queue_full: Arc<AtomicU64>,
    pub reflexive: SocketAddr,
    /// The STUN addresses this device was told in `welcome.stun`.
    pub advertised: Vec<SocketAddr>,
    pub reported_reflexive: Vec<SocketAddr>,
    pub host: Vec<SocketAddr>,
    pub nat: NatMapping,
    pub registered_at: Instant,
    /// When the last valid frame of the coordination stream arrived (the `hello` at first).
    /// QUIC keepalives are not frames: `idle_timeout` ends a silent connection.
    pub last_frame_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Offered,
    GoSent,
}

/// One punch in progress.
pub(crate) struct Punch {
    pub a: DeviceId,
    pub b: DeviceId,
    pub request_id: u64,
    pub stage: Stage,
    pub mode: PunchMode,
    pub token_a: PunchToken,
    pub token_b: PunchToken,
    pub created: Instant,
    pub go_at: Option<Instant>,
    /// `None` when the deadline is past the clock's range: the punch does not expire.
    pub expires: Option<Instant>,
    pub result_a: Option<PunchResult>,
    pub result_b: Option<PunchResult>,
    pub timer: Option<AbortHandle>,
}

/// What the caller has to arrange after a state change: a deadline timer for a punch.
#[must_use]
pub(crate) enum After {
    Nothing,
    Timer {
        punch_id: u64,
        stage: Stage,
        at: Instant,
    },
}

/// The whole state.
pub(crate) struct State {
    pub devices: HashMap<DeviceId, Registration>,
    pub punches: HashMap<u64, Punch>,
    /// The `punch_request` times of each device ID within `punch_request_window`. Kept across
    /// registrations of the same ID, so a new connection does not reset the rate.
    request_times: HashMap<DeviceId, VecDeque<Instant>>,
    next_punch_id: u64,
    next_conn_id: u64,
    /// `punch_cancel{expired}` messages sent.
    pub expired_cancels: u64,
}

/// Why a registration was not made.
pub(crate) enum RegisterRefusal {
    Busy,
}

/// Queue `msg` for `reg`. A full queue means the device does not read what the server sends:
/// its connection is closed as `busy` and the registration is removed when the connection ends.
pub(crate) fn send(reg: &Registration, msg: ServerMessage) {
    match reg.tx.try_send(msg) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            if reg.conn.close_reason().is_none() {
                bump(&reg.queue_full);
                info!(
                    code = CloseCode::Busy.name(),
                    "send queue full, connection closed"
                );
            }
            reg.conn
                .close(CloseCode::Busy.var_int(), CloseCode::Busy.reason());
        }
        // A closed writer means the connection is ending; its registration is removed then.
        Err(TrySendError::Closed(_)) => {
            debug!("writer of a closing connection dropped a message");
        }
    }
}

fn cancel_to(reg: Option<&Registration>, punch_id: u64, reason: CancelReason) {
    if let Some(reg) = reg {
        send(
            reg,
            ServerMessage::PunchCancel(PunchCancel { punch_id, reason }),
        );
    }
}

fn refuse_to(reg: Option<&Registration>, request_id: u64, reason: RefuseReason) {
    if let Some(reg) = reg {
        send(
            reg,
            ServerMessage::PunchRefused(PunchRefused { request_id, reason }),
        );
    }
}

/// A timer for `punch_id` at `at`, none when the deadline is past the clock's range.
fn timer(punch_id: u64, stage: Stage, at: Option<Instant>) -> After {
    match at {
        Some(at) => After::Timer {
            punch_id,
            stage,
            at,
        },
        None => After::Nothing,
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl State {
    pub fn new() -> Self {
        State {
            devices: HashMap::new(),
            punches: HashMap::new(),
            request_times: HashMap::new(),
            next_punch_id: 1,
            next_conn_id: 1,
            expired_cancels: u64::MIN,
        }
    }

    pub fn new_conn_id(&mut self) -> u64 {
        let id = self.next_conn_id;
        self.next_conn_id += 1;
        id
    }

    /// Register `device`. A registration of the same ID is replaced and its connection
    /// closed as `superseded`. Returns the replaced registration's connection.
    pub fn register(
        &mut self,
        p: &ServerParams,
        counters: &Counters,
        device: DeviceId,
        reg: Registration,
    ) -> Result<Option<quinn::Connection>, RegisterRefusal> {
        if !self.devices.contains_key(&device) && self.devices.len() >= p.max_devices {
            bump(&counters.busy_max_devices);
            return Err(RegisterRefusal::Busy);
        }
        let old = self.remove_device(device, None, CancelReason::PeerGone);
        if old.is_some() {
            bump(&counters.superseded);
        }
        bump(&counters.registrations_total);
        self.devices.insert(device, reg);
        Ok(old)
    }

    /// Remove `device` (only when its connection is `conn_id`, if given) and end its punches:
    /// the other side of each gets `punch_cancel{reason}`. Returns the removed connection.
    pub fn remove_device(
        &mut self,
        device: DeviceId,
        conn_id: Option<u64>,
        reason: CancelReason,
    ) -> Option<quinn::Connection> {
        if conn_id.is_some_and(|id| self.devices.get(&device).is_some_and(|r| r.conn_id != id)) {
            return None;
        }
        let reg = self.devices.remove(&device)?;
        info!(
            device = %device,
            registered_ms = millis(reg.registered_at.elapsed()),
            since_last_frame_ms = millis(reg.last_frame_at.elapsed()),
            "registration removed"
        );
        let ids: Vec<u64> = self
            .punches
            .iter()
            .filter(|(_, s)| s.a == device || s.b == device)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(s) = self.end_punch(id) {
                let other = if s.a == device { s.b } else { s.a };
                cancel_to(self.devices.get(&other), id, reason);
            }
        }
        Some(reg.conn)
    }

    fn end_punch(&mut self, punch_id: u64) -> Option<Punch> {
        let s = self.punches.remove(&punch_id)?;
        if let Some(t) = &s.timer {
            t.abort();
        }
        Some(s)
    }

    /// `nat_report`.
    pub fn nat_report(
        &mut self,
        p: &ServerParams,
        counters: &Counters,
        device: DeviceId,
        report: NatReport,
    ) {
        let Some(reg) = self.devices.get_mut(&device) else {
            return;
        };
        reg.nat = report.mapping;
        let mut reported = Kept::new(stored_reported(p));
        let mut rejected = 0;
        for r in &report.stun_results {
            let server = parse_address(&r.server);
            let Some(text) = &r.reflexive else {
                continue;
            };
            match (server, parse_address(text)) {
                (Some(server), Some(addr))
                    if reg.advertised.contains(&server)
                        && candidate_allowed(CandidateKind::ReportedReflexive, addr)
                        && addr.is_ipv4() == reg.reflexive.is_ipv4() =>
                {
                    reported.push(addr);
                }
                _ => rejected += 1,
            }
        }
        let mut truncated = reported.truncated;
        reg.reported_reflexive = reported.addrs;
        if let Some(host) = &report.host_candidates {
            let host = filter_host(host, stored_host(p));
            reg.host = host.addrs;
            rejected += host.rejected;
            truncated += host.truncated;
        }
        add(&counters.candidates_rejected, rejected);
        add(&counters.candidates_truncated, truncated);
        info!(device = %device, mapping = ?reg.nat, "nat report");
        debug!(device = %device, reported = ?reg.reported_reflexive, host = ?reg.host, "nat report addresses");
    }

    /// `punch_request` from `a`.
    pub fn punch_request(
        &mut self,
        p: &ServerParams,
        counters: &Counters,
        a: DeviceId,
        req: PunchRequest,
        now: Instant,
    ) -> After {
        let refuse = |state: &State, reason| {
            refuse_to(state.devices.get(&a), req.request_id, reason);
            info!(device = %a, peer = %req.peer, reason = ?reason, "punch request refused");
            After::Nothing
        };
        if !self.devices.contains_key(&a) {
            return After::Nothing;
        }
        if !self.request_allowed(p, a, now) {
            bump(&counters.busy_request_rate);
            return refuse(self, RefuseReason::Busy);
        }
        let b = req.peer;
        if b == a {
            return refuse(self, RefuseReason::SelfPeer);
        }
        if !self.devices.contains_key(&b) {
            return refuse(self, RefuseReason::NotFound);
        }
        // One session per pair: the same A replaces its old session; the other direction is
        // busy.
        let existing: Vec<u64> = self
            .punches
            .iter()
            .filter(|(_, s)| (s.a == a && s.b == b) || (s.a == b && s.b == a))
            .map(|(id, _)| *id)
            .collect();
        for id in existing {
            if self.punches.get(&id).is_some_and(|s| s.a == a) {
                self.end_punch(id);
                cancel_to(self.devices.get(&b), id, CancelReason::Superseded);
                info!(device = %a, peer = %b, punch_id = id, "punch superseded by a new request");
            } else {
                bump(&counters.busy_pair);
                return refuse(self, RefuseReason::Busy);
            }
        }
        if self.punches.len() >= p.max_punches {
            bump(&counters.busy_max_punches);
            return refuse(self, RefuseReason::Busy);
        }
        if self.punches.values().filter(|s| s.a == a).count() >= p.max_punches_per_device {
            bump(&counters.busy_per_device);
            return refuse(self, RefuseReason::Busy);
        }
        let (Some(ra), Some(rb)) = (self.devices.get(&a), self.devices.get(&b)) else {
            return After::Nothing;
        };
        let mode = if ra.nat == NatMapping::Dependent && rb.nat == NatMapping::Dependent {
            PunchMode::HostOnly
        } else {
            PunchMode::All
        };
        if mode == PunchMode::HostOnly && (ra.host.is_empty() || rb.host.is_empty()) {
            return refuse(self, RefuseReason::NatUnsupported);
        }
        let (Ok(token_a), Ok(token_b)) = (PunchToken::random(), PunchToken::random()) else {
            return refuse(self, RefuseReason::Busy);
        };
        let punch_id = self.next_punch_id;
        self.next_punch_id += 1;
        let expires = now.checked_add(p.offer_timeout);
        self.punches.insert(
            punch_id,
            Punch {
                a,
                b,
                request_id: req.request_id,
                stage: Stage::Offered,
                mode,
                token_a,
                token_b,
                created: now,
                go_at: None,
                expires,
                result_a: None,
                result_b: None,
                timer: None,
            },
        );
        if let Some(rb) = self.devices.get(&b) {
            send(
                rb,
                ServerMessage::PunchOffer(PunchOffer { punch_id, from: a }),
            );
        }
        info!(device = %a, peer = %b, punch_id, mode = ?mode, "punch offered");
        timer(punch_id, Stage::Offered, expires)
    }

    /// Count a `punch_request` of `device` at `now` against `punch_requests_per_minute`:
    /// false when the window is full. Times out of the window are dropped, and the table holds
    /// at most `max_devices` device IDs: when it is full, IDs with no time left in the window
    /// go first, then IDs that are not registered.
    fn request_allowed(&mut self, p: &ServerParams, device: DeviceId, now: Instant) -> bool {
        let in_window = |t: &Instant| now.duration_since(*t) < p.punch_request_window;
        if !self.request_times.contains_key(&device) && self.request_times.len() >= p.max_devices {
            self.request_times
                .retain(|_, times| times.back().is_some_and(in_window));
            if self.request_times.len() >= p.max_devices {
                let devices = &self.devices;
                self.request_times.retain(|id, _| devices.contains_key(id));
            }
        }
        let times = self.request_times.entry(device).or_default();
        while times.front().is_some_and(|t| !in_window(t)) {
            times.pop_front();
        }
        if times.len() >= p.punch_requests_per_minute {
            return false;
        }
        times.push_back(now);
        true
    }

    /// `punch_answer` from `b`.
    pub fn punch_answer(
        &mut self,
        p: &ServerParams,
        counters: &Counters,
        b: DeviceId,
        answer: PunchAnswer,
        now: Instant,
    ) -> After {
        let punch_id = answer.punch_id;
        let Some(s) = self.punches.get(&punch_id) else {
            return After::Nothing;
        };
        if s.b != b || s.stage != Stage::Offered {
            return After::Nothing;
        }
        if !answer.accept {
            let Some(s) = self.end_punch(punch_id) else {
                return After::Nothing;
            };
            let reason = match answer.reason {
                Some(AnswerReason::LimitsInvalid) => RefuseReason::PeerLimitsInvalid,
                Some(AnswerReason::NotPaired) | None => RefuseReason::Declined,
            };
            refuse_to(self.devices.get(&s.a), s.request_id, reason);
            info!(device = %s.a, peer = %b, punch_id, reason = ?reason, "punch declined");
            return After::Nothing;
        }
        let (Some(ra), Some(rb)) = (self.devices.get(&s.a), self.devices.get(&s.b)) else {
            return After::Nothing;
        };
        let limits = CandidateLimits {
            max_reported_reflexive: p.max_reported_reflexive,
            max_host_candidates: p.max_host_candidates,
        };
        // The candidates sent to each side are the other side's addresses.
        let for_a = select_candidates(
            rb.reflexive,
            &rb.reported_reflexive,
            &rb.host,
            s.mode,
            limits,
        );
        let for_b = select_candidates(
            ra.reflexive,
            &ra.reported_reflexive,
            &ra.host,
            s.mode,
            limits,
        );
        for sel in [&for_a, &for_b] {
            add(&counters.candidates_rejected, sel.rejected);
            add(&counters.candidates_truncated, sel.truncated);
        }
        if for_a.candidates.is_empty() || for_b.candidates.is_empty() {
            let Some(s) = self.end_punch(punch_id) else {
                return After::Nothing;
            };
            refuse_to(
                self.devices.get(&s.a),
                s.request_id,
                RefuseReason::NatUnsupported,
            );
            self.cancel_expired(s.b, punch_id);
            return After::Nothing;
        }
        // start_after = max(rtt_A, rtt_B)/2 + margin − rtt_self/2, capped at start_delay_max.
        let rtt_a = ra.conn.rtt();
        let rtt_b = rb.conn.rtt();
        let half_max = rtt_a.max(rtt_b) / 2;
        let start_after = |own: Duration| {
            half_max
                .saturating_add(p.start_delay_margin)
                .saturating_sub(own / 2)
                .min(p.start_delay_max)
        };
        let go = |request_id, peer, role, candidates, peer_nat, local, remote, own_rtt| {
            ServerMessage::PunchGo(PunchGo {
                punch_id,
                request_id,
                peer,
                role,
                mode: s.mode,
                candidates,
                peer_nat,
                token_local: local,
                token_peer: remote,
                start_after_ms: millis(start_after(own_rtt)),
                window_ms: millis(p.punch_window),
                handshake_ms: millis(p.quic_handshake_timeout),
            })
        };
        send(
            ra,
            go(
                Some(s.request_id),
                s.b,
                Role::Controlling,
                for_a.candidates,
                rb.nat,
                s.token_a,
                s.token_b,
                rtt_a,
            ),
        );
        send(
            rb,
            go(
                None,
                s.a,
                Role::Controlled,
                for_b.candidates,
                ra.nat,
                s.token_b,
                s.token_a,
                rtt_b,
            ),
        );
        let a = s.a;
        let expires = now.checked_add(p.punch_session_ttl());
        if let Some(s) = self.punches.get_mut(&punch_id) {
            if let Some(t) = s.timer.take() {
                t.abort();
            }
            s.stage = Stage::GoSent;
            s.go_at = Some(now);
            s.expires = expires;
        }
        info!(device = %a, peer = %b, punch_id, "punch go sent");
        timer(punch_id, Stage::GoSent, expires)
    }

    /// `punch_result` from `device`.
    pub fn punch_result(&mut self, device: DeviceId, result: PunchResult) {
        let punch_id = result.punch_id;
        let Some(s) = self.punches.get_mut(&punch_id) else {
            return;
        };
        if s.stage != Stage::GoSent {
            return;
        }
        let other = if s.a == device {
            s.result_a = Some(result);
            s.b
        } else if s.b == device {
            s.result_b = Some(result);
            s.a
        } else {
            return;
        };
        info!(device = %device, peer = %other, punch_id, ok = result.ok, reason = ?result.reason, "punch result");
        if result.reason == Some(ResultReason::LimitsInvalid) {
            self.end_punch(punch_id);
            cancel_to(
                self.devices.get(&other),
                punch_id,
                CancelReason::PeerLimitsInvalid,
            );
            return;
        }
        if s.result_a.is_some() && s.result_b.is_some() {
            self.end_punch(punch_id);
        }
    }

    /// Send `punch_cancel{expired}` to `to` if it is registered, and count it.
    fn cancel_expired(&mut self, to: DeviceId, punch_id: u64) {
        if self.devices.contains_key(&to) {
            cancel_to(self.devices.get(&to), punch_id, CancelReason::Expired);
            self.expired_cancels += 1;
        }
    }

    /// A punch deadline passed.
    pub fn expire(&mut self, punch_id: u64, stage: Stage) {
        if self.punches.get(&punch_id).is_none_or(|s| s.stage != stage) {
            return;
        }
        let Some(s) = self.end_punch(punch_id) else {
            return;
        };
        match stage {
            Stage::Offered => {
                refuse_to(self.devices.get(&s.a), s.request_id, RefuseReason::NotFound);
                info!(
                    device = %s.a,
                    peer = %s.b,
                    punch_id,
                    age_ms = millis(s.created.elapsed()),
                    "punch offer timed out"
                );
            }
            Stage::GoSent => {
                if s.result_a.is_none() {
                    self.cancel_expired(s.a, punch_id);
                }
                if s.result_b.is_none() {
                    self.cancel_expired(s.b, punch_id);
                }
                info!(
                    device = %s.a,
                    peer = %s.b,
                    punch_id,
                    since_go_ms = s.go_at.map(|t| millis(t.elapsed())),
                    "punch session expired"
                );
            }
        }
    }

    /// Store the timer of `punch_id` for `stage`. A punch that is gone or in another stage
    /// does not take it: the timer is aborted. A timer the punch held is aborted too.
    pub fn set_timer(&mut self, punch_id: u64, stage: Stage, handle: AbortHandle) {
        match self.punches.get_mut(&punch_id) {
            Some(s) if s.stage == stage => {
                if let Some(old) = s.timer.replace(handle) {
                    old.abort();
                }
            }
            _ => handle.abort(),
        }
    }

    /// Abort every punch timer (server shutdown).
    pub fn abort_timers(&mut self) {
        for s in self.punches.values_mut() {
            if let Some(t) = s.timer.take() {
                t.abort();
            }
        }
    }
}

/// Addresses kept in order without duplicates, up to a cap.
#[derive(Default)]
pub(crate) struct Kept {
    pub addrs: Vec<SocketAddr>,
    cap: usize,
    /// Host texts that did not parse or broke the rule.
    pub rejected: usize,
    /// Unique addresses past the cap.
    pub truncated: usize,
}

impl Kept {
    fn new(cap: usize) -> Self {
        Kept {
            cap,
            ..Kept::default()
        }
    }

    fn push(&mut self, addr: SocketAddr) {
        if self.addrs.contains(&addr) {
            return;
        }
        if self.addrs.len() >= self.cap {
            self.truncated += 1;
            return;
        }
        self.addrs.push(addr);
    }
}

/// How many reported reflexive addresses a registration stores: `max_reported_reflexive`,
/// and one more because one of them may be the server reflexive address and give way to it
/// when `punch_go` is built (docs/design/rendezvous.md "후보 주소 규칙").
pub(crate) fn stored_reported(p: &ServerParams) -> usize {
    p.max_reported_reflexive.saturating_add(1)
}

/// How many host candidates a registration stores: `max_host_candidates`, and as many more as
/// the reflexive candidates of the same device that may take the place of a duplicate. So the
/// stored lists give the same `punch_go` as the full lists while staying within the cap of
/// the candidate rule (at most 64 in all).
pub(crate) fn stored_host(p: &ServerParams) -> usize {
    p.max_host_candidates
        .saturating_add(p.max_reported_reflexive)
        .saturating_add(1)
}

/// Parse and filter host candidates: in order, without duplicates, at most `cap`.
pub(crate) fn filter_host(texts: &[String], cap: usize) -> Kept {
    let mut kept = Kept::new(cap);
    for t in texts {
        match parse_address(t) {
            Some(a) if candidate_allowed(CandidateKind::Host, a) => kept.push(a),
            _ => kept.rejected += 1,
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_candidates_are_unique_and_capped() {
        let texts: Vec<String> = [
            "192.168.1.2:5000",
            "192.168.1.2:5000",
            "8.8.8.8:5000",
            "10.0.0.2:5000",
            "not an address",
            "10.0.0.3:5000",
            "10.0.0.2:5000",
            "10.0.0.4:5000",
        ]
        .iter()
        .map(|t| t.to_string())
        .collect();
        let kept = filter_host(&texts, 3);
        let want: Vec<SocketAddr> = ["192.168.1.2:5000", "10.0.0.2:5000", "10.0.0.3:5000"]
            .iter()
            .map(|t| t.parse().unwrap())
            .collect();
        assert_eq!(kept.addrs, want);
        assert_eq!(kept.rejected, 2);
        // 10.0.0.4 is past the cap; the repeated 10.0.0.2 is a duplicate, not counted.
        assert_eq!(kept.truncated, 1);
        let p = crate::settings::ServerSettings::new()
            .set(
                "listen.primary",
                crate::settings::Value::Address("127.0.0.1:0".parse().unwrap()),
            )
            .unwrap()
            .resolve()
            .unwrap();
        // Defaults: 4 host + 3 reported + 1 server reflexive, and 3 + 1 reported.
        assert_eq!(stored_host(&p), 8);
        assert_eq!(stored_reported(&p), 4);
    }

    #[test]
    fn stored_lists_give_the_same_punch_go() {
        use crate::wire::candidate::{CandidateLimits, select_candidates};
        use crate::wire::message::PunchMode;
        let limits = CandidateLimits {
            max_reported_reflexive: 1,
            max_host_candidates: 2,
        };
        let a = |t: &str| -> SocketAddr { t.parse().unwrap() };
        // The device's own global IPv6 address is its reflexive address and its first host
        // candidate (no NAT); the reported address repeats it too.
        let reflexive = a("[2001:db8::1]:5000");
        let reported = vec![reflexive, a("[2001:db8::2]:5000"), a("[2001:db8::3]:5000")];
        let host = vec![
            reflexive,
            a("[2001:db8::2]:5000"),
            a("[2001:db8::4]:5000"),
            a("[2001:db8::5]:5000"),
            a("[2001:db8::6]:5000"),
        ];
        let stored = |list: &[SocketAddr], cap| {
            let mut k = Kept::new(cap);
            for x in list {
                k.push(*x);
            }
            k.addrs
        };
        let stored_reported = stored(&reported, 1 + 1);
        let stored_host = stored(&host, 2 + 1 + 1);
        for mode in [PunchMode::All, PunchMode::HostOnly] {
            let full = select_candidates(reflexive, &reported, &host, mode, limits);
            let kept = select_candidates(reflexive, &stored_reported, &stored_host, mode, limits);
            assert_eq!(full.candidates, kept.candidates, "{mode:?}");
        }
    }
}
