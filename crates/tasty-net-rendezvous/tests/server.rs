//! Server tests (docs/design/testing.md "P2P 시험", `tasty-net-rendezvous` `tests/server.rs`).
//! The server runs in the test process on loopback ports chosen by the OS.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_rendezvous::access::Allowlist;
use tasty_net_rendezvous::packet_io::{PacketIo, UdpPacketIo};
use tasty_net_rendezvous::server::{MTU_DISCOVERY, Server, ServerConfig, ServerIo, StartError};
use tasty_net_rendezvous::settings::{ServerSettings, Source, Value};
use tasty_net_rendezvous::tls::{self, TlsOptions};
use tasty_net_rendezvous::wire::candidate::CandidateKind;
use tasty_net_rendezvous::wire::frame::{read_frame, write_frame};
use tasty_net_rendezvous::wire::message::{
    AnswerReason, CancelReason, DeviceMessage, Hello, NatMapping, NatReport, PunchAnswer,
    PunchMode, PunchRequest, PunchResult, RefuseReason, ResultReason, Role, ServerMessage,
    StunResult, Welcome,
};
use tasty_net_rendezvous::wire::{ALPN_RENDEZVOUS, CloseCode, PROTO, stun};
use tokio::net::UdpSocket;

const DEADLINE: Duration = Duration::from_secs(10);
const LIMIT: u32 = 1024 * 1024;

fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

/// A global subscriber that takes no event but keeps every callsite's interest at
/// "sometimes". Without it a callsite first hit on a thread with no subscriber is cached as
/// "never" for the whole process, racing with the thread-local [`Capture`] of another test.
struct Quiet;

impl tracing::Subscriber for Quiet {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Every test builds its settings here before the server emits anything.
fn local_settings() -> ServerSettings {
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| {
        tracing::subscriber::set_global_default(Quiet).expect("no global subscriber yet");
    });
    let mut s = ServerSettings::new();
    s.set(
        "listen.primary",
        Value::Address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
    )
    .unwrap();
    s.set("listen.alternate_port", Value::Port(0)).unwrap();
    s
}

fn ms(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

async fn start(settings: ServerSettings) -> Server {
    let mut config = ServerConfig::new(key(100));
    config.settings = settings;
    Server::start(config).await.expect("server starts")
}

fn client_endpoint() -> quinn::Endpoint {
    quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap()
}

async fn connect_with_alpn(
    server: &Server,
    key: &DeviceKey,
    alpn: &[u8],
) -> Result<(quinn::Endpoint, quinn::Connection), quinn::ConnectionError> {
    connect_with(server, key, alpn, None).await
}

async fn connect_with(
    server: &Server,
    key: &DeviceKey,
    alpn: &[u8],
    transport: Option<quinn::TransportConfig>,
) -> Result<(quinn::Endpoint, quinn::Connection), quinn::ConnectionError> {
    let endpoint = client_endpoint();
    let crypto = tls::client_config(key, server.public_key(), alpn, TlsOptions::default()).unwrap();
    let mut config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));
    if let Some(t) = transport {
        config.transport_config(Arc::new(t));
    }
    let conn = tokio::time::timeout(
        DEADLINE,
        endpoint
            .connect_with(config, server.addrs().primary, tls::SERVER_NAME)
            .unwrap(),
    )
    .await
    .expect("handshake ends in time")?;
    Ok((endpoint, conn))
}

struct Device {
    id: DeviceId,
    _endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl Device {
    async fn open(server: &Server, key: &DeviceKey) -> Device {
        Device::open_with(server, key, None).await
    }

    async fn open_with(
        server: &Server,
        key: &DeviceKey,
        transport: Option<quinn::TransportConfig>,
    ) -> Device {
        let (endpoint, conn) = connect_with(server, key, ALPN_RENDEZVOUS, transport)
            .await
            .expect("handshake");
        let (send, recv) = conn.open_bi().await.unwrap();
        Device {
            id: key.id(),
            _endpoint: endpoint,
            conn,
            send,
            recv,
        }
    }

    async fn register(server: &Server, key: &DeviceKey, host: &[&str]) -> (Device, Welcome) {
        let mut d = Device::open(server, key).await;
        d.send(&DeviceMessage::Hello(Hello {
            proto: PROTO,
            host_candidates: host.iter().map(|s| s.to_string()).collect(),
        }))
        .await;
        match d.recv().await {
            ServerMessage::Welcome(w) => (d, w),
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    fn local_addr(&self) -> SocketAddr {
        self._endpoint.local_addr().unwrap()
    }

    async fn send_raw(&mut self, json: &[u8]) {
        write_frame(&mut self.send, json, LIMIT).await.unwrap();
    }

    async fn send(&mut self, msg: &DeviceMessage) {
        self.send_raw(&msg.to_json().unwrap()).await;
    }

    async fn recv_raw(&mut self) -> Vec<u8> {
        tokio::time::timeout(DEADLINE, read_frame(&mut self.recv, LIMIT, DEADLINE))
            .await
            .expect("a message in time")
            .expect("a frame")
            .expect("not the end")
    }

    async fn recv(&mut self) -> ServerMessage {
        ServerMessage::parse(&self.recv_raw().await).unwrap()
    }

    /// Nothing arrives within `wait`.
    async fn quiet(&mut self, wait: Duration) {
        if let Ok(got) =
            tokio::time::timeout(wait, read_frame(&mut self.recv, LIMIT, DEADLINE)).await
        {
            panic!("expected nothing, got {got:?}");
        }
    }

    async fn closed_with(&self) -> u64 {
        closed_with(&self.conn).await
    }
}

async fn closed_with(conn: &quinn::Connection) -> u64 {
    match tokio::time::timeout(DEADLINE, conn.closed())
        .await
        .expect("closed in time")
    {
        quinn::ConnectionError::ApplicationClosed(close) => close.error_code.into_inner(),
        other => panic!("expected an application close, got {other:?}"),
    }
}

fn code(c: CloseCode) -> u64 {
    u64::from(c.code())
}

/// A requests B and B accepts: both `punch_go` messages.
async fn punch(
    a: &mut Device,
    b: &mut Device,
    request_id: u64,
) -> (u64, ServerMessage, ServerMessage) {
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id,
        peer: b.id,
    }))
    .await;
    let punch_id = match b.recv().await {
        ServerMessage::PunchOffer(o) => {
            assert_eq!(o.from, a.id);
            o.punch_id
        }
        other => panic!("expected punch_offer, got {other:?}"),
    };
    b.send(&DeviceMessage::PunchAnswer(PunchAnswer {
        punch_id,
        accept: true,
        reason: None,
    }))
    .await;
    (punch_id, a.recv().await, b.recv().await)
}

