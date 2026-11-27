//! Punching over the mock network (docs/design/testing.md "시험 목록", the `tests/punch.rs`
//! rows): NAT pairs, policy, cancels and deadlines, the limits checks.

mod support;

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tasty_net_identity::DeviceId;
use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{
    AttemptPhase, CauseDetail, P2pCancel, P2pCloseReason, P2pConfig, P2pConnectError,
    P2pConnection, P2pEndpoint, P2pError, P2pEvent, PairingStore, Phase, RemoteKind, Side,
};
use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};
use tasty_net_rendezvous::tls::{self, SERVER_NAME, TlsOptions};
use tasty_net_rendezvous::wire::ALPN_P2P;
use tasty_net_rendezvous::wire::message::{
    CancelReason, NatMapping, PunchCancel, PunchMode, ServerMessage,
};

use support::fake::{Fake, Script};
use support::{
    Device, Log, MockIo, NatSpec, Rule, TestServer, Verdict, World, attempt_to, block_all, device,
    device_with, is_quic, key, only_to, pair, raw_endpoint, registered, server_settings, settings,
};

const LONG: Duration = Duration::from_secs(40);

fn server_ips(s: &TestServer) -> Vec<IpAddr> {
    let ip = s.info.addr.ip();
    let IpAddr::V4(v4) = ip else { unreachable!() };
    let o = v4.octets();
    vec![ip, IpAddr::from([o[0], o[1], o[2], o[3] + 1])]
}

/// Server settings with a short window and handshake, for tests that wait for failures.
fn fast_server() -> ServerSettings {
    let mut s = server_settings();
    s.set(
        "punch_window",
        ServerValue::Duration(Duration::from_secs(1)),
    )
    .unwrap();
    s.set(
        "quic_handshake_timeout",
        ServerValue::Duration(Duration::from_secs(1)),
    )
    .unwrap();
    s
}

fn error(r: Result<P2pConnection, P2pError>) -> P2pConnectError {
    match r {
        Ok(_) => panic!("connected"),
        Err(e) => *e
            .connect_error()
            .unwrap_or_else(|| panic!("not a connect error: {e:?}")),
    }
}

/// A connects to B and B takes the connection.
fn connect(a: &Device, b: &Device) -> (P2pConnection, P2pConnection) {
    let ca = a.ep.connect(b.id()).expect("connect");
    let cb =
        b.ep.accept_timeout(Some(Duration::from_secs(10)))
            .expect("accept");
    (ca, cb)
}

/// One request and its echo over a new stream.
fn echo(a: &P2pConnection, b: &P2pConnection) {
    let mut s = a.open_bi().expect("open");
    s.write_all(b"ping").unwrap();
    s.finish().unwrap();
    let mut r = b
        .accept_bi(Some(Duration::from_secs(10)))
        .expect("accept_bi");
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"ping");
    r.write_all(b"pong").unwrap();
    r.finish().unwrap();
    let mut back = Vec::new();
    s.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"pong");
}

fn sent_to_ip(d: &Device, ip: IpAddr) -> usize {
    d.port.sent().iter().filter(|a| a.ip() == ip).count()
}

fn closed_with(log: &[P2pEvent], name: &str) -> bool {
    log.iter().any(|e| {
        matches!(e, P2pEvent::Transition { to: Phase::Attempt(AttemptPhase::Closed), cause, .. } if cause.name == name)
    })
}

fn closed_by_cancel(log: &[P2pEvent], reason: CancelReason) -> bool {
    log.iter().any(|e| {
        matches!(e, P2pEvent::Transition {
            to: Phase::Attempt(AttemptPhase::Closed),
            cause,
            ..
        } if cause.name == "punch_cancelled" && cause.detail == Some(CauseDetail::Cancel(reason)))
    })
}

/// The phase an attempt was in when it closed.
fn phase_before_close(log: &[P2pEvent]) -> Option<AttemptPhase> {
    log.iter().find_map(|e| match e {
        P2pEvent::Transition {
            from: Some(Phase::Attempt(from)),
            to: Phase::Attempt(AttemptPhase::Closed),
            ..
        } => Some(*from),
        _ => None,
    })
}

fn count_to(log: &[P2pEvent], to: AttemptPhase) -> usize {
    log.iter()
        .filter(|e| matches!(e, P2pEvent::Transition { to: Phase::Attempt(t), .. } if *t == to))
        .count()
}

fn two_nats(w: &World, sa: NatSpec, sb: NatSpec) -> (support::NatHandle, support::NatHandle) {
    (w.net.nat(1, sa), w.net.nat(2, sb))
}

