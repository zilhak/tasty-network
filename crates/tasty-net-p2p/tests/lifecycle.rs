//! The endpoint's registration, reconnects and stops, keepalive and address changes, the state
//! machines, and the coordination frame limits (docs/design/testing.md "시험 목록", the
//! `tests/lifecycle.rs` rows).

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{
    AttemptPhase, CauseDetail, EndpointPhase, P2pCloseReason, P2pConnectError, P2pEndpoint,
    P2pEvent, Phase, RendezvousServer, StopCause, Subject,
};
use tasty_net_rendezvous::tls::{self, TlsOptions};
use tasty_net_rendezvous::wire::message::ServerMessage;

use support::fake::{Fake, Script, default_limits};
use support::{
    Device, Log, NatSpec, World, block_all, device, device_with, key, pair, raw_endpoint,
    registered, server_settings, settings, wait_until,
};

const LONG: Duration = Duration::from_secs(40);

/// Endpoint transitions as (to, cause name, at).
fn endpoint_path(log: &[P2pEvent]) -> Vec<(EndpointPhase, &'static str, SystemTime)> {
    log.iter()
        .filter_map(|e| match e {
            P2pEvent::Transition {
                subject: Subject::Endpoint,
                to: Phase::Endpoint(to),
                cause,
                at,
                ..
            } => Some((*to, cause.name, *at)),
            _ => None,
        })
        .collect()
}

/// The waits of the endpoint: from entering `Backoff` to the next `Connecting`.
fn backoff_waits(log: &[P2pEvent]) -> Vec<Duration> {
    let path = endpoint_path(log);
    path.windows(2)
        .filter(|w| w[0].0 == EndpointPhase::Backoff && w[1].0 == EndpointPhase::Connecting)
        .map(|w| w[1].2.duration_since(w[0].2).unwrap_or_default())
        .collect()
}

fn ms(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

fn stopped_with(d: &Device, cause: StopCause) {
    assert!(
        wait_until(LONG, || d.ep.status().state == EndpointPhase::Stopped),
        "{:?}",
        d.ep.status()
    );
    let st = d.ep.status();
    assert_eq!(st.stopped, Some(cause));
    // It stays stopped: no reconnect.
    std::thread::sleep(Duration::from_millis(1500));
    let mut log = Log::default();
    log.drain(&d.ep);
    let connecting = endpoint_path(&log.0)
        .iter()
        .filter(|p| p.0 == EndpointPhase::Connecting)
        .count();
    assert_eq!(connecting, 1, "{:?}", log.0);
    assert_eq!(d.ep.status().state, EndpointPhase::Stopped);
    let e =
        d.ep.connect(key(77).id())
            .expect_err("no connect while stopped");
    assert_eq!(
        e.connect_error(),
        Some(&P2pConnectError::RendezvousUnavailable {
            stopped: Some(cause)
        })
    );
}

#[test]
fn server_reconnect_follows_backoff() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    n.set_rule(block_all());
    let a = device(
        &n,
        2,
        1,
        &s,
        settings(&[
            ("rendezvous.connect_timeout", ms(200)),
            ("rendezvous.reconnect.initial", ms(300)),
            ("rendezvous.reconnect.multiplier", Value::Factor(2.0)),
            ("rendezvous.reconnect.jitter", Value::Factor(0.0)),
            (
                "rendezvous.reconnect.max_delay",
                Value::Duration(Duration::from_secs(10)),
            ),
        ]),
    );
    let mut log = Log::default();
    assert!(wait_until(LONG, || {
        log.drain(&a.ep);
        backoff_waits(&log.0).len() >= 3
    }));
    let waits = backoff_waits(&log.0);
    for (i, want) in [300u64, 600, 1200].into_iter().enumerate() {
        let got = waits[i];
        let want = Duration::from_millis(want);
        assert!(
            got >= want - Duration::from_millis(20) && got <= want + Duration::from_millis(400),
            "wait {i}: {got:?}, want {want:?} ({waits:?})"
        );
    }
    let path = endpoint_path(&log.0);
    assert_eq!(path[0].1, "enabled");
    assert!(
        path.iter()
            .any(|p| p.0 == EndpointPhase::Backoff && p.1 == "connect_timeout")
    );
    assert!(a.ep.status().reconnect_attempt >= 3);
    // Once the server can be reached, the endpoint registers.
    n.clear_rule();
    registered(&a);
    assert!(log.wait(&a.ep, LONG, |e| matches!(e, P2pEvent::Transition { to: Phase::Endpoint(EndpointPhase::Registered), cause, .. } if cause.name == "registered")).is_some());
}

/// A registration that ended does not count as held for the failures after it: once the server
/// is gone, connect timeouts longer than `reset_after` still add up to `ReconnectExhausted`, and
/// the status shows no registration time in `Backoff` (docs/design/p2p.md "재시도와 포기").
#[test]
fn failures_after_a_registration_add_up() {
    let w = World::new();
    let mut s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let a = device(
        &n,
        2,
        1,
        &s,
        settings(&[
            ("rendezvous.connect_timeout", ms(400)),
            ("rendezvous.reconnect.initial", ms(100)),
            ("rendezvous.reconnect.jitter", Value::Factor(0.0)),
            ("rendezvous.reconnect.reset_after", ms(200)),
            ("rendezvous.reconnect.max_attempts", Value::Count(3)),
        ]),
    );
    registered(&a);
    assert!(a.ep.status().registered_at.is_some());
    w.stop(&mut s);
    let mut log = Log::default();
    assert!(
        log.wait(&a.ep, LONG, |e| matches!(
            e,
            P2pEvent::Transition {
                to: Phase::Endpoint(EndpointPhase::Backoff),
                ..
            }
        ))
        .is_some()
    );
    assert_eq!(a.ep.status().registered_at, None);
    assert!(
        wait_until(LONG, || a.ep.status().state == EndpointPhase::Stopped),
        "{:?}",
        a.ep.status()
    );
    assert_eq!(a.ep.status().stopped, Some(StopCause::ReconnectExhausted));
}

