//! Channels over the P2P adapter (docs/design/transport.md "채널 대응", "P2P의 닫기 범위"):
//! two real P2P endpoints on loopback OS sockets and a rendezvous server in this process.
//!
//! Every test uses its own device keys: one key has one P2P endpoint in a process, and the
//! tests of this file run in parallel.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tasty_net_frame::{ReadFailure, StreamFrame, StreamTag};
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::{P2pCloseReason, P2pConfig, P2pConnectError, P2pEndpoint, PairingList};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelReader,
    ChannelWriter, ConnectError, DialConfig, DomainAccept, DomainPolicy, DomainReject, EndReason,
    HandshakeError, Holder, Link, LinkTimeouts, P2pListener, P2pTransport, PeerInfo, RejectReason,
    RequestHandler, Transport, dial_handshake, dial_request, domain_code,
};

/// How long a test waits for something that comes in well under a second on loopback.
const PATIENCE: Duration = Duration::from_secs(20);

/// A rendezvous server on loopback OS sockets.
struct Rendezvous {
    rt: tokio::runtime::Runtime,
    server: Option<Server>,
    info: tasty_net_p2p::RendezvousServer,
}

impl Rendezvous {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut settings = ServerSettings::new();
        settings
            .set(
                "listen.primary",
                ServerValue::Address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            )
            .expect("primary");
        settings
            .set("listen.alternate_port", ServerValue::Port(0))
            .expect("alternate");
        let mut config = ServerConfig::new(DeviceKey::from_seed([250; 32]));
        config.settings = settings;
        let server = rt.block_on(Server::start(config)).expect("server");
        let info = tasty_net_p2p::RendezvousServer {
            addr: server.addrs().primary,
            public_key: server.public_key(),
        };
        Self {
            rt,
            server: Some(server),
            info,
        }
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            self.rt.block_on(server.shutdown());
        }
    }
}

fn key(seed: u8) -> DeviceKey {
    DeviceKey::from_seed([seed; 32])
}

fn timeouts() -> LinkTimeouts {
    LinkTimeouts {
        read_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
        role_proposal_timeout: Duration::from_millis(500),
        drain_cap: Duration::from_millis(500),
    }
}

fn endpoint(server: &Rendezvous, seed: u8, peer: DeviceId) -> P2pEndpoint {
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(key(seed), pairing);
    config.enabled = true;
    config.server = Some(server.info);
    P2pEndpoint::bind(config).expect("bind")
}

/// What the accepting side's domain check does: everything is taken, except an attach whose
/// `ext` is `"refuse"`.
#[derive(Default)]
struct Policy {
    links: AtomicUsize,
}

impl DomainPolicy for Policy {
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        if request.ext() == Some("\"refuse\"") {
            return Err(DomainReject::new("test_refused"));
        }
        self.links.fetch_add(1, Ordering::SeqCst);
        Ok(DomainAccept::default())
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }
}

/// Answers every request with its params.
struct Echo;

impl RequestHandler for Echo {
    fn handle(&self, _peer: &PeerInfo, _method: &str, params: Value) -> Result<Value, Value> {
        Ok(params)
    }
}

/// What the accepting side does with an attach link.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Writes every frame back; at the peer's end it ends its own side.
    Echo,
    /// Writes its last frame and ends its side first, waits, and only then reads what the
    /// dialer sent; the frames go to the test.
    LastFrameFirst,
}

