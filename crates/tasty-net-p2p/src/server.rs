//! The coordination connection to the rendezvous server: connect, register, judge the NAT,
//! keep the connection, reconnect with backoff (docs/design/p2p.md "흐름" 1–5, 14, "재시도와
//! 포기", "endpoint" state table; docs/design/rendezvous.md "메시지").

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use quinn::ConnectionError;
use tasty_net_rendezvous::tls::SERVER_NAME;
use tasty_net_rendezvous::wire::CloseCode;
use tasty_net_rendezvous::wire::PROTO;
use tasty_net_rendezvous::wire::frame::{FrameError, encode_frame, read_frame, read_frame_by};
use tasty_net_rendezvous::wire::message::{
    DeviceMessage, Hello, Limits, NatReport, ServerMessage, StunResult, Welcome,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::endpoint::{Core, Link, PunchSignal, RENDEZVOUS_ALPN, RequestOutcome, lock};
use crate::error::StopCause;
use crate::events::{Cause, CauseDetail, EndpointPhase};
use crate::nat::{self, NatJudgement};

/// How a registration ended or failed.
enum End {
    /// Reconnect after the backoff.
    Retry(Cause),
    /// Do not reconnect.
    Stop(StopCause),
    /// The endpoint shuts down.
    Shutdown,
}

fn io_cause(kind: std::io::ErrorKind, what: impl ToString) -> Cause {
    Cause::with(
        "io",
        CauseDetail::Io {
            kind,
            what: what.to_string(),
        },
    )
}

fn malformed(what: impl ToString) -> Cause {
    io_cause(std::io::ErrorKind::InvalidData, what)
}

/// The cause of a coordination connection that the server closed or that was lost.
fn lost(e: &ConnectionError) -> End {
    match e {
        ConnectionError::ApplicationClosed(c) => {
            let code = c.error_code.into_inner();
            match CloseCode::from_code(code) {
                Some(CloseCode::AccessDenied) => End::Stop(StopCause::AccessDenied),
                Some(CloseCode::Superseded) => End::Stop(StopCause::Superseded),
                Some(CloseCode::UnsupportedProto) => End::Stop(StopCause::UnsupportedProto),
                _ => End::Retry(Cause::with("server_closed", CauseDetail::ServerCode(code))),
            }
        }
        ConnectionError::TimedOut => End::Retry(Cause::named("idle_timeout")),
        other => End::Retry(io_cause(crate::conn::connection_error_kind(other), other)),
    }
}

/// The TLS-stage failures of a handshake: a peer key this side refused (a local TLS error) and
/// the peer's `no_application_protocol` alert.
pub(crate) enum TlsStage {
    KeyRefused,
    NoAlpn,
}

pub(crate) fn tls_stage(e: &ConnectionError) -> Option<TlsStage> {
    let no_alpn = quinn::TransportErrorCode::crypto(u8::from(
        rustls::AlertDescription::NoApplicationProtocol,
    ));
    match e {
        ConnectionError::TransportError(_) => Some(TlsStage::KeyRefused),
        ConnectionError::ConnectionClosed(c) if c.error_code == no_alpn => Some(TlsStage::NoAlpn),
        _ => None,
    }
}

/// The endpoint's server loop: connect, register, keep, back off, until stopped or shut down.
pub(crate) async fn run(core: Arc<Core>) {
    let mut failures: u64 = u64::MIN;
    let mut from_backoff = false;
    loop {
        if core.is_shutting_down() {
            return;
        }
        lock(&core.reg).attempt += 1;
        lock(&core.reg).next_at = None;
        core.endpoint_to(
            EndpointPhase::Connecting,
            Cause::named(if from_backoff {
                "backoff_elapsed"
            } else {
                "enabled"
            }),
        );
        let started = Instant::now();
        let end = session(&core).await;
        clear_registration(&core);
        // The shutdown sets its flag and reports `Off` before it closes the QUIC endpoint, so a
        // session that ended with the closing socket's error ends here without a transition.
        if core.is_shutting_down() {
            return;
        }
        let cause = match end {
            End::Shutdown => return,
            End::Stop(c) => {
                lock(&core.reg).stopped = Some(c);
                core.endpoint_to(
                    EndpointPhase::Stopped,
                    Cause::with(c.name(), CauseDetail::Stop(c)),
                );
                return;
            }
            End::Retry(c) => c,
        };
        let registered_long = lock(&core.reg).registered_at.is_some()
            && started.elapsed() >= core.params.reconnect.reset_after;
        if registered_long {
            failures = u64::MIN;
        }
        failures += 1;
        let max = core.params.reconnect.max_attempts;
        if max > u64::MIN && failures >= max {
            let c = StopCause::ReconnectExhausted;
            lock(&core.reg).stopped = Some(c);
            core.endpoint_to(
                EndpointPhase::Stopped,
                Cause::with(c.name(), CauseDetail::Stop(c)),
            );
            return;
        }
        let delay = core
            .params
            .reconnect
            .delay(failures, core.params.reconnect.draw_jitter());
        lock(&core.reg).next_at = Instant::now().checked_add(delay);
        core.endpoint_to(EndpointPhase::Backoff, cause);
        let mut shutdown = core.shutdown.subscribe();
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => return,
        }
        from_backoff = true;
    }
}