#[test]
fn reconnect_delay_never_exceeds_max_delay() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    n.set_rule(block_all());
    let a = device(
        &n,
        2,
        1,
        &s,
        settings(&[
            ("rendezvous.connect_timeout", ms(100)),
            ("rendezvous.reconnect.initial", ms(200)),
            ("rendezvous.reconnect.multiplier", Value::Factor(4.0)),
            ("rendezvous.reconnect.jitter", Value::Factor(0.9)),
            ("rendezvous.reconnect.max_delay", ms(500)),
        ]),
    );
    let mut log = Log::default();
    assert!(wait_until(LONG, || {
        log.drain(&a.ep);
        backoff_waits(&log.0).len() >= 6
    }));
    for wait in backoff_waits(&log.0) {
        assert!(wait <= Duration::from_millis(500 + 150), "{wait:?}");
    }
    let st = a.ep.status();
    if let Some(next) = st.next_attempt_in {
        assert!(next <= Duration::from_millis(500));
    }
}

/// Register `key` on `server` from `port` as a plain coordination client: `hello`, then the
/// server's `welcome`. Another process with the same key does the same.
fn register_raw(
    w: &World,
    port: Arc<support::Port>,
    key: &tasty_net_identity::DeviceKey,
    server: RendezvousServer,
) -> (quinn::Connection, quinn::SendStream, quinn::RecvStream) {
    use tasty_net_rendezvous::wire::frame::{encode_frame, read_frame};
    use tasty_net_rendezvous::wire::message::{DeviceMessage, Hello};
    let ep = raw_endpoint(&w.rt, port, None);
    let tls = tls::client_config(
        key,
        server.public_key,
        tasty_net_rendezvous::wire::ALPN_RENDEZVOUS,
        TlsOptions {
            max_fragment_size: None,
        },
    )
    .unwrap();
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let config = quinn::ClientConfig::new(Arc::new(crypto));
    w.rt.block_on(async move {
        let conn = ep
            .connect_with(config, server.addr, tls::SERVER_NAME)
            .unwrap()
            .await
            .expect("coordination connection");
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = DeviceMessage::Hello(Hello {
            proto: tasty_net_rendezvous::wire::PROTO,
            host_candidates: Vec::new(),
        });
        let frame = encode_frame(&hello.to_json().unwrap(), 1 << 16).unwrap();
        send.write_all(&frame).await.unwrap();
        let welcome = read_frame(&mut recv, 1 << 16, Duration::from_secs(10))
            .await
            .unwrap()
            .expect("welcome");
        assert!(
            matches!(
                ServerMessage::parse(&welcome),
                Ok(ServerMessage::Welcome(_))
            ),
            "{}",
            String::from_utf8_lossy(&welcome)
        );
        (conn, send, recv)
    })
}

/// The same key registers from elsewhere (as another process would): the server supersedes
/// the endpoint, which stops. One process cannot open a second endpoint for the key.
#[test]
fn superseded_endpoint_stops() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let first = device(&n1, 2, 1, &s, settings(&[]));
    registered(&first);
    let elsewhere = n2.socket(2);
    let same_key = support::device_key(&first.port, 1);
    assert_eq!(same_key.id(), first.id());
    let _second = register_raw(&w, elsewhere, &same_key, s.info);
    stopped_with(&first, StopCause::Superseded);
}

#[test]
fn alpn_mismatch_stops_without_retry() {
    let w = World::new();
    let addr = SocketAddr::from(([198, 51, 100, 20], 3478));
    let server_key = key(240);
    let tls = tls::server_config(
        &server_key,
        b"other/1",
        TlsOptions {
            max_fragment_size: None,
        },
    )
    .unwrap();
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let ep = raw_endpoint(
        &w.rt,
        w.net.public(addr),
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
    let n = w.net.nat(1, NatSpec::eim());
    let a = device_with(
        n.socket(2),
        1,
        RendezvousServer {
            addr,
            public_key: server_key.id(),
        },
        settings(&[]),
        true,
    );
    stopped_with(&a, StopCause::AlpnMismatch);
}

#[test]
fn server_key_mismatch_stops_without_retry() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let a = device_with(
        n.socket(2),
        1,
        RendezvousServer {
            addr: s.info.addr,
            public_key: key(241).id(),
        },
        settings(&[]),
        true,
    );
    stopped_with(&a, StopCause::ServerKeyMismatch);
}