/// Two paired endpoints, a dialing transport on `a` and an accepting side on `b`.
struct Pair {
    _server: Rendezvous,
    a: P2pEndpoint,
    b: P2pEndpoint,
    transport: P2pTransport,
    dial: DialConfig,
    policy: Arc<Policy>,
    /// The frames the accepting side read in [`Mode::LastFrameFirst`], and how its read ended.
    read: mpsc::Receiver<(Vec<StreamFrame>, Option<ReadFailure>)>,
    stop: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
    served: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Pair {
    fn start(seed: u8, mode: Mode) -> Self {
        let server = Rendezvous::start();
        let (ka, kb) = (key(seed), key(seed + 1));
        let a = endpoint(&server, seed, kb.id());
        let b = endpoint(&server, seed + 1, ka.id());
        let policy = Arc::new(Policy::default());
        let accept = AcceptConfig::new(kb, format!("acceptor-{seed}"), None, policy.clone())
            .with_timeouts(timeouts());
        let listener = P2pListener::new(b.clone(), accept).expect("listener");
        let stop = Arc::new(AtomicBool::new(false));
        let served: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (read_tx, read) = mpsc::channel();
        let accepting = {
            let stop = Arc::clone(&stop);
            let served = Arc::clone(&served);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(inbound) = listener.accept_timeout(Some(Duration::from_millis(100)))
                    else {
                        continue;
                    };
                    let read_tx = read_tx.clone();
                    let thread = std::thread::spawn(move || {
                        serve(inbound.handshake(&AttemptCancel::new()), mode, &read_tx);
                    });
                    served.lock().expect("served").push(thread);
                }
            })
        };
        Self {
            _server: server,
            transport: P2pTransport::new(a.clone()),
            a,
            b,
            dial: DialConfig::new(ka, format!("dialer-{seed}")).with_timeouts(timeouts()),
            policy,
            read,
            stop,
            accepting: Some(accepting),
            served,
        }
    }

    /// Dials an attach link with `ext`, retrying while the peer is not yet registered.
    fn link(&self, ext: Option<&str>) -> Result<Link, HandshakeError> {
        let started = Instant::now();
        loop {
            let cancel = AttemptCancel::new();
            let result = self
                .transport
                .connect(&self.b.id(), &cancel)
                .map_err(HandshakeError::Connect)
                .and_then(|conn| dial_handshake(conn, &self.dial, Holder::Dialer, ext, &cancel));
            match result {
                Err(HandshakeError::Connect(ConnectError::P2p(e)))
                    if started.elapsed() < PATIENCE
                        && matches!(
                            e.connect_error(),
                            Some(
                                P2pConnectError::PeerNotFound
                                    | P2pConnectError::RendezvousUnavailable { .. }
                            )
                        ) =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                other => return other,
            }
        }
    }

    /// How many connections the endpoints track (with attempts in progress).
    fn connections(&self) -> (usize, usize) {
        (self.a.connections().len(), self.b.connections().len())
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.a.close();
        self.b.close();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accepting) = self.accepting.take() {
            accepting.join().expect("accept thread");
        }
        for thread in std::mem::take(&mut *self.served.lock().expect("served")) {
            thread.join().expect("serving thread");
        }
    }
}

fn serve(
    accepted: Result<Accepted, HandshakeError>,
    mode: Mode,
    read_tx: &mpsc::Sender<(Vec<StreamFrame>, Option<ReadFailure>)>,
) {
    match accepted {
        Ok(Accepted::Link(mut link)) => {
            let Some((reader, writer)) = link.take_ordered() else {
                return;
            };
            match mode {
                Mode::Echo => echo(reader, writer),
                Mode::LastFrameFirst => last_frame_first(reader, writer, read_tx),
            }
        }
        Ok(Accepted::Bulk(bulk)) => echo(bulk.reader, bulk.writer),
        Ok(Accepted::Request(server)) => drop(server.serve(&Echo)),
        Err(_) => {}
    }
}

/// Writes every frame back; at the reader's end ends the sending side too.
fn echo(mut reader: ChannelReader, mut writer: ChannelWriter) {
    while let Ok(frame) = reader.read_frame() {
        if writer.write_frame(frame.tag, &frame.payload).is_err() {
            return;
        }
    }
    drop(writer.half_close());
}

fn last_frame_first(
    mut reader: ChannelReader,
    mut writer: ChannelWriter,
    read_tx: &mpsc::Sender<(Vec<StreamFrame>, Option<ReadFailure>)>,
) {
    writer
        .write_frame(StreamTag::Detach, b"acceptor-last")
        .expect("the acceptor's last frame");
    writer.half_close().expect("half close");
    // The dialer has read the acceptor's last frame and ended its side by now; the acceptor's
    // EOF must still wait for this side to read the dialer's frames.
    std::thread::sleep(Duration::from_millis(300));
    let mut frames = Vec::new();
    let end = loop {
        match reader.read_frame() {
            Ok(frame) => frames.push(frame),
            Err(e) => break Some(e.failure()),
        }
    };
    drop(read_tx.send((frames, end)));
}

fn tag(i: u32) -> StreamTag {
    [
        StreamTag::Data,
        StreamTag::Control,
        StreamTag::MeshData,
        StreamTag::Data,
        StreamTag::Detach,
    ][i as usize % 5]
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "{what} did not happen");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An attach link with its ordered halves taken.
struct Open {
    reader: ChannelReader,
    writer: ChannelWriter,
    /// Kept so the link lives as long as its halves are used.
    _link: Link,
}

impl Open {
    fn new(mut link: Link) -> Self {
        let (reader, writer) = link.take_ordered().expect("halves");
        Self {
            reader,
            writer,
            _link: link,
        }
    }