fn clear_registration(core: &Core) {
    *lock(&core.link) = None;
    // Attempts still waiting for the server lose it.
    lock(&core.requests).clear();
    for tx in lock(&core.punches).values() {
        let _sent = tx.send(PunchSignal::Lost).is_ok();
    }
    let mut r = lock(&core.reg);
    r.limits = None;
    r.mismatch = None;
    r.stun.clear();
}

/// One coordination connection from connect to end.
async fn session(core: &Arc<Core>) -> End {
    let config = match core.client_config(
        core.server.public_key,
        RENDEZVOUS_ALPN,
        &core.params.rendezvous_quic,
    ) {
        Ok(c) => c,
        Err(e) => return End::Retry(io_cause(std::io::ErrorKind::Other, e)),
    };
    let connecting = match core
        .endpoint
        .connect_with(config, core.server.addr, SERVER_NAME)
    {
        Ok(c) => c,
        Err(e) => return End::Retry(io_cause(std::io::ErrorKind::Other, e)),
    };
    let mut shutdown = core.shutdown.subscribe();
    let conn = tokio::select! {
        r = tokio::time::timeout(core.params.connect_timeout, connecting) => match r {
            Err(_) => return End::Retry(Cause::named("connect_timeout")),
            Ok(Err(e)) => return match tls_stage(&e) {
                Some(TlsStage::KeyRefused) => End::Stop(StopCause::ServerKeyMismatch),
                Some(TlsStage::NoAlpn) => End::Stop(StopCause::AlpnMismatch),
                None => lost(&e),
            },
            Ok(Ok(c)) => c,
        },
        _ = shutdown.changed() => return End::Shutdown,
    };
    let end = registered(core, &conn, &mut shutdown).await;
    if matches!(end, End::Shutdown) {
        conn.close(CloseCode::Closed.var_int(), CloseCode::Closed.reason());
    }
    end
}

