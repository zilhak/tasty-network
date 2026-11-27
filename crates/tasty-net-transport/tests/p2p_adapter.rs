//! Channels over the P2P adapter (docs/design/transport.md "채널 대응", "P2P의 닫기 범위"):
//! two real P2P endpoints on loopback OS sockets and a rendezvous server in this process.
//!
//! Every test uses its own device keys: one key has one P2P endpoint in a process, and the
//! tests of this file run in parallel.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tasty_net_frame::{ReadFailure, StreamFrame, StreamTag};
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::settings::Value as P2pValue;
use tasty_net_p2p::{
    EndpointPhase, P2pCloseReason, P2pConfig, P2pConnectError, P2pEndpoint, PairingList,
};
use tasty_net_rendezvous::packet_io::{PacketIo, UdpPacketIo};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};
use tasty_net_transport::settings::SettingSource;
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, Cause, ChannelReader,
    ChannelWriter, ConnectError, ConnectOptions, Connection, ConnectionEvent, ConnectionId,
    ConnectionInfo, ConnectionState, ConnectionTable, DialConfig, DomainAccept, DomainPolicy,
    DomainReject, EdgeTransport, EndReason, HandshakeError, Holder, Link, LinkTimeouts,
    P2pListener, P2pTransport, PathKind, PeerInfo, Phase, RejectReason, RequestHandler, Transport,
    Via, dial_handshake, dial_request, domain_code,
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
    endpoint_with(server, seed, peer, None, None)
}

/// [`endpoint`] on `gate` instead of an OS socket of its own, with short QUIC timers and a
/// short `path.poll_interval` (how often the adapter copies an attempt to its entry); with
/// `streams`, the peer may open at most that many streams at a time on its connections.
fn endpoint_with(
    server: &Rendezvous,
    seed: u8,
    peer: DeviceId,
    gate: Option<Arc<Gate>>,
    streams: Option<u64>,
) -> P2pEndpoint {
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(key(seed), pairing);
    config.enabled = true;
    config.server = Some(server.info);
    if let Some(gate) = gate {
        config.packet_io = Some(gate);
        config
            .settings
            .set(
                "quic.keepalive_interval",
                P2pValue::Duration(Duration::from_millis(300)),
            )
            .set("quic.idle_timeout", P2pValue::Duration(IDLE_TIMEOUT))
            .set(
                "path.poll_interval",
                P2pValue::Duration(Duration::from_millis(100)),
            );
    }
    if let Some(streams) = streams {
        config
            .settings
            .set("quic.max_bidi_streams", P2pValue::Count(streams));
    }
    P2pEndpoint::bind(config).expect("bind")
}

/// The QUIC idle timeout of the endpoints of [`a_silent_peer_ends_the_link_at_the_idle_timeout`].
const IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Packet I/O over a loopback UDP socket that can be shut: once shut it sends and takes
/// nothing, as a device that went away without a close.
#[derive(Debug)]
struct Gate {
    inner: UdpPacketIo,
    shut: AtomicBool,
}

impl Gate {
    fn bind(server: &Rendezvous) -> Arc<Self> {
        let inner = server
            .rt
            .block_on(UdpPacketIo::bind(SocketAddr::from((
                Ipv4Addr::LOCALHOST,
                0,
            ))))
            .expect("udp");
        Arc::new(Self {
            inner,
            shut: AtomicBool::new(false),
        })
    }

    fn is_shut(&self) -> bool {
        self.shut.load(Ordering::SeqCst)
    }
}