#[test]
fn eim_pair_punches() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    assert_eq!(a.ep.status().nat, NatMapping::Independent);
    let (ca, cb) = connect(&a, &b);
    echo(&ca, &cb);
    let ia = ca.info().expect("info");
    assert_eq!(ia.side, Side::Dialer);
    assert_eq!(ia.state, AttemptPhase::Connected);
    assert_eq!(ia.nat.expect("nat").mode, PunchMode::All);
    assert_eq!(
        ia.remote_candidate.expect("candidate").kind,
        RemoteKind::ServerReflexive
    );
    assert_eq!(ia.rendezvous, Some(s.info));
    assert_eq!(cb.info().expect("info").side, Side::Acceptor);
    assert_eq!(cb.remote_device_id(), a.id());
}

#[test]
fn same_lan_uses_host_candidate() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let a = device(&n, 2, 1, &s, settings(&[]));
    let b = device(&n, 3, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let (ca, cb) = connect(&a, &b);
    echo(&ca, &cb);
    let c = ca.info().unwrap().remote_candidate.unwrap();
    assert_eq!(c.kind, RemoteKind::Host);
    assert_eq!(c.addr, b.port.addr());
}

#[test]
fn one_sided_dependent_uses_peer_reflexive() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::symmetric(), NatSpec::full_cone());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    assert_eq!(a.ep.status().nat, NatMapping::Dependent);
    assert_eq!(b.ep.status().nat, NatMapping::Independent);
    let (ca, cb) = connect(&a, &b);
    echo(&ca, &cb);
    let ib = cb.info().unwrap();
    assert_eq!(ib.remote_candidate.unwrap().kind, RemoteKind::PeerReflexive);
    // A's mapping towards B is not the one the server saw.
    assert_ne!(
        Some(ib.remote_candidate.unwrap().addr),
        a.ep.status().reflexive
    );
}

#[test]
fn dependent_pair_tries_host_candidates_only() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::symmetric());
    let a = device(&n, 2, 1, &s, settings(&[]));
    let b = device(&n, 3, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let (ca, cb) = connect(&a, &b);
    echo(&ca, &cb);
    let ia = ca.info().unwrap();
    assert_eq!(ia.nat.unwrap().mode, PunchMode::HostOnly);
    assert_eq!(ia.remote_candidate.unwrap().kind, RemoteKind::Host);
    // No probe went to B's reflexive address.
    assert_eq!(sent_to_ip(&a, n.public_ip()), 0);
    assert!(sent_to_ip(&a, b.port.addr().ip()) > 0);
}

#[test]
fn dependent_pair_without_host_path_is_unsupported() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let mut spec = NatSpec::symmetric();
    spec.cgnat = true;
    let (n1, n2) = two_nats(&w, spec, spec);
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    // No address passes the host rule, so the server refuses at once.
    assert_eq!(error(a.ep.connect(b.id())), P2pConnectError::NatUnsupported);
    assert_eq!(sent_to_ip(&a, n2.public_ip()), 0);
    assert!(sent_to_ip(&a, s.info.addr.ip()) > 0);
}

/// Two devices behind one APDM NAT whose inside traffic is source-translated: they are both
/// `dependent` with host candidates, and every probe arrives from a non-host address.
fn host_only_pair(attempts: u64) -> (World, TestServer, Device, Device) {
    let w = World::new();
    let s = w.server(1, fast_server());
    let mut spec = NatSpec::symmetric();
    spec.lan_snat = true;
    let n = w.net.nat(1, spec);
    let st = || settings(&[("punch.attempts", Value::Count(attempts))]);
    let a = device(&n, 2, 1, &s, st());
    let b = device(&n, 3, 2, &s, st());
    pair(&a, &b);
    registered(&a);
    registered(&b);
    (w, s, a, b)
}

#[test]
fn host_only_adds_no_peer_reflexive() {
    let (_w, _s, a, b) = host_only_pair(1);
    let r = a.ep.connect(b.id());
    assert_eq!(error(r), P2pConnectError::NatUnsupported);
    let (stun, _) = b.ep.counters().unwrap();
    // Probes with the right token came from A's public mapping: counted, not answered, not
    // added.
    assert!(stun.probes_dropped.load(Ordering::Relaxed) > 0);
    assert_eq!(stun.probes_answered.load(Ordering::Relaxed), 0);
    let mut log = Log::default();
    log.drain(&b.ep);
    assert_eq!(count_to(&log.0, AttemptPhase::Handshaking), 0);
    assert_eq!(count_to(&log.0, AttemptPhase::Punching), 1);
}

#[test]
fn host_only_exhaustion_is_nat_unsupported() {
    let (_w, _s, a, b) = host_only_pair(2);
    let r = a.ep.connect(b.id());
    assert_eq!(error(r), P2pConnectError::NatUnsupported);
    let mut log = Log::default();
    log.drain(&a.ep);
    assert_eq!(count_to(&log.0, AttemptPhase::RetryWait), 1);
    assert_eq!(count_to(&log.0, AttemptPhase::Punching), 2);
}