async fn registered(
    core: &Arc<Core>,
    conn: &quinn::Connection,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> End {
    let p = &core.params;
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return lost(&e),
    };
    let hello = DeviceMessage::Hello(Hello {
        proto: PROTO,
        host_candidates: crate::endpoint::host_candidates(core),
    });
    let frame = match hello
        .to_json()
        .map_err(|e| e.to_string())
        .and_then(|b| encode_frame(&b, p.max_message_bytes).map_err(|e| e.to_string()))
    {
        Ok(f) => f,
        Err(e) => return End::Retry(malformed(e)),
    };
    if let Err(e) = send.write_all(&frame).await {
        return match conn.close_reason() {
            Some(c) => lost(&c),
            None => End::Retry(io_cause(std::io::ErrorKind::BrokenPipe, e)),
        };
    }
    let first_by = tokio::time::Instant::now() + p.hello_timeout;
    let welcome = tokio::select! {
        r = read_frame_by(&mut recv, p.max_message_bytes, p.frame_timeout, Some(first_by)) => r,
        _ = shutdown.changed() => return End::Shutdown,
    };
    let welcome = match welcome {
        Ok(Some(body)) => match ServerMessage::parse(&body) {
            Ok(ServerMessage::Welcome(w)) => w,
            Ok(other) => {
                return close_malformed(conn, format!("{} before welcome", other.type_name()));
            }
            Err(e) => return close_malformed(conn, e),
        },
        Err(FrameError::Idle) => {
            conn.close(CloseCode::Closed.var_int(), CloseCode::Closed.reason());
            return End::Retry(Cause::named("hello_timeout"));
        }
        Ok(None) | Err(FrameError::Io(_)) | Err(FrameError::Truncated) => {
            return match conn.close_reason() {
                Some(c) => lost(&c),
                None => match tokio::time::timeout(p.frame_timeout, conn.closed()).await {
                    Ok(c) => lost(&c),
                    Err(_) => close_malformed(conn, "the coordination stream ended"),
                },
            };
        }
        Err(e) => return close_malformed(conn, e),
    };
    on_welcome(core, &welcome);
    let (tx, mut rx) = mpsc::unbounded_channel::<DeviceMessage>();
    *lock(&core.link) = Some(Link { tx });
    let server_limit = welcome.limits.max_message_bytes;
    let registered_at = SystemTime::now();
    lock(&core.reg).registered_at = Some(registered_at);
    core.endpoint_to(EndpointPhase::Registered, Cause::named("registered"));
    if let Some(key) = limits_mismatch(core, &welcome.limits) {
        lock(&core.reg).mismatch = Some(key);
        core.endpoint_to(
            EndpointPhase::Registered,
            Cause::with(
                "server_limits_mismatch",
                limits_detail(core, key, &welcome.limits),
            ),
        );
    }
    tokio::spawn(rejudge(core.clone()));
    // The reader owns the receiving side: a frame is never cut by a select.
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let limit = p.max_message_bytes;
    let frame_timeout = p.frame_timeout;
    let reader = tokio::spawn(async move {
        loop {
            let r = read_frame(&mut recv, limit, frame_timeout).await;
            let stop = !matches!(r, Ok(Some(_)));
            if msg_tx.send(r).is_err() || stop {
                return;
            }
        }
    });
    let end = loop {
        tokio::select! {
            m = rx.recv() => {
                let Some(m) = m else { break End::Retry(Cause::named("server_closed")) };
                match m.to_json().map_err(|e| e.to_string()).and_then(|b| {
                    encode_frame(&b, server_limit).map_err(|e| e.to_string())
                }) {
                    Ok(f) => {
                        if let Err(e) = send.write_all(&f).await {
                            debug!(error = %e, "coordination write failed");
                        }
                    }
                    Err(e) => debug!(error = %e, "coordination message not sent"),
                }
            }
            r = msg_rx.recv() => match r {
                Some(Ok(Some(body))) => match ServerMessage::parse(&body) {
                    Ok(m) => dispatch(core, m),
                    Err(e) => break close_malformed(conn, e),
                },
                Some(Ok(None)) | Some(Err(FrameError::Io(_))) | Some(Err(FrameError::Truncated)) | None => {
                    break match tokio::time::timeout(frame_timeout, conn.closed()).await {
                        Ok(c) => lost(&c),
                        Err(_) => close_malformed(conn, "the coordination stream ended"),
                    };
                }
                Some(Err(e)) => break close_malformed(conn, e),
            },
            e = conn.closed() => break lost(&e),
            _ = shutdown.changed() => break End::Shutdown,
        }
    };
    reader.abort();
    end
}

fn close_malformed(conn: &quinn::Connection, what: impl ToString) -> End {
    conn.close(
        CloseCode::Malformed.var_int(),
        CloseCode::Malformed.reason(),
    );
    End::Retry(malformed(what))
}

fn on_welcome(core: &Core, w: &Welcome) {
    let mut r = lock(&core.reg);
    r.reflexive = Some(crate::socket::canonical(w.reflexive));
    r.stun = w
        .stun
        .iter()
        .map(|a| crate::socket::canonical(*a))
        .collect();
    r.limits = Some(w.limits);
}

/// The first server value that does not fit this device's settings
/// (docs/design/rendezvous.md "메시지 표" `welcome.limits`).
pub(crate) fn limits_mismatch(core: &Core, l: &Limits) -> Option<&'static str> {
    let p = &core.params;
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let candidates = u64::from(l.max_reported_reflexive)
        .saturating_add(u64::from(l.max_host_candidates))
        .saturating_add(1);
    if ms(p.request_timeout) <= l.offer_timeout_ms {
        Some("punch.request_timeout")
    } else if l.punch_window_ms > ms(p.window_max) {
        Some("punch.window_max")
    } else if l.quic_handshake_ms > ms(p.handshake_timeout_max) {
        Some("quic.handshake_timeout_max")
    } else if candidates > u64::try_from(p.max_candidates).unwrap_or(u64::MAX) {
        Some("punch.max_candidates")
    } else if l.max_message_bytes > p.max_message_bytes {
        Some("rendezvous.max_message_bytes")
    } else {
        None
    }
}