#[test]
fn server_limits_mismatch_is_reported() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[(
            "quic.handshake_timeout_max",
            Value::Duration(Duration::from_secs(2)),
        )]),
    );
    // The mismatch is in place as soon as the registration shows, so no attempt can pass the
    // check between the two.
    assert!(wait_until(LONG, || {
        let st = a.ep.status();
        let shown = st.state == EndpointPhase::Registered;
        if shown {
            assert_eq!(st.limits_mismatch, Some("quic.handshake_timeout_max"));
        }
        shown
    }));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let mut log = Log::default();
    let e = log
        .wait(&a.ep, LONG, |e| matches!(e, P2pEvent::Transition { cause, .. } if cause.name == "server_limits_mismatch"))
        .expect("event");
    let P2pEvent::Transition {
        from, to, cause, ..
    } = e
    else {
        unreachable!()
    };
    assert_eq!(from, Some(Phase::Endpoint(EndpointPhase::Registered)));
    assert_eq!(to, Phase::Endpoint(EndpointPhase::Registered));
    match cause.detail {
        Some(CauseDetail::Limits { key, .. }) => assert_eq!(key, "quic.handshake_timeout_max"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        a.ep.status().limits_mismatch,
        Some("quic.handshake_timeout_max")
    );
    let e = a.ep.connect(b.id()).err().unwrap();
    assert_eq!(
        e.connect_error(),
        Some(&P2pConnectError::ServerLimitsInvalid {
            key: "quic.handshake_timeout_max"
        })
    );
    // B was never asked.
    let mut lb = Log::default();
    lb.drain(&b.ep);
    assert!(lb.attempt_path().is_empty());
}

/// Keepalive and idle settings short enough for the tests.
fn short_timers() -> Vec<(&'static str, Value)> {
    vec![
        ("quic.keepalive_interval", ms(300)),
        ("quic.idle_timeout", Value::Duration(Duration::from_secs(3))),
        ("rendezvous.keepalive_interval", ms(300)),
        (
            "rendezvous.idle_timeout",
            Value::Duration(Duration::from_secs(3)),
        ),
        ("path.poll_interval", ms(100)),
    ]
}

fn echo(a: &tasty_net_p2p::P2pConnection, b: &tasty_net_p2p::P2pConnection) {
    use std::io::{Read, Write};
    let mut s = a.open_bi().unwrap();
    s.write_all(b"x").unwrap();
    s.finish().unwrap();
    let mut r = b.accept_bi(Some(Duration::from_secs(10))).unwrap();
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"x");
}

#[test]
fn keepalive_keeps_mapping_alive() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let mut spec = NatSpec::eim();
    spec.expiry = Duration::from_millis(1000);
    let (n1, n2) = (w.net.nat(1, spec), w.net.nat(2, spec));
    let a = device(&n1, 2, 1, &s, settings(&short_timers()));
    let b = device(&n2, 2, 2, &s, settings(&short_timers()));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let ca = a.ep.connect(b.id()).unwrap();
    let cb = b.ep.accept_timeout(Some(Duration::from_secs(10))).unwrap();
    // Four mapping lifetimes with nothing but keepalives.
    std::thread::sleep(Duration::from_secs(4));
    echo(&ca, &cb);
    assert_eq!(ca.info().unwrap().state, AttemptPhase::Connected);
    assert_eq!(a.ep.status().state, EndpointPhase::Registered);
    assert_eq!(b.ep.status().state, EndpointPhase::Registered);
    // The same address as at the start: the mapping never expired.
    assert_eq!(
        cb.remote_address(),
        ca.info().unwrap().local_reflexive.unwrap()
    );
}

#[test]
fn client_remap_is_observed_on_acceptor() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (
        w.net.nat(1, NatSpec::eim()),
        w.net.nat(2, NatSpec::full_cone()),
    );
    let a = device(&n1, 2, 1, &s, settings(&short_timers()));
    let b = device(&n2, 2, 2, &s, settings(&short_timers()));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let ca = a.ep.connect(b.id()).unwrap();
    let cb = b.ep.accept_timeout(Some(Duration::from_secs(10))).unwrap();
    let before = cb.remote_address();
    n1.remap();
    echo(&ca, &cb);
    assert!(wait_until(LONG, || cb.remote_address() != before));
    let mut lb = Log::default();
    let e = lb
        .wait(&b.ep, LONG, |e| matches!(e, P2pEvent::Transition { cause, .. } if cause.name == "remote_address_observed_change"))
        .expect("event");
    let P2pEvent::Transition {
        from, to, cause, ..
    } = e
    else {
        unreachable!()
    };
    assert_eq!(from, Some(Phase::Attempt(AttemptPhase::Connected)));
    assert_eq!(to, Phase::Attempt(AttemptPhase::Connected));
    match cause.detail {
        Some(CauseDetail::Address { from, to }) => {
            assert_eq!(from, before);
            assert_eq!(to, cb.remote_address());
        }
        other => panic!("{other:?}"),
    }
    let info = cb.info().unwrap();
    assert!(
        info.history
            .iter()
            .any(|h| h.cause.name == "remote_address_observed_change")
    );
    // A, the client, records nothing of the kind.
    let mut la = Log::default();
    la.drain(&a.ep);
    assert!(!la
        .0
        .iter()
        .any(|e| matches!(e, P2pEvent::Transition { cause, .. } if cause.name == "remote_address_observed_change")));
}