#[test]
fn peer_udp_blocked_fails_after_all_attempts() {
    let w = World::new();
    let s = w.server(1, fast_server());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    n1.set_rule(only_to(server_ips(&s)));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let bound = a.ep.status().attempt_bound.expect("bound");
    let t = Instant::now();
    assert_eq!(error(a.ep.connect(b.id())), P2pConnectError::PunchFailed);
    assert!(t.elapsed() <= bound, "{:?} > {bound:?}", t.elapsed());
    let mut log = Log::default();
    log.drain(&a.ep);
    // punch.attempts (2): two punch attempts, one wait between them.
    assert_eq!(count_to(&log.0, AttemptPhase::Punching), 2);
    assert_eq!(count_to(&log.0, AttemptPhase::RetryWait), 1);
    assert!(closed_with(&log.0, "punch_window_elapsed"));
}

#[test]
fn retry_supersedes_previous_session() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    n1.set_rule(only_to(server_ips(&s)));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let bound = a.ep.status().attempt_bound.expect("bound");
    assert_eq!(bound, Duration::from_millis(29_500));
    let t = Instant::now();
    assert_eq!(error(a.ep.connect(b.id())), P2pConnectError::PunchFailed);
    assert!(t.elapsed() <= bound, "{:?}", t.elapsed());
    let mut log = Log::default();
    log.drain(&b.ep);
    // B was still inside the old session's accept deadline when A asked again.
    assert!(
        closed_by_cancel(&log.0, CancelReason::Superseded),
        "{:?}",
        log.0
    );
    assert_eq!(count_to(&log.0, AttemptPhase::Offered), 2);
}

#[test]
fn server_unreachable_is_rendezvous_unavailable() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    n1.set_rule(block_all());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&b);
    let e = error(a.ep.connect(b.id()));
    assert_eq!(e, P2pConnectError::RendezvousUnavailable { stopped: None });
    assert!(e.is_retryable());
    // A tried the server, never the peer.
    assert!(sent_to_ip(&a, s.info.addr.ip()) > 0);
    assert_eq!(sent_to_ip(&a, n2.public_ip()), 0);
}

#[test]
fn declined_peer_gets_no_address() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    a.pairing.add(b.id());
    registered(&a);
    registered(&b);
    assert_eq!(error(a.ep.connect(b.id())), P2pConnectError::Declined);
    assert_eq!(sent_to_ip(&a, n2.public_ip()), 0);
    assert!(sent_to_ip(&a, s.info.addr.ip()) > 0);
    let mut log = Log::default();
    log.drain(&a.ep);
    assert_eq!(count_to(&log.0, AttemptPhase::Scheduled), 0);
    assert!(closed_with(&log.0, "declined"));
    let mut lb = Log::default();
    lb.drain(&b.ep);
    assert!(closed_with(&lb.0, "not_paired"));
}

#[test]
fn peer_on_other_server_is_not_found() {
    let w = World::new();
    let s1 = w.server(1, server_settings());
    let s2 = w.server(2, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s1, settings(&[]));
    let b = device(&n2, 2, 2, &s2, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    assert_eq!(error(a.ep.connect(b.id())), P2pConnectError::PeerNotFound);
    for ip in server_ips(&s2) {
        assert_eq!(sent_to_ip(&a, ip), 0);
    }
    assert!(sent_to_ip(&a, s1.info.addr.ip()) > 0);
    assert_eq!(sent_to_ip(&a, n2.public_ip()), 0);
}

#[test]
fn disabled_endpoint_sends_nothing() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let off = device_with(n.socket(2), 1, s.info, settings(&[]), false);
    let on = device(&n, 3, 2, &s, settings(&[]));
    registered(&on);
    assert!(!off.ep.is_enabled());
    assert_eq!(error(off.ep.connect(on.id())), P2pConnectError::Disabled);
    assert!(
        off.ep
            .accept_timeout(Some(Duration::from_millis(100)))
            .is_err()
    );
    assert!(off.port.sent().is_empty());
    assert!(!on.port.sent().is_empty());
}

#[test]
fn self_dial_is_rejected_locally() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let a = device(&n, 2, 1, &s, settings(&[]));
    registered(&a);
    let e = error(a.ep.connect(a.id()));
    assert_eq!(e, P2pConnectError::SelfAttach);
    assert!(!e.is_retryable());
    let mut log = Log::default();
    log.drain(&a.ep);
    assert_eq!(count_to(&log.0, AttemptPhase::Requesting), 0);
}

