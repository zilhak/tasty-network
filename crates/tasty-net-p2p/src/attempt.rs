//! Connection attempts: the dialer (A) and the acceptor (B) of docs/design/p2p.md "펀칭 절차"
//! 1–11, "재시도와 포기" and the "연결 시도와 연결" state table.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tasty_net_rendezvous::tls::SERVER_NAME;
use tasty_net_rendezvous::wire::ALPN_P2P;
use tasty_net_rendezvous::wire::message::{
    AnswerReason, CancelReason, DeviceMessage, Limits, PunchAnswer, PunchGo, PunchMode,
    PunchRequest, PunchResult, RefuseReason, ResultReason, Role,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::close::P2pCloseReason;
use crate::conn::{ConnInner, P2pCancel};
use crate::endpoint::{
    AcceptSession, Core, Incoming, PunchHold, PunchKey, PunchSignal, RequestOutcome, lock,
};
use crate::error::P2pConnectError;
use crate::events::{AttemptPhase, Cause, CauseDetail, EndpointPhase};
use crate::info::{AddressPair, AppliedSetting, NatInfo, Side, Tracked};
use crate::probe;
use crate::server::{TlsStage, tls_stage};
use crate::settings::{P2pParams, Source, Value};
use crate::socket::ProbeSession;

/// The bound of one connection attempt with the server's values (docs/design/p2p.md "재시도와
/// 포기", [`P2pParams::attempt_bound`]).
pub(crate) fn attempt_bound(p: &P2pParams, l: &Limits) -> Duration {
    p.attempt_bound(
        Duration::from_millis(l.start_delay_max_ms),
        Duration::from_millis(l.punch_window_ms),
        Duration::from_millis(l.quic_handshake_ms),
    )
}

/// The key of a registration whose limits make the attempt bound too long to be a deadline
/// (docs/design/p2p.md "재시도와 포기").
pub(crate) const ATTEMPT_BOUND_KEY: &str = "attempt_bound";

/// The setting of `punch_go` that does not fit (docs/design/rendezvous.md "한계값 검사" 2).
///
/// A `punch_go` for another peer or with the other role breaks the server's rules the same way
/// (docs/design/p2p.md flow 3): the dialer is `controlling` towards the peer it asked for, the
/// acceptor `controlled` towards the peer of its offer.
fn go_limits_invalid(
    p: &P2pParams,
    l: &Limits,
    go: &PunchGo,
    peer: DeviceId,
    role: Role,
) -> Option<&'static str> {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    if go.peer != peer {
        Some("punch_go.peer")
    } else if go.role != role {
        Some("punch_go.role")
    } else if go.window_ms > ms(p.window_max) {
        Some("punch.window_max")
    } else if go.handshake_ms > ms(p.handshake_timeout_max) {
        Some("quic.handshake_timeout_max")
    } else if go.window_ms > l.punch_window_ms {
        Some("punch_window_ms")
    } else if go.handshake_ms > l.quic_handshake_ms {
        Some("quic_handshake_ms")
    } else if go.start_after_ms > l.start_delay_max_ms {
        Some("start_delay_max_ms")
    } else if go.candidates.len() > p.max_candidates {
        Some("punch.max_candidates")
    } else {
        None
    }
}

