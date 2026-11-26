//! Handshake v2 over real loopback TCP sockets: a test-only transport dials the acceptor,
//! and the authenticated link carries frames in order.

mod common;

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tasty_net_frame::StreamTag;
use tasty_net_identity::ConnectionToken;
use tasty_net_transport::{
    AttemptCancel, ConnectError, Connection, DialAuth, Endpoint, EndpointHandle, HandshakeError,
    Holder, RawChannel, RejectReason, Transport, TransportKind, dial_handshake,
};

use common::{client, echo_server, token};

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