#[test]
fn identity_mismatch_fails_handshake() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    // QUIC to B's public address reaches a device with another key; probes still reach B.
    let b_pub = b.ep.status().reflexive.expect("reflexive");
    let impostor = w.net.intercept(b_pub, true, server_ips(&s));
    let tls = tls::server_config(
        &key(99),
        ALPN_P2P,
        TlsOptions {
            max_fragment_size: None,
        },
    )
    .unwrap();
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let ep = raw_endpoint(
        &w.rt,
        impostor,
        Some(quinn::ServerConfig::with_crypto(Arc::new(crypto))),
    );
    let accepting = ep.clone();
    w.rt.spawn(async move {
        while let Some(i) = accepting.accept().await {
            tokio::spawn(async move {
                let _c = i.await;
            });
        }
    });
    let e = error(a.ep.connect(b.id()));
    assert_eq!(e, P2pConnectError::PeerIdentityMismatch);
    assert!(!e.is_retryable());
    let mut log = Log::default();
    log.drain(&a.ep);
    assert!(closed_with(&log.0, "peer_identity_mismatch"));
    assert_eq!(count_to(&log.0, AttemptPhase::RetryWait), 0);
}

/// Paired at the offer, not any more when the connection is up.
#[derive(Debug, Default)]
struct OnceThenNot(AtomicU64);

impl PairingStore for OnceThenNot {
    fn is_paired(&self, _: &DeviceId) -> bool {
        self.0.fetch_add(1, Ordering::Relaxed) == 0
    }
}

#[test]
fn unpaired_quic_is_closed_with_not_paired() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let port = n2.socket(2);
    let mut config = P2pConfig::new(
        support::device_key(&port, 2),
        Arc::new(OnceThenNot::default()),
    );
    config.enabled = true;
    config.server = Some(s.info);
    config.packet_io = Some(Arc::new(MockIo(port)));
    let b = P2pEndpoint::bind(config).unwrap();
    a.pairing.add(b.id());
    registered(&a);
    assert!(support::wait_until(Duration::from_secs(20), || b
        .status()
        .state
        == tasty_net_p2p::EndpointPhase::Registered));
    let ca =
        a.ep.connect(b.id())
            .expect("the connection comes up before B closes it");
    assert!(ca.wait_closed(Duration::from_secs(10)).is_ok());
    assert_eq!(ca.close_reason(), Some(P2pCloseReason::NotPaired));
    let mut log = Log::default();
    log.drain(&b);
    assert!(closed_with(&log.0, "not_paired"));
    assert_eq!(count_to(&log.0, AttemptPhase::Connected), 0);
}

fn raw_client(
    rt: &tokio::runtime::Runtime,
    port: Arc<support::Port>,
    expected: DeviceId,
) -> (quinn::Endpoint, quinn::ClientConfig) {
    let ep = raw_endpoint(rt, port, None);
    let tls = tls::client_config(
        &key(98),
        expected,
        ALPN_P2P,
        TlsOptions {
            max_fragment_size: None,
        },
    )
    .unwrap();
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    (ep, quinn::ClientConfig::new(Arc::new(crypto)))
}

#[test]
fn quic_initial_without_session_is_refused() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(2, NatSpec::full_cone());
    let b = device(&n, 2, 2, &s, settings(&[]));
    registered(&b);
    let b_pub = b.ep.status().reflexive.unwrap();
    let (ep, config) = raw_client(
        &w.rt,
        w.net.public(SocketAddr::from(([192, 0, 2, 50], 6000))),
        b.id(),
    );
    let r = w.rt.block_on(async {
        let c = ep.connect_with(config, b_pub, SERVER_NAME).unwrap();
        tokio::time::timeout(Duration::from_secs(10), c).await
    });
    match r {
        Ok(Err(quinn::ConnectionError::ConnectionClosed(c))) => {
            assert_eq!(c.error_code, quinn::TransportErrorCode::CONNECTION_REFUSED);
        }
        other => panic!("{other:?}"),
    }
    let (stun, _) = b.ep.counters().unwrap();
    assert!(stun.quic_refused.load(Ordering::Relaxed) >= 1);
    assert_eq!(stun.session_mismatch.load(Ordering::Relaxed), 0);
    assert!(b.ep.connections().is_empty());
}

/// A blocked from B, so its punch stays in `Punching`; returns its canceller and thread.
fn hanging_dial<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    a: &'s Device,
    peer: DeviceId,
) -> (
    P2pCancel,
    std::thread::ScopedJoinHandle<'s, Result<P2pConnection, P2pError>>,
) {
    let cancel = P2pCancel::new();
    let c = cancel.clone();
    let h = scope.spawn(move || a.ep.connect_with_cancel(peer, &c));
    (cancel, h)
}