    /// Writes `payload` and reads it back from the echo side.
    fn round_trip(&mut self, payload: &[u8]) {
        self.writer
            .write_frame(StreamTag::Data, payload)
            .expect("write");
        assert_eq!(self.reader.read_frame().expect("echo").payload, payload);
    }
}

/// Frames of mixed tags on one stream arrive in the order they were written, and the end
/// comes after the last of them (docs/design/transport.md "순서 계약과 채널").
#[test]
fn p2p_ordered_stream_preserves_mixed_tag_order() {
    let pair = Pair::start(10, Mode::Echo);
    let mut link = pair.link(None).expect("link");
    let (mut reader, mut writer) = link.take_ordered().expect("halves");
    const N: u32 = 2000;
    let sender = std::thread::spawn(move || {
        for i in 0..N {
            writer.write_frame(tag(i), &i.to_be_bytes()).expect("write");
        }
        writer.half_close().expect("half close");
        writer
    });
    for i in 0..N {
        let frame = reader.read_frame().expect("frame");
        assert_eq!(frame.tag, tag(i), "frame {i}");
        assert_eq!(frame.payload, i.to_be_bytes(), "frame {i}");
    }
    let end = reader.read_frame().expect_err("EOF after the last frame");
    assert_eq!(end.failure(), ReadFailure::PeerClosed);
    drop(sender.join().expect("sender"));
}

/// A refusing ACK on the first stream of a new connection reaches the dialer, which then
/// closes the connection it made: no link was shared on it.
#[test]
fn rejected_first_stream_ack_is_read() {
    let pair = Pair::start(12, Mode::Echo);
    match pair.link(Some("\"refuse\"")) {
        Err(HandshakeError::Rejected { reason, ext, .. }) => {
            assert_eq!(reason, RejectReason::Domain);
            assert_eq!(
                ext.as_deref().and_then(domain_code).as_deref(),
                Some("test_refused")
            );
        }
        other => panic!("expected the refusing ACK, got {other:?}"),
    }
    assert_eq!(pair.transport.live_connections(), 0);
    wait_until("the connection's close", || pair.connections() == (0, 0));
}

/// Both sides send a last frame and end their side; each application reads the other's last
/// frame. The acceptor ends its side first and reads late: its EOF waits until it read the
/// dialer's end, so the dialer's close of the connection drops nothing.
#[test]
fn bidirectional_last_frames_are_read() {
    let pair = Pair::start(14, Mode::LastFrameFirst);
    let mut link = pair.link(None).expect("link");
    let (mut reader, mut writer) = link.take_ordered().expect("halves");
    writer
        .write_frame(StreamTag::Detach, b"dialer-last")
        .expect("the dialer's last frame");
    writer.half_close().expect("half close");
    let last = reader.read_frame().expect("the acceptor's last frame");
    assert_eq!(
        (last.tag, last.payload.as_slice()),
        (StreamTag::Detach, &b"acceptor-last"[..])
    );
    let end = reader.read_frame().expect_err("EOF");
    assert_eq!(end.failure(), ReadFailure::PeerClosed);
    assert_eq!(end.io_kind(), std::io::ErrorKind::UnexpectedEof);
    // The last channel ends: the dialer closes the connection.
    drop((reader, writer, link));
    let (frames, ended) = pair
        .read
        .recv_timeout(PATIENCE)
        .expect("the acceptor's reads");
    assert_eq!(frames.len(), 1, "the acceptor read the dialer's last frame");
    assert_eq!(frames[0].payload, b"dialer-last");
    assert_eq!(ended, Some(ReadFailure::PeerClosed));
    wait_until("the connection's close", || pair.connections() == (0, 0));
}

/// Close scope, first row: a refusing ACK closes that stream only; the link already on the
/// connection keeps working, on the same connection.
#[test]
fn a_refused_stream_leaves_the_other_links() {
    let pair = Pair::start(16, Mode::Echo);
    let mut first = Open::new(pair.link(None).expect("first link"));
    assert!(matches!(
        pair.link(Some("\"refuse\"")),
        Err(HandshakeError::Rejected {
            reason: RejectReason::Domain,
            ..
        })
    ));
    first.round_trip(b"still here");
    assert_eq!(pair.transport.live_connections(), 1);
    assert_eq!(pair.connections(), (1, 1), "one QUIC connection");
}