impl PacketIo for Gate {
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        loop {
            match self.inner.poll_recv_from(cx, buf) {
                Poll::Ready(Ok(_)) if self.is_shut() => {}
                other => return other,
            }
        }
    }

    fn poll_recv_with_dst(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr, Option<IpAddr>)>> {
        loop {
            match self.inner.poll_recv_with_dst(cx, buf) {
                Poll::Ready(Ok(_)) if self.is_shut() => {}
                other => return other,
            }
        }
    }

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_send_ready(cx)
    }

    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.is_shut() {
            return Ok(());
        }
        self.inner.try_send_to(buf, dest)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }

    fn dual_stack(&self) -> bool {
        self.inner.dual_stack()
    }
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
    transport: Arc<P2pTransport>,
    dial: DialConfig,
    policy: Arc<Policy>,
    /// The connection tables of `a` (the dialer) and `b`.
    table_a: ConnectionTable,
    table_b: ConnectionTable,
    /// The frames the accepting side read in [`Mode::LastFrameFirst`], and how its read ended.
    read: mpsc::Receiver<(Vec<StreamFrame>, Option<ReadFailure>)>,
    stop: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
    served: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Pair {
    fn start(seed: u8, mode: Mode) -> Self {
        Self::start_with(seed, mode, false).0
    }

    /// [`Pair::start`]; with `gated`, both endpoints run with short QUIC timers and `b` runs on
    /// the returned gate.
    fn start_with(seed: u8, mode: Mode, gated: bool) -> (Self, Option<Arc<Gate>>) {
        Self::start_full(seed, mode, gated, None)
    }

    /// [`Pair::start`] where `a` may open at most `streams` streams at a time to `b`.
    fn start_limited(seed: u8, mode: Mode, streams: u64) -> Self {
        Self::start_full(seed, mode, false, Some(streams)).0
    }

    fn start_full(
        seed: u8,
        mode: Mode,
        gated: bool,
        streams: Option<u64>,
    ) -> (Self, Option<Arc<Gate>>) {
        let server = Rendezvous::start();
        let (ka, kb) = (key(seed), key(seed + 1));
        let gate = gated.then(|| Gate::bind(&server));
        let (a, b) = if gated {
            (
                endpoint_with(&server, seed, kb.id(), Some(Gate::bind(&server)), None),
                endpoint_with(&server, seed + 1, ka.id(), gate.clone(), streams),
            )
        } else {
            (
                endpoint(&server, seed, kb.id()),
                endpoint_with(&server, seed + 1, ka.id(), None, streams),
            )
        };
        let policy = Arc::new(Policy::default());
        let (table_a, table_b) = (ConnectionTable::default(), ConnectionTable::default());
        let accept = AcceptConfig::new(kb, format!("acceptor-{seed}"), None, policy.clone())
            .with_timeouts(timeouts())
            .with_connections(table_b.clone());
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
        let pair = Self {
            _server: server,
            transport: Arc::new(P2pTransport::new(a.clone()).with_connections(table_a.clone())),
            a,
            b,
            dial: DialConfig::new(ka, format!("dialer-{seed}")).with_timeouts(timeouts()),
            policy,
            table_a,
            table_b,
            read,
            stop,
            accepting: Some(accepting),
            served,
        };
        (pair, gate)
    }

    /// Dials an attach link with `ext`, retrying while the peer is not yet registered.
    fn link(&self, ext: Option<&str>) -> Result<Link, HandshakeError> {
        self.link_with(ext, &ConnectOptions::new())
    }

    /// [`Pair::link`] whose entry follows `options`.
    fn link_with(
        &self,
        ext: Option<&str>,
        options: &ConnectOptions,
    ) -> Result<Link, HandshakeError> {
        let cancel = AttemptCancel::new();
        let conn = self
            .connect(&cancel, options)
            .map_err(HandshakeError::Connect)?;
        dial_handshake(conn, &self.dial, Holder::Dialer, ext, &cancel)
    }

    /// A channel to `b` without the handshake, once `b` is registered.
    fn connect(
        &self,
        cancel: &AttemptCancel,
        options: &ConnectOptions,
    ) -> Result<Connection, ConnectError> {
        let started = Instant::now();
        loop {
            match self.transport.connect_with(&self.b.id(), cancel, options) {
                Err(ConnectError::P2p(e))
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

/// The entries of `table` that are not `disconnected` (a dial retried while the peer was not
/// registered leaves ended ones).
fn live(table: &ConnectionTable) -> Vec<ConnectionInfo> {
    table
        .connections()
        .into_iter()
        .filter(|e| e.state != ConnectionState::Disconnected)
        .collect()
}

fn entry(table: &ConnectionTable, id: ConnectionId) -> ConnectionInfo {
    table.connection(id).expect("the entry is kept")
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
    // The entry was never `connected`: it ends with the refusal, not covered by the QUIC
    // connection having been up.
    let ended = pair
        .table_a
        .connections()
        .last()
        .cloned()
        .expect("the entry");
    wait_until("the entry's end", || {
        entry(&pair.table_a, ended.id).state == ConnectionState::Disconnected
    });
    let ended = entry(&pair.table_a, ended.id);
    assert!(
        ended
            .history
            .iter()
            .all(|h| h.state != ConnectionState::Connected),
        "{:#?}",
        ended.history
    );
    assert!(
        matches!(
            &ended.last_cause,
            Some(Cause::Rejected(RejectReason::Domain))
        ),
        "{:?}",
        ended.last_cause
    );
    // The end itself carries the refusal: the history's last line and the last transition
    // event, which a consumer such as the redialer goes by.
    let last = ended.history.last().expect("the history");
    assert_eq!(last.state, ConnectionState::Disconnected, "{last:?}");
    assert!(
        matches!(&last.cause, Some(Cause::Rejected(RejectReason::Domain))),
        "{:#?}",
        ended.history
    );
    let event = pair
        .table_a
        .events()
        .into_iter()
        .filter_map(|event| match event {
            ConnectionEvent::Transition { id, entry } if id == ended.id => Some(entry),
            _ => None,
        })
        .next_back()
        .expect("the entry's transitions");
    assert!(
        matches!(&event.cause, Some(Cause::Rejected(RejectReason::Domain))),
        "{event:?}"
    );
}

/// The acceptor ends the refusing ACK's stream at once: a dialer that reads on after the ACK
/// sees the EOF before the drain cap, without ending its own side first
/// (docs/design/transport.md "P2P의 닫기 범위", "거절 ACK만 예외다").
#[test]
fn the_refusing_ack_is_followed_by_the_eof_at_once() {
    let pair = Pair::start(50, Mode::Echo);
    let cancel = AttemptCancel::new();
    let Connection { mut channel, .. } = pair
        .connect(&cancel, &ConnectOptions::new())
        .expect("a channel");
    channel
        .control()
        .set_read_timeout(Some(PATIENCE))
        .expect("read timeout");
    // Not a hello: the acceptor refuses it as malformed.
    channel.write_all(b"{}\n").expect("a first line");
    let ack = tasty_net_frame::read_frame(&mut channel).expect("the ACK");
    assert_eq!(ack.tag, StreamTag::Control);
    let ack: Value = serde_json::from_slice(&ack.payload).expect("the ACK is JSON");
    assert_eq!(ack["ok"], json!(false), "{ack}");
    let read_ack = Instant::now();
    let mut rest = Vec::new();
    channel.read_to_end(&mut rest).expect("the EOF");
    let waited = read_ack.elapsed();
    assert!(rest.is_empty(), "{rest:?}");
    assert!(
        waited < timeouts().drain_cap,
        "the EOF came {waited:?} after the ACK, not before the drain cap"
    );
}

/// A channel that waits for a stream on a shared connection where the peer allows no more
/// streams ends with its attempt's cancel (docs/design/transport.md "시도 취소, 닫기, 회수").
#[test]
fn a_cancel_ends_the_wait_for_a_stream() {
    let pair = Pair::start_limited(52, Mode::Echo, 1);
    let _link = pair.link(None).expect("the link takes the one stream");
    assert_eq!(pair.transport.live_connections(), 1);
    let cancel = AttemptCancel::new();
    let (done_tx, done) = mpsc::channel();
    {
        let transport = Arc::clone(&pair.transport);
        let cancel = cancel.clone();
        let peer = pair.b.id();
        // Not joined: before the fix it never returns, and the test fails below instead.
        std::thread::spawn(move || {
            let result = transport.connect_with(&peer, &cancel, &ConnectOptions::new());
            let _sent = done_tx.send(result.map(|_| ()));
        });
    }
    assert!(
        done.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second channel did not wait for a stream"
    );
    cancel.cancel();
    match done.recv_timeout(PATIENCE) {
        Ok(Err(ConnectError::Cancelled)) => {}
        Ok(other) => panic!("expected a cancel, got {other:?}"),
        Err(_) => panic!("the cancel did not end the wait for a stream"),
    }
    // The shared connection and its link stay.
    assert_eq!(pair.transport.live_connections(), 1);
}

/// Two attempts of one transport to the same peer at once: each entry follows its own attempt,
/// never the other's. The peer is silent, so both punches run for a while (the server's offer
/// timeout) and the entries are copied at every poll meanwhile.
#[test]
fn concurrent_attempts_to_one_peer_keep_their_own_entries() {
    let (pair, gate) = Pair::start_with(54, Mode::Echo, true);
    let gate = gate.expect("gate");
    // `b` is registered once a link stood; the link's end closes its connection.
    drop(pair.link(None).expect("link"));
    wait_until("the first connection's close", || {
        pair.transport.live_connections() == 0
    });
    gate.shut.store(true, Ordering::SeqCst);
    let attempts: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| {
            let cancel = AttemptCancel::new();
            let transport = Arc::clone(&pair.transport);
            let peer = pair.b.id();
            let options = ConnectOptions::new().with_name(name);
            let (cancel_in, done) = (cancel.clone(), mpsc::channel());
            std::thread::spawn(move || {
                let result = transport.connect_with(&peer, &cancel_in, &options);
                let _sent = done.0.send(result.map(|_| ()));
            });
            (name, cancel, done.1)
        })
        .collect();
    let named = |name: &str| {
        pair.table_a
            .connections()
            .into_iter()
            .find(|e| e.peer.name.as_deref() == Some(name))
    };
    wait_until("both entries", || {
        named("one").is_some() && named("two").is_some()
    });
    // Several polls while both punch: the entries never show the same attempt.
    let watch_until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < watch_until {
        let (one, two) = (named("one").expect("one"), named("two").expect("two"));
        assert!(
            one.p2p_id.is_none() || one.p2p_id != two.p2p_id,
            "both entries follow attempt {:?}",
            one.p2p_id
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // One attempt ends; the other's entry then follows its own attempt alone.
    let (_, cancel_one, done_one) = &attempts[0];
    cancel_one.cancel();
    let ended = done_one
        .recv_timeout(PATIENCE)
        .expect("the first attempt's end");
    assert!(matches!(ended, Err(ConnectError::Cancelled)), "{ended:?}");
    let one = named("one").expect("one");
    let two = named("two").expect("two");
    assert!(
        one.p2p_id.is_none() || one.p2p_id != two.p2p_id,
        "{:?} {:?}",
        one.p2p_id,
        two.p2p_id
    );
    let (_, cancel_two, done_two) = &attempts[1];
    cancel_two.cancel();
    let _ended = done_two
        .recv_timeout(PATIENCE)
        .expect("the second attempt's end");
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
    // Closed in the middle of its registration handshake, the endpoint's connection to the
    // server is dropped without the server hearing of it: the server's side stays until the
    // server's idle timeout (45 s), and the server's shutdown at the end of the test waits
    // for it. Closing after the registration ends that connection cleanly.
    wait_until("the registration on the server", || {
        a.status().state == EndpointPhase::Registered
    });
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

/// One P2P link is one `p2p_udp` entry on each side, paired with the P2P connection's number,
/// with the link's `link_id`, the `udp` segment, the rendezvous server, the P2P state history
/// and the P2P connection's applied values (docs/design/transport.md "필드" P2P column). A bulk
/// channel on the same QUIC connection is counted on the same entry; the connection's close
/// ends both entries.
#[test]
fn a_p2p_link_shows_one_p2p_udp_entry_with_its_link_id() {
    let pair = Pair::start(40, Mode::Echo);
    let link = pair.link(None).expect("link");
    let link_id = link.link_id();
    let bulk = link.open_bulk(None).expect("bulk");
    let mut link = Open::new(link);
    link.round_trip(b"one");

    let p2p = pair.a.connections();
    assert_eq!(p2p.len(), 1);
    let entries = live(&pair.table_a);
    assert_eq!(entries.len(), 1, "{entries:#?}");
    let a = &entries[0];
    assert_eq!(a.path, PathKind::P2pUdp);
    assert_eq!(a.state, ConnectionState::Connected);
    assert_eq!(a.local_side, tasty_net_transport::Side::Dialer);
    assert_eq!(a.p2p_id, Some(p2p[0].id));
    assert_eq!(a.peer.device_id, Some(pair.b.id()));
    assert!(a.peer.instance_verified);
    assert_eq!(a.links.len(), 1);
    assert_eq!(a.links[0].link_id, link_id);
    assert_eq!(a.links[0].transport, EdgeTransport::P2pUdp);
    assert_eq!(a.bulk_channels, 1);
    assert_eq!(a.segments.len(), 1);
    assert_eq!(a.segments[0].name, "udp");
    let local = a.segments[0].local.as_deref().expect("local side");
    assert!(local.starts_with("local_bind="), "{local}");
    assert!(local.contains(", local_ip="), "{local}");
    assert!(local.contains(", local_reflexive="), "{local}");
    assert_eq!(
        a.segments[0].remote,
        p2p[0].remote_addr.map(|addr| addr.to_string())
    );
    assert!(a.segments[0].remote.is_some());
    assert_eq!(
        a.via,
        Via::Rendezvous {
            server: Some(pair._server.info),
            candidate: p2p[0].remote_candidate.map(|c| c.kind),
            nat: p2p[0].nat,
        }
    );
    assert!(a.rtt.is_some());
    assert!(a.attempt.as_ref().is_some_and(|at| at.number >= 1));
    let phases: Vec<_> = a.history.iter().map(|h| h.phase).collect();
    assert!(
        phases.contains(&Some(Phase::P2p(tasty_net_p2p::AttemptPhase::Requesting))),
        "{phases:?}"
    );
    // The QUIC connection up is not yet `connected`: the handshake v2 on its first channel
    // runs as `connecting`, and the link standing makes it `connected`.
    let quic_up = a
        .history
        .iter()
        .position(|h| {
            h.phase == Some(Phase::Handshake)
                && matches!(&h.cause, Some(Cause::P2p(c)) if c.name == "quic_established")
        })
        .unwrap_or_else(|| panic!("{:#?}", a.history));
    assert_eq!(a.history[quic_up].state, ConnectionState::Connecting);
    assert!(
        a.history[..quic_up]
            .iter()
            .all(|h| h.state == ConnectionState::Connecting),
        "{:#?}",
        a.history
    );
    assert!(
        a.history[quic_up..]
            .iter()
            .any(|h| h.state == ConnectionState::Connected && h.phase.is_none()),
        "{:#?}",
        a.history
    );
    assert!(
        a.applied.iter().any(|s| s.source.as_str() == "server"),
        "{:?}",
        a.applied
    );
    assert!(
        a.applied
            .iter()
            .any(|s| s.key == "link.drain_cap" && s.source == SettingSource::Configured),
        "{:?}",
        a.applied
    );

    let p2p_b = pair.b.connections();
    assert_eq!(p2p_b.len(), 1);
    wait_until("the accepting entry's bulk count", || {
        live(&pair.table_b)
            .first()
            .is_some_and(|b| b.bulk_channels == 1)
    });
    let entries = live(&pair.table_b);
    assert_eq!(entries.len(), 1, "{entries:#?}");
    let b = &entries[0];
    assert_eq!(b.path, PathKind::P2pUdp);
    assert_eq!(b.state, ConnectionState::Connected);
    assert_eq!(b.local_side, tasty_net_transport::Side::Acceptor);
    assert_eq!(b.p2p_id, Some(p2p_b[0].id));
    assert_eq!(b.peer.device_id, Some(pair.a.id()));
    assert_eq!(b.links.len(), 1);
    assert_eq!(b.links[0].link_id, link_id);
    assert_eq!(
        b.parent, None,
        "a bulk channel is on its parent's own entry"
    );

    let (a_id, b_id) = (a.id, b.id);
    drop(bulk);
    drop(link);
    wait_until("both entries' end", || {
        entry(&pair.table_a, a_id).state == ConnectionState::Disconnected
            && entry(&pair.table_b, b_id).state == ConnectionState::Disconnected
    });
    // This end closed it after its last link: the link's own end is the cause.
    let cause = entry(&pair.table_a, a_id).last_cause;
    assert!(
        matches!(cause, Some(Cause::Ended | Cause::ClosedHere)),
        "{cause:?}"
    );
}

/// A new punch for a link that reconnects an ended entry is a new entry that names it
/// (`ConnectOptions::after`) and starts `reconnecting`.
#[test]
fn a_reconnection_names_its_previous_entry() {
    let pair = Pair::start(42, Mode::Echo);
    let mut first = Open::new(pair.link(None).expect("link"));
    first.round_trip(b"one");
    let old = live(&pair.table_a)[0].id;
    drop(first);
    wait_until("the first entry's end", || {
        entry(&pair.table_a, old).state == ConnectionState::Disconnected
    });
    wait_until("the first connection's close", || {
        pair.connections() == (0, 0)
    });
    let mut again = Open::new(
        pair.link_with(None, &ConnectOptions::new().after(old))
            .expect("reconnect"),
    );
    again.round_trip(b"two");
    let entries = live(&pair.table_a);
    assert_eq!(entries.len(), 1, "{entries:#?}");
    let new = &entries[0];
    assert_ne!(new.id, old);
    assert_eq!(new.previous, Some(old));
    assert_eq!(new.state, ConnectionState::Connected);
    assert_eq!(
        new.history.first().map(|h| h.state),
        Some(ConnectionState::Reconnecting)
    );
}

/// A reconnection that finds the shared QUIC connection still open uses it: its own entry
/// ends naming the connection's entry once its handshake succeeded, and the link is a row of
/// that entry.
#[test]
fn a_reconnection_on_the_shared_connection_joins_its_entry() {
    let pair = Pair::start(44, Mode::Echo);
    let mut first = Open::new(pair.link(None).expect("link"));
    first.round_trip(b"one");
    let shared = live(&pair.table_a)[0].id;
    let before = pair.table_a.connections().len();
    let mut second = Open::new(
        pair.link_with(None, &ConnectOptions::new().after(shared))
            .expect("reconnect"),
    );
    second.round_trip(b"two");
    let all = pair.table_a.connections();
    assert_eq!(all.len(), before + 1, "{all:#?}");
    let joined = all.last().expect("the reconnection's entry");
    assert_eq!(joined.previous, Some(shared));
    assert_eq!(joined.state, ConnectionState::Disconnected);
    assert_eq!(joined.last_cause, Some(Cause::SharedTunnel(shared)));
    assert_eq!(entry(&pair.table_a, shared).links.len(), 2);
    assert_eq!(pair.connections(), (1, 1));
}

/// A link whose peer goes silent (no CONNECTION_CLOSE) ends at the QUIC idle timeout, well
/// before the link's own read timeout: the read fails as `HeartbeatTimeout`, the link shows no
/// close code, and the connection's entry ends as lost.
#[test]
fn a_silent_peer_ends_the_link_at_the_idle_timeout() {
    let (pair, gate) = Pair::start_with(46, Mode::Echo, true);
    let gate = gate.expect("gate");
    let mut link = pair.link(None).expect("link");
    let (mut reader, writer) = link.take_ordered().expect("halves");
    writer.write_frame(StreamTag::Data, b"one").expect("write");
    assert_eq!(reader.read_frame().expect("echo").payload, b"one");
    let id = live(&pair.table_a)[0].id;
    gate.shut.store(true, Ordering::SeqCst);
    let silent = Instant::now();
    let failed = reader.read_frame().expect_err("the connection timed out");
    assert_eq!(failed.failure(), ReadFailure::HeartbeatTimeout);
    assert!(
        silent.elapsed() < timeouts().read_timeout,
        "{:?}: the QUIC idle timeout ends it first",
        silent.elapsed()
    );
    assert_eq!(link.close_reason(), None);
    wait_until("the entry's end", || {
        entry(&pair.table_a, id).state == ConnectionState::Disconnected
    });
    assert_eq!(entry(&pair.table_a, id).last_cause, Some(Cause::Lost));
}