#[test]
fn quic_from_other_device_gets_session_mismatch() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::full_cone());
    // A's probes reach B, so B's session takes A's address; A's QUIC never leaves.
    let servers = server_ips(&s);
    n1.set_rule(Arc::new(move |dest: SocketAddr, p: &[u8]| {
        if is_quic(p) && !servers.contains(&dest.ip()) {
            Verdict::Drop
        } else {
            Verdict::Pass
        }
    }));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let a_pub = a.ep.status().reflexive.unwrap();
    let b_pub = b.ep.status().reflexive.unwrap();
    std::thread::scope(|sc| {
        let (cancel, h) = hanging_dial(sc, &a, b.id());
        // A's probe succeeded: B has had A's probes from A's address.
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Handshaking))
                .is_some()
        );
        // Another device sends QUIC from A's address; B's QUIC to that address reaches it.
        let c_port = w.net.intercept(a_pub, true, server_ips(&s));
        let (ep, config) = raw_client(&w.rt, c_port.clone(), b.id());
        let r = w.rt.block_on(async {
            let c = ep.connect_with(config, b_pub, SERVER_NAME).unwrap();
            let conn = tokio::time::timeout(Duration::from_secs(10), c).await;
            match conn {
                // B closes once TLS has shown the device ID, which may be before or after
                // the client sees the handshake done.
                Ok(Ok(conn)) => Some(conn.closed().await),
                Ok(Err(e)) => Some(e),
                Err(_) => None,
            }
        });
        match r {
            Some(quinn::ConnectionError::ApplicationClosed(c)) => {
                assert_eq!(
                    u64::from(c.error_code),
                    P2pCloseReason::SessionMismatch.code()
                );
            }
            other => {
                let (stun, demux) = b.ep.counters().unwrap();
                panic!(
                    "{other:?}: B {stun:?} {demux:?}, C got {} sent {:?}",
                    c_port.received(),
                    c_port.sent().len()
                );
            }
        }
        let (stun, _) = b.ep.counters().unwrap();
        assert_eq!(stun.session_mismatch.load(Ordering::Relaxed), 1);
        cancel.cancel();
        assert_eq!(error(h.join().unwrap()), P2pConnectError::Cancelled);
    });
}

#[test]
fn second_session_for_same_pair_is_busy() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    n1.set_rule(only_to(server_ips(&s)));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    std::thread::scope(|sc| {
        let (cancel, h) = hanging_dial(sc, &a, b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Punching))
                .is_some()
        );
        let e = error(b.ep.connect(a.id()));
        assert_eq!(e, P2pConnectError::Busy);
        assert!(e.is_retryable());
        cancel.cancel();
        assert_eq!(error(h.join().unwrap()), P2pConnectError::Cancelled);
        assert!(closed_with(la.drain(&a.ep), "cancelled"));
    });
}

#[test]
fn punch_cancel_ends_the_attempt() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    n1.set_rule(only_to(server_ips(&s)));
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[("punch.attempts", Value::Count(1))]),
    );
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    std::thread::scope(|sc| {
        let (_cancel, h) = hanging_dial(sc, &a, b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Punching))
                .is_some()
        );
        // B goes away: the server cancels A's punch.
        b.ep.close();
        let e = error(h.join().unwrap());
        assert_eq!(
            e,
            P2pConnectError::PunchCancelled {
                reason: CancelReason::PeerGone
            }
        );
        assert!(closed_by_cancel(la.drain(&a.ep), CancelReason::PeerGone));
    });
}

#[test]
fn go_timeout_closes_the_offer() {
    let w = World::new();
    let s = w.server(1, fast_server());
    let mut slow = NatSpec::eim();
    slow.latency = Duration::from_millis(400);
    let (n1, n2) = two_nats(&w, NatSpec::eim(), slow);
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[("punch.attempts", Value::Count(1))]),
    );
    let b = device(
        &n2,
        2,
        2,
        &s,
        settings(&[(
            "punch.go_timeout",
            Value::Duration(Duration::from_millis(300)),
        )]),
    );
    pair(&a, &b);
    registered(&a);
    registered(&b);
    // The answer and the go each take 400 ms; B waits 300 ms.
    let _r = a.ep.connect(b.id());
    let mut lb = Log::default();
    assert!(
        lb.wait(&b.ep, LONG, |e| matches!(e, P2pEvent::Transition { to: Phase::Attempt(AttemptPhase::Closed), cause, .. } if cause.name == "go_timeout"))
            .is_some(),
        "{:?}",
        lb.0
    );
    assert_eq!(phase_before_close(&lb.0), Some(AttemptPhase::Offered));
}

