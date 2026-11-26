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
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelControl,
    ConnectError, Connection, DialAuth, DomainAccept, DomainPolicy, DomainReject, Endpoint,
    EndpointHandle, HandshakeError, Holder, LinkTimeouts, MAX_HANDSHAKE_LINE, P2pCloseReason,
    RawChannel, RejectReason, Transport, TransportKind, accept_handshake, accept_p2p_handshake,
    dial_handshake, read_first_line,
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

/// A P2P stream stand-in over loopback TCP: the control of a real TCP channel (so a close
/// wakes a blocked read on every OS, through this crate's own wake path), with the close code
/// the test sets laid over it, as a QUIC stream reports the code its connection was closed
/// with.
struct CodedControl {
    inner: Arc<dyn ChannelControl>,
    reason: std::sync::Mutex<Option<P2pCloseReason>>,
}

impl CodedControl {
    fn over(stream: &TcpStream) -> Arc<Self> {
        let tcp = RawChannel::from_tcp(stream.try_clone().expect("clone")).expect("channel");
        Arc::new(Self {
            inner: tcp.control(),
            reason: std::sync::Mutex::new(None),
        })
    }
}

impl ChannelControl for CodedControl {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn shutdown_write(&self) -> io::Result<()> {
        self.inner.shutdown_write()
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        self.inner.shutdown_and_wake()
    }

    fn close_reason(&self) -> Option<P2pCloseReason> {
        *self.reason.lock().expect("reason")
    }
}

/// A P2P channel on the accepting side, its control, and the dialing socket.
fn p2p_channel() -> (RawChannel, Arc<CodedControl>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let peer = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
    let (stream, _) = listener.accept().expect("accept");
    let control = CodedControl::over(&stream);
    let channel = RawChannel::new(
        Box::new(stream.try_clone().expect("clone")),
        Box::new(stream),
        control.clone(),
        TransportKind::P2p,
    );
    (channel, control, peer)
}

fn p2p_config(max_handshakes: usize) -> AcceptConfig {
    AcceptConfig::new(
        key(2),
        "acceptor-1",
        None,
        Arc::new(BulkPolicy { take_bulk: true }),
    )
    .with_timeouts(timeouts())
    .with_max_handshakes(max_handshakes)
}

/// A cancel ends a P2P handshake still waiting for the first line, long before its deadline.
#[test]
fn a_cancel_ends_the_p2p_first_line_wait() {
    let (channel, _control, _peer) = p2p_channel();
    let cancel = AttemptCancel::new();
    let attempt = cancel.clone();
    let config = p2p_config(4);
    let started = Instant::now();
    let acceptor = std::thread::spawn(move || {
        accept_p2p_handshake(channel, key(1).id(), 1, &config, &attempt)
    });
    std::thread::sleep(Duration::from_millis(200));
    cancel.cancel();
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::Cancelled)),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// The concurrent-handshake limit is checked before the first line: a stream over the limit
/// is refused `busy` without waiting for a line it never sends.
#[test]
fn a_p2p_stream_over_the_limit_is_refused_before_its_first_line() {
    let config = p2p_config(1);
    let (first, _first_control, _first_peer) = p2p_channel();
    let cancel = AttemptCancel::new();
    let attempt = cancel.clone();
    let holder_config = config.clone();
    let holder = std::thread::spawn(move || {
        accept_p2p_handshake(first, key(1).id(), 1, &holder_config, &attempt)
    });
    std::thread::sleep(Duration::from_millis(200));
    let (second, _second_control, _second_peer) = p2p_channel();
    let started = Instant::now();
    let result = accept_p2p_handshake(second, key(1).id(), 1, &config, &AttemptCancel::new());
    assert!(
        matches!(result, Err(HandshakeError::Refused(RejectReason::Busy))),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    cancel.cancel();
    let held = holder.join().expect("holder");
    assert!(matches!(held, Err(HandshakeError::Cancelled)), "{held:?}");
}

/// A connection closed with a code while the first line is read ends the handshake with that
/// code, not a bare `PeerClosed`.
#[test]
fn a_p2p_close_during_the_first_line_reports_its_code() {
    let (channel, control, mut peer) = p2p_channel();
    let config = p2p_config(4);
    let acceptor = std::thread::spawn(move || {
        accept_p2p_handshake(channel, key(1).id(), 1, &config, &AttemptCancel::new())
    });
    io::Write::write_all(&mut peer, b"{\"type\":\"stream.op").expect("partial line");
    std::thread::sleep(Duration::from_millis(100));
    *control.reason.lock().expect("reason") = Some(P2pCloseReason::NotPaired);
    drop(peer);
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(
            result,
            Err(HandshakeError::Closed(P2pCloseReason::NotPaired))
        ),
        "{result:?}"
    );
}