#[test]
fn acceptor_remap_ends_by_idle_timeout() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let a = device(&n1, 2, 1, &s, settings(&short_timers()));
    let b = device(&n2, 2, 2, &s, settings(&short_timers()));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let ca = a.ep.connect(b.id()).unwrap();
    let cb = b.ep.accept_timeout(Some(Duration::from_secs(10))).unwrap();
    echo(&ca, &cb);
    // The last packet either side gets is before the remap; the idle timeout is 3 s and the
    // timer restarts at most once more (a sent ack-eliciting packet), plus a margin.
    let remapped = Instant::now();
    n2.remap();
    let limit = Duration::from_secs(3 + 3 + 2);
    for (d, c) in [(&a, &ca), (&b, &cb)] {
        let mut log = Log::default();
        let e = log
            .wait(&d.ep, limit, |e| {
                matches!(
                    e,
                    P2pEvent::Transition {
                        to: Phase::Attempt(AttemptPhase::Closed),
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("closed within {limit:?}: {:?}", log.0));
        let P2pEvent::Transition { cause, .. } = e else {
            unreachable!()
        };
        assert_eq!(cause.name, "idle_timeout");
        assert!(c.wait_closed(Duration::from_secs(1)).is_ok());
    }
    assert!(remapped.elapsed() >= Duration::from_secs(2));
}

#[test]
fn reflexive_change_triggers_rejudge() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let a = device(&n, 2, 1, &s, settings(&short_timers()));
    registered(&a);
    let before = a.ep.status().reflexive.unwrap();
    let remapped = SystemTime::now();
    n.remap();
    // The server sees the coordination connection come from the new mapping and says so.
    assert!(wait_until(LONG, || a.ep.status().reflexive != Some(before)));
    let mut log = Log::default();
    let e = log
        .wait(&a.ep, LONG, |e| matches!(e, P2pEvent::Transition { cause, at, .. } if cause.name == "nat_changed" && *at >= remapped))
        .expect("nat_changed");
    let P2pEvent::Transition { from, to, .. } = e else {
        unreachable!()
    };
    assert_eq!(from, Some(Phase::Endpoint(EndpointPhase::Registered)));
    assert_eq!(to, Phase::Endpoint(EndpointPhase::Registered));
    let st = a.ep.status();
    assert_ne!(st.reflexive, Some(before));
    assert_eq!(st.nat_answers[0].reflexive, st.reflexive);
}

/// Every transition of one subject starts where the previous one ended.
fn chained(log: &[P2pEvent], subject: Subject) -> Vec<(Option<Phase>, Phase, &'static str)> {
    let path: Vec<_> = log
        .iter()
        .filter_map(|e| match e {
            P2pEvent::Transition {
                subject: s,
                from,
                to,
                cause,
                ..
            } if *s == subject => Some((*from, *to, cause.name)),
            _ => None,
        })
        .collect();
    for w in path.windows(2) {
        assert_eq!(w[1].0, Some(w[0].1), "{path:?}");
    }
    path
}

#[test]
fn state_events_follow_the_tables() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);
    let ca = a.ep.connect(b.id()).unwrap();
    let cb = b.ep.accept_timeout(Some(Duration::from_secs(10))).unwrap();
    let id_a = ca.id();
    let id_b = cb.id();
    ca.close(P2pCloseReason::Closed);
    assert!(cb.wait_closed(Duration::from_secs(10)).is_ok());
    let (mut la, mut lb) = (Log::default(), Log::default());
    for (d, l, id) in [(&a, &mut la, id_a), (&b, &mut lb, id_b)] {
        assert!(l
            .wait(&d.ep, LONG, |e| matches!(e, P2pEvent::Transition { subject: Subject::Connection(i), to: Phase::Attempt(AttemptPhase::Closed), .. } if *i == id))
            .is_some());
    }
    let ep = chained(&la.0, Subject::Endpoint);
    assert_eq!(
        ep[..2],
        [
            (
                Some(Phase::Endpoint(EndpointPhase::Off)),
                Phase::Endpoint(EndpointPhase::Connecting),
                "enabled"
            ),
            (
                Some(Phase::Endpoint(EndpointPhase::Connecting)),
                Phase::Endpoint(EndpointPhase::Registered),
                "registered"
            ),
        ]
    );
    let names = |p: Vec<(Option<Phase>, Phase, &'static str)>| -> Vec<(Phase, &'static str)> {
        p.into_iter().map(|(_, to, c)| (to, c)).collect()
    };
    use AttemptPhase::*;
    let at = |p: AttemptPhase, c: &'static str| (Phase::Attempt(p), c);
    let path_a = chained(&la.0, Subject::Connection(id_a));
    assert_eq!(path_a[0].0, None);
    assert_eq!(
        names(path_a),
        vec![
            at(Requesting, "connect_called"),
            at(Scheduled, "punch_go"),
            at(Punching, "start_elapsed"),
            at(Handshaking, "probe_succeeded"),
            at(Connected, "quic_established"),
            at(Closed, "closed_by_local"),
        ]
    );
    let path_b = chained(&lb.0, Subject::Connection(id_b));
    assert_eq!(path_b[0].0, None);
    assert_eq!(
        names(path_b),
        vec![
            at(Offered, "punch_offer"),
            at(Scheduled, "punch_go"),
            at(Punching, "start_elapsed"),
            at(Handshaking, "quic_incoming"),
            at(Connected, "quic_established"),
            at(Closed, "closed_by_peer"),
        ]
    );
    // The closed connection's code goes with the cause.
    let closed_b = lb.0.iter().rev().find_map(|e| match e {
        P2pEvent::Transition {
            to: Phase::Attempt(Closed),
            cause,
            ..
        } => Some(cause.clone()),
        _ => None,
    });
    assert_eq!(
        closed_b.and_then(|c| c.detail),
        Some(CauseDetail::Close(P2pCloseReason::Closed))
    );
    // A closed attempt leaves the list.
    assert!(a.ep.connections().iter().all(|i| i.id != id_a));
    // Turning the endpoint off ends with `disabled`.
    a.ep.close();
    la.drain(&a.ep);
    let last = chained(&la.0, Subject::Endpoint);
    assert_eq!(
        last.last().map(|p| (p.1, p.2)),
        Some((Phase::Endpoint(EndpointPhase::Off), "disabled"))
    );
}