/// Delay every packet of a NAT once `on` is set.
fn delay_when(on: Arc<AtomicBool>, by: Duration) -> Rule {
    Arc::new(move |_, _| {
        if on.load(Ordering::Relaxed) {
            Verdict::Delay(by)
        } else {
            Verdict::Pass
        }
    })
}

#[test]
fn request_timeout_is_retryable() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let slow = Arc::new(AtomicBool::new(false));
    n1.set_rule(delay_when(slow.clone(), Duration::from_millis(3500)));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    slow.store(true, Ordering::Relaxed);
    let t = Instant::now();
    let e = error(a.ep.connect(b.id()));
    assert_eq!(e, P2pConnectError::RequestTimeout);
    assert!(e.is_retryable());
    // punch.request_timeout (3 s).
    assert!(t.elapsed() >= Duration::from_secs(3));
    let mut log = Log::default();
    assert!(closed_with(log.drain(&a.ep), "request_timeout"));
}

#[test]
fn rendezvous_lost_before_go() {
    let w = World::new();
    let mut s = w.server(1, server_settings());
    let mut slow = NatSpec::eim();
    slow.latency = Duration::from_secs(1);
    let (n1, n2) = two_nats(&w, NatSpec::eim(), slow);
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    std::thread::scope(|sc| {
        let (_cancel, h) = hanging_dial(sc, &a, b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Requesting))
                .is_some()
        );
        // B's answer is still on its way when the server goes.
        std::thread::sleep(Duration::from_millis(200));
        w.stop(&mut s);
        let e = error(h.join().unwrap());
        assert_eq!(e, P2pConnectError::RendezvousLost);
        assert!(e.is_retryable());
        assert_eq!(count_to(la.drain(&a.ep), AttemptPhase::Scheduled), 0);
    });
}

/// Drop A's packets to non-server addresses until `open`; after it, delay QUIC by `quic`.
fn gate(
    servers: Vec<IpAddr>,
    open: Arc<AtomicBool>,
    quic: Arc<std::sync::Mutex<Option<Instant>>>,
) -> Rule {
    Arc::new(move |dest, packet| {
        if servers.contains(&dest.ip()) {
            return Verdict::Pass;
        }
        if !open.load(Ordering::Relaxed) {
            return Verdict::Drop;
        }
        if is_quic(packet)
            && let Some(at) = *quic.lock().unwrap()
            && let Some(left) = at.checked_duration_since(Instant::now())
        {
            return Verdict::Delay(left);
        }
        Verdict::Pass
    })
}

#[test]
fn late_probe_success_still_gets_handshake_time() {
    let w = World::new();
    let mut ss = server_settings();
    ss.set(
        "punch_window",
        ServerValue::Duration(Duration::from_secs(2)),
    )
    .unwrap();
    ss.set(
        "quic_handshake_timeout",
        ServerValue::Duration(Duration::from_secs(3)),
    )
    .unwrap();
    let s = w.server(1, ss);
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let open = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(std::sync::Mutex::new(None));
    n1.set_rule(gate(server_ips(&s), open.clone(), hold.clone()));
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[("punch.attempts", Value::Count(1))]),
    );
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    std::thread::scope(|sc| {
        let (_cancel, h) = hanging_dial(sc, &a, b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Punching))
                .is_some()
        );
        let window_end = Instant::now() + Duration::from_secs(2);
        // Probes get through only in the last 300 ms of the window, and the handshake ends
        // after the window.
        std::thread::sleep(Duration::from_millis(1700));
        *hold.lock().unwrap() = Some(window_end + Duration::from_millis(500));
        open.store(true, Ordering::Relaxed);
        let ca = h.join().unwrap().expect("connected after the window");
        assert!(Instant::now() > window_end);
        let cb = b.ep.accept_timeout(Some(Duration::from_secs(5))).unwrap();
        echo(&ca, &cb);
    });
}

