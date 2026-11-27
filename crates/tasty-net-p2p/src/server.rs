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
use tokio::task::JoinSet;
use tracing::debug;

use crate::endpoint::{Core, PunchHold, PunchSignal, RENDEZVOUS_ALPN, RequestOutcome, lock};
use crate::error::StopCause;
use crate::events::{Cause, CauseDetail, EndpointPhase};
use crate::nat;

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

/// The TLS-stage failures of a handshake: a peer key this side refused and the peer's
/// `no_application_protocol` alert.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TlsStage {
    KeyRefused,
    NoAlpn,
}

/// Whether a local error code is a certificate error of this side's verifier.
fn key_refused(code: quinn::TransportErrorCode) -> bool {
    [
        rustls::AlertDescription::AccessDenied,
        rustls::AlertDescription::BadCertificate,
        rustls::AlertDescription::DecryptError,
    ]
    .map(alert_code)
    .contains(&code)
}

/// The QUIC code of a TLS alert (RFC 9001 §4.8).
fn alert_code(alert: rustls::AlertDescription) -> quinn::TransportErrorCode {
    quinn::TransportErrorCode::crypto(u8::from(alert))
}

/// The TLS stage of a failed handshake, if it failed there. A key is refused only by this
/// side's own verifier, which fails with a certificate error: a key other than the expected
/// one (`access_denied`), a key that is not a valid ed25519 raw public key (`bad_certificate`)
/// or a signature it does not verify (`decrypt_error`). Other local errors (QUIC transport
/// parameters, a protocol violation) are not the key's.
/// The TLS stage of a handshake this side took (the acceptor): its own verifier refused the
/// key, or it offered none of the peer's protocols.
pub(crate) fn incoming_tls_stage(e: &ConnectionError) -> Option<TlsStage> {
    match e {
        ConnectionError::TransportError(t) if key_refused(t.code) => Some(TlsStage::KeyRefused),
        ConnectionError::TransportError(t)
            if t.code == alert_code(rustls::AlertDescription::NoApplicationProtocol) =>
        {
            Some(TlsStage::NoAlpn)
        }
        _ => None,
    }
}