const MARKER: &str = "device-bytes-7f3a";

#[tokio::test]
async fn server_never_forwards_device_bytes() {
    let server = start(local_settings()).await;
    let host_a = [
        "192.168.1.10:5000",
        "8.8.8.8:5000",
        MARKER,
        "[2001:db8::10]:5000",
    ];
    let (mut a, welcome_a) = Device::register(&server, &key(1), &host_a).await;
    let (mut b, _) = Device::register(&server, &key(2), &["10.0.0.20:6000"]).await;
    assert_eq!(welcome_a.reflexive, a.local_addr());
    let alt = welcome_a.stun[0];
    a.send(&DeviceMessage::NatReport(NatReport {
        mapping: NatMapping::IndependentPortOnly,
        stun_results: vec![
            StunResult {
                server: alt.to_string(),
                reflexive: Some("127.0.0.1:6000".into()),
            },
            // Not a server address this device was told: dropped.
            StunResult {
                server: "203.0.113.1:3478".into(),
                reflexive: Some("127.0.0.1:6001".into()),
            },
            StunResult {
                server: alt.to_string(),
                reflexive: Some(MARKER.into()),
            },
            StunResult {
                server: alt.to_string(),
                reflexive: None,
            },
        ],
        host_candidates: None,
    }))
    .await;
    // A STUN probe sent to the server is not passed on either.
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let probe = stun::binding_request(
        &[9; 12],
        Some(&tasty_net_rendezvous::wire::message::PunchToken([7; 16])),
    );
    udp.send_to(&probe, server.addrs().primary).await.unwrap();

    let (punch_id, go_a, go_b) = punch(&mut a, &mut b, 41).await;
    let ServerMessage::PunchGo(go_b) = go_b else {
        panic!("{go_b:?}");
    };
    let ServerMessage::PunchGo(go_a) = go_a else {
        panic!("{go_a:?}");
    };
    // What B got about A is exactly the closed set: A's ID from TLS, the server-observed
    // address, the accepted reported reflexive address and the accepted host addresses.
    assert_eq!(go_b.peer, a.id);
    assert_eq!(go_b.punch_id, punch_id);
    assert_eq!(go_b.request_id, None);
    assert_eq!(go_b.role, Role::Controlled);
    assert_eq!(go_b.mode, PunchMode::All);
    assert_eq!(go_b.peer_nat, NatMapping::IndependentPortOnly);
    let got: Vec<(CandidateKind, SocketAddr)> =
        go_b.candidates.iter().map(|c| (c.kind, c.addr)).collect();
    assert_eq!(
        got,
        vec![
            (CandidateKind::ServerReflexive, a.local_addr()),
            (
                CandidateKind::ReportedReflexive,
                "127.0.0.1:6000".parse().unwrap()
            ),
            (CandidateKind::Host, "192.168.1.10:5000".parse().unwrap()),
            (CandidateKind::Host, "[2001:db8::10]:5000".parse().unwrap()),
        ]
    );
    assert_eq!(go_a.peer, b.id);
    assert_eq!(go_a.request_id, Some(41));
    assert_eq!(go_a.role, Role::Controlling);
    assert_eq!(go_a.token_local, go_b.token_peer);
    assert_eq!(go_a.token_peer, go_b.token_local);
    assert_ne!(go_a.token_local, go_a.token_peer);
    assert_eq!(go_a.window_ms, 5000);
    assert_eq!(go_a.handshake_ms, 5000);
    assert!(go_a.start_after_ms <= 1000);

    // Both report; nothing more reaches B, and the server holds no session.
    a.send(&DeviceMessage::PunchResult(PunchResult {
        punch_id,
        ok: true,
        reason: None,
    }))
    .await;
    b.send(&DeviceMessage::PunchResult(PunchResult {
        punch_id,
        ok: true,
        reason: None,
    }))
    .await;
    b.quiet(Duration::from_millis(300)).await;
    assert_eq!(server.status().punches, 0);
    assert!(server.status().stun_dropped >= 1);
    assert!(
        server.status().candidates_rejected >= 3,
        "{:?}",
        server.status()
    );
}