/// Close scope, second row: a request channel's end closes its stream only.
#[test]
fn closing_a_request_channel_leaves_the_link() {
    let pair = Pair::start(18, Mode::Echo);
    let mut link = Open::new(pair.link(None).expect("link"));
    let cancel = AttemptCancel::new();
    let conn = pair
        .transport
        .connect(&pair.b.id(), &cancel)
        .expect("request stream");
    let client = dial_request(conn, &pair.dial, &cancel).expect("request channel");
    let answer = client
        .call("echo", json!({"n": 1}), Duration::from_secs(10))
        .expect("answer");
    assert_eq!(answer, json!({"n": 1}));
    drop(client);
    link.round_trip(b"after the request channel");
    assert_eq!(pair.connections(), (1, 1), "one QUIC connection");
}

/// Close scope, third row: a link's normal end closes its bulk channels by force, and the
/// other link on the connection stays.
#[test]
fn a_link_normal_end_closes_its_bulk_channels_only() {
    let pair = Pair::start(20, Mode::Echo);
    let mut first = pair.link(None).expect("first link");
    let mut second = Open::new(pair.link(None).expect("second link"));
    let mut bulk = first.open_bulk(None).expect("bulk");
    bulk.writer
        .write_frame(StreamTag::Data, b"bulk")
        .expect("bulk write");
    assert_eq!(
        bulk.reader.read_frame().expect("bulk echo").payload,
        b"bulk"
    );
    let (mut reader, mut writer) = first.take_ordered().expect("halves");
    // The normal end of the first link: both sides end and the reader read the end.
    writer.half_close().expect("half close");
    assert_eq!(
        reader.read_frame().expect_err("EOF").failure(),
        ReadFailure::PeerClosed
    );
    reader.finish();
    let failed = bulk
        .reader
        .read_frame()
        .expect_err("the bulk channel closed");
    assert_eq!(failed.failure(), ReadFailure::PeerClosed);
    second.round_trip(b"the other link");
    drop((reader, writer, first, bulk));
    second.round_trip(b"still the other link");
}

/// Close scope, fourth row: a forced close (and a dropped link) closes that link's stream
/// only.
#[test]
fn a_forced_close_leaves_the_other_links() {
    let pair = Pair::start(22, Mode::Echo);
    let mut first = pair.link(None).expect("first link");
    let mut second = Open::new(pair.link(None).expect("second link"));
    let third = pair.link(None).expect("third link");
    let (mut reader, writer) = first.take_ordered().expect("halves");
    first.closer().force_close(EndReason::Forced);
    assert!(
        reader.read_frame().is_err(),
        "the forced close woke the reader"
    );
    drop((reader, writer, first));
    second.round_trip(b"after the forced close");
    drop(third);
    second.round_trip(b"after the drop");
    assert_eq!(pair.connections(), (1, 1), "one QUIC connection");
}

/// Close scope, fifth row: a connection the accepting side closes with `not_paired` during
/// the handshake ends the handshake with that code (the accepting endpoint does it when the
/// peer is not on its pairing list; here the test closes the connection with the code).
#[test]
fn a_not_paired_close_ends_the_handshake_with_its_code() {
    let server = Rendezvous::start();
    let (ka, kb) = (key(24), key(25));
    let a = endpoint(&server, 24, kb.id());
    let b = endpoint(&server, 25, ka.id());
    let closer = {
        let b = b.clone();
        std::thread::spawn(move || {
            let conn = b
                .accept_timeout(Some(PATIENCE))
                .expect("the dialer's connection");
            // Closed once the dialer's handshake runs on its first stream.
            let stream = conn.accept_bi(Some(PATIENCE)).expect("the first stream");
            conn.close(P2pCloseReason::NotPaired);
            drop(stream);
        })
    };
    let transport = P2pTransport::new(a.clone());
    let dial = DialConfig::new(ka, "dialer-24").with_timeouts(timeouts());
    let started = Instant::now();
    let error = loop {
        let cancel = AttemptCancel::new();
        let result = transport
            .connect(&b.id(), &cancel)
            .map_err(HandshakeError::Connect)
            .and_then(|conn| dial_handshake(conn, &dial, Holder::Dialer, None, &cancel));
        match result {
            Err(HandshakeError::Connect(ConnectError::P2p(_))) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => break error,
            Ok(_) => panic!("a link on a closed connection"),
        }
    };
    assert!(
        matches!(error, HandshakeError::Closed(P2pCloseReason::NotPaired)),
        "{error:?}"
    );
    assert!(!error.is_retryable());
    closer.join().expect("closer");
    a.close();
    b.close();
}