/// The values applied to a connection: this device's punch and QUIC settings and the server's
/// values.
fn applied(core: &Core, go: &PunchGo, l: &Limits) -> Vec<AppliedSetting> {
    let mut out: Vec<AppliedSetting> = core
        .settings
        .entries_with(core.os)
        .into_iter()
        .filter(|e| {
            e.key.starts_with("punch.")
                || e.key.starts_with("socket.")
                || e.key.starts_with("tls.")
                || e.key.starts_with("quic.")
                || e.key.starts_with("nat.")
                || e.key.starts_with("path.")
                || e.key.starts_with("history.")
        })
        .map(|e| AppliedSetting {
            key: e.key,
            value: e.value,
            source: e.source,
        })
        .collect();
    let ms = |n: u64| Value::Duration(Duration::from_millis(n));
    let count = |n: u32| Value::Count(u64::from(n));
    for (key, value) in [
        ("punch_go.start_after_ms", ms(go.start_after_ms)),
        ("punch_go.window_ms", ms(go.window_ms)),
        ("punch_go.handshake_ms", ms(go.handshake_ms)),
        ("welcome.limits.offer_timeout_ms", ms(l.offer_timeout_ms)),
        (
            "welcome.limits.start_delay_max_ms",
            ms(l.start_delay_max_ms),
        ),
        ("welcome.limits.punch_window_ms", ms(l.punch_window_ms)),
        ("welcome.limits.quic_handshake_ms", ms(l.quic_handshake_ms)),
        (
            "welcome.limits.punch_session_ttl_ms",
            ms(l.punch_session_ttl_ms),
        ),
        (
            "welcome.limits.max_host_candidates",
            count(l.max_host_candidates),
        ),
        (
            "welcome.limits.max_reported_reflexive",
            count(l.max_reported_reflexive),
        ),
        (
            "welcome.limits.max_message_bytes",
            Value::Bytes(u64::from(l.max_message_bytes)),
        ),
    ] {
        out.push(AppliedSetting {
            key,
            value,
            source: Source::Server,
        });
    }
    out
}

/// An attempt's end without a connection.
struct Fail {
    error: P2pConnectError,
    cause: Cause,
}

impl Fail {
    fn new(error: P2pConnectError, name: &'static str) -> Self {
        Fail {
            error,
            cause: Cause::with(name, CauseDetail::Connect(error)),
        }
    }
}

/// A task an attempt spawned that ends with the attempt: dropping the guard aborts it, so an
/// attempt future dropped by a cancel or by the endpoint's end takes the task along instead of
/// leaving it detached.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    fn abort(&self) {
        self.0.abort();
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Removes a punch's server channel, probe session and accept session when the punch ends,
/// also when its future is dropped (cancel).
struct Cleanup {
    core: Arc<Core>,
    /// Removes the punch's server channel when it drops, after the body below.
    _punch: Option<PunchHold>,
    session: Option<Arc<ProbeSession>>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            self.core.router.remove_session(s);
            lock(&self.core.accepting).retain(|a| !Arc::ptr_eq(&a.probe, s));
        }
    }
}

/// Removes a punch request's waiter when the request ends, also when its future is dropped
/// (a cancel while `Requesting`).
struct Asked {
    core: Arc<Core>,
    request_id: u64,
}

impl Drop for Asked {
    fn drop(&mut self) {
        lock(&self.core.requests).remove(&self.request_id);
    }
}

/// The next cancel reason from the server; signals that are not cancels are skipped, and a
/// closed channel never ends.
async fn next_cancel(rx: &mut mpsc::UnboundedReceiver<PunchSignal>) -> CancelReason {
    loop {
        match rx.recv().await {
            Some(PunchSignal::Cancel(r)) => return r,
            Some(_) => {}
            None => std::future::pending::<()>().await,
        }
    }
}

/// Report a punch's end to the server it came from; after that registration ended, nothing is
/// sent (docs/design/p2p.md "조정 연결을 잃으면").
fn result(core: &Core, (generation, punch_id): PunchKey, reason: Option<ResultReason>) {
    let _sent = core.send_on(
        generation,
        DeviceMessage::PunchResult(PunchResult {
            punch_id,
            ok: reason.is_none(),
            reason,
        }),
    );
}