#[test]
fn session_ttl_covers_acceptor_deadline() {
    let w = World::new();
    let mut ss = server_settings();
    ss.set(
        "punch_window",
        ServerValue::Duration(Duration::from_secs(1)),
    )
    .unwrap();
    ss.set(
        "quic_handshake_timeout",
        ServerValue::Duration(Duration::from_secs(10)),
    )
    .unwrap();
    ss.set(
        "punch_session_grace",
        ServerValue::Duration(Duration::from_secs(1)),
    )
    .unwrap();
    let s = w.server(1, ss);
    let mut slow = NatSpec::eim();
    slow.latency = Duration::from_millis(300);
    let (n1, n2) = two_nats(&w, NatSpec::eim(), slow);
    let open = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(std::sync::Mutex::new(None));
    n1.set_rule(gate(server_ips(&s), open.clone(), hold.clone()));
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[("punch.attempts", Value::Count(1))]),
    );
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    std::thread::scope(|sc| {
        let (_cancel, h) = hanging_dial(sc, &a, b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&a.ep, LONG, attempt_to(AttemptPhase::Punching))
                .is_some()
        );
        let t0 = Instant::now();
        // The probes succeed early; A's QUIC is held until shortly before B's accept
        // deadline (window 1 s + handshake 10 s after B's start).
        *hold.lock().unwrap() = Some(t0 + Duration::from_secs(9));
        open.store(true, Ordering::Relaxed);
        let ca = h.join().unwrap().expect("connected");
        let cb = b.ep.accept_timeout(Some(Duration::from_secs(5))).unwrap();
        echo(&ca, &cb);
        assert!(t0.elapsed() >= Duration::from_secs(9));
        let mut lb = Log::default();
        lb.drain(&b.ep);
        assert!(
            !closed_by_cancel(&lb.0, CancelReason::Expired),
            "{:?}",
            lb.0
        );
        assert!(!closed_by_cancel(la.drain(&a.ep), CancelReason::Expired));
        // The results arrived before the session ended.
        std::thread::sleep(Duration::from_secs(2));
        lb.drain(&b.ep);
        assert!(!closed_by_cancel(&lb.0, CancelReason::Expired));
        assert_eq!(cb.info().unwrap().state, AttemptPhase::Connected);
    });
}

#[test]
fn acceptor_with_invalid_limits_declines_with_reason() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device(&n1, 2, 1, &s, settings(&[]));
    // The server's window (5 s) is beyond B's cap.
    let b = device(
        &n2,
        2,
        2,
        &s,
        settings(&[("punch.window_max", Value::Duration(Duration::from_secs(2)))]),
    );
    pair(&a, &b);
    registered(&a);
    registered(&b);
    assert_eq!(b.ep.status().limits_mismatch, Some("punch.window_max"));
    let e = error(a.ep.connect(b.id()));
    assert_eq!(e, P2pConnectError::PeerLimitsInvalid);
    assert!(!e.is_retryable());
    let mut lb = Log::default();
    assert!(closed_with(lb.drain(&b.ep), "limits_invalid"));
    assert_eq!(sent_to_ip(&a, n2.public_ip()), 0);
}

/// One run of the scripted server: A dials B.
struct Scripted {
    w: World,
    fake: Fake,
    a: Device,
    b: Device,
    n1: support::NatHandle,
    n2: support::NatHandle,
}

fn scripted(script: Script) -> Scripted {
    let w = World::new();
    let fake = Fake::start(
        &w.net,
        &w.rt,
        SocketAddr::from(([198, 51, 100, 9], 3478)),
        script,
    );
    let (n1, n2) = two_nats(&w, NatSpec::eim(), NatSpec::eim());
    let a = device_with(n1.socket(2), 1, fake.info, settings(&[]), true);
    let b = device_with(n2.socket(2), 2, fake.info, settings(&[]), true);
    pair(&a, &b);
    registered(&a);
    registered(&b);
    Scripted {
        w,
        fake,
        a,
        b,
        n1,
        n2,
    }
}

fn cancel_msg(punch_id: u64) -> ServerMessage {
    ServerMessage::PunchCancel(PunchCancel {
        punch_id,
        reason: CancelReason::PeerLimitsInvalid,
    })
}