#[tokio::test]
async fn unknown_fields_are_not_forwarded() {
    let server = start(local_settings()).await;
    let mut a = Device::open(&server, &key(1)).await;
    a.send_raw(
        format!(
            r#"{{"type":"hello","proto":1,"host_candidates":["192.168.1.2:7000"],"{MARKER}":"{MARKER}","relay":{{"data":"{MARKER}"}}}}"#
        )
        .as_bytes(),
    )
    .await;
    assert!(matches!(a.recv().await, ServerMessage::Welcome(_)));
    let (mut b, _) = Device::register(&server, &key(2), &["192.168.1.3:7000"]).await;
    a.send_raw(
        format!(
            r#"{{"type":"punch_request","request_id":5,"peer":"{}","note":"{MARKER}"}}"#,
            b.id
        )
        .as_bytes(),
    )
    .await;
    let offer = b.recv_raw().await;
    assert!(!String::from_utf8_lossy(&offer).contains(MARKER));
    let ServerMessage::PunchOffer(offer) = ServerMessage::parse(&offer).unwrap() else {
        panic!("expected an offer");
    };
    b.send_raw(
        format!(
            r#"{{"type":"punch_answer","punch_id":{},"accept":true,"extra":"{MARKER}"}}"#,
            offer.punch_id
        )
        .as_bytes(),
    )
    .await;
    let go_a = a.recv_raw().await;
    let go_b = b.recv_raw().await;
    for raw in [&go_a, &go_b] {
        let text = String::from_utf8_lossy(raw);
        assert!(!text.contains(MARKER), "{text}");
        assert!(text.contains(r#""type":"punch_go""#), "{text}");
    }
    // An unknown type closes the connection as malformed.
    a.send_raw(br#"{"type":"relay","data":"x"}"#).await;
    assert_eq!(a.closed_with().await, code(CloseCode::Malformed));
}

#[tokio::test]
async fn second_stream_is_refused() {
    let server = start(local_settings()).await;
    let (d, _) = Device::register(&server, &key(1), &[]).await;
    let (mut send, _recv) = d.conn.open_bi().await.unwrap();
    send.write_all(b"x").await.unwrap();
    assert_eq!(d.closed_with().await, code(CloseCode::Malformed));
    tokio::time::timeout(DEADLINE, async {
        while server.status().registrations != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("registration removed");
}

async fn stun_request(
    socket: &UdpSocket,
    to: SocketAddr,
    packet: &[u8],
) -> Option<stun::StunMessage> {
    socket.send_to(packet, to).await.unwrap();
    let mut buf = [0u8; 1500];
    match tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await {
        Ok(Ok((n, from))) => {
            assert_eq!(from, to, "answers come from the address that was asked");
            Some(stun::parse(&buf[..n]).unwrap())
        }
        _ => None,
    }
}

#[tokio::test]
async fn stun_answers_only_the_sender() {
    let server = start(local_settings()).await;
    let x = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let y = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    for to in [server.addrs().primary, server.addrs().alternate_port] {
        let txid = [3; 12];
        let got = stun_request(&x, to, &stun::binding_request(&txid, None))
            .await
            .expect("an answer");
        assert_eq!(got.class, stun::StunClass::BindingSuccess);
        assert_eq!(got.transaction_id, txid);
        assert_eq!(got.mapped, Some(x.local_addr().unwrap()));
        // CHANGE-REQUEST asks the server to answer from elsewhere: not answered at all.
        let mut change = stun::binding_request(&txid, None);
        change.extend_from_slice(&[0x00, 0x03, 0x00, 0x04, 0, 0, 0, 0x06]);
        change[3] = 8;
        assert!(stun_request(&x, to, &change).await.is_none());
        // A punch probe is for devices.
        let probe = stun::binding_request(
            &txid,
            Some(&tasty_net_rendezvous::wire::message::PunchToken([1; 16])),
        );
        assert!(stun_request(&x, to, &probe).await.is_none());
    }
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), y.recv_from(&mut buf))
            .await
            .is_err(),
        "another socket gets nothing"
    );
    // QUIC-looking bytes on a STUN-only port are dropped.
    x.send_to(&[0xc0, 0, 0, 0], server.addrs().alternate_port)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), x.recv_from(&mut buf))
            .await
            .is_err()
    );
    assert_eq!(server.status().stun_answered, 2);
}

#[tokio::test]
async fn stun_rate_limit_per_source() {
    let mut s = local_settings();
    s.set("stun_per_source_per_second", Value::Count(5))
        .unwrap();
    let server = start(s).await;
    let x = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let to = server.addrs().primary;
    for i in 0..20u8 {
        x.send_to(&stun::binding_request(&[i; 12], None), to)
            .await
            .unwrap();
    }
    let mut answered = 0;
    let mut buf = [0u8; 256];
    while tokio::time::timeout(Duration::from_millis(300), x.recv_from(&mut buf))
        .await
        .is_ok()
    {
        answered += 1;
    }
    // The requests went out within one window, so at most 5 are answered (the window may
    // turn over once while sending).
    assert!((5..=10).contains(&answered), "answered {answered}");
    assert!(
        server.status().stun_rate_limited >= 10,
        "{:?}",
        server.status()
    );
    // Another source has its own count.
    let y = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    assert!(
        stun_request(&y, to, &stun::binding_request(&[1; 12], None))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn stun_sources_per_window_are_capped() {
    let mut s = local_settings();
    s.set("max_stun_sources", Value::Count(2)).unwrap();
    let server = start(s).await;
    let to = server.addrs().primary;
    let x = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let y = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let z = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    // x and y fill the window; z, a third source in the same window, is not answered. The
    // window may turn over between two sends (then another socket is the third), so the
    // rounds repeat and every unanswered request must be a counted drop.
    let mut dropped = 0;
    for round in 0..3u8 {
        let request = stun::binding_request(&[round; 12], None);
        for socket in [&x, &y, &z] {
            socket.send_to(&request, to).await.unwrap();
        }
        let mut buf = [0u8; 256];
        for socket in [&x, &y, &z] {
            if tokio::time::timeout(Duration::from_millis(300), socket.recv_from(&mut buf))
                .await
                .is_err()
            {
                dropped += 1;
            }
        }
    }
    assert!(dropped >= 1, "{:?}", server.status());
    assert_eq!(server.status().stun_sources_full, dropped);
    assert_eq!(server.status().stun_rate_limited, 0);
    // In a later window z is counted again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        stun_request(&z, to, &stun::binding_request(&[9; 12], None))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn pending_connections_are_capped() {
    let mut s = local_settings();
    s.set("max_pending_connections", Value::Count(1)).unwrap();
    let server = start(s).await;
    // A connection that has not sent hello holds the one pending place.
    let mut first = Device::open(&server, &key(1)).await;
    eventually("the first connection is pending", || {
        server.status().pending == 1
    })
    .await;
    match connect_with_alpn(&server, &key(2), ALPN_RENDEZVOUS).await {
        Err(quinn::ConnectionError::ConnectionClosed(close)) => {
            // CONNECTION_REFUSED (RFC 9000 §20.1).
            assert_eq!(u64::from(close.error_code), 0x2, "{close:?}");
        }
        other => panic!("expected a refused connection, got {other:?}"),
    }
    assert_eq!(server.status().pending_refused, 1);
    // Once the first one registers, the place is free.
    first
        .send(&DeviceMessage::Hello(Hello {
            proto: PROTO,
            host_candidates: Vec::new(),
        }))
        .await;
    assert!(matches!(first.recv().await, ServerMessage::Welcome(_)));
    eventually("the place is free", || server.status().pending == 0).await;
    let (_d, _) = Device::register(&server, &key(2), &[]).await;
    assert_eq!(server.status().registrations, 2);
}

#[tokio::test]
async fn access_policy_rejects() {
    let allowed = key(1);
    let mut config = ServerConfig::new(key(100));
    config.settings = local_settings();
    config.access = Arc::new(Allowlist::new([allowed.id()]));
    let server = Server::start(config).await.unwrap();
    let (_a, _) = Device::register(&server, &allowed, &[]).await;
    let (_endpoint, conn) = connect_with_alpn(&server, &key(2), ALPN_RENDEZVOUS)
        .await
        .expect("TLS succeeds; the policy refuses after it");
    assert_eq!(closed_with(&conn).await, code(CloseCode::AccessDenied));
    assert_eq!(server.status().registrations, 1);
    assert_eq!(server.status().access_denied, 1);
}

#[tokio::test]
async fn superseded_registration_is_closed() {
    let server = start(local_settings()).await;
    let (mut peer, _) = Device::register(&server, &key(2), &["192.168.0.2:1"]).await;
    let (mut first, _) = Device::register(&server, &key(1), &["192.168.0.1:1"]).await;
    // The first registration has a punch offered by the peer. The new registration starts
    // only once the offer arrived: messages of two connections have no order between them.
    peer.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: first.id,
    }))
    .await;
    match first.recv().await {
        ServerMessage::PunchOffer(o) => assert_eq!(o.from, peer.id),
        other => panic!("expected punch_offer, got {other:?}"),
    }
    let (mut second, _) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(first.closed_with().await, code(CloseCode::Superseded));
    // The peer's session with the old registration is ended.
    match peer.recv().await {
        ServerMessage::PunchCancel(c) => assert_eq!(c.reason, CancelReason::PeerGone),
        other => panic!("expected punch_cancel, got {other:?}"),
    }
    assert_eq!(server.status().registrations, 2);
    assert_eq!(server.status().superseded, 1);
    // The new registration works.
    second
        .send(&DeviceMessage::PunchRequest(PunchRequest {
            request_id: 2,
            peer: second.id,
        }))
        .await;
    match second.recv().await {
        ServerMessage::PunchRefused(r) => assert_eq!(r.reason, RefuseReason::SelfPeer),
        other => panic!("{other:?}"),
    }
}

