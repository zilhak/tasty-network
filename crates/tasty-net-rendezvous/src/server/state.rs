//! Server state: registrations and punch sessions (docs/design/rendezvous.md "서버 상태").
//!
//! Everything is in memory. Every message the server sends to a device is a
//! [`ServerMessage`] the server builds from this state; nothing a device sent is passed on
//! except the closed set of normalised values in docs/design/rendezvous.md "중계 없음".

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use crate::settings::ServerParams;
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
    pub stun_dropped: AtomicU64,
    pub candidates_rejected: AtomicU64,
    pub candidates_truncated: AtomicU64,
    pub registrations_total: AtomicU64,
    pub superseded: AtomicU64,
    pub access_denied: AtomicU64,
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
    pub tx: UnboundedSender<ServerMessage>,
    pub reflexive: SocketAddr,
    /// The STUN addresses this device was told in `welcome.stun`.
    pub advertised: Vec<SocketAddr>,
    pub reported_reflexive: Vec<SocketAddr>,
    pub host: Vec<SocketAddr>,
    pub nat: NatMapping,
    pub registered_at: Instant,
    pub request_times: VecDeque<Instant>,
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
    pub expires: Instant,
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
    next_punch_id: u64,
    next_conn_id: u64,
}

/// Why a registration was not made.
pub(crate) enum RegisterRefusal {
    Busy,
}

fn send(reg: &Registration, msg: ServerMessage) {
    // A closed writer means the connection is ending; its registration is removed then.
    if reg.tx.send(msg).is_err() {
        debug!("writer of a closing connection dropped a message");
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

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl State {
    pub fn new() -> Self {
        State {
            devices: HashMap::new(),
            punches: HashMap::new(),
            next_punch_id: 1,
            next_conn_id: 1,
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
    pub fn nat_report(&mut self, counters: &Counters, device: DeviceId, report: NatReport) {
        let Some(reg) = self.devices.get_mut(&device) else {
            return;
        };
        reg.nat = report.mapping;
        let mut reported = Vec::new();
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
        reg.reported_reflexive = reported;
        if let Some(host) = &report.host_candidates {
            let (kept, dropped) = filter_host(host);
            reg.host = kept;
            rejected += dropped;
        }
        add(&counters.candidates_rejected, rejected);
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
        let Some(reg) = self.devices.get_mut(&a) else {
            return After::Nothing;
        };
        while reg
            .request_times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= p.punch_request_window)
        {
            reg.request_times.pop_front();
        }
        if reg.request_times.len() >= p.punch_requests_per_minute {
            bump(&counters.busy_request_rate);
            return refuse(self, RefuseReason::Busy);
        }
        reg.request_times.push_back(now);
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
        let expires = now + p.offer_timeout;
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
        After::Timer {
            punch_id,
            stage: Stage::Offered,
            at: expires,
        }
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
            cancel_to(self.devices.get(&s.b), punch_id, CancelReason::Expired);
            return After::Nothing;
        }
        // start_after = max(rtt_A, rtt_B)/2 + margin − rtt_self/2, capped at start_delay_max.
        let rtt_a = ra.conn.rtt();
        let rtt_b = rb.conn.rtt();
        let half_max = rtt_a.max(rtt_b) / 2;
        let start_after = |own: Duration| {
            (half_max + p.start_delay_margin)
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
        let expires = now + p.punch_session_ttl();
        if let Some(s) = self.punches.get_mut(&punch_id) {
            if let Some(t) = s.timer.take() {
                t.abort();
            }
            s.stage = Stage::GoSent;
            s.go_at = Some(now);
            s.expires = expires;
        }
        info!(device = %a, peer = %b, punch_id, "punch go sent");
        After::Timer {
            punch_id,
            stage: Stage::GoSent,
            at: expires,
        }
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
                    cancel_to(self.devices.get(&s.a), punch_id, CancelReason::Expired);
                }
                if s.result_b.is_none() {
                    cancel_to(self.devices.get(&s.b), punch_id, CancelReason::Expired);
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

    pub fn set_timer(&mut self, punch_id: u64, handle: AbortHandle) {
        match self.punches.get_mut(&punch_id) {
            Some(s) => s.timer = Some(handle),
            None => handle.abort(),
        }
    }
}

/// Parse and filter host candidates: kept in order, and how many were dropped.
pub(crate) fn filter_host(texts: &[String]) -> (Vec<SocketAddr>, usize) {
    let mut kept = Vec::new();
    let mut dropped = 0;
    for t in texts {
        match parse_address(t) {
            Some(a) if candidate_allowed(CandidateKind::Host, a) => kept.push(a),
            _ => dropped += 1,
        }
    }
    (kept, dropped)
}