fn refused(reason: RefuseReason) -> Fail {
    let (error, name) = match reason {
        RefuseReason::NotFound => (P2pConnectError::PeerNotFound, "not_found"),
        RefuseReason::Declined => (P2pConnectError::Declined, "declined"),
        RefuseReason::PeerLimitsInvalid => {
            (P2pConnectError::PeerLimitsInvalid, "peer_limits_invalid")
        }
        RefuseReason::NatUnsupported => (P2pConnectError::NatUnsupported, "nat_unsupported"),
        RefuseReason::Busy => (P2pConnectError::Busy, "busy"),
        RefuseReason::SelfPeer => (P2pConnectError::SelfAttach, "self"),
    };
    Fail::new(error, name)
}

fn cancelled(reason: CancelReason) -> Cause {
    Cause::with("punch_cancelled", CauseDetail::Cancel(reason))
}

/// B takes a handshake from `remote`: `Handshaking` with its addresses and deadline.
fn handshaking(core: &Core, t: &Tracked, remote: SocketAddr, limit: Duration) {
    let pair = AddressPair {
        local: core.io.local_addr().ok(),
        remote,
    };
    let deadline = Instant::now() + limit;
    t.update(|r| r.handshake = Some((pair, deadline.into_std())));
    t.to(AttemptPhase::Handshaking, Cause::named("quic_incoming"));
}

/// The end of a connection attempt at its bound: the cause of the phase it was in, the error
/// by the rule of an elapsed window (docs/design/p2p.md "연결 시도 상태").
fn bound_elapsed(t: &Tracked, last_mode: PunchMode) -> Fail {
    if t.phase() == AttemptPhase::Requesting {
        return Fail::new(P2pConnectError::RequestTimeout, "request_timeout");
    }
    let mut mode = last_mode;
    t.update(|r| mode = r.info.nat.map_or(last_mode, |n| n.mode));
    let error = if mode == PunchMode::HostOnly {
        P2pConnectError::NatUnsupported
    } else {
        P2pConnectError::PunchFailed
    };
    let name = if t.phase() == AttemptPhase::Handshaking {
        "handshake_timeout"
    } else {
        "punch_window_elapsed"
    };
    Fail {
        error,
        cause: Cause::with(name, CauseDetail::Connect(error)),
    }
}

/// What ends a dialer's punch attempt that can be retried.
enum Retry {
    /// Retry (or fail) with this cause.
    Again(Cause),
    /// Fail now.
    Fail(Fail),
}

/// The dialer (A).
pub(crate) async fn dial(
    core: Arc<Core>,
    peer: DeviceId,
    cancel: P2pCancel,
) -> Result<Arc<ConnInner>, P2pConnectError> {
    {
        let r = lock(&core.reg);
        if r.phase != Some(EndpointPhase::Registered) {
            return Err(P2pConnectError::RendezvousUnavailable { stopped: r.stopped });
        }
    }
    let t = core.tracker.start(
        Side::Dialer,
        peer,
        AttemptPhase::Requesting,
        Cause::named("connect_called"),
        |i| {
            i.local_bind = core.io.local_addr().ok();
            i.rendezvous = Some(core.server);
        },
    );
    // The endpoint's end cancels the attempt here too; its caller was already woken.
    let mut down = core.shutdown.subscribe();
    let r = tokio::select! {
        r = dial_attempts(&core, &t, peer) => r,
        () = cancel.cancelled() => Err(Fail::new(P2pConnectError::Cancelled, "cancelled")),
        _ = down.wait_for(|down| *down) => Err(Fail {
            error: P2pConnectError::Cancelled,
            cause: Cause::named("endpoint_off"),
        }),
    };
    match r {
        Ok(conn) => Ok(conn),
        Err(f) => {
            t.to(AttemptPhase::Closed, f.cause);
            Err(f.error)
        }
    }
}