/// `cond` becomes true within the deadline.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let until = tokio::time::Instant::now() + DEADLINE;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < until,
            "{what} within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn device_that_does_not_read_is_closed() {
    let mut s = local_settings();
    s.set("max_queued_messages", Value::Count(8)).unwrap();
    let server = start(s).await;
    // A device whose receive window holds about 1 KiB and that never reads.
    let mut t = quinn::TransportConfig::default();
    t.stream_receive_window(quinn::VarInt::from_u32(1024));
    t.receive_window(quinn::VarInt::from_u32(1024));
    let mut d = Device::open_with(&server, &key(1), Some(t)).await;
    d.send(&DeviceMessage::Hello(Hello {
        proto: PROTO,
        host_candidates: Vec::new(),
    }))
    .await;
    let me = d.id;
    let flood = async {
        for request_id in 0..10_000 {
            let msg = DeviceMessage::PunchRequest(PunchRequest {
                request_id,
                peer: me,
            });
            if write_frame(&mut d.send, &msg.to_json().unwrap(), LIMIT)
                .await
                .is_err()
            {
                break;
            }
        }
    };
    tokio::time::timeout(DEADLINE, flood)
        .await
        .expect("the flood ends");
    assert_eq!(closed_with(&d.conn).await, code(CloseCode::Busy));
    eventually("the registration is removed", || {
        server.status().registrations == 0
    })
    .await;
    assert_eq!(server.status().queue_full, 1);
    // The slot is free again.
    let (_d, _) = Device::register(&server, &key(2), &[]).await;
    assert_eq!(server.status().registrations, 1);
}

#[tokio::test]
async fn reset_coordination_stream_removes_the_registration() {
    let mut s = local_settings();
    s.set("max_devices", Value::Count(1)).unwrap();
    let server = start(s).await;
    let (mut d, _) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(server.status().registrations, 1);
    d.send.reset(quinn::VarInt::from_u32(0)).unwrap();
    assert_eq!(d.closed_with().await, code(CloseCode::Malformed));
    eventually("the registration is removed", || {
        server.status().registrations == 0
    })
    .await;
    // The max_devices slot is free again.
    let (_d, _) = Device::register(&server, &key(2), &[]).await;
    assert_eq!(server.status().registrations, 1);
    assert_eq!(server.status().busy_max_devices, 0);
}

#[tokio::test]
async fn request_rate_survives_a_new_registration() {
    let mut s = local_settings();
    s.set("punch_requests_per_minute", Value::Count(2)).unwrap();
    let server = start(s).await;
    let unknown = key(9).id();
    let request = |request_id| {
        DeviceMessage::PunchRequest(PunchRequest {
            request_id,
            peer: unknown,
        })
    };
    let (mut d, _) = Device::register(&server, &key(1), &[]).await;
    for request_id in 0..2 {
        d.send(&request(request_id)).await;
        assert_eq!(refused(&mut d).await, RefuseReason::NotFound);
    }
    // The same device ID on a new connection is still within the minute.
    let (mut d, _) = Device::register(&server, &key(1), &[]).await;
    d.send(&request(2)).await;
    assert_eq!(refused(&mut d).await, RefuseReason::Busy);
    assert_eq!(server.status().busy_request_rate, 1);
}

async fn refused(d: &mut Device) -> RefuseReason {
    match d.recv().await {
        ServerMessage::PunchRefused(r) => r.reason,
        other => panic!("expected punch_refused, got {other:?}"),
    }
}