/// What a recording channel's writer saw: the length of each write call, and the flushes.
#[derive(Default)]
struct Recorded {
    writes: std::sync::Mutex<Vec<usize>>,
    flushes: std::sync::atomic::AtomicUsize,
}

struct RecordingWriter(Arc<Recorded>);

impl io::Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.writes.lock().expect("writes").push(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A control that does nothing, for a channel without a socket.
struct NoSocket;

impl ChannelControl for NoSocket {
    fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn shutdown_write(&self) -> io::Result<()> {
        Ok(())
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        Ok(())
    }
}

/// O13: a frame leaves in one write of one buffer (header and payload together) followed by
/// one flush, and a TCP channel turns Nagle off, so a small frame is not held back.
#[test]
fn a_frame_is_one_write_and_one_flush_on_a_nodelay_socket() {
    let recorded = Arc::new(Recorded::default());
    let channel = RawChannel::new(
        Box::new(io::empty()),
        Box::new(RecordingWriter(Arc::clone(&recorded))),
        Arc::new(NoSocket),
        TransportKind::Memory,
    );
    let framed = channel
        .framed(&tasty_net_transport::ChannelConfig {
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
            heartbeat: tasty_net_transport::HeartbeatPolicy::WhenIdle,
            drain_cap: Duration::from_millis(300),
        })
        .expect("framed");
    framed
        .writer
        .write_frame(StreamTag::Data, &[7; 1000])
        .expect("write");
    assert_eq!(
        *recorded.writes.lock().expect("writes"),
        vec![tasty_net_frame::FRAME_HEADER_LEN + 1000]
    );
    assert_eq!(recorded.flushes.load(Ordering::SeqCst), 1);
    drop(framed);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let stream = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
    let probe = stream.try_clone().expect("clone");
    assert!(
        !probe.nodelay().expect("nodelay"),
        "the default keeps Nagle on"
    );
    let _channel = RawChannel::from_tcp(stream).expect("channel");
    assert!(probe.nodelay().expect("nodelay"));
}

/// Over TCP too, a cancel of the accepting attempt during the approval wait is `Cancelled`
/// and no domain callback runs.
#[test]
fn a_cancel_during_the_approval_wait_is_cancelled_over_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let policy = Arc::new(common::EchoPolicy::default());
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let asked = std::sync::Mutex::new(asked_tx);
    let config = AcceptConfig::new(key(2), "acceptor-1", Some(token('a')), policy.clone())
        .with_timeouts(timeouts())
        .with_role_proposals(move |proposal| {
            asked.lock().expect("lock").send(proposal).expect("asked");
        });
    let cancel = AttemptCancel::new();
    let attempt = cancel.clone();
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut channel = RawChannel::from_tcp(stream).expect("channel");
        let line = read_first_line(
            &mut channel,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("first line");
        accept_handshake(channel, &line, Vec::new(), &config, &attempt)
    });
    let dialer = std::thread::spawn(move || {
        let transport = Loopback {
            addr,
            token: token('a'),
        };
        let conn = transport
            .connect(&(), &AttemptCancel::new())
            .expect("connect");
        dial_handshake(
            conn,
            &client(),
            Holder::Acceptor,
            None,
            &AttemptCancel::new(),
        )
        .map(drop)
    });
    let proposal = asked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the proposal");
    std::thread::sleep(Duration::from_millis(120));
    cancel.cancel();
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::Cancelled)),
        "{result:?}"
    );
    proposal.accept();
    assert!(dialer.join().expect("dialer").is_err());
    assert_eq!(policy.links.load(Ordering::SeqCst), 0);
}

/// A writer whose `slow_at`-th write call (counted from 1) waits `delay` first.
struct SlowWriter {
    inner: TcpStream,
    calls: usize,
    slow_at: usize,
    delay: Duration,
}