/// Close scope, sixth row: when the last link and channel on the connection end, the dialer
/// closes the QUIC connection (`P2pConnection::close`, code 0) while both endpoints stay up;
/// it does not wait for the idle timeout.
#[test]
fn the_last_channel_closes_the_connection() {
    let pair = Pair::start(26, Mode::Echo);
    let mut link = Open::new(pair.link(None).expect("link"));
    let cancel = AttemptCancel::new();
    let conn = pair
        .transport
        .connect(&pair.b.id(), &cancel)
        .expect("request stream");
    let client = dial_request(conn, &pair.dial, &cancel).expect("request channel");
    link.round_trip(b"one");
    assert_eq!(pair.connections(), (1, 1));
    drop(link);
    // The request channel still holds the connection.
    assert_eq!(
        client
            .call("echo", json!("two"), Duration::from_secs(10))
            .expect("answer"),
        json!("two")
    );
    assert_eq!(pair.connections(), (1, 1));
    drop(client);
    wait_until("the connection's close", || pair.connections() == (0, 0));
    assert_eq!(pair.transport.live_connections(), 0);
    assert!(pair.a.is_enabled() && pair.b.is_enabled());
    // A new link makes a new connection.
    let mut again = Open::new(pair.link(None).expect("a new link"));
    again.round_trip(b"three");
}

/// Streams are opened only by the side that opened the connection: a stream the accepting
/// side opens is refused.
#[test]
fn a_stream_the_acceptor_opens_is_refused() {
    let server = Rendezvous::start();
    let (ka, kb) = (key(28), key(29));
    let a = endpoint(&server, 28, kb.id());
    let b = endpoint(&server, 29, ka.id());
    let transport = P2pTransport::new(a.clone());
    let started = Instant::now();
    let conn = loop {
        match transport.connect(&b.id(), &AttemptCancel::new()) {
            Ok(conn) => break conn,
            Err(ConnectError::P2p(_)) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("connect: {error:?}"),
        }
    };
    let accepted = b.accept_timeout(Some(PATIENCE)).expect("accepted");
    let mut stream = accepted.open_bi().expect("open");
    stream.set_read_timeout(Some(Duration::from_secs(10)));
    use std::io::{Read, Write};
    let mut byte = [0u8; 1];
    let refused = match stream.write_all(b"from the acceptor") {
        Err(error) => error,
        Ok(()) => stream
            .read(&mut byte)
            .expect_err("the dialer refused the stream"),
    };
    assert!(
        matches!(
            refused.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        ),
        "{refused:?}"
    );
    drop((stream, conn, accepted));
    a.close();
    b.close();
}

/// The target is this device's own ID: the adapter ends it as a self attach before anything
/// is sent.
#[test]
fn connecting_to_this_device_is_a_self_attach() {
    let server = Rendezvous::start();
    let a = endpoint(&server, 30, key(31).id());
    let transport = P2pTransport::new(a.clone());
    let error = transport
        .connect(&a.id(), &AttemptCancel::new())
        .expect_err("self");
    assert!(matches!(error, ConnectError::SelfAttach), "{error:?}");
    assert!(!HandshakeError::Connect(error).is_retryable());
    a.close();
}

/// A link that stands when the peer closes the whole connection (its endpoint going off)
/// ends with a peer close, and the link shows the code the peer sent.
#[test]
fn a_link_shows_the_code_its_connection_was_closed_with() {
    let pair = Pair::start(32, Mode::Echo);
    let mut link = pair.link(None).expect("link");
    let (mut reader, _writer) = link.take_ordered().expect("halves");
    assert_eq!(link.close_reason(), None);
    pair.b.close();
    let failed = reader.read_frame().expect_err("the connection closed");
    assert_eq!(failed.failure(), ReadFailure::PeerClosed);
    assert_eq!(link.close_reason(), Some(P2pCloseReason::Closed));
}

/// A bulk channel carries megabytes on the same connection.
#[test]
fn a_bulk_channel_carries_megabytes() {
    let pair = Pair::start(34, Mode::Echo);
    let link = pair.link(None).expect("link");
    let bulk = link.open_bulk(None).expect("bulk");
    let sent: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut writer = bulk.writer;
    let mut reader = bulk.reader;
    let data = sent.clone();
    let sender = std::thread::spawn(move || {
        for chunk in data.chunks(64 * 1024) {
            writer.write_frame(StreamTag::Data, chunk).expect("write");
        }
        writer.half_close().expect("half close");
        writer
    });
    let mut got = Vec::with_capacity(sent.len());
    while let Ok(frame) = reader.read_frame() {
        got.extend_from_slice(&frame.payload);
    }
    assert_eq!(got.len(), sent.len());
    assert!(got == sent, "the bytes match");
    drop(sender.join().expect("sender"));
    assert_eq!(pair.policy.links.load(Ordering::SeqCst), 1);
}