#[tokio::test]
async fn limits_return_busy() {
    // max_devices.
    let mut s = local_settings();
    s.set("max_devices", Value::Count(1)).unwrap();
    let server = start(s).await;
    let (_a, _) = Device::register(&server, &key(1), &[]).await;
    let mut b = Device::open(&server, &key(2)).await;
    b.send(&DeviceMessage::Hello(Hello {
        proto: PROTO,
        host_candidates: Vec::new(),
    }))
    .await;
    assert_eq!(b.closed_with().await, code(CloseCode::Busy));
    assert_eq!(server.status().busy_max_devices, 1);

    // Per device, pair direction, request rate, unknown peer and self.
    let mut s = local_settings();
    s.set("max_punches_per_device", Value::Count(1)).unwrap();
    s.set("punch_requests_per_minute", Value::Count(4)).unwrap();
    let server = start(s).await;
    let (mut a, _) = Device::register(&server, &key(1), &[]).await;
    let (mut b, _) = Device::register(&server, &key(2), &[]).await;
    let (_c, _) = Device::register(&server, &key(3), &[]).await;
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: b.id,
    }))
    .await;
    assert!(matches!(b.recv().await, ServerMessage::PunchOffer(_)));
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 2,
        peer: key(3).id(),
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::Busy);
    b.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 3,
        peer: a.id,
    }))
    .await;
    assert_eq!(refused(&mut b).await, RefuseReason::Busy);
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 4,
        peer: key(9).id(),
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::NotFound);
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 5,
        peer: a.id,
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::SelfPeer);
    // The fifth request within a minute.
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 6,
        peer: b.id,
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::Busy);
    let st = server.status();
    assert_eq!(st.busy_per_device, 1, "{st:?}");
    assert_eq!(st.busy_pair, 1, "{st:?}");
    assert_eq!(st.busy_request_rate, 1, "{st:?}");

    // max_punches over all devices.
    let mut s = local_settings();
    s.set("max_punches", Value::Count(1)).unwrap();
    let server = start(s).await;
    let (mut a, _) = Device::register(&server, &key(1), &[]).await;
    let (mut b, _) = Device::register(&server, &key(2), &[]).await;
    let (mut c, _) = Device::register(&server, &key(3), &[]).await;
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: b.id,
    }))
    .await;
    assert!(matches!(b.recv().await, ServerMessage::PunchOffer(_)));
    c.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: a.id,
    }))
    .await;
    assert_eq!(refused(&mut c).await, RefuseReason::Busy);
    assert_eq!(server.status().busy_max_punches, 1);

    // Both dependent and one side without host candidates: nat_unsupported.
    let server = start(local_settings()).await;
    let (mut a, _) = Device::register(&server, &key(1), &["192.168.1.1:1"]).await;
    let (mut b, _) = Device::register(&server, &key(2), &[]).await;
    for d in [&mut a, &mut b] {
        d.send(&DeviceMessage::NatReport(NatReport {
            mapping: NatMapping::Dependent,
            stun_results: Vec::new(),
            host_candidates: None,
        }))
        .await;
        // The server handles a stream's frames in order: a refused self request answered
        // after the report means the report is in.
        let id = d.id;
        d.send(&DeviceMessage::PunchRequest(PunchRequest {
            request_id: 0,
            peer: id,
        }))
        .await;
        assert_eq!(refused(d).await, RefuseReason::SelfPeer);
    }
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: b.id,
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::NatUnsupported);
}