async fn dial_attempts(
    core: &Arc<Core>,
    t: &Tracked,
    peer: DeviceId,
) -> Result<Arc<ConnInner>, Fail> {
    let p = &core.params;
    let mut last_mode = PunchMode::All;
    let mut deadline = None;
    for n in 1..=p.punch_attempts {
        t.update(|r| r.info.attempt.number = n);
        let (limits, mismatch, local_nat, reflexive) = {
            let r = lock(&core.reg);
            if r.phase != Some(EndpointPhase::Registered) {
                return Err(Fail::new(
                    P2pConnectError::RendezvousLost,
                    "rendezvous_lost",
                ));
            }
            (
                r.limits,
                r.mismatch,
                r.nat.as_ref().map(|j| j.mapping).unwrap_or_default(),
                r.reflexive,
            )
        };
        if let Some(key) = mismatch {
            return Err(Fail::new(
                P2pConnectError::ServerLimitsInvalid { key },
                "server_limits_invalid",
            ));
        }
        let Some(limits) = limits else {
            return Err(Fail::new(
                P2pConnectError::RendezvousLost,
                "rendezvous_lost",
            ));
        };
        // The bound shown is the bound kept: past it the attempt ends where it is. The
        // settings and the registration's limits were checked to fit; a clock near the end of
        // its range still ends the attempt instead of panicking.
        let end = match deadline {
            Some(end) => end,
            None => {
                let Some(end) = Instant::now().checked_add(attempt_bound(p, &limits)) else {
                    return Err(Fail::new(
                        P2pConnectError::ServerLimitsInvalid {
                            key: ATTEMPT_BOUND_KEY,
                        },
                        "server_limits_invalid",
                    ));
                };
                t.update(|r| r.bound_at = Some(end.into_std()));
                *deadline.insert(end)
            }
        };
        t.update(|r| r.info.local_reflexive = reflexive);
        let once = tokio::time::timeout_at(end, punch_once(core, t, peer, &limits, local_nat, n));
        let outcome = match once.await {
            Err(_) => return Err(bound_elapsed(t, last_mode)),
            Ok(Ok(conn)) => return Ok(conn),
            Ok(Err((mode, outcome))) => {
                last_mode = mode.unwrap_or(last_mode);
                outcome
            }
        };
        let cause = match outcome {
            Retry::Fail(f) => return Err(f),
            Retry::Again(c) => c,
        };
        if n == p.punch_attempts {
            let error = if last_mode == PunchMode::HostOnly {
                P2pConnectError::NatUnsupported
            } else {
                P2pConnectError::PunchFailed
            };
            return Err(Fail {
                error,
                cause: Cause {
                    name: cause.name,
                    detail: Some(CauseDetail::Connect(error)),
                },
            });
        }
        // RetryWait: re-check the reflexive address and wait the retry delay, whichever ends
        // later.
        let retry_at = Instant::now() + p.retry_delay;
        t.update(|r| r.retry_at = Some(retry_at.into_std()));
        t.to(AttemptPhase::RetryWait, cause);
        // The judgement belongs to the registration current now; without one there is none.
        // It ends with the attempt: a cancel or the endpoint's end drops it with this wait.
        let judge = core
            .generation()
            .map(|g| AbortOnDrop(tokio::spawn(crate::server::rejudge(core.clone(), g))));
        let waited = tokio::time::timeout_at(end, async {
            tokio::time::sleep_until(retry_at).await;
            if let Some(mut judge) = judge {
                let _judged = (&mut judge.0).await.is_ok();
            }
        });
        if waited.await.is_err() {
            return Err(bound_elapsed(t, last_mode));
        }
        t.update(|r| r.retry_at = None);
        t.to(AttemptPhase::Requesting, Cause::named("retry_elapsed"));
    }
    Err(Fail::new(
        P2pConnectError::PunchFailed,
        "punch_window_elapsed",
    ))
}

