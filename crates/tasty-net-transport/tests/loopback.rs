//! Handshake v2 over real loopback TCP sockets: a test-only transport dials the acceptor,
//! and the authenticated link carries frames in order.

mod common;

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tasty_net_frame::{ReadFailure, StreamTag};
use tasty_net_identity::ConnectionToken;
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ConnectError, Connection,
    DialAuth, DomainAccept, DomainPolicy, DomainReject, Endpoint, EndpointHandle, HandshakeError,
    Holder, LinkTimeouts, MAX_HANDSHAKE_LINE, RawChannel, RejectReason, Transport, TransportKind,
    accept_handshake, dial_handshake, read_first_line,
};

use common::{client, echo_server, key, timeouts, token};

/// Plain loopback TCP to one address, with the token the test hands it.
struct Loopback {
    addr: SocketAddr,
    token: ConnectionToken,
}

struct LoopbackEndpoint {
    addr: SocketAddr,
    token: ConnectionToken,
}

fn open(addr: SocketAddr) -> Result<RawChannel, ConnectError> {
    let stream = TcpStream::connect(addr).map_err(ConnectError::Io)?;
    RawChannel::from_tcp(stream).map_err(ConnectError::Io)
}

impl Transport for Loopback {
    type Target = ();

    fn connect(&self, _target: &(), _cancel: &AttemptCancel) -> Result<Connection, ConnectError> {
        Ok(Connection {
            channel: open(self.addr)?,
            endpoint: Endpoint::new(Arc::new(LoopbackEndpoint {
                addr: self.addr,
                token: self.token.clone(),
            })),
        })
    }
}

impl EndpointHandle for LoopbackEndpoint {
    fn connect_more(&self, _cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        open(self.addr)
    }

    fn auth(&self) -> Option<DialAuth> {
        Some(DialAuth::Token(self.token.clone()))
    }
}

#[test]
fn a_loopback_link_echoes_and_opens_a_bulk_channel() {
    let server = echo_server(token('a'));
    let transport = Loopback {
        addr: server.addr,
        token: token('a'),
    };
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&(), &cancel).expect("connect");
    let mut link =
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    assert_eq!(link.client_id(), Some(1));
    assert_eq!(link.peer().transport(), TransportKind::Tcp);
    assert_eq!(server.policy.links.load(Ordering::SeqCst), 1);

    let (mut reader, writer) = link.take_ordered().expect("ordered");
    writer
        .write_frame(StreamTag::Data, b"hello")
        .expect("write");
    let frame = reader.read_frame().expect("echo");
    assert_eq!(
        (frame.tag, frame.payload.as_slice()),
        (StreamTag::Data, &b"hello"[..])
    );

    let mut bulk = link.open_bulk(None).expect("bulk");
    bulk.writer
        .write_frame(StreamTag::Data, b"bulk")
        .expect("write");
    let frame = bulk.reader.read_frame().expect("bulk echo");
    assert_eq!(frame.payload, b"bulk");
}

#[test]
fn a_wrong_token_is_refused_over_tcp() {
    let server = echo_server(token('a'));
    let transport = Loopback {
        addr: server.addr,
        token: token('b'),
    };
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&(), &cancel).expect("connect");
    match dial_handshake(conn, &client(), Holder::Dialer, None, &cancel) {
        Err(HandshakeError::Rejected { reason, .. }) => {
            assert_eq!(reason, RejectReason::TokenInvalid);
        }
        other => panic!("expected a token refusal: {other:?}"),
    }
    assert_eq!(server.policy.links.load(Ordering::SeqCst), 0);
}

/// 10,000 frames of mixed tags come back in the order they were written, on a link that
/// passed the handshake (docs/design/transport.md, the order contract of the ordered channel).
#[test]
fn a_handshaken_link_keeps_the_order_of_mixed_tag_frames() {
    const FRAMES: u32 = 10_000;
    let server = echo_server(token('a'));
    let transport = Loopback {
        addr: server.addr,
        token: token('a'),
    };
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&(), &cancel).expect("connect");
    let mut link =
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let (mut reader, writer) = link.take_ordered().expect("ordered");
    let tag = |i: u32| match i % 3 {
        0 => StreamTag::Data,
        1 => StreamTag::Control,
        _ => StreamTag::MeshData,
    };
    let sender = std::thread::spawn(move || -> io::Result<()> {
        for i in 0..FRAMES {
            writer.write_frame(tag(i), &i.to_be_bytes())?;
        }
        Ok(())
    });
    for i in 0..FRAMES {
        let frame = reader.read_frame().expect("frame");
        assert_eq!(frame.tag, tag(i), "frame {i}");
        assert_eq!(frame.payload, i.to_be_bytes(), "frame {i}");
    }
    sender.join().expect("sender").expect("write");
}