#[tokio::test]
async fn punch_session_expires() {
    let mut s = local_settings();
    s.set("offer_timeout", ms(200)).unwrap();
    s.set("start_delay_margin", ms(50)).unwrap();
    s.set("start_delay_max", ms(100)).unwrap();
    s.set("punch_window", ms(100)).unwrap();
    s.set("quic_handshake_timeout", ms(100)).unwrap();
    s.set("punch_session_grace", ms(1000)).unwrap();
    let server = start(s).await;
    let (mut a, _) = Device::register(&server, &key(1), &[]).await;
    let (mut b, welcome_b) = Device::register(&server, &key(2), &[]).await;
    assert_eq!(
        welcome_b.limits.punch_session_ttl_ms,
        100 + 100 + 200 + 1000
    );
    assert_eq!(welcome_b.limits.offer_timeout_ms, 200);

    // B does not answer: A gets not_found after offer_timeout.
    let started = tokio::time::Instant::now();
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 1,
        peer: b.id,
    }))
    .await;
    assert!(matches!(b.recv().await, ServerMessage::PunchOffer(_)));
    assert_eq!(refused(&mut a).await, RefuseReason::NotFound);
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(server.status().punches, 0);

    // After punch_go, no result: both get punch_cancel{expired} after the TTL. The clock
    // starts before the request, so before the server sends punch_go: the TTL is a strict
    // lower bound of what it measures.
    let gone = tokio::time::Instant::now();
    let (punch_id, go_a, _) = punch(&mut a, &mut b, 2).await;
    let ServerMessage::PunchGo(go_a) = go_a else {
        panic!("{go_a:?}");
    };
    assert!(go_a.start_after_ms <= 100);
    for d in [&mut a, &mut b] {
        match d.recv().await {
            ServerMessage::PunchCancel(c) => {
                assert_eq!(c.punch_id, punch_id);
                assert_eq!(c.reason, CancelReason::Expired);
            }
            other => panic!("expected punch_cancel, got {other:?}"),
        }
    }
    assert!(
        gone.elapsed() >= Duration::from_millis(1400),
        "{:?}",
        gone.elapsed()
    );
    assert_eq!(server.status().punches, 0);

    // A side that reported is not told; limits_invalid cancels the other side.
    let (punch_id, _, _) = punch(&mut a, &mut b, 3).await;
    a.send(&DeviceMessage::PunchResult(PunchResult {
        punch_id,
        ok: false,
        reason: Some(ResultReason::LimitsInvalid),
    }))
    .await;
    match b.recv().await {
        ServerMessage::PunchCancel(c) => assert_eq!(c.reason, CancelReason::PeerLimitsInvalid),
        other => panic!("{other:?}"),
    }
    // A retry from the same A supersedes its session; B is told.
    let (old, _, _) = punch(&mut a, &mut b, 4).await;
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 5,
        peer: b.id,
    }))
    .await;
    match b.recv().await {
        ServerMessage::PunchCancel(c) => {
            assert_eq!(c.punch_id, old);
            assert_eq!(c.reason, CancelReason::Superseded);
        }
        other => panic!("{other:?}"),
    }
    // A refusing answer reaches A as declined or peer_limits_invalid.
    let ServerMessage::PunchOffer(offer) = b.recv().await else {
        panic!("expected the offer of request 5");
    };
    b.send(&DeviceMessage::PunchAnswer(PunchAnswer {
        punch_id: offer.punch_id,
        accept: false,
        reason: Some(AnswerReason::NotPaired),
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::Declined);
    a.send(&DeviceMessage::PunchRequest(PunchRequest {
        request_id: 6,
        peer: b.id,
    }))
    .await;
    let ServerMessage::PunchOffer(offer) = b.recv().await else {
        panic!("expected the offer of request 6");
    };
    b.send(&DeviceMessage::PunchAnswer(PunchAnswer {
        punch_id: offer.punch_id,
        accept: false,
        reason: Some(AnswerReason::LimitsInvalid),
    }))
    .await;
    assert_eq!(refused(&mut a).await, RefuseReason::PeerLimitsInvalid);
    assert_eq!(server.status().punches, 0);
}

#[tokio::test]
async fn frame_timeout_closes() {
    let mut s = local_settings();
    s.set("frame_timeout", ms(300)).unwrap();
    s.set("hello_timeout", ms(500)).unwrap();
    let server = start(s).await;
    // A frame started and not finished.
    let (mut d, _) = Device::register(&server, &key(1), &[]).await;
    let started = tokio::time::Instant::now();
    d.send.write_all(&[0, 0]).await.unwrap();
    assert_eq!(d.closed_with().await, code(CloseCode::Malformed));
    assert!(started.elapsed() >= Duration::from_millis(300));
    // A length over max_message_bytes closes at once.
    let (mut d, _) = Device::register(&server, &key(2), &[]).await;
    d.send
        .write_all(&(16 * 1024 + 1u32).to_be_bytes())
        .await
        .unwrap();
    assert_eq!(d.closed_with().await, code(CloseCode::Malformed));
    // No hello: hello_timeout.
    let d = Device::open(&server, &key(3)).await;
    assert_eq!(d.closed_with().await, code(CloseCode::HelloTimeout));
    // A hello that started and did not end is malformed, even past hello_timeout.
    let mut s = local_settings();
    s.set("hello_timeout", ms(300)).unwrap();
    s.set("frame_timeout", ms(800)).unwrap();
    let slow = start(s).await;
    let mut d = Device::open(&slow, &key(5)).await;
    let started = tokio::time::Instant::now();
    d.send.write_all(&[0, 0]).await.unwrap();
    assert_eq!(d.closed_with().await, code(CloseCode::Malformed));
    assert!(started.elapsed() >= Duration::from_millis(800));
    // Nothing at all on that server: hello_timeout.
    let d = Device::open(&slow, &key(6)).await;
    assert_eq!(d.closed_with().await, code(CloseCode::HelloTimeout));
    // Another protocol number.
    let mut d = Device::open(&server, &key(4)).await;
    d.send(&DeviceMessage::Hello(Hello {
        proto: PROTO + 1,
        host_candidates: Vec::new(),
    }))
    .await;
    assert_eq!(d.closed_with().await, code(CloseCode::UnsupportedProto));
}

#[tokio::test]
async fn advertised_addresses_are_used() {
    let mut s = local_settings();
    s.set("advertise.alternate_port", Value::Port(40_000))
        .unwrap();
    s.set(
        "advertise.alternate_ip",
        Value::Ip("203.0.113.9".parse().unwrap()),
    )
    .unwrap();
    let server = start(s).await;
    let primary_port = server.addrs().primary.port();
    let (_d, w) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(
        w.stun,
        vec![
            "127.0.0.1:40000".parse().unwrap(),
            SocketAddr::from(([203, 0, 113, 9], primary_port)),
            "203.0.113.9:40000".parse().unwrap(),
        ]
    );
    // advertise.primary_ip replaces the listen IP; an IPv6 second IP is not sent to an IPv4
    // device; without advertise.alternate_port the bound alternate port is used.
    let mut s = local_settings();
    s.set(
        "advertise.primary_ip",
        Value::Ip("198.51.100.1".parse().unwrap()),
    )
    .unwrap();
    s.set(
        "advertise.alternate_ip",
        Value::Ip("2001:db8::9".parse().unwrap()),
    )
    .unwrap();
    let server = start(s).await;
    let (_d, w) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(
        w.stun,
        vec![SocketAddr::new(
            "198.51.100.1".parse().unwrap(),
            server.addrs().alternate_port.port()
        )]
    );
    // The welcome limits are the server values.
    assert_eq!(w.limits.max_message_bytes, 16 * 1024);
    assert_eq!(w.limits.max_host_candidates, 4);
    assert_eq!(w.limits.max_reported_reflexive, 3);
    assert_eq!(w.limits.punch_session_ttl_ms, 21_000);
}

#[tokio::test]
async fn advertise_defaults_follow_the_bound_sockets() {
    // With injected packet I/O, listen.* is not what the sockets are bound to: the alternate
    // STUN address takes the IP of the primary socket and the port of the alternate socket.
    let mut s = local_settings();
    s.set(
        "listen.primary",
        Value::Address("127.0.0.9:3478".parse().unwrap()),
    )
    .unwrap();
    let io = |addr: &str| {
        let addr: SocketAddr = addr.parse().unwrap();
        async move {
            let io: Arc<dyn PacketIo> = Arc::new(UdpPacketIo::bind(addr).await.unwrap());
            io
        }
    };
    let mut config = ServerConfig::new(key(100));
    config.settings = s.clone();
    let server = Server::start_with_io(
        config,
        ServerIo {
            primary: io("127.0.0.1:0").await,
            alternate_port: io("127.0.0.1:0").await,
            alternate_ip: None,
        },
    )
    .await
    .expect("server starts");
    let (_d, w) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(w.stun, vec![server.addrs().alternate_port]);
    // A wildcard primary socket without advertise.primary_ip is refused.
    let mut config = ServerConfig::new(key(100));
    config.settings = s;
    match Server::start_with_io(
        config,
        ServerIo {
            primary: io("0.0.0.0:0").await,
            alternate_port: io("0.0.0.0:0").await,
            alternate_ip: None,
        },
    )
    .await
    {
        Err(StartError::Settings(e)) => assert!(e.to_string().contains("advertise.primary_ip")),
        other => panic!("expected a settings error, got {other:?}"),
    }
}

/// Packet I/O over a real socket that first returns `error` once.
#[derive(Debug)]
struct FailOnce {
    io: UdpPacketIo,
    error: Mutex<Option<std::io::Error>>,
}

impl PacketIo for FailOnce {
    fn poll_recv_from(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<(usize, SocketAddr)>> {
        if let Some(e) = self.error.lock().unwrap().take() {
            return std::task::Poll::Ready(Err(e));
        }
        self.io.poll_recv_from(cx, buf)
    }
    fn poll_send_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.io.poll_send_ready(cx)
    }
    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        self.io.try_send_to(buf, dest)
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.io.local_addr()
    }
}