/// One punch attempt of the dialer: request, schedule, probe, handshake. On failure, the mode
/// of the attempt (once `punch_go` arrived) and whether it may be retried.
async fn punch_once(
    core: &Arc<Core>,
    t: &Tracked,
    peer: DeviceId,
    limits: &Limits,
    local_nat: tasty_net_rendezvous::wire::message::NatMapping,
    n: u32,
) -> Result<Arc<ConnInner>, (Option<PunchMode>, Retry)> {
    let p = &core.params;
    let fail = |f: Fail| (None, Retry::Fail(f));
    let request_id = core
        .next_request
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Some(generation) = core.generation() else {
        return Err(fail(Fail::new(
            P2pConnectError::RendezvousLost,
            "rendezvous_lost",
        )));
    };
    let (tx, rx) = oneshot::channel();
    lock(&core.requests).insert(request_id, tx);
    let asked = Asked {
        core: core.clone(),
        request_id,
    };
    if !core.send_on(
        generation,
        DeviceMessage::PunchRequest(PunchRequest { request_id, peer }),
    ) {
        return Err(fail(Fail::new(
            P2pConnectError::RendezvousLost,
            "rendezvous_lost",
        )));
    }
    let (go, mut signals, hold) = match tokio::time::timeout(p.request_timeout, rx).await {
        Err(_) => {
            return Err(fail(Fail::new(
                P2pConnectError::RequestTimeout,
                "request_timeout",
            )));
        }
        Ok(Err(_)) => {
            return Err(fail(Fail::new(
                P2pConnectError::RendezvousLost,
                "rendezvous_lost",
            )));
        }
        Ok(Ok(RequestOutcome::Refused(reason))) => return Err(fail(refused(reason))),
        Ok(Ok(RequestOutcome::Go(go, signals, hold))) => (go, signals, hold),
    };
    drop(asked);
    let mode = Some(go.mode);
    let mut cleanup = Cleanup {
        core: core.clone(),
        _punch: Some(hold),
        session: None,
    };
    t.update(|r| {
        r.info.nat = Some(NatInfo {
            local: local_nat,
            peer: go.peer_nat,
            mode: go.mode,
        });
        r.info.applied = applied(core, &go, limits);
    });
    t.to(AttemptPhase::Scheduled, Cause::named("punch_go"));
    if let Some(key) = go_limits_invalid(p, limits, &go, peer, Role::Controlling) {
        result(
            core,
            (generation, go.punch_id),
            Some(ResultReason::LimitsInvalid),
        );
        return Err((
            mode,
            Retry::Fail(Fail::new(
                P2pConnectError::ServerLimitsInvalid { key },
                "server_limits_invalid",
            )),
        ));
    }
    let (candidates, _dropped) = probe::received_candidates(&go);
    let session = Arc::new(ProbeSession::new(
        peer,
        go.token_local,
        go.token_peer,
        go.mode,
        Duration::from_millis(go.handshake_ms),
        candidates,
        p.max_candidates,
    ));
    core.router.add_session(session.clone());
    cleanup.session = Some(session.clone());
    let on_cancel = |r: CancelReason| -> (Option<PunchMode>, Retry) {
        if r == CancelReason::PeerLimitsInvalid {
            (
                mode,
                Retry::Fail(Fail {
                    error: P2pConnectError::PeerLimitsInvalid,
                    cause: cancelled(r),
                }),
            )
        } else if n < p.punch_attempts {
            (mode, Retry::Again(cancelled(r)))
        } else {
            (
                mode,
                Retry::Fail(Fail {
                    error: P2pConnectError::PunchCancelled { reason: r },
                    cause: cancelled(r),
                }),
            )
        }
    };
    let start = Instant::now() + Duration::from_millis(go.start_after_ms);
    tokio::select! {
        () = tokio::time::sleep_until(start) => {}
        r = next_cancel(&mut signals) => return Err(on_cancel(r)),
    }
    let window_end = start + Duration::from_millis(go.window_ms);
    t.update(|r| {
        r.probe = Some(session.clone());
        r.window_end_at = Some(window_end.into_std());
    });
    t.to(AttemptPhase::Punching, Cause::named("start_elapsed"));
    let found = tokio::select! {
        f = probe::run(&core.router, &core.io, &session, p.probe_interval, window_end, true) => f,
        r = next_cancel(&mut signals) => return Err(on_cancel(r)),
    };
    let Some(found) = found else {
        result(core, (generation, go.punch_id), Some(ResultReason::Timeout));
        return Err((mode, Retry::Again(Cause::named("punch_window_elapsed"))));
    };
    let pair = AddressPair {
        local: core.io.local_addr().ok(),
        remote: found.addr,
    };
    let deadline = Instant::now() + Duration::from_millis(go.handshake_ms);
    t.update(|r| {
        r.info.remote_candidate = Some(found);
        r.handshake = Some((pair, deadline.into_std()));
    });
    t.to(
        AttemptPhase::Handshaking,
        Cause::with("probe_succeeded", CauseDetail::Peer(found.addr)),
    );
    let config = core
        .client_config(peer, ALPN_P2P, &p.quic)
        .map_err(|_| fail(Fail::new(P2pConnectError::PunchFailed, "handshake_timeout")))?;
    // The QUIC endpoint is gone only while the endpoint shuts down.
    let connecting = core
        .quic()
        .ok_or(quinn::ConnectError::EndpointStopping)
        .and_then(|q| q.connect_with(config, found.addr, SERVER_NAME))
        .map_err(|_| (mode, Retry::Again(Cause::named("handshake_timeout"))))?;
    let handshake = tokio::time::timeout(Duration::from_millis(go.handshake_ms), connecting);
    tokio::pin!(handshake);
    let conn = loop {
        tokio::select! {
            r = &mut handshake => break r,
            r = next_cancel(&mut signals) => {
                // Once QUIC is under way only the peer's refusal of the limits ends it.
                if r == CancelReason::PeerLimitsInvalid {
                    return Err(on_cancel(r));
                }
            }
        }
    };
    let conn = match conn {
        Err(_) => {
            result(core, (generation, go.punch_id), Some(ResultReason::Timeout));
            return Err((mode, Retry::Again(Cause::named("handshake_timeout"))));
        }
        Ok(Err(e)) => {
            result(
                core,
                (generation, go.punch_id),
                Some(ResultReason::HandshakeFailed),
            );
            return Err(match tls_stage(&e) {
                Some(TlsStage::KeyRefused) => (
                    mode,
                    Retry::Fail(Fail::new(
                        P2pConnectError::PeerIdentityMismatch,
                        "peer_identity_mismatch",
                    )),
                ),
                Some(TlsStage::NoAlpn) => (
                    mode,
                    Retry::Fail(Fail::new(P2pConnectError::AlpnMismatch, "alpn_mismatch")),
                ),
                None => (mode, Retry::Again(Cause::named("handshake_timeout"))),
            });
        }
        Ok(Ok(c)) => c,
    };
    result(core, (generation, go.punch_id), None);
    drop(cleanup);
    let wrapped = ConnInner::new(conn.clone(), t.id, peer);
    t.update(|r| r.conn = Some(conn.clone()));
    t.to(AttemptPhase::Connected, Cause::named("quic_established"));
    tokio::spawn(crate::endpoint::watch_connection(
        core.clone(),
        t.clone(),
        wrapped.clone(),
        conn,
        None,
    ));
    Ok(wrapped)
}