pub(crate) fn tls_stage(e: &ConnectionError) -> Option<TlsStage> {
    match e {
        ConnectionError::TransportError(t) if key_refused(t.code) => Some(TlsStage::KeyRefused),
        ConnectionError::ConnectionClosed(c)
            if c.error_code == alert_code(rustls::AlertDescription::NoApplicationProtocol) =>
        {
            Some(TlsStage::NoAlpn)
        }
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
        let end = session(&core).await;
        // How long the registration that just ended was held; none when it did not register.
        let held = lock(&core.reg).registered_since.map(|t| t.elapsed());
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
        // A registration held longer than reset_after starts the failures again
        // (docs/design/p2p.md "재시도와 포기").
        if held.is_some_and(|h| h >= core.params.reconnect.reset_after) {
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
    r.registered_at = None;
    r.registered_since = None;
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
    let Some(quic) = core.quic() else {
        return End::Shutdown;
    };
    let connecting = match quic.connect_with(config, core.server.addr, SERVER_NAME) {
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
        // A frame the stream ended in the middle of is malformed (docs/design/rendezvous.md
        // "프레임"), whatever closes the connection after it.
        Err(FrameError::Truncated) => return close_malformed(conn, FrameError::Truncated),
        Ok(None) | Err(FrameError::Io(_)) => {
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
    // The mismatch is in place before anything can see the registration: an attempt that sees
    // `Registered` also sees it and ends with ServerLimitsInvalid.
    let mismatch = limits_mismatch(core, &welcome.limits);
    {
        let mut r = lock(&core.reg);
        r.mismatch = mismatch;
        r.registered_at = Some(SystemTime::now());
        r.registered_since = Some(Instant::now());
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<DeviceMessage>();
    let generation = core.open_link(tx);
    let server_limit = welcome.limits.max_message_bytes;
    core.endpoint_to(EndpointPhase::Registered, Cause::named("registered"));
    if let Some(key) = mismatch {
        core.endpoint_to(
            EndpointPhase::Registered,
            Cause::with(
                "server_limits_mismatch",
                limits_detail(core, key, &welcome.limits),
            ),
        );
    }
    // The judgements this registration starts belong to it: when the registration ends (the
    // session or the endpoint ends), dropping the set aborts the ones still running, so no STUN
    // request of theirs goes out after that and no transaction ID of theirs waits.
    let mut judging = JoinSet::new();
    judging.spawn(rejudge(core.clone(), generation));
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
                    Ok(m) => dispatch(core, generation, m, &mut judging),
                    Err(e) => break close_malformed(conn, e),
                },
                Some(Ok(None)) | Some(Err(FrameError::Io(_))) | None => {
                    break match tokio::time::timeout(frame_timeout, conn.closed()).await {
                        Ok(c) => lost(&c),
                        Err(_) => close_malformed(conn, "the coordination stream ended"),
                    };
                }
                Some(Err(e)) => break close_malformed(conn, e),
            },
            e = conn.closed() => {
                // The reader may not have handed over what the stream held before the close:
                // a frame cut in the middle is malformed even when the close is seen first.
                break match tokio::time::timeout(frame_timeout, cut_frame(&mut msg_rx)).await {
                    Ok(true) => close_malformed(conn, FrameError::Truncated),
                    _ => lost(&e),
                };
            }
            Some(_) = judging.join_next() => {}
            _ = shutdown.changed() => break End::Shutdown,
        }
    };
    reader.abort();
    end
}

/// Whether the reader's remaining results end with a frame cut in the middle. The reader ends
/// at its first result that is not a frame, so this ends with it.
async fn cut_frame(
    results: &mut mpsc::UnboundedReceiver<Result<Option<Vec<u8>>, FrameError>>,
) -> bool {
    while let Some(r) = results.recv().await {
        if matches!(r, Err(FrameError::Truncated)) {
            return true;
        }
    }
    false
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
    } else if !crate::settings::bound_fits(crate::attempt::attempt_bound(p, l)) {
        Some(crate::attempt::ATTEMPT_BOUND_KEY)
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
        crate::attempt::ATTEMPT_BOUND_KEY => (
            format!("{:?}", crate::attempt::attempt_bound(p, l)),
            format!(
                "start_delay_max_ms {} punch_window_ms {} quic_handshake_ms {}",
                l.start_delay_max_ms, l.punch_window_ms, l.quic_handshake_ms
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

/// Act on a message of the server of registration `generation`. A judgement it starts goes
/// into `judging`, the set the registration owns.
pub(crate) fn dispatch(
    core: &Arc<Core>,
    generation: u64,
    m: ServerMessage,
    judging: &mut JoinSet<()>,
) {
    match m {
        ServerMessage::Welcome(_) => debug!("a second welcome ignored"),
        ServerMessage::ReflexiveChanged(c) => {
            lock(&core.reg).reflexive = Some(crate::socket::canonical(c.reflexive));
            judging.spawn(rejudge(core.clone(), generation));
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
                    // The answer carries the channel's hold: a dialer that gives the request
                    // up, before or after the answer is queued, drops it and the channel.
                    let (sig_tx, sig_rx) = tokio::sync::mpsc::unbounded_channel();
                    let hold = PunchHold::insert(core, (generation, go.punch_id), sig_tx);
                    let _sent = tx.send(RequestOutcome::Go(go, sig_rx, hold)).is_ok();
                }
            }
            None => {
                if let Some(tx) = lock(&core.punches).get(&(generation, go.punch_id)) {
                    let _sent = tx.send(PunchSignal::Go(go)).is_ok();
                }
            }
        },
        ServerMessage::PunchOffer(o) => {
            tokio::spawn(crate::attempt::accept(
                core.clone(),
                generation,
                o.punch_id,
                o.from,
            ));
        }
        ServerMessage::PunchCancel(c) => {
            if let Some(tx) = lock(&core.punches).get(&(generation, c.punch_id)) {
                let _sent = tx.send(PunchSignal::Cancel(c.reason)).is_ok();
            }
        }
    }
}

/// Judge the NAT again and report it (after registering, after `reflexive_changed`, before a
/// punch retry), for registration `generation`: the one that scheduled the judgement.
///
/// A judgement belongs to that registration: it reads the STUN servers only while that
/// registration is still current, and writes and reports its result only if it still is at
/// the end. A judgement scheduled on one registration that first runs after the next one began
/// does nothing.
pub(crate) async fn rejudge(core: Arc<Core>, generation: u64) {
    let targets: Vec<_> = {
        let r = lock(&core.reg);
        if core.generation() != Some(generation) {
            debug!("a judgement of an earlier registration not begun");
            return;
        }
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
    let (sent, changed) = {
        let mut r = lock(&core.reg);
        if core.generation() != Some(generation) {
            debug!("a judgement of an earlier registration dropped");
            return;
        }
        // The report goes into the link's queue before the judgement shows in the status:
        // whatever this device asks after seeing it (a punch_request) reaches the server
        // after the report, on the same stream.
        let sent = core.send_on(generation, report);
        let before = r
            .nat
            .as_ref()
            .map(|n| (n.mapping, n.answers.first().and_then(|a| a.reflexive)));
        let changed = before != Some((j.mapping, j.answers.first().and_then(|a| a.reflexive)));
        r.nat = Some(j);
        (sent, changed && r.phase == Some(EndpointPhase::Registered))
    };
    if sent && changed {
        core.endpoint_to(EndpointPhase::Registered, Cause::named("nat_changed"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_certificate_alerts_of_this_side_are_a_refused_key() {
        for alert in [
            rustls::AlertDescription::AccessDenied,
            rustls::AlertDescription::BadCertificate,
            rustls::AlertDescription::DecryptError,
        ] {
            assert!(key_refused(alert_code(alert)), "{alert:?}");
        }
        for code in [
            quinn::TransportErrorCode::TRANSPORT_PARAMETER_ERROR,
            quinn::TransportErrorCode::PROTOCOL_VIOLATION,
            alert_code(rustls::AlertDescription::InternalError),
            alert_code(rustls::AlertDescription::HandshakeFailure),
        ] {
            assert!(!key_refused(code), "{code:?}");
        }
        assert_eq!(tls_stage(&ConnectionError::TimedOut), None);
    }
}