/// Takes every attach link and refuses or takes every bulk channel.
struct BulkPolicy {
    take_bulk: bool,
}

impl DomainPolicy for BulkPolicy {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        if self.take_bulk {
            Ok(DomainAccept::default())
        } else {
            Err(DomainReject::new("no_bulk"))
        }
    }
}

/// Each accepting handshake's result with how long it took.
type Timed = Vec<(Result<Accepted, HandshakeError>, Duration)>;

/// Accepts `n` channels on a loopback listener with `policy` and the given drain cap, and
/// returns each result with how long its accepting handshake took.
fn accept_n(
    take_bulk: bool,
    drain_cap: Duration,
    n: usize,
) -> (SocketAddr, std::thread::JoinHandle<Timed>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let config = AcceptConfig::new(
        key(2),
        "acceptor-1",
        Some(token('a')),
        Arc::new(BulkPolicy { take_bulk }),
    )
    .with_timeouts(LinkTimeouts {
        drain_cap,
        ..timeouts()
    });
    let handle = std::thread::spawn(move || {
        (0..n)
            .map(|_| {
                let (stream, _) = listener.accept().expect("accept");
                let mut channel = RawChannel::from_tcp(stream).expect("channel");
                let line = read_first_line(
                    &mut channel,
                    MAX_HANDSHAKE_LINE,
                    Instant::now() + Duration::from_secs(5),
                )
                .expect("first line");
                let started = Instant::now();
                let result =
                    accept_handshake(channel, &line, Vec::new(), &config, &AttemptCancel::new());
                (result, started.elapsed())
            })
            .collect()
    });
    (addr, handle)
}

/// Dropping a bulk channel closes its socket at once, while the parent link lives on: the
/// peer sees `PeerClosed`, not a heartbeat timeout after the read timeout.
#[test]
fn a_dropped_bulk_channel_closes_its_socket_under_a_live_parent() {
    let (addr, acceptor) = accept_n(true, Duration::from_millis(500), 2);
    let transport = Loopback {
        addr,
        token: token('a'),
    };
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&(), &cancel).expect("connect");
    let link = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let bulk = link.open_bulk(None).expect("bulk");
    let mut accepted = acceptor.join().expect("acceptor").into_iter();
    let _parent = accepted.next().expect("parent").0.expect("parent link");
    let mut far_bulk = match accepted.next().expect("bulk").0 {
        Ok(Accepted::Bulk(bulk)) => bulk,
        other => panic!("{other:?}"),
    };
    drop(bulk);
    let started = Instant::now();
    let end = far_bulk
        .reader
        .read_frame()
        .expect_err("the bulk channel ended");
    assert_eq!(end.failure(), ReadFailure::PeerClosed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "ended after {:?}, the read timeout is 10 s",
        started.elapsed()
    );
    drop(link);
}

/// A refused bulk attempt closes the dialer's socket, so the accepting end's linger after the
/// refusal ends on that close instead of waiting out the drain cap.
#[test]
fn a_refused_bulk_attempt_does_not_hold_the_acceptor_for_the_drain_cap() {
    let drain_cap = Duration::from_secs(3);
    let (addr, acceptor) = accept_n(false, drain_cap, 2);
    let transport = Loopback {
        addr,
        token: token('a'),
    };
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&(), &cancel).expect("connect");
    let link = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    match link.open_bulk(None) {
        Err(HandshakeError::Rejected { reason, .. }) => assert_eq!(reason, RejectReason::Domain),
        other => panic!("{other:?}"),
    }
    let accepted = acceptor.join().expect("acceptor");
    let (refused, took) = &accepted[1];
    assert!(
        matches!(refused, Err(HandshakeError::Refused(RejectReason::Domain))),
        "{refused:?}"
    );
    assert!(
        *took < Duration::from_millis(1500),
        "the refusal took {took:?} with a {drain_cap:?} drain cap"
    );
    drop(link);
}
