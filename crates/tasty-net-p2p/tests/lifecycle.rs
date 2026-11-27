//! The endpoint's registration, reconnects and stops, keepalive and address changes, the state
//! machines, and the coordination frame limits (docs/design/testing.md "시험 목록", the
//! `tests/lifecycle.rs` rows).

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{
    AttemptPhase, CauseDetail, EndpointPhase, P2pCloseReason, P2pConnectError, P2pEvent, Phase,
    RendezvousServer, StopCause, Subject,
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

#[test]
fn superseded_endpoint_stops() {
    let w = World::new();
    let s = w.server(1, server_settings());
    let (n1, n2) = (w.net.nat(1, NatSpec::eim()), w.net.nat(2, NatSpec::eim()));
    let first = device(&n1, 2, 1, &s, settings(&[]));
    registered(&first);
    // The same key registers from elsewhere.
    let second = device(&n2, 2, 1, &s, settings(&[]));
    registered(&second);
    stopped_with(&first, StopCause::Superseded);
    assert_eq!(second.ep.status().state, EndpointPhase::Registered);
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