fn limits_detail(core: &Core, key: &'static str, l: &Limits) -> CauseDetail {
    let p = &core.params;
    let (device, server) = match key {
        "punch.request_timeout" => (
            format!("{:?}", p.request_timeout),
            format!("offer_timeout_ms {}", l.offer_timeout_ms),
        ),
        "punch.window_max" => (
            format!("{:?}", p.window_max),
            format!("punch_window_ms {}", l.punch_window_ms),
        ),
        "quic.handshake_timeout_max" => (
            format!("{:?}", p.handshake_timeout_max),
            format!("quic_handshake_ms {}", l.quic_handshake_ms),
        ),
        "punch.max_candidates" => (
            p.max_candidates.to_string(),
            format!(
                "max_reported_reflexive {} max_host_candidates {}",
                l.max_reported_reflexive, l.max_host_candidates
            ),
        ),
        _ => (
            p.max_message_bytes.to_string(),
            format!("max_message_bytes {}", l.max_message_bytes),
        ),
    };
    CauseDetail::Limits {
        key,
        device,
        server,
    }
}

fn dispatch(core: &Arc<Core>, m: ServerMessage) {
    match m {
        ServerMessage::Welcome(_) => debug!("a second welcome ignored"),
        ServerMessage::ReflexiveChanged(c) => {
            lock(&core.reg).reflexive = Some(crate::socket::canonical(c.reflexive));
            tokio::spawn(rejudge(core.clone()));
        }
        ServerMessage::PunchRefused(r) => {
            if let Some(tx) = lock(&core.requests).remove(&r.request_id) {
                let _sent = tx.send(RequestOutcome::Refused(r.reason)).is_ok();
            }
        }
        ServerMessage::PunchGo(go) => match go.request_id {
            Some(id) => {
                if let Some(tx) = lock(&core.requests).remove(&id) {
                    // The punch's channel exists before the dialer wakes, so a cancel that
                    // follows at once is not lost.
                    let (sig_tx, sig_rx) = tokio::sync::mpsc::unbounded_channel();
                    lock(&core.punches).insert(go.punch_id, sig_tx);
                    let _sent = tx.send(RequestOutcome::Go(go, sig_rx)).is_ok();
                }
            }
            None => {
                if let Some(tx) = lock(&core.punches).get(&go.punch_id) {
                    let _sent = tx.send(PunchSignal::Go(go)).is_ok();
                }
            }
        },
        ServerMessage::PunchOffer(o) => {
            tokio::spawn(crate::attempt::accept(core.clone(), o.punch_id, o.from));
        }
        ServerMessage::PunchCancel(c) => {
            if let Some(tx) = lock(&core.punches).get(&c.punch_id) {
                let _sent = tx.send(PunchSignal::Cancel(c.reason)).is_ok();
            }
        }
    }
}

/// Judge the NAT again and report it (after registering, after `reflexive_changed`, before a
/// punch retry). Returns the judgement.
pub(crate) async fn rejudge(core: Arc<Core>) -> NatJudgement {
    let targets: Vec<_> = {
        let r = lock(&core.reg);
        std::iter::once(core.server.addr)
            .chain(r.stun.iter().copied())
            .collect()
    };
    let j = nat::detect(
        &core.router,
        &core.io,
        &targets,
        core.params.stun_rto,
        core.params.stun_attempts,
    )
    .await;
    let report = DeviceMessage::NatReport(NatReport {
        mapping: j.mapping,
        stun_results: j
            .answers
            .iter()
            .map(|a| StunResult {
                server: a.server.to_string(),
                reflexive: a.reflexive.map(|r| r.to_string()),
            })
            .collect(),
        host_candidates: Some(crate::endpoint::host_candidates(&core)),
    });
    let changed = {
        let mut r = lock(&core.reg);
        let before = r
            .nat
            .as_ref()
            .map(|n| (n.mapping, n.answers.first().and_then(|a| a.reflexive)));
        let changed = before != Some((j.mapping, j.answers.first().and_then(|a| a.reflexive)));
        r.nat = Some(j.clone());
        changed && r.phase == Some(EndpointPhase::Registered)
    };
    if core.send(report) && changed {
        core.endpoint_to(EndpointPhase::Registered, Cause::named("nat_changed"));
    }
    j
}