impl io::Write for SlowWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls == self.slow_at {
            std::thread::sleep(self.delay);
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// An ACK whose write completes only after the handshake deadline does not make a link: the
/// accepting attach ends with `TimedOut` and revokes the credential it issued.
#[test]
fn an_ack_written_past_the_deadline_times_out_and_revokes_its_credential() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let policy = Arc::new(common::EchoPolicy::default());
    let config = AcceptConfig::new(key(2), "acceptor-1", Some(token('a')), policy.clone())
        .with_timeouts(LinkTimeouts {
            handshake_timeout: Duration::from_millis(800),
            ..timeouts()
        });
    let registry = config.clone();
    let acceptor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let control = CodedControl::over(&stream);
        let mut channel = RawChannel::new(
            Box::new(stream.try_clone().expect("clone")),
            // Write 1 is the challenge, write 2 the success ACK.
            Box::new(SlowWriter {
                inner: stream,
                calls: 0,
                slow_at: 2,
                delay: Duration::from_millis(1200),
            }),
            control,
            TransportKind::Tcp,
        );
        let line = read_first_line(
            &mut channel,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("first line");
        accept_handshake(channel, &line, Vec::new(), &config, &AttemptCancel::new())
    });
    let transport = Loopback {
        addr,
        token: token('a'),
    };
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let dialer = dial_handshake(conn, &client(), Holder::Dialer, None, &AttemptCancel::new());
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::TimedOut)),
        "{result:?}"
    );
    assert_eq!(
        policy.links.load(Ordering::SeqCst),
        1,
        "the domain accepted"
    );
    assert_eq!(
        registry.bulk_registry().live(),
        0,
        "the credential was revoked"
    );
    drop(dialer);
}

/// Sets its flag when dropped: a consumer's registration guard.
struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Takes every attach link and gives every bulk channel a guard that marks its drop.
struct GuardPolicy {
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl DomainPolicy for GuardPolicy {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept {
            guard: Some(Box::new(DropFlag(Arc::clone(&self.dropped)))),
            ..DomainAccept::default()
        })
    }
}

/// A writer that stalls in its first flush, after the bytes went out.
struct FlushStall {
    inner: TcpStream,
    stalled: bool,
    delay: Duration,
}

impl io::Write for FlushStall {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.stalled {
            self.stalled = true;
            std::thread::sleep(self.delay);
        }
        self.inner.flush()
    }
}

/// The bulk guard is the parent's before the success ACK goes out: a parent that ends while
/// the accepting side is still held up right after writing that ACK drops the guard at once,
/// not when the accepting side resumes.
#[test]
fn a_parent_that_ends_right_after_the_bulk_ack_drops_its_guard() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let config = AcceptConfig::new(
        key(2),
        "acceptor-1",
        Some(token('a')),
        Arc::new(GuardPolicy {
            dropped: Arc::clone(&dropped),
        }),
    )
    .with_timeouts(timeouts());
    let (parent_tx, parent_rx) = std::sync::mpsc::channel();
    let acceptor = std::thread::spawn(move || {
        let accept = |channel: RawChannel| {
            let mut channel = channel;
            let line = read_first_line(
                &mut channel,
                MAX_HANDSHAKE_LINE,
                Instant::now() + Duration::from_secs(5),
            )
            .expect("first line");
            accept_handshake(channel, &line, Vec::new(), &config, &AttemptCancel::new())
        };
        let (stream, _) = listener.accept().expect("accept parent");
        let parent = accept(RawChannel::from_tcp(stream).expect("channel"));
        parent_tx.send(parent).expect("parent");
        let (stream, _) = listener.accept().expect("accept bulk");
        let control = CodedControl::over(&stream);
        let channel = RawChannel::new(
            Box::new(stream.try_clone().expect("clone")),
            Box::new(FlushStall {
                inner: stream,
                stalled: false,
                delay: Duration::from_millis(1500),
            }),
            control,
            TransportKind::Tcp,
        );
        accept(channel).map(drop)
    });
    let transport = Loopback {
        addr,
        token: token('a'),
    };
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let dialer =
        dial_handshake(conn, &client(), Holder::Dialer, None, &AttemptCancel::new()).expect("dial");
    let parent = match parent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("parent")
    {
        Ok(Accepted::Link(link)) => link,
        other => panic!("{other:?}"),
    };
    // The dialer has the ACK while the accepting side is held in the flush after it.
    let bulk = dialer.open_bulk(None).expect("bulk");
    assert!(!dropped.load(Ordering::SeqCst));
    let ended = Instant::now();
    drop(parent);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the parent's end dropped the guard"
    );
    assert!(ended.elapsed() < Duration::from_millis(500));
    drop(bulk);
    drop(acceptor.join().expect("acceptor"));
}