#[test]
fn invalid_punch_go_is_reported_by_both() {
    // B gets a window beyond punch.window_max: B sends no probe; A, still Scheduled, gets
    // the cancel.
    let r = scripted(Script {
        start_after_ms: 1000,
        go_b: Box::new(|g| g.window_ms = 31_000),
        ..Script::default()
    });
    let e = error(r.a.ep.connect(r.b.id()));
    assert_eq!(e, P2pConnectError::PeerLimitsInvalid);
    let mut la = Log::default();
    la.drain(&r.a.ep);
    assert!(closed_by_cancel(&la.0, CancelReason::PeerLimitsInvalid));
    assert_eq!(phase_before_close(&la.0), Some(AttemptPhase::Scheduled));
    assert_eq!(count_to(&la.0, AttemptPhase::Requesting), 1);
    let mut lb = Log::default();
    assert!(
        lb.wait(&r.b.ep, LONG, attempt_to(AttemptPhase::Closed))
            .is_some()
    );
    assert!(closed_with(&lb.0, "server_limits_invalid"));
    assert_eq!(sent_to_ip(&r.b, r.n1.public_ip()), 0);
    assert!(r.fake.received().iter().any(|(d, m)| *d == r.b.id()
        && matches!(m, tasty_net_rendezvous::wire::message::DeviceMessage::PunchResult(p) if p.reason == Some(tasty_net_rendezvous::wire::message::ResultReason::LimitsInvalid))));

    // A gets more candidates than punch.max_candidates; B, still Scheduled, gets the cancel.
    let r = scripted(Script {
        start_after_ms: 1000,
        go_a: Box::new(|g| {
            let c = g.candidates[0];
            g.candidates.resize(9, c);
        }),
        ..Script::default()
    });
    let e = error(r.a.ep.connect(r.b.id()));
    assert_eq!(
        e,
        P2pConnectError::ServerLimitsInvalid {
            key: "punch.max_candidates"
        }
    );
    assert_eq!(sent_to_ip(&r.a, r.n2.public_ip()), 0);
    let mut lb = Log::default();
    assert!(
        lb.wait(&r.b.ep, LONG, attempt_to(AttemptPhase::Closed))
            .is_some()
    );
    assert!(closed_by_cancel(&lb.0, CancelReason::PeerLimitsInvalid));
    assert_eq!(phase_before_close(&lb.0), Some(AttemptPhase::Scheduled));

    // The cancel arrives while the other side is Punching.
    let r = scripted(Script {
        start_after_ms: 0,
        go_b: Box::new(|g| g.handshake_ms = 31_000),
        forward_limits_invalid: Some(Duration::from_millis(300)),
        ..Script::default()
    });
    assert_eq!(
        error(r.a.ep.connect(r.b.id())),
        P2pConnectError::PeerLimitsInvalid
    );
    let mut la = Log::default();
    la.drain(&r.a.ep);
    assert_eq!(phase_before_close(&la.0), Some(AttemptPhase::Punching));
    let r = scripted(Script {
        start_after_ms: 0,
        go_a: Box::new(|g| g.start_after_ms = 2000),
        forward_limits_invalid: Some(Duration::from_millis(300)),
        ..Script::default()
    });
    assert!(matches!(
        error(r.a.ep.connect(r.b.id())),
        P2pConnectError::ServerLimitsInvalid { .. }
    ));
    let mut lb = Log::default();
    assert!(
        lb.wait(&r.b.ep, LONG, attempt_to(AttemptPhase::Closed))
            .is_some()
    );
    assert!(closed_by_cancel(&lb.0, CancelReason::PeerLimitsInvalid));
    assert_eq!(phase_before_close(&lb.0), Some(AttemptPhase::Punching));

    // The cancel arrives while A is Handshaking (A's QUIC is held back).
    let r = scripted(Script::default());
    r.n1.set_rule(Arc::new(|dest: SocketAddr, p: &[u8]| {
        if is_quic(p) && dest.ip() != IpAddr::from([198, 51, 100, 9]) {
            Verdict::Delay(Duration::from_secs(2))
        } else {
            Verdict::Pass
        }
    }));
    std::thread::scope(|sc| {
        let (_c, h) = hanging_dial(sc, &r.a, r.b.id());
        let mut la = Log::default();
        assert!(
            la.wait(&r.a.ep, LONG, attempt_to(AttemptPhase::Handshaking))
                .is_some()
        );
        r.fake
            .send(r.a.id(), cancel_msg(r.fake.last_punch().unwrap()));
        assert_eq!(error(h.join().unwrap()), P2pConnectError::PeerLimitsInvalid);
        la.drain(&r.a.ep);
        assert_eq!(phase_before_close(&la.0), Some(AttemptPhase::Handshaking));
        assert_eq!(count_to(&la.0, AttemptPhase::RetryWait), 0);
    });

    // The cancel arrives while B is Handshaking (B's QUIC is held back).
    let r = scripted(Script::default());
    r.n2.set_rule(Arc::new(|dest: SocketAddr, p: &[u8]| {
        if is_quic(p) && dest.ip() != IpAddr::from([198, 51, 100, 9]) {
            Verdict::Delay(Duration::from_secs(3))
        } else {
            Verdict::Pass
        }
    }));
    std::thread::scope(|sc| {
        let (cancel, h) = hanging_dial(sc, &r.a, r.b.id());
        let mut lb = Log::default();
        assert!(
            lb.wait(&r.b.ep, LONG, attempt_to(AttemptPhase::Handshaking))
                .is_some()
        );
        r.fake
            .send(r.b.id(), cancel_msg(r.fake.last_punch().unwrap()));
        assert!(
            lb.wait(&r.b.ep, LONG, attempt_to(AttemptPhase::Closed))
                .is_some()
        );
        assert!(closed_by_cancel(&lb.0, CancelReason::PeerLimitsInvalid));
        assert_eq!(phase_before_close(&lb.0), Some(AttemptPhase::Handshaking));
        cancel.cancel();
        let _ended = h.join();
    });
    drop(r.w);
}
