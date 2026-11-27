//! The values a connection reports it uses (docs/design/testing.md "설정값 시험",
//! `connection_reports_applied_settings`).

mod support;

use std::net::IpAddr;
use std::time::Duration;

use tasty_net_p2p::settings::{Source, Value};
use tasty_net_p2p::{AttemptPhase, P2pCloseReason};
use tasty_net_rendezvous::settings::{Source as ServerSource, Value as ServerValue};

use support::{
    NatSpec, TestServer, World, device, only_to, pair, registered, server_settings, settings,
};

fn server_ips(s: &TestServer) -> Vec<IpAddr> {
    let ip = s.info.addr.ip();
    let IpAddr::V4(v4) = ip else { unreachable!() };
    let o = v4.octets();
    vec![ip, IpAddr::from([o[0], o[1], o[2], o[3] + 1])]
}

const PREFIXES: [&str; 5] = ["punch.", "quic.", "nat.", "path.", "history."];

#[test]
fn connection_reports_applied_settings() {
    let w = World::new();
    let window = Duration::from_secs(2);
    let mut ss = server_settings();
    ss.set("punch_window", ServerValue::Duration(window))
        .unwrap();
    ss.set(
        "quic_handshake_timeout",
        ServerValue::Duration(Duration::from_secs(1)),
    )
    .unwrap();
    let s = w.server(1, ss);
    // The operator's value shows as configured on the server.
    let on_server = w
        .running(&s)
        .settings()
        .into_iter()
        .find(|e| e.key == "punch_window")
        .unwrap();
    assert_eq!(on_server.source, ServerSource::Configured);
    assert_eq!(on_server.value, ServerValue::Duration(window));

    let n1 = w.net.nat(1, NatSpec::eim());
    let n2 = w.net.nat(2, NatSpec::eim());
    let retry = Duration::from_secs(4);
    let a = device(
        &n1,
        2,
        1,
        &s,
        settings(&[("punch.retry_delay", Value::Duration(retry))]),
    );
    let b = device(&n2, 2, 2, &s, settings(&[]));
    pair(&a, &b);
    registered(&a);
    registered(&b);

    // A connection that stands: its device values are the endpoint's, the server values are
    // the server's.
    let ca = a.ep.connect(b.id()).expect("connect");
    let cb =
        b.ep.accept_timeout(Some(Duration::from_secs(10)))
            .expect("accept");
    let shown = a.ep.settings();
    for (i, conn) in [&ca, &cb].into_iter().enumerate() {
        let info = conn.info().expect("info");
        let device_part: Vec<_> = info
            .applied
            .iter()
            .filter(|e| e.source != Source::Server)
            .map(|e| (e.key, e.value, e.source))
            .collect();
        let want: Vec<_> = shown
            .iter()
            .filter(|e| PREFIXES.iter().any(|p| e.key.starts_with(p)))
            .map(|e| (e.key, e.value, e.source))
            .collect();
        // B's device values are B's own; only A's are compared with A's endpoint.
        if i == 0 {
            assert_eq!(device_part, want);
        }
        let get = |key: &str| {
            info.applied
                .iter()
                .find(|e| e.key == key)
                .unwrap_or_else(|| panic!("no {key} in {:?}", info.applied))
        };
        let w = get("punch_go.window_ms");
        assert_eq!(w.source, Source::Server);
        assert_eq!(w.value, Value::Duration(window));
        let h = get("punch_go.handshake_ms");
        assert_eq!(h.source, Source::Server);
        assert_eq!(h.value, Value::Duration(Duration::from_secs(1)));
        assert_eq!(get("punch_go.start_after_ms").source, Source::Server);
        assert!(
            info.applied
                .iter()
                .any(|e| e.key.starts_with("welcome.limits.") && e.source == Source::Server)
        );
    }
    let info = ca.info().unwrap();
    let r = info
        .applied
        .iter()
        .find(|e| e.key == "punch.retry_delay")
        .unwrap();
    assert_eq!(
        (r.value, r.source),
        (Value::Duration(retry), Source::Configured)
    );
    let rtt = info
        .applied
        .iter()
        .find(|e| e.key == "quic.initial_rtt")
        .unwrap();
    assert_eq!(rtt.source, Source::Default);
    ca.close(P2pCloseReason::Closed);
    drop(cb);

    // In RetryWait the attempt number and the time to the next attempt show.
    n1.set_rule(only_to(server_ips(&s)));
    let peer = b.id();
    let ep = a.ep.clone();
    let dial = std::thread::spawn(move || ep.connect(peer).map(|_| ()));
    let waiting = support::wait_until(Duration::from_secs(20), || {
        a.ep.connections()
            .iter()
            .any(|c| c.state == AttemptPhase::RetryWait)
    });
    assert!(waiting, "{:?}", a.ep.connections());
    let info =
        a.ep.connections()
            .into_iter()
            .find(|c| c.state == AttemptPhase::RetryWait)
            .unwrap();
    assert_eq!(info.attempt.number, 1, "{info:?}");
    let left = info.attempt.retry_in.expect("retry_in");
    assert!(left > Duration::ZERO && left <= retry, "{left:?}");
    assert!(info.attempt.bound_in.is_some(), "{info:?}");
    assert!(dial.join().unwrap().is_err());
}