#[tokio::test]
async fn datagram_receive_error_is_dropped() {
    // Windows reports a datagram larger than the buffer as WSAEMSGSIZE (10040); quinn would
    // end the driver of the primary port on it.
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let primary = FailOnce {
        io: UdpPacketIo::bind(local).await.unwrap(),
        error: Mutex::new(Some(std::io::Error::from_raw_os_error(10040))),
    };
    let mut config = ServerConfig::new(key(100));
    config.settings = local_settings();
    let server = Server::start_with_io(
        config,
        ServerIo {
            primary: Arc::new(primary),
            alternate_port: Arc::new(UdpPacketIo::bind(local).await.unwrap()),
            alternate_ip: None,
        },
    )
    .await
    .expect("server starts");
    let (_d, _) = Device::register(&server, &key(1), &[]).await;
    assert_eq!(server.status().receive_errors, 1);
    assert_eq!(server.status().registrations, 1);
}

/// Packet I/O whose every receive fails at once with `kind`.
#[derive(Debug)]
struct AlwaysFails {
    kind: std::io::ErrorKind,
    polls: Arc<std::sync::atomic::AtomicU64>,
}

impl PacketIo for AlwaysFails {
    fn poll_recv_from(
        &self,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> std::task::Poll<std::io::Result<(usize, SocketAddr)>> {
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::task::Poll::Ready(Err(self.kind.into()))
    }
    fn poll_send_ready(
        &self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn try_send_to(&self, _: &[u8], _: SocketAddr) -> std::io::Result<()> {
        Ok(())
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, 9)))
    }
}

async fn server_with_failing_stun_port(
    kind: std::io::ErrorKind,
) -> (Server, Arc<std::sync::atomic::AtomicU64>) {
    let polls = Arc::<std::sync::atomic::AtomicU64>::default();
    let mut config = ServerConfig::new(key(100));
    config.settings = local_settings();
    let server = Server::start_with_io(
        config,
        ServerIo {
            primary: Arc::new(
                UdpPacketIo::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .await
                    .unwrap(),
            ),
            alternate_port: Arc::new(AlwaysFails {
                kind,
                polls: polls.clone(),
            }),
            alternate_ip: None,
        },
    )
    .await
    .expect("server starts");
    (server, polls)
}

/// The receive count stays the same for a while once the port has received at least once:
/// the task that polls has ended.
async fn polls_stopped(polls: &std::sync::atomic::AtomicU64) -> bool {
    let load = || polls.load(std::sync::atomic::Ordering::Relaxed);
    eventually("the port receives", || load() > 0).await;
    let before = load();
    tokio::time::sleep(Duration::from_millis(100)).await;
    load() == before
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stun_port_errors_do_not_hold_the_executor() {
    // A receive error of one datagram, over and over: the port keeps trying but yields, so the
    // server still serves and its shutdown ends the task.
    let (server, polls) = server_with_failing_stun_port(std::io::ErrorKind::ConnectionReset).await;
    let (_d, _) = Device::register(&server, &key(1), &[]).await;
    assert!(!polls_stopped(&polls).await, "the port keeps receiving");
    assert!(server.status().receive_errors > 0);
    tokio::time::timeout(DEADLINE, server.shutdown())
        .await
        .expect("shutdown ends in time");
    assert!(
        polls_stopped(&polls).await,
        "the port task ended with the server"
    );
    // A broken socket ends its port at once.
    let (server, polls) = server_with_failing_stun_port(std::io::ErrorKind::NotConnected).await;
    assert!(polls_stopped(&polls).await, "the port task ended by itself");
    assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 1);
    let (_d, _) = Device::register(&server, &key(1), &[]).await;
    tokio::time::timeout(DEADLINE, server.shutdown())
        .await
        .expect("shutdown ends in time");
}

#[tokio::test]
async fn mtu_discovery_follows_the_socket() {
    let mtu = |server: &Server| {
        server
            .settings()
            .into_iter()
            .find(|e| e.key == MTU_DISCOVERY)
            .expect("the MTU discovery line")
    };
    // The real socket asks the OS not to fragment where it can; then discovery runs.
    let server = start(local_settings()).await;
    let probe = UdpPacketIo::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let line = mtu(&server);
    assert_eq!(line.value, Value::Switch(!probe.may_fragment()));
    assert_eq!(line.source, Source::Computed);
    if cfg!(target_os = "linux") {
        assert_eq!(line.value, Value::Switch(true));
    }
    // Injected packet I/O that does not know is shown as off.
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let mut config = ServerConfig::new(key(100));
    config.settings = local_settings();
    let server = Server::start_with_io(
        config,
        ServerIo {
            primary: Arc::new(FailOnce {
                io: UdpPacketIo::bind(local).await.unwrap(),
                error: Mutex::new(None),
            }),
            alternate_port: Arc::new(UdpPacketIo::bind(local).await.unwrap()),
            alternate_ip: None,
        },
    )
    .await
    .expect("server starts");
    assert_eq!(mtu(&server).value, Value::Switch(false));
}