/// A device on a scripted server; returns the world, the server and the device.
fn on_fake(script: Script, s: &[(&str, Value)]) -> (World, Fake, Device) {
    let w = World::new();
    let fake = Fake::start(
        &w.net,
        &w.rt,
        SocketAddr::from(([198, 51, 100, 9], 3478)),
        script,
    );
    let n = w.net.nat(1, NatSpec::eim());
    let d = device_with(n.socket(2), 1, fake.info, settings(s), true);
    (w, fake, d)
}

fn header(len: u32) -> Vec<u8> {
    len.to_be_bytes().to_vec()
}

/// The first `Backoff` with its cause, waiting at most `limit`.
fn first_backoff(d: &Device, limit: Duration) -> (tasty_net_p2p::Cause, Instant) {
    let mut log = Log::default();
    let e = log
        .wait(&d.ep, limit, |e| {
            matches!(
                e,
                P2pEvent::Transition {
                    to: Phase::Endpoint(EndpointPhase::Backoff),
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("backoff: {:?}", log.0));
    let P2pEvent::Transition { cause, .. } = e else {
        unreachable!()
    };
    (cause, Instant::now())
}

#[test]
fn first_frame_over_device_limit_is_rejected() {
    // The first frame declares one byte more than the device takes; no body follows, so the
    // device can only refuse it from the header.
    let (_w, _fake, d) = on_fake(
        Script {
            first: Some(header(4097)),
            ..Script::default()
        },
        &[("rendezvous.max_message_bytes", Value::Bytes(4096))],
    );
    let (cause, _) = first_backoff(&d, LONG);
    assert_eq!(cause.name, "io");
    match cause.detail {
        Some(CauseDetail::Io { kind, what }) => {
            assert_eq!(kind, std::io::ErrorKind::InvalidData);
            assert!(what.contains("4097") && what.contains("4096"), "{what}");
        }
        other => panic!("{other:?}"),
    }
    assert_ne!(d.ep.status().state, EndpointPhase::Registered);
}

#[test]
fn first_frame_timeout_is_device_setting() {
    // Half a header, then nothing: the device's frame deadline (not its hello timeout) ends it.
    let (_w, _fake, d) = on_fake(
        Script {
            first: Some(vec![0, 0]),
            ..Script::default()
        },
        &[
            ("rendezvous.frame_timeout", ms(400)),
            (
                "rendezvous.hello_timeout",
                Value::Duration(Duration::from_secs(20)),
            ),
        ],
    );
    let mut log = Log::default();
    assert!(
        log.wait(&d.ep, LONG, |e| matches!(
            e,
            P2pEvent::Transition {
                to: Phase::Endpoint(EndpointPhase::Connecting),
                ..
            }
        ))
        .is_some()
    );
    let start = Instant::now();
    let (cause, at) = first_backoff(&d, LONG);
    let took = at - start;
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(cause.name, "io");
    assert!(matches!(
        cause.detail,
        Some(CauseDetail::Io {
            kind: std::io::ErrorKind::InvalidData,
            ..
        })
    ));
}

/// A `reflexive_changed` frame padded with JSON whitespace to `len` bytes.
fn padded_reflexive_changed(addr: SocketAddr, len: usize) -> Vec<u8> {
    let mut body = format!(r#"{{"type":"reflexive_changed","reflexive":"{addr}"}}"#).into_bytes();
    body.resize(len, b' ');
    assert!(ServerMessage::parse(&body).is_ok());
    let mut out = header(u32::try_from(len).unwrap());
    out.extend(body);
    out
}

#[test]
fn frame_limits_after_welcome() {
    // The server takes 400 bytes; the device takes its default 64 KiB.
    let new_addr = SocketAddr::from(([203, 0, 113, 77], 41_000));
    let mut limits = default_limits();
    limits.max_message_bytes = 400;
    let (_w, fake, d) = on_fake(
        Script {
            limits,
            after_welcome: vec![padded_reflexive_changed(new_addr, 1000)],
            ..Script::default()
        },
        &[],
    );
    registered(&d);
    // A frame above the server's limit but within the device's is read.
    assert!(wait_until(LONG, || d.ep.status().reflexive == Some(new_addr)));
    // The device's frames after the welcome stay within the server's limit.
    let peer = key(78).id();
    let _r = d.ep.connect(peer);
    let sizes = fake.sizes();
    assert!(sizes.len() >= 2, "{sizes:?}");
    assert!(sizes[1..].iter().all(|n| *n <= 400), "{sizes:?}");
    assert_eq!(d.ep.status().limits_mismatch, None);
    // A frame above the device's limit ends the coordination connection.
    fake.send_raw(d.id(), header(64 * 1024 + 1));
    let (cause, _) = first_backoff(&d, LONG);
    assert_eq!(cause.name, "io");

    // A server limit above the device's is a mismatch.
    let mut limits = default_limits();
    limits.max_message_bytes = 128 * 1024;
    let (_w, _fake, d) = on_fake(
        Script {
            limits,
            ..Script::default()
        },
        &[],
    );
    registered(&d);
    assert_eq!(
        d.ep.status().limits_mismatch,
        Some("rendezvous.max_message_bytes")
    );
}

/// Panics seen by the panic hook on the threads `f` marks as watched.
fn panics_while(f: impl FnOnce()) -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    let before = SEEN.load(Ordering::SeqCst);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if WATCHED.with(std::cell::Cell::get) {
            SEEN.fetch_add(1, Ordering::SeqCst);
        }
        prev(info);
    }));
    f();
    SEEN.load(Ordering::SeqCst) - before
}