/// The acceptor (B) of `punch_offer{punch_id, from}`.
pub(crate) async fn accept(core: Arc<Core>, generation: u64, punch_id: u64, from: DeviceId) {
    // The endpoint's end cancels the attempt: its guards clean up, and the shutdown closes its
    // record with `endpoint_off`.
    let mut down = core.shutdown.subscribe();
    tokio::select! {
        () = accept_punch(core.clone(), generation, punch_id, from) => {}
        _ = down.wait_for(|down| *down) => {}
    }
}

async fn accept_punch(core: Arc<Core>, generation: u64, punch_id: u64, from: DeviceId) {
    let p = &core.params;
    let key: PunchKey = (generation, punch_id);
    let (sig_tx, mut signals) = mpsc::unbounded_channel();
    let mut cleanup = Cleanup {
        core: core.clone(),
        _punch: Some(PunchHold::insert(&core, key, sig_tx)),
        session: None,
    };
    let (mismatch, limits, local_nat, reflexive) = {
        let r = lock(&core.reg);
        (
            r.mismatch,
            r.limits,
            r.nat.as_ref().map(|j| j.mapping).unwrap_or_default(),
            r.reflexive,
        )
    };
    let t = core.tracker.start(
        Side::Acceptor,
        from,
        AttemptPhase::Offered,
        Cause::named("punch_offer"),
        |i| {
            i.local_bind = core.io.local_addr().ok();
            i.local_reflexive = reflexive;
            i.rendezvous = Some(core.server);
        },
    );
    let answer = |accept: bool, reason: Option<AnswerReason>| {
        core.send_on(
            generation,
            DeviceMessage::PunchAnswer(PunchAnswer {
                punch_id,
                accept,
                reason,
            }),
        )
    };
    let (Some(limits), None) = (limits, mismatch) else {
        answer(false, Some(AnswerReason::LimitsInvalid));
        t.to(AttemptPhase::Closed, Cause::named("limits_invalid"));
        return;
    };
    if !core.pairing.is_paired(&from) {
        answer(false, Some(AnswerReason::NotPaired));
        t.to(AttemptPhase::Closed, Cause::named("not_paired"));
        return;
    }
    if !answer(true, None) {
        t.to(AttemptPhase::Closed, Cause::named("rendezvous_lost"));
        return;
    }
    let go = match tokio::time::timeout(p.go_timeout, signals.recv()).await {
        Err(_) => {
            t.to(AttemptPhase::Closed, Cause::named("go_timeout"));
            return;
        }
        Ok(Some(PunchSignal::Go(go))) => go,
        Ok(Some(PunchSignal::Cancel(r))) => {
            t.to(AttemptPhase::Closed, cancelled(r));
            return;
        }
        Ok(Some(PunchSignal::Lost)) | Ok(None) => {
            t.to(AttemptPhase::Closed, Cause::named("rendezvous_lost"));
            return;
        }
    };
    t.update(|r| {
        r.info.attempt.number = 1;
        r.info.nat = Some(NatInfo {
            local: local_nat,
            peer: go.peer_nat,
            mode: go.mode,
        });
        r.info.applied = applied(&core, &go, &limits);
    });
    t.to(AttemptPhase::Scheduled, Cause::named("punch_go"));
    if go_limits_invalid(p, &limits, &go, from, Role::Controlled).is_some() {
        result(&core, key, Some(ResultReason::LimitsInvalid));
        t.to(AttemptPhase::Closed, Cause::named("server_limits_invalid"));
        return;
    }
    let (candidates, _dropped) = probe::received_candidates(&go);
    let session = Arc::new(ProbeSession::new(
        from,
        go.token_local,
        go.token_peer,
        go.mode,
        Duration::from_millis(go.handshake_ms),
        candidates,
        p.max_candidates,
    ));
    let start = Instant::now() + Duration::from_millis(go.start_after_ms);
    let window_end = start + Duration::from_millis(go.window_ms);
    let accept_until = window_end + Duration::from_millis(go.handshake_ms);
    session.accept_until(accept_until.into_std());
    let (inc_tx, mut incoming) = mpsc::unbounded_channel();
    core.router.add_session(session.clone());
    lock(&core.accepting).push(AcceptSession {
        probe: session.clone(),
        to: inc_tx,
    });
    cleanup.session = Some(session.clone());
    tokio::select! {
        () = tokio::time::sleep_until(start) => {}
        r = next_cancel(&mut signals) => {
            t.to(AttemptPhase::Closed, cancelled(r));
            return;
        }
    }
    t.update(|r| {
        r.probe = Some(session.clone());
        r.window_end_at = Some(window_end.into_std());
    });
    t.to(AttemptPhase::Punching, Cause::named("start_elapsed"));
    // The probes end with this future, also when the endpoint's end drops it in `accept`.
    let probes = {
        let (router, io, session) = (core.router.clone(), core.io.clone(), session.clone());
        let interval = p.probe_interval;
        AbortOnDrop(tokio::spawn(async move {
            probe::run(&router, &io, &session, interval, window_end, false).await
        }))
    };
    let mut pending = usize::MIN;
    loop {
        tokio::select! {
            // A handshake that came in is seen before the deadline that would end the session.
            biased;
            inc = incoming.recv() => match inc {
                Some(Incoming::Started(addr)) => {
                    pending += 1;
                    if t.phase() == AttemptPhase::Punching {
                        handshaking(&core, &t, addr, session.handshake);
                    }
                }
                Some(Incoming::Ended) => pending = pending.saturating_sub(1),
                Some(Incoming::Failed(cause)) => {
                    pending = pending.saturating_sub(1);
                    if pending == usize::MIN {
                        probes.abort();
                        let reason = if cause.name == "handshake_timeout" {
                            ResultReason::Timeout
                        } else {
                            ResultReason::HandshakeFailed
                        };
                        result(&core, key, Some(reason));
                        t.to(AttemptPhase::Closed, cause);
                        return;
                    }
                }
                Some(Incoming::Established(conn)) => {
                    probes.abort();
                    if t.phase() == AttemptPhase::Punching {
                        let addr = crate::socket::canonical(conn.remote_address());
                        handshaking(&core, &t, addr, session.handshake);
                    }
                    if !core.pairing.is_paired(&from) {
                        conn.close(
                            P2pCloseReason::NotPaired.var_int(),
                            P2pCloseReason::NotPaired.reason(),
                        );
                        result(&core, key, Some(ResultReason::HandshakeFailed));
                        t.to(
                            AttemptPhase::Closed,
                            Cause::with("not_paired", CauseDetail::Close(P2pCloseReason::NotPaired)),
                        );
                        return;
                    }
                    result(&core, key, None);
                    drop(cleanup);
                    let addr = crate::socket::canonical(conn.remote_address());
                    let wrapped = ConnInner::new(conn.clone(), t.id, from);
                    t.update(|r| {
                        r.conn = Some(conn.clone());
                        r.info.remote_candidate = session.candidate(addr);
                    });
                    t.to(AttemptPhase::Connected, Cause::named("quic_established"));
                    let _queued = core.accepted_tx.send(wrapped.clone()).is_ok();
                    // B observes the address from the one it hands the connection out with.
                    tokio::spawn(crate::endpoint::watch_connection(
                        core.clone(),
                        t.clone(),
                        wrapped,
                        conn,
                        Some(addr),
                    ));
                    return;
                }
                None => {}
            },
            () = tokio::time::sleep_until(accept_until), if pending == usize::MIN => {
                probes.abort();
                result(&core, key, Some(ResultReason::Timeout));
                // A handshake was taken: its deadline is what passed.
                let name = if t.phase() == AttemptPhase::Handshaking {
                    "handshake_timeout"
                } else {
                    "accept_deadline_elapsed"
                };
                t.to(AttemptPhase::Closed, Cause::named(name));
                return;
            }
            r = next_cancel(&mut signals) => {
                // A superseded session ends in any phase: the dialer moved to a new one.
                let handshaking = t.phase() == AttemptPhase::Handshaking;
                if matches!(r, CancelReason::PeerLimitsInvalid | CancelReason::Superseded)
                    || !handshaking
                {
                    probes.abort();
                    t.to(AttemptPhase::Closed, cancelled(r));
                    return;
                }
            }
        }
    }
}