#[tokio::test]
async fn dual_stack_addresses_are_ipv4() {
    // Windows opens [::] as IPv6 only, so an IPv4 device cannot reach it there.
    if cfg!(windows) {
        return;
    }
    let mut s = local_settings();
    s.set("listen.primary", Value::Address("[::]:0".parse().unwrap()))
        .unwrap();
    s.set(
        "advertise.primary_ip",
        Value::Ip("127.0.0.1".parse().unwrap()),
    )
    .unwrap();
    let mut config = ServerConfig::new(key(100));
    config.settings = s;
    let Ok(server) = Server::start(config).await else {
        // No IPv6 on this host.
        return;
    };
    let port = server.addrs().primary.port();
    let alt = server.addrs().alternate_port.port();
    // An IPv4 device on the IPv4 loopback reaches the dual-stack socket.
    let endpoint = client_endpoint();
    let crypto = tls::client_config(
        &key(1),
        server.public_key(),
        ALPN_RENDEZVOUS,
        TlsOptions::default(),
    )
    .unwrap();
    let config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));
    let conn = tokio::time::timeout(
        DEADLINE,
        endpoint
            .connect_with(
                config,
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                tls::SERVER_NAME,
            )
            .unwrap(),
    )
    .await
    .expect("handshake ends in time")
    .expect("handshake");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = DeviceMessage::Hello(Hello {
        proto: PROTO,
        host_candidates: Vec::new(),
    });
    write_frame(&mut send, &hello.to_json().unwrap(), LIMIT)
        .await
        .unwrap();
    let body = tokio::time::timeout(DEADLINE, read_frame(&mut recv, LIMIT, DEADLINE))
        .await
        .expect("a welcome in time")
        .unwrap()
        .unwrap();
    let ServerMessage::Welcome(w) = ServerMessage::parse(&body).unwrap() else {
        panic!("expected welcome");
    };
    // The device is told its IPv4 address, not ::ffff:127.0.0.1, and gets the IPv4 STUN
    // address.
    assert_eq!(w.reflexive, endpoint.local_addr().unwrap());
    assert!(w.reflexive.is_ipv4());
    assert_eq!(w.stun, vec![SocketAddr::from((Ipv4Addr::LOCALHOST, alt))]);
    // The STUN answer on the dual-stack port maps to the IPv4 address too.
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let got = stun_request(
        &udp,
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        &stun::binding_request(&[4; 12], None),
    )
    .await
    .expect("an answer");
    assert_eq!(got.mapped, Some(udp.local_addr().unwrap()));
}

#[tokio::test]
async fn wrong_alpn_fails_in_tls() {
    let server = start(local_settings()).await;
    match connect_with_alpn(&server, &key(1), b"tasty-rendezvous/2").await {
        Err(quinn::ConnectionError::ConnectionClosed(close)) => {
            // TLS alert no_application_protocol (120) as QUIC crypto error 0x100 + 120: the
            // handshake ends before any application close code.
            assert_eq!(u64::from(close.error_code), 0x100 + 120, "{close:?}");
        }
        other => panic!("expected a TLS alert, got {other:?}"),
    }
    assert_eq!(server.status().registrations_total, 0);
}

/// A subscriber that keeps each event as one line.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Line(String);

impl tracing::field::Visit for Line {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push_str(&format!(" {}={value}", field.name()));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target().starts_with("tasty_net_rendezvous") && *m.level() <= tracing::Level::INFO
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, e: &tracing::Event<'_>) {
        let mut line = Line(String::new());
        e.record(&mut line);
        self.0.lock().unwrap().push(line.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn settings_are_logged_at_start() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let mut s = local_settings();
    s.set("max_devices", Value::Count(77)).unwrap();
    let server = start(s).await;
    let lines = capture.0.lock().unwrap().clone();
    let started = lines
        .iter()
        .find(|l| l.contains("rendezvous server started"))
        .expect("a start line");
    assert!(
        started.contains(&server.public_key().to_string()),
        "{started}"
    );
    let entries = server.settings();
    let mut logged = BTreeSet::new();
    for e in &entries {
        let want = format!(" key={} value={} source={}", e.key, e.value, e.source);
        let hits = lines.iter().filter(|l| l.contains(&want)).count();
        assert_eq!(hits, 1, "{want} in {lines:#?}");
        logged.insert(e.key);
    }
    assert_eq!(logged.len(), entries.len());
    let by_key = |k: &str| entries.iter().find(|e| e.key == k).unwrap().source;
    assert_eq!(by_key("max_devices"), Source::Configured);
    assert_eq!(by_key("listen.primary"), Source::Configured);
    assert_eq!(by_key("hello_timeout"), Source::Default);
    assert_eq!(by_key("tls.resumption"), Source::Fixed);
    assert_eq!(by_key("punch_session_ttl"), Source::Computed);
    assert!(
        lines
            .iter()
            .any(|l| l.contains(" key=punch_session_ttl value=21 s source=computed"))
    );
    // Without a running connection no IP address is in the default-level lines except the
    // server's own bind addresses on the start line.
    assert!(
        lines
            .iter()
            .filter(|l| !l.contains("rendezvous server started") && !l.contains("key=listen."))
            .all(|l| !l.contains("127.0.0.1"))
    );
}

#[tokio::test]
async fn wildcard_binding_is_warned_at_start() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let warned = |lines: &[String]| lines.iter().any(|l| l.contains("wildcard address"));
    let _local = start(local_settings()).await;
    assert!(!warned(&capture.0.lock().unwrap()));
    let mut s = local_settings();
    s.set(
        "listen.primary",
        Value::Address("0.0.0.0:0".parse().unwrap()),
    )
    .unwrap();
    s.set(
        "advertise.primary_ip",
        Value::Ip("127.0.0.1".parse().unwrap()),
    )
    .unwrap();
    let _wildcard = start(s).await;
    assert!(warned(&capture.0.lock().unwrap()));
}

#[tokio::test]
async fn start_refuses_inconsistent_settings() {
    let mut s = local_settings();
    s.set("keepalive_interval", ms(45_000)).unwrap();
    let mut config = ServerConfig::new(key(100));
    config.settings = s;
    match Server::start(config).await {
        Err(StartError::Settings(e)) => assert!(e.to_string().contains("keepalive_interval")),
        other => panic!("expected a settings error, got {other:?}"),
    }
}