thread_local! {
    static WATCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The blocking calls refuse an async task with `InAsyncContext` without a panic (the panic
/// hook is not called), and work in `spawn_blocking` (docs/design/p2p.md "endpoint와 런타임").
#[test]
fn blocking_calls_in_a_task_are_refused_without_a_panic() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let d = device(&n, 2, 1, &s, settings(&[]));
    registered(&d);
    let ep = d.ep.clone();
    let mut refused = None;
    let panics = panics_while(|| {
        refused = Some(w.rt.block_on(async move {
            tokio::spawn(async move {
                WATCHED.with(|x| x.set(true));
                let got = (
                    ep.accept_timeout(Some(Duration::from_millis(10))).err(),
                    ep.next_event(Duration::from_millis(10)).err(),
                    ep.connect(key(9).id()).err(),
                );
                WATCHED.with(|x| x.set(false));
                got
            })
            .await
            .expect("task")
        }));
    });
    assert_eq!(panics, 0);
    let (accept, next, connect) = refused.unwrap();
    assert!(
        matches!(accept, Some(tasty_net_p2p::P2pError::InAsyncContext)),
        "{accept:?}"
    );
    assert!(
        matches!(next, Some(tasty_net_p2p::P2pError::InAsyncContext)),
        "{next:?}"
    );
    assert!(
        matches!(connect, Some(tasty_net_p2p::P2pError::InAsyncContext)),
        "{connect:?}"
    );
    let ep = d.ep.clone();
    let blocking = w.rt.block_on(async move {
        tokio::task::spawn_blocking(move || ep.next_event(Duration::from_millis(10)))
            .await
            .expect("blocking")
    });
    assert!(blocking.is_ok(), "{blocking:?}");
    d.ep.close();
}

/// A task in `tokio::task::coop::unconstrained` has no budget, and tokio cannot tell it from
/// `spawn_blocking` without a panic, so its blocking calls are not refused: they block that
/// worker until they end, and do not panic (src/runtime.rs `in_async_task`, docs/design/
/// transport.md "동기·비동기 경계"). The task runs on the test's runtime, not on the endpoint's
/// own, so the endpoint's workers stay free and the calls cannot deadlock.
#[test]
fn blocking_calls_in_an_unconstrained_task_end_without_a_panic() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let d = device(&n, 2, 1, &s, settings(&[]));
    registered(&d);
    let ep = d.ep.clone();
    let mut ended = None;
    let panics = panics_while(|| {
        ended = Some(w.rt.block_on(async move {
            tokio::spawn(tokio::task::coop::unconstrained(async move {
                WATCHED.with(|x| x.set(true));
                let got = (
                    ep.accept_timeout(Some(Duration::from_millis(10))).err(),
                    ep.next_event(Duration::from_millis(10)),
                );
                WATCHED.with(|x| x.set(false));
                got
            }))
            .await
            .expect("task")
        }));
    });
    assert_eq!(panics, 0);
    let (accept, next) = ended.unwrap();
    assert!(
        matches!(accept, Some(tasty_net_p2p::P2pError::TimedOut)),
        "{accept:?}"
    );
    assert!(next.is_ok(), "{next:?}");
    d.ep.close();
}

/// `close` wakes every caller that waits in `next_event`: the queued events, `Off` among them,
/// still come first and once each, then every waiter and every later call gets `Closed` at
/// once instead of waiting out its limit (docs/design/p2p.md "상태 기계").
#[test]
fn close_wakes_every_event_waiter_and_later_calls_are_closed() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let d = device(&n, 2, 1, &s, settings(&[]));
    registered(&d);
    let _before = d.ep.events();
    let read_all = |ep: &P2pEndpoint| {
        let mut got = Vec::new();
        loop {
            match ep.next_event(LONG) {
                Ok(Some(e)) => got.push(e),
                other => return (got, other.err()),
            }
        }
    };
    let started = Instant::now();
    let (offs, ends) = std::thread::scope(|sc| {
        let waiters: Vec<_> = (0..3).map(|_| sc.spawn(|| read_all(&d.ep))).collect();
        std::thread::sleep(Duration::from_millis(200));
        d.ep.close();
        let mut offs = 0;
        let mut ends = Vec::new();
        for w in waiters {
            let (got, end) = w.join().unwrap();
            offs += endpoint_path(&got)
                .iter()
                .filter(|(to, ..)| *to == EndpointPhase::Off)
                .count();
            ends.push(end);
        }
        (offs, ends)
    });
    assert_eq!(offs, 1);
    for end in ends {
        assert!(
            matches!(end, Some(tasty_net_p2p::P2pError::Closed)),
            "{end:?}"
        );
    }
    let later = d.ep.next_event(LONG);
    assert!(
        matches!(later, Err(tasty_net_p2p::P2pError::Closed)),
        "{later:?}"
    );
    assert!(started.elapsed() < LONG, "{:?}", started.elapsed());
}

/// A device whose socket the test also holds, and the socket's reference count before the
/// endpoint took it: the count comes back when the endpoint gives the socket up.
fn counted_device(
    nat: &support::NatHandle,
    host: u8,
    seed: u8,
    server: &support::TestServer,
) -> (Device, Arc<support::Port>, usize) {
    let port = nat.socket(host);
    let before = Arc::strong_count(&port);
    let d = device_with(port.clone(), seed, server.info, settings(&[]), true);
    (d, port, before)
}

/// A connects to B; both sides of the connection.
fn connected(
    a: &Device,
    b: &Device,
) -> (tasty_net_p2p::P2pConnection, tasty_net_p2p::P2pConnection) {
    pair(a, b);
    registered(a);
    registered(b);
    std::thread::scope(|s| {
        let accepted = s.spawn(|| b.ep.accept_timeout(Some(LONG)));
        let ca = a.ep.connect(b.id()).expect("connect");
        (ca, accepted.join().unwrap().expect("accept"))
    })
}

/// `close` wakes a blocked `accept` and `accept_timeout` with `Closed`; the limit of
/// `accept_timeout` counts the wait behind another caller's `accept`; after the close, every
/// blocking call returns `Closed` without entering the runtime (no panic of a timer on a
/// runtime that is gone) and the socket is given up (`Off`: no socket).
#[test]
fn close_wakes_blocked_accepts_and_later_calls_are_closed() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let (d, port, before) = counted_device(&n, 2, 1, &s);
    registered(&d);
    std::thread::scope(|sc| {
        let forever = sc.spawn(|| d.ep.accept().err());
        let limited = sc.spawn(|| d.ep.accept_timeout(Some(LONG)).err());
        std::thread::sleep(Duration::from_millis(200));
        // Behind the blocked accepts the limit still holds.
        let started = Instant::now();
        let r = d.ep.accept_timeout(Some(Duration::from_millis(200)));
        assert!(matches!(r, Err(tasty_net_p2p::P2pError::TimedOut)), "{r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let closing = Instant::now();
        d.ep.close();
        let (forever, limited) = (forever.join().unwrap(), limited.join().unwrap());
        assert!(
            matches!(forever, Some(tasty_net_p2p::P2pError::Closed)),
            "{forever:?}"
        );
        assert!(
            matches!(limited, Some(tasty_net_p2p::P2pError::Closed)),
            "{limited:?}"
        );
        assert!(
            closing.elapsed() < Duration::from_secs(10),
            "{:?}",
            closing.elapsed()
        );
    });
    let started = Instant::now();
    let r = d.ep.accept_timeout(Some(LONG));
    assert!(matches!(r, Err(tasty_net_p2p::P2pError::Closed)), "{r:?}");
    assert!(matches!(
        d.ep.accept(),
        Err(tasty_net_p2p::P2pError::Closed)
    ));
    let r = d.ep.connect(key(9).id());
    assert!(matches!(r, Err(tasty_net_p2p::P2pError::Closed)), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(d.ep.status().state, EndpointPhase::Off);
    assert_eq!(d.ep.local_addr(), None);
    // The endpoint handle lives on, the socket does not.
    assert!(
        wait_until(Duration::from_secs(5), || Arc::strong_count(&port)
            == before + 1),
        "socket references: {} (before the endpoint {before}, one held by the device)",
        Arc::strong_count(&port)
    );
}

/// A connection keeps the endpoint running after every endpoint handle is dropped; the
/// endpoint ends (and gives its socket up) when the connection is dropped too.
#[test]
fn a_connection_outlives_the_endpoint_handles() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let (a, port_a, before_a) = counted_device(&n1, 2, 1, &s);
    let b = device(&n2, 2, 2, &s, settings(&[]));
    let (ca, cb) = connected(&a, &b);
    let Device { ep, port, .. } = a;
    drop((ep, port));
    std::thread::sleep(Duration::from_millis(300));
    assert!(ca.close_reason().is_none(), "{:?}", ca.close_reason());
    echo(&ca, &cb);
    assert_eq!(ca.info().map(|i| i.state), Some(AttemptPhase::Connected));
    // The socket is still the endpoint's: the device's handle is dropped, the gate holds one.
    // The mock network may hold one more for a moment while it carries a datagram, so the
    // count is at least one above the count before the endpoint.
    assert!(
        Arc::strong_count(&port_a) > before_a,
        "socket references: {} (before the endpoint {before_a})",
        Arc::strong_count(&port_a)
    );
    drop(ca);
    assert!(
        wait_until(Duration::from_secs(10), || Arc::strong_count(&port_a)
            == before_a),
        "socket references: {} (before the endpoint {before_a})",
        Arc::strong_count(&port_a)
    );
    assert!(
        cb.wait_closed(LONG).expect("wait").is_some(),
        "B sees the end"
    );
    b.ep.close();
}

/// After `close`, a connection handle still held answers without a panic and its calls end at
/// once; the socket is given up although the endpoint and the connection handles live.
#[test]
fn connection_calls_after_close_end_at_once() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let (a, port_a, before_a) = counted_device(&n1, 2, 1, &s);
    let b = device(&n2, 2, 2, &s, settings(&[]));
    let (ca, _cb) = connected(&a, &b);
    let mut stream = ca.open_bi().expect("stream");
    a.ep.close();
    let started = Instant::now();
    assert!(ca.open_bi().is_err());
    assert!(ca.accept_bi(Some(LONG)).is_err());
    assert!(ca.wait_closed(LONG).expect("closed").is_some());
    let mut buf = [0u8; 4];
    stream.set_read_timeout(Some(LONG));
    assert!(std::io::Read::read(&mut stream, &mut buf).is_err());
    stream.set_write_timeout(Some(LONG));
    assert!(std::io::Write::write(&mut stream, b"late").is_err());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        wait_until(Duration::from_secs(5), || Arc::strong_count(&port_a)
            == before_a + 1),
        "socket references: {} (before the endpoint {before_a}, one held by the device)",
        Arc::strong_count(&port_a)
    );
    b.ep.close();
}

/// One device key has one endpoint that is on in a process, with or without a key directory;
/// the key is free again at `Off` (docs/design/p2p.md "endpoint와 런타임").
#[test]
fn one_key_has_one_endpoint_in_a_process() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let first = device(&n1, 2, 1, &s, settings(&[]));
    let pairing = Arc::new(tasty_net_p2p::PairingList::new());
    let config = |key: tasty_net_identity::DeviceKey| {
        let mut c = tasty_net_p2p::P2pConfig::new(key, pairing.clone());
        c.enabled = true;
        c.server = Some(s.info);
        c.packet_io = Some(Arc::new(support::MockIo(n2.socket(3))));
        c
    };
    let same = support::device_key(&first.port, 1);
    let r = tasty_net_p2p::P2pEndpoint::bind(config(same.clone()));
    assert!(
        matches!(r, Err(tasty_net_p2p::P2pError::EndpointInUse)),
        "{r:?}"
    );
    // An endpoint that is off holds nothing.
    let mut off = config(same.clone());
    off.enabled = false;
    assert!(tasty_net_p2p::P2pEndpoint::bind(off).is_ok());
    first.ep.close();
    let again = tasty_net_p2p::P2pEndpoint::bind(config(same)).expect("free after Off");
    again.close();
}

/// A second `bind` on the same key directory is `EndpointInUse`: through the process list for
/// the same key, and through the directory's lock file for another key (as another process
/// with that directory would meet it).
#[test]
fn a_second_bind_on_the_same_key_directory_is_in_use() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let n = w.net.nat(1, NatSpec::eim());
    let dir = std::env::temp_dir().join(format!(
        "tasty-net-p2p-key-dir-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pairing = Arc::new(tasty_net_p2p::PairingList::new());
    let config = |key: tasty_net_identity::DeviceKey, host: u8| {
        let port = n.socket(host);
        let mut c = tasty_net_p2p::P2pConfig::new(key, pairing.clone());
        c.enabled = true;
        c.server = Some(s.info);
        c.key_dir = Some(dir.clone());
        c.packet_io = Some(Arc::new(support::MockIo(port)));
        c
    };
    let port = n.socket(2);
    let (k1, k2) = (support::device_key(&port, 1), support::device_key(&port, 2));
    let first = tasty_net_p2p::P2pEndpoint::bind(config(k1.clone(), 3)).expect("first");
    let same_key = tasty_net_p2p::P2pEndpoint::bind(config(k1.clone(), 4));
    assert!(
        matches!(same_key, Err(tasty_net_p2p::P2pError::EndpointInUse)),
        "{same_key:?}"
    );
    let same_dir = tasty_net_p2p::P2pEndpoint::bind(config(k2.clone(), 5));
    assert!(
        matches!(same_dir, Err(tasty_net_p2p::P2pError::EndpointInUse)),
        "{same_dir:?}"
    );
    first.close();
    let after = tasty_net_p2p::P2pEndpoint::bind(config(k2, 6)).expect("the lock is free at Off");
    after.close();
    drop(after);
    let _removed = std::fs::remove_file(dir.join("p2p.lock"));
    let _removed = std::fs::remove_dir(&dir);
}

/// The first close of a connection is the one sent and kept: a second close with another
/// reason changes neither side's reason.
#[test]
fn a_second_close_keeps_the_first_reason() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let a = device(&n1, 2, 1, &s, settings(&[]));
    let b = device(&n2, 2, 2, &s, settings(&[]));
    let (ca, cb) = connected(&a, &b);
    ca.close(P2pCloseReason::SessionMismatch);
    ca.close(P2pCloseReason::Closed);
    assert_eq!(ca.close_reason(), Some(P2pCloseReason::SessionMismatch));
    assert!(cb.wait_closed(LONG).expect("wait").is_some());
    assert_eq!(cb.close_reason(), Some(P2pCloseReason::SessionMismatch));
    a.ep.close();
    b.ep.close();
}

/// A frame the stream ends in the middle of is malformed at once, before and after the
/// `welcome`, also when the server closes the connection right after (docs/design/rendezvous.md
/// "프레임"): not `server_closed`, and not after the frame deadline.
#[test]
fn a_frame_cut_by_the_end_of_the_stream_is_malformed() {
    let mut cut = header(100);
    cut.extend_from_slice(b"{\"type\":");
    for script in [
        Script {
            first: Some(cut.clone()),
            finish_after: true,
            ..Script::default()
        },
        Script {
            after_welcome: vec![cut.clone()],
            finish_after: true,
            ..Script::default()
        },
    ] {
        let before = script.first.is_some();
        let (_w, _fake, d) = on_fake(script, &[]);
        let start = Instant::now();
        let (cause, at) = first_backoff(&d, LONG);
        assert!(at - start < Duration::from_secs(10), "{:?}", at - start);
        assert_eq!(cause.name, "io", "before welcome {before}: {cause:?}");
        assert!(
            matches!(
                cause.detail,
                Some(CauseDetail::Io {
                    kind: std::io::ErrorKind::InvalidData,
                    ..
                })
            ),
            "before welcome {before}: {cause:?}"
        );
        d.ep.close();
    }
}
