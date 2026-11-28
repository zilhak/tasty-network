//! Channels over the P2P adapter (docs/design/transport.md "채널 대응", "P2P의 닫기 범위"):
//! two real P2P endpoints on loopback OS sockets and a rendezvous server in this process.
//!
//! Every test uses its own device keys: one key has one P2P endpoint in a process, and the
//! tests of this file run in parallel.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
    P2pListener, P2pTransport, PathKind, PeerInfo, Phase, Redialer, RedialerConfig, RejectReason,
    RequestHandler, Transport, Via, dial_handshake, dial_request, domain_code,
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
    endpoint_with(server, seed, peer, None, &[])
}

/// [`endpoint`] on `gate` instead of an OS socket of its own, with short QUIC timers and a
/// short `path.poll_interval` (how often the adapter copies an attempt to its entry); then
/// `settings`.
fn endpoint_with(
    server: &Rendezvous,
    seed: u8,
    peer: DeviceId,
    gate: Option<Arc<Gate>>,
    settings: &[(&str, P2pValue)],
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
    for (key, value) in settings {
        config.settings.set(key, *value);
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
    /// [`Mode::LastFrameFirst`] that waits for the test's release (`Pair::late_read`) instead.
    LastFrameLate,
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
    /// What [`Mode::LastFrameLate`] waits for before it reads.
    late_read: Arc<Release>,
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
        Self::start_full(seed, mode, gated, &[], timeouts())
    }

    /// [`Pair::start`] with these settings on `b`.
    fn start_set(seed: u8, mode: Mode, b_settings: &[(&str, P2pValue)]) -> Self {
        Self::start_full(seed, mode, false, b_settings, timeouts()).0
    }

    /// [`Pair::start`] where the accepting side waits up to `drain_cap` for the dialer's end.
    fn start_draining(seed: u8, mode: Mode, drain_cap: Duration) -> Self {
        let accept = LinkTimeouts {
            drain_cap,
            ..timeouts()
        };
        Self::start_full(seed, mode, false, &[], accept).0
    }

    fn start_full(
        seed: u8,
        mode: Mode,
        gated: bool,
        b_settings: &[(&str, P2pValue)],
        accept_timeouts: LinkTimeouts,
    ) -> (Self, Option<Arc<Gate>>) {
        let server = Rendezvous::start();
        let (ka, kb) = (key(seed), key(seed + 1));
        let gate = gated.then(|| Gate::bind(&server));
        let (a, b) = if gated {
            (
                endpoint_with(&server, seed, kb.id(), Some(Gate::bind(&server)), &[]),
                endpoint_with(&server, seed + 1, ka.id(), gate.clone(), b_settings),
            )
        } else {
            (
                endpoint(&server, seed, kb.id()),
                endpoint_with(&server, seed + 1, ka.id(), None, b_settings),
            )
        };
        let policy = Arc::new(Policy::default());
        let (table_a, table_b) = (ConnectionTable::default(), ConnectionTable::default());
        let accept = AcceptConfig::new(kb, format!("acceptor-{seed}"), None, policy.clone())
            .with_timeouts(accept_timeouts)
            .with_connections(table_b.clone());
        let listener = P2pListener::new(b.clone(), accept).expect("listener");
        let stop = Arc::new(AtomicBool::new(false));
        let served: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (read_tx, read) = mpsc::channel();
        let late_read = Arc::new(Release::default());
        let accepting = {
            let stop = Arc::clone(&stop);
            let served = Arc::clone(&served);
            let late_read = Arc::clone(&late_read);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(inbound) = listener.accept_timeout(Some(Duration::from_millis(100)))
                    else {
                        continue;
                    };
                    let read_tx = read_tx.clone();
                    let late_read = Arc::clone(&late_read);
                    let thread = std::thread::spawn(move || {
                        let accepted = inbound.handshake(&AttemptCancel::new());
                        serve(accepted, mode, &read_tx, &late_read);
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
            late_read,
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
    late_read: &Release,
) {
    match accepted {
        Ok(Accepted::Link(mut link)) => {
            let Some((reader, writer)) = link.take_ordered() else {
                return;
            };
            match mode {
                Mode::Echo => echo(reader, writer),
                Mode::LastFrameFirst => {
                    last_frame_first(reader, writer, read_tx, || {
                        std::thread::sleep(Duration::from_millis(300));
                        true
                    });
                }
                Mode::LastFrameLate => {
                    last_frame_first(reader, writer, read_tx, || late_read.wait());
                }
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

/// A release one side opens and the other waits for: an order of events between two threads
/// that does not depend on when each started its clock.
#[derive(Default)]
struct Release {
    state: Mutex<ReleaseState>,
    opened: Condvar,
}

#[derive(Default)]
struct ReleaseState {
    open: bool,
    /// The waiting side stopped waiting before the release opened.
    gave_up: bool,
}

impl Release {
    fn open(&self) {
        self.state.lock().expect("release").open = true;
        self.opened.notify_all();
    }

    /// Waits until the release is open, up to [`PATIENCE`]: whether it opened. A wait that
    /// runs out is recorded ([`Release::gave_up`]).
    #[must_use]
    fn wait(&self) -> bool {
        let state = self.state.lock().expect("release");
        let (mut state, _) = self
            .opened
            .wait_timeout_while(state, PATIENCE, |state| !state.open)
            .expect("release");
        state.gave_up = !state.open;
        state.open
    }

    /// Whether the waiting side stopped waiting before the release opened.
    fn gave_up(&self) -> bool {
        self.state.lock().expect("release").gave_up
    }
}

fn last_frame_first(
    mut reader: ChannelReader,
    mut writer: ChannelWriter,
    read_tx: &mpsc::Sender<(Vec<StreamFrame>, Option<ReadFailure>)>,
    wait: impl FnOnce() -> bool,
) {
    writer
        .write_frame(StreamTag::Detach, b"acceptor-last")
        .expect("the acceptor's last frame");
    writer.half_close().expect("half close");
    // The dialer has read the acceptor's last frame and ended its side by now; the acceptor's
    // EOF must still wait for this side to read the dialer's frames. Without the go-ahead it
    // reads nothing and sends no reads: the test fails on its wait for them.
    if !wait() {
        return;
    }
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
    // `a` may open one stream at a time to `b`.
    let pair = Pair::start_set(
        52,
        Mode::Echo,
        &[("quic.max_bidi_streams", P2pValue::Count(1))],
    );
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

/// A punch with an entry (the redialer's) and one without (a transport without a table) of
/// the same transport to the same peer: the entry does not take the other punch's attempt,
/// and waits while both run; once the other ends, it follows its own.
#[test]
fn a_punch_without_an_entry_is_counted_among_the_transports_punches() {
    let (pair, gate) = Pair::start_with(84, Mode::Echo, true);
    let gate = gate.expect("gate");
    // `b` is registered once a link stood; the link's end closes its connection.
    drop(pair.link(None).expect("link"));
    let lost = pair.table_a.connections()[0].id;
    wait_until("the first connection's close", || {
        pair.transport.live_connections() == 0
    });
    gate.shut.store(true, Ordering::SeqCst);
    let plain = Arc::new(P2pTransport::new(pair.a.clone()));
    let peer = pair.b.id();
    let attempts = |a: &P2pEndpoint| -> Vec<u64> {
        a.connections()
            .into_iter()
            .filter(|info| {
                info.side == tasty_net_p2p::Side::Dialer
                    && !matches!(
                        info.state,
                        tasty_net_p2p::AttemptPhase::Connected
                            | tasty_net_p2p::AttemptPhase::Closed
                    )
            })
            .map(|info| info.id)
            .collect()
    };
    let cancel_plain = AttemptCancel::new();
    let plain_done = {
        let (plain, cancel) = (Arc::clone(&plain), cancel_plain.clone());
        std::thread::spawn(move || plain.connect(&peer, &cancel).map(|_| ()))
    };
    wait_until("the plain attempt", || attempts(&pair.a).len() == 1);
    let other = attempts(&pair.a)[0];
    let table = ConnectionTable::default();
    let cancel_redial = AttemptCancel::new();
    let redial_done = {
        let (plain, cancel, table) = (Arc::clone(&plain), cancel_redial.clone(), table.clone());
        std::thread::spawn(move || {
            let config = RedialerConfig::new()
                .with_initial(Duration::from_millis(1))
                .with_jitter(0.0)
                .with_max_attempts(1);
            let redialer = Redialer::new(&table, config).expect("valid settings");
            redialer.redial(lost, None, &cancel, |options| {
                plain
                    .connect_with(&peer, &cancel, options)
                    .map(drop)
                    .map_err(HandshakeError::Connect)
            })
        })
    };
    wait_until("the redialer's attempt", || attempts(&pair.a).len() == 2);
    let own = attempts(&pair.a)
        .into_iter()
        .find(|id| *id != other)
        .expect("the redialer's attempt");
    let pending = || table.connections().into_iter().next().expect("the entry");
    // Several polls while both punch (within the server's offer timeout): the entry follows
    // neither attempt.
    let watch_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < watch_until {
        let shown = pending().p2p_id;
        assert_eq!(
            shown, None,
            "the entry follows attempt {shown:?} (own {own}, other {other})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    cancel_plain.cancel();
    let ended = plain_done.join().expect("the plain attempt");
    assert!(matches!(ended, Err(ConnectError::Cancelled)), "{ended:?}");
    wait_until("the entry on its own attempt", || {
        pending().p2p_id == Some(own)
    });
    cancel_redial.cancel();
    let _ended = redial_done.join().expect("the redialer");
}

/// A cancel of the attempt before its first handshake succeeded closes the new connection it
/// owns (docs/design/transport.md "연결 공유의 시작", close scope fourth row).
#[test]
fn a_cancel_before_the_first_handshake_closes_the_new_connection() {
    let pair = Pair::start(56, Mode::Echo);
    let cancel = AttemptCancel::new();
    let conn = pair
        .connect(&cancel, &ConnectOptions::new())
        .expect("a channel");
    assert_eq!(pair.connections().0, 1, "the new connection");
    cancel.cancel();
    wait_until("the connection's close", || pair.connections() == (0, 0));
    assert_eq!(pair.transport.live_connections(), 0);
    drop(conn);
}

/// Until its first handshake succeeded, a new connection is its attempt's alone: its endpoint
/// opens no more channels on it (docs/design/transport.md "연결 공유의 시작"). Once shared,
/// it does.
#[test]
fn a_new_connection_takes_no_more_channels_before_its_first_handshake() {
    let pair = Pair::start(72, Mode::Echo);
    let cancel = AttemptCancel::new();
    let conn = pair
        .connect(&cancel, &ConnectOptions::new())
        .expect("a channel");
    let more = conn.endpoint.connect_more(&AttemptCancel::new());
    assert!(
        matches!(&more, Err(ConnectError::Io(e)) if e.kind() == io::ErrorKind::NotConnected),
        "{more:?}"
    );
    let endpoint = conn.endpoint.clone();
    let link = dial_handshake(conn, &pair.dial, Holder::Dialer, None, &cancel).expect("link");
    endpoint
        .connect_more(&AttemptCancel::new())
        .expect("a channel on the shared connection");
    drop(link);
}

/// Whether `error` is the refusal of a blocking call in an async task.
fn in_async_context(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(io) = error.downcast_ref::<io::Error>()
            && let Some(inner) = io.get_ref()
            && matches!(
                inner.downcast_ref::<tasty_net_p2p::P2pError>(),
                Some(tasty_net_p2p::P2pError::InAsyncContext)
            )
        {
            return true;
        }
        next = error.source();
    }
    false
}

/// Another channel on a shared connection, asked for in an async task, is refused there as
/// `tasty-net-p2p` refuses its blocking calls, though its open waits on a thread of its own;
/// outside it opens.
#[test]
fn a_channel_asked_for_in_an_async_task_is_refused() {
    let pair = Pair::start(86, Mode::Echo);
    let cancel = AttemptCancel::new();
    let conn = pair
        .connect(&cancel, &ConnectOptions::new())
        .expect("a channel");
    let endpoint = conn.endpoint.clone();
    let link = dial_handshake(conn, &pair.dial, Holder::Dialer, None, &cancel).expect("link");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let refused = runtime.block_on(async { endpoint.connect_more(&AttemptCancel::new()) });
    assert!(
        matches!(
            &refused,
            Err(ConnectError::P2p(tasty_net_p2p::P2pError::InAsyncContext))
        ),
        "{refused:?}"
    );
    endpoint
        .connect_more(&AttemptCancel::new())
        .expect("a channel outside the async task");
    drop(link);
}

/// A connect whose channel on the shared connection fails for a reason other than the
/// connection's end (here a call in an async task; a thread that cannot start is the same path)
/// returns that error: it punches no new connection and starts no entry for one.
#[test]
fn a_failed_channel_on_a_live_shared_connection_is_the_connects_error() {
    let pair = Pair::start(92, Mode::Echo);
    let link = pair.link(None).expect("link");
    // The dial retried while the peer was not registered may have left ended entries.
    let entries = pair.table_a.connections().len();
    assert_eq!(live(&pair.table_a).len(), 1);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let refused = runtime.block_on(async {
        pair.transport
            .connect_with(&pair.b.id(), &AttemptCancel::new(), &ConnectOptions::new())
    });
    assert!(
        matches!(
            &refused,
            Err(ConnectError::P2p(tasty_net_p2p::P2pError::InAsyncContext))
        ),
        "{refused:?}"
    );
    assert_eq!(
        pair.table_a.connections().len(),
        entries,
        "a new entry was started: {:?}",
        pair.table_a.connections()
    );
    assert_eq!(pair.connections().0, 1);
    pair.connect(&AttemptCancel::new(), &ConnectOptions::new())
        .expect("a channel on the shared connection outside the async task");
    assert_eq!(pair.table_a.connections().len(), entries);
    drop(link);
}

/// A listener asked for a channel in an async task hands out none and keeps the waiting one:
/// the refusal of its connection check there is no end of the connection.
#[test]
fn a_waiting_channel_stays_when_an_async_task_asks_for_it() {
    let server = Rendezvous::start();
    let (a, b, listener) = draining_ends(&server, 88, timeouts().drain_cap);
    let conn = dial_raw(&a, &b);
    let mut stream = conn.open_bi().expect("a stream");
    stream
        .write_all(b"waits")
        .expect("bytes that open the stream");
    wait_until("the waiting channel", || {
        format!("{listener:?}").contains("waiting: 1")
    });
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let refused =
        runtime.block_on(async { listener.accept_timeout(Some(Duration::from_millis(500))) });
    match refused {
        Err(error) => assert!(in_async_context(&error), "{error:?}"),
        Ok(inbound) => panic!("a channel was handed out in an async task: {inbound:?}"),
    }
    let inbound = listener
        .accept_timeout(Some(Duration::from_millis(500)))
        .expect("the channel that waited");
    drop((inbound, stream, conn, listener));
    a.close();
    b.close();
}

/// A channel that waited in the listener while its connection ended is not handed out.
#[test]
fn the_listener_hands_out_no_channel_of_an_ended_connection() {
    let server = Rendezvous::start();
    let (ka, kb) = (key(74), key(75));
    let a = endpoint(&server, 74, kb.id());
    let b = endpoint(&server, 75, ka.id());
    let accept = AcceptConfig::new(kb, "acceptor-75", None, Arc::new(Policy::default()))
        .with_timeouts(timeouts());
    let listener = P2pListener::new(b.clone(), accept).expect("listener");
    let started = Instant::now();
    let conn = loop {
        match a.connect(b.id()) {
            Ok(conn) => break conn,
            Err(_) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("connect: {error:?}"),
        }
    };
    let mut stream = conn.open_bi().expect("a stream");
    stream
        .write_all(b"waits")
        .expect("bytes that open the stream");
    // Nobody accepts: the channel waits in the listener.
    wait_until("the waiting channel", || {
        format!("{listener:?}").contains("waiting: 1")
    });
    conn.close(P2pCloseReason::Closed);
    wait_until("the end on the accepting side", || {
        b.connections()
            .iter()
            .all(|info| info.state == tasty_net_p2p::AttemptPhase::Closed)
    });
    match listener.accept_timeout(Some(Duration::from_millis(500))) {
        Err(error) => {
            let kind = std::error::Error::source(&error)
                .and_then(|e| e.downcast_ref::<io::Error>())
                .map(io::Error::kind);
            assert_eq!(kind, Some(io::ErrorKind::TimedOut), "{error:?}");
        }
        Ok(inbound) => panic!("a channel of the ended connection was handed out: {inbound:?}"),
    }
    drop((stream, conn, listener));
    a.close();
    b.close();
}

/// The ACK among the Control frames the accepting side writes on `stream`.
fn read_ack(stream: &mut tasty_net_p2p::P2pStream) -> Value {
    loop {
        let frame = tasty_net_frame::read_frame(stream).expect("a frame");
        if frame.tag != StreamTag::Control {
            continue;
        }
        let message: Value = serde_json::from_slice(&frame.payload).expect("JSON");
        if message.get("ok").is_some() {
            return message;
        }
    }
}

/// A dialer straight on `tasty-net-p2p` that opens a stream, sends a line the acceptor refuses
/// and then neither ends its side nor closes: the connection, its stream and when the
/// refusing ACK was read.
fn refused_raw(
    pair: &Pair,
) -> (
    tasty_net_p2p::P2pConnection,
    tasty_net_p2p::P2pStream,
    Instant,
) {
    let started = Instant::now();
    let conn = loop {
        match pair.a.connect(pair.b.id()) {
            Ok(conn) => break conn,
            Err(_) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("connect: {error:?}"),
        }
    };
    let mut stream = conn.open_bi().expect("a stream");
    stream.set_read_timeout(Some(PATIENCE));
    stream.write_all(b"{}\n").expect("a first line");
    let ack = read_ack(&mut stream);
    assert_eq!(ack["ok"], json!(false), "{ack}");
    (conn, stream, Instant::now())
}

/// The accepting side closes the connection with code 0 at the drain cap after its last
/// channel when the dialer does not close it (docs/design/transport.md "연결을 닫는 때").
#[test]
fn the_acceptor_closes_an_idle_connection_after_the_drain_cap() {
    let pair = Pair::start(58, Mode::Echo);
    let (conn, _stream, acked) = refused_raw(&pair);
    let closed = conn.wait_closed(PATIENCE).expect("the wait");
    assert!(
        closed.is_some(),
        "the acceptor did not close the connection"
    );
    let waited = acked.elapsed();
    assert_eq!(conn.close_reason(), Some(P2pCloseReason::Closed));
    assert!(
        waited >= timeouts().drain_cap,
        "closed {waited:?} after the ACK, before the drain cap"
    );
}

/// The drain cap closes the idle accepted connection also when the connection's thread waits
/// longer for a stream (`path.poll_interval` past the drain cap).
#[test]
fn the_drain_cap_does_not_wait_for_the_poll_interval() {
    let poll = Duration::from_secs(4);
    let pair = Pair::start_set(
        60,
        Mode::Echo,
        &[("path.poll_interval", P2pValue::Duration(poll))],
    );
    let (conn, _stream, acked) = refused_raw(&pair);
    let closed = conn.wait_closed(PATIENCE).expect("the wait");
    assert!(
        closed.is_some(),
        "the acceptor did not close the connection"
    );
    let waited = acked.elapsed();
    assert_eq!(conn.close_reason(), Some(P2pCloseReason::Closed));
    // The refusal's linger and then the drain cap, each `drain_cap` long: well before `poll`.
    assert!(
        waited < poll / 2,
        "closed {waited:?} after the ACK: it waited for the poll interval {poll:?}"
    );
}

/// Binds the endpoint of the key `seed` again, as long as `tasty-net-p2p` refuses it because
/// the earlier endpoint of that key is not off yet (it ends once every handle of it and of its
/// connections is gone); how long that took.
fn bind_again(server: &Rendezvous, seed: u8, peer: DeviceId) -> Duration {
    let started = Instant::now();
    loop {
        let pairing = Arc::new(PairingList::new());
        pairing.add(peer);
        let mut config = P2pConfig::new(key(seed), pairing);
        config.enabled = true;
        config.server = Some(server.info);
        match P2pEndpoint::bind(config) {
            Ok(again) => {
                let took = started.elapsed();
                // Closed once registered: a close in the middle of the registration keeps the
                // server's shutdown at the end of the test waiting.
                wait_until("the registration of the endpoint bound again", || {
                    again.status().state == EndpointPhase::Registered
                });
                again.close();
                return took;
            }
            Err(tasty_net_p2p::P2pError::EndpointInUse) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("bind again: {error:?}"),
        }
    }
}

/// An accepting side whose idle connections wait `drain_cap` for the dialer's close, on its
/// own endpoint `b` (of key `seed + 1`), next to a dialing endpoint `a`.
fn draining_ends(
    server: &Rendezvous,
    seed: u8,
    drain_cap: Duration,
) -> (P2pEndpoint, P2pEndpoint, P2pListener) {
    let (ka, kb) = (key(seed), key(seed + 1));
    let a = endpoint(server, seed, kb.id());
    let b = endpoint(server, seed + 1, ka.id());
    let accept = AcceptConfig::new(
        kb,
        format!("acceptor-{seed}"),
        None,
        Arc::new(Policy::default()),
    )
    .with_timeouts(LinkTimeouts {
        drain_cap,
        ..timeouts()
    });
    let listener = P2pListener::new(b.clone(), accept).expect("listener");
    (a, b, listener)
}

/// `a`'s connection to `b`, once `b` is registered.
fn dial_raw(a: &P2pEndpoint, b: &P2pEndpoint) -> tasty_net_p2p::P2pConnection {
    let started = Instant::now();
    loop {
        match a.connect(b.id()) {
            Ok(conn) => return conn,
            Err(_) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("connect: {error:?}"),
        }
    }
}

/// The dialer closes an accepted connection while the accepting side waits the drain cap for
/// it: the wait ends with the connection, which keeps nothing of the endpoint after that.
#[test]
fn the_drain_wait_ends_when_the_dialer_closes() {
    let long = Duration::from_secs(10);
    let server = Rendezvous::start();
    let (a, b, listener) = draining_ends(&server, 80, long);
    let conn = dial_raw(&a, &b);
    let mut stream = conn.open_bi().expect("a stream");
    stream.set_read_timeout(Some(PATIENCE));
    stream.write_all(b"{}\n").expect("a first line");
    let inbound = listener
        .accept_timeout(Some(PATIENCE))
        .expect("the channel");
    let refusing = std::thread::spawn(move || inbound.handshake(&AttemptCancel::new()));
    let ack = read_ack(&mut stream);
    assert_eq!(ack["ok"], json!(false), "{ack}");
    // The refusal lingers until this side's end; then it was the accepting side's last
    // channel, which waits for the dialer's close from here on.
    stream.finish().expect("this side's end");
    let refused = refusing.join().expect("the refusing handshake");
    assert!(refused.is_err());
    conn.close(P2pCloseReason::Closed);
    drop((stream, conn, listener, b));
    let took = bind_again(&server, 81, key(80).id());
    assert!(
        took < long / 2,
        "the accepting endpoint ended {took:?} after the close: the drain wait kept it"
    );
    a.close();
}

/// A channel that waited in the listener goes with its ended connection, and its end starts no
/// drain wait on that connection.
#[test]
fn a_waiting_channel_of_an_ended_connection_starts_no_drain_wait() {
    let long = Duration::from_secs(10);
    let server = Rendezvous::start();
    let (a, b, listener) = draining_ends(&server, 82, long);
    let conn = dial_raw(&a, &b);
    let mut stream = conn.open_bi().expect("a stream");
    stream
        .write_all(b"waits")
        .expect("bytes that open the stream");
    // Nobody accepts: the channel waits in the listener.
    wait_until("the waiting channel", || {
        format!("{listener:?}").contains("waiting: 1")
    });
    conn.close(P2pCloseReason::Closed);
    wait_until("the channel's removal", || {
        format!("{listener:?}").contains("waiting: 0")
    });
    drop((stream, conn, listener, b));
    let took = bind_again(&server, 83, key(82).id());
    assert!(
        took < long / 2,
        "the accepting endpoint ended {took:?} after the close: a drain wait kept it"
    );
    a.close();
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

/// A forced close takes the link off its connection at once: the dialer closes the connection
/// after its last link was closed by force, while the consumer still holds the link's halves.
/// (Were the link still counted, the accepting side would close the connection at its drain
/// cap, and the dialer's entry would end with that close's code.)
#[test]
fn a_forced_close_of_the_last_link_closes_the_connection_with_its_handles_kept() {
    let pair = Pair::start(62, Mode::Echo);
    let mut link = pair.link(None).expect("link");
    let (reader, writer) = link.take_ordered().expect("halves");
    let id = live(&pair.table_a)[0].id;
    link.closer().force_close(EndReason::Forced);
    wait_until("the connection's close", || pair.connections() == (0, 0));
    wait_until("the entry's end", || {
        entry(&pair.table_a, id).state == ConnectionState::Disconnected
    });
    let ended = entry(&pair.table_a, id);
    assert!(
        !matches!(ended.last_cause, Some(Cause::P2pClosed(_))),
        "the accepting side closed it: {:?}",
        ended.last_cause
    );
    drop((reader, writer, link));
}

/// The dialer drops its halves after its last frames without reading the acceptor's end: the
/// connection's close still waits for that end, so the acceptor's application, which reads
/// late, reads every frame, also the ones still on their way when the halves were dropped.
#[test]
fn a_dialer_that_drops_its_halves_early_loses_no_last_frame() {
    // The acceptor's end waits for this side's at most its drain cap (docs/design/transport.md
    // "연결을 닫는 때"); a long one keeps that wait out of the way of the late reads, below the
    // dialer's own wait (the drain cap of its link).
    let mut pair = Pair::start_draining(64, Mode::LastFrameFirst, Duration::from_millis(1500));
    pair.dial = pair.dial.clone().with_timeouts(LinkTimeouts {
        drain_cap: Duration::from_secs(2),
        ..timeouts()
    });
    drop_the_halves_before_the_late_reads(&pair);
}

/// Both sides frame the link with a drain cap past the default one, and the acceptor reads
/// this side's frames late: past that default, within the drain cap. The dialer drops its
/// halves before it read the acceptor's end; its last read waits the link's drain cap, so the
/// connection stays until the acceptor read every frame and ended its side
/// (docs/design/transport.md "연결을 닫는 때"). The acceptor reads only once the dialer has
/// checked the connection past the default drain cap from its own drop: the two sides keep no
/// common clock.
#[test]
fn a_late_read_within_the_links_drain_cap_loses_no_last_frame() {
    const FRAMES: usize = 4;
    let drain_cap = Duration::from_secs(10);
    let past_default = tasty_net_transport::DEFAULT_DRAIN_CAP + Duration::from_millis(300);
    assert!(past_default < drain_cap);
    let mut pair = Pair::start_draining(90, Mode::LastFrameLate, drain_cap);
    pair.dial = pair.dial.clone().with_timeouts(LinkTimeouts {
        drain_cap,
        ..timeouts()
    });
    let mut link = pair.link(None).expect("link");
    let (mut reader, mut writer) = link.take_ordered().expect("halves");
    // Small frames: the writes do not wait for the acceptor's reads.
    let small = vec![7u8; 16 << 10];
    for _ in 1..FRAMES {
        writer
            .write_frame(StreamTag::Data, &small)
            .expect("a frame");
    }
    writer
        .write_frame(StreamTag::Detach, b"dialer-last")
        .expect("the dialer's last frame");
    writer.half_close().expect("half close");
    reader.finish();
    let dropped = Instant::now();
    drop((reader, writer, link));
    std::thread::sleep(past_default.saturating_sub(dropped.elapsed()));
    assert!(
        !pair.late_read.gave_up(),
        "the acceptor stopped waiting for its release within {PATIENCE:?}: the check says nothing"
    );
    assert_eq!(
        pair.connections().0,
        1,
        "the dialer closed the connection {past_default:?} after its drop, before the acceptor read"
    );
    pair.late_read.open();
    let (frames, ended) = pair
        .read
        .recv_timeout(PATIENCE)
        .expect("the acceptor's reads");
    assert_eq!(frames.len(), FRAMES, "the acceptor read every frame");
    assert_eq!(frames[FRAMES - 1].payload, b"dialer-last");
    assert_eq!(ended, Some(ReadFailure::PeerClosed));
    wait_until("the connection's close", || pair.connections() == (0, 0));
}

/// The dialer writes large frames and its last one, ends its side and drops the halves without
/// reading on; the accepting side (in a last-frame-first mode) reads every frame.
fn drop_the_halves_before_the_late_reads(pair: &Pair) {
    const FRAMES: usize = 4;
    let mut link = pair.link(None).expect("link");
    let (mut reader, mut writer) = link.take_ordered().expect("halves");
    let big = vec![7u8; 1 << 20];
    for _ in 1..FRAMES {
        writer.write_frame(StreamTag::Data, &big).expect("a frame");
    }
    writer
        .write_frame(StreamTag::Detach, b"dialer-last")
        .expect("the dialer's last frame");
    writer.half_close().expect("half close");
    // Done here: nothing more is read.
    reader.finish();
    drop((reader, writer, link));
    let (frames, ended) = pair
        .read
        .recv_timeout(PATIENCE)
        .expect("the acceptor's reads");
    assert_eq!(frames.len(), FRAMES, "the acceptor read every frame");
    assert_eq!(frames[FRAMES - 1].payload, b"dialer-last");
    assert_eq!(ended, Some(ReadFailure::PeerClosed));
    wait_until("the connection's close", || pair.connections() == (0, 0));
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

/// The peer closes the connection with code 0 during the first handshake: the connection's
/// entry ends with that code (`p2p_closed`, `closed`), which may be retried.
#[test]
fn a_plain_close_during_the_first_handshake_keeps_its_code_in_the_entry() {
    let server = Rendezvous::start();
    let (ka, kb) = (key(78), key(79));
    let a = endpoint(&server, 78, kb.id());
    let b = endpoint(&server, 79, ka.id());
    let closer = {
        let b = b.clone();
        std::thread::spawn(move || {
            let conn = b
                .accept_timeout(Some(PATIENCE))
                .expect("the dialer's connection");
            let stream = conn.accept_bi(Some(PATIENCE)).expect("the first stream");
            conn.close(P2pCloseReason::Closed);
            drop(stream);
        })
    };
    let table = ConnectionTable::default();
    let transport = P2pTransport::new(a.clone()).with_connections(table.clone());
    let dial = DialConfig::new(ka, "dialer-78").with_timeouts(timeouts());
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
        matches!(error, HandshakeError::Closed(P2pCloseReason::Closed)),
        "{error:?}"
    );
    assert!(error.is_retryable());
    closer.join().expect("closer");
    let ended = table.connections().last().cloned().expect("the entry");
    wait_until("the entry's end", || {
        entry(&table, ended.id).state == ConnectionState::Disconnected
    });
    let ended = entry(&table, ended.id);
    let closed = Cause::P2pClosed(P2pCloseReason::Closed);
    assert_eq!(
        ended.last_cause,
        Some(closed.clone()),
        "{:#?}",
        ended.history
    );
    assert_eq!(
        ended.history.last().and_then(|h| h.cause.clone()),
        Some(closed)
    );
    a.close();
    b.close();
}

/// The accepting side closes the connection with `not_paired` as soon as it takes it, before
/// any stream (as `tasty-net-p2p` does for a peer off its pairing list): the code still ends
/// the handshake, not retried, whether it came before or after the dialer's first stream.
#[test]
fn a_not_paired_close_before_the_first_stream_keeps_its_code() {
    let server = Rendezvous::start();
    let (ka, kb) = (key(66), key(67));
    let a = endpoint(&server, 66, kb.id());
    let b = endpoint(&server, 67, ka.id());
    let table = ConnectionTable::default();
    let transport = P2pTransport::new(a.clone()).with_connections(table.clone());
    let dial = DialConfig::new(ka, "dialer-66").with_timeouts(timeouts());
    for _ in 0..3 {
        let closer = {
            let b = b.clone();
            std::thread::spawn(move || {
                let conn = b
                    .accept_timeout(Some(PATIENCE))
                    .expect("the dialer's connection");
                conn.close(P2pCloseReason::NotPaired);
            })
        };
        let started = Instant::now();
        let error = loop {
            let cancel = AttemptCancel::new();
            let result = transport
                .connect(&b.id(), &cancel)
                .map_err(HandshakeError::Connect)
                .and_then(|conn| dial_handshake(conn, &dial, Holder::Dialer, None, &cancel));
            match result {
                Err(HandshakeError::Connect(ConnectError::P2p(e)))
                    if started.elapsed() < PATIENCE && e.connect_error().is_some() =>
                {
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
        let ended = table.connections().last().cloned().expect("the entry");
        wait_until("the entry's end", || {
            entry(&table, ended.id).state == ConnectionState::Disconnected
        });
        let ended = entry(&table, ended.id);
        assert_eq!(
            ended.last_cause,
            Some(Cause::P2pClosed(P2pCloseReason::NotPaired)),
            "{:#?}",
            ended.history
        );
    }
    a.close();
    b.close();
}

/// Message 1 of a handshake written by hand, as a peer that does not use this crate's dialer.
fn open_line(params: Value) -> Vec<u8> {
    let mut line = json!({"jsonrpc": "2.0", "id": 1, "method": "stream.open", "params": params})
        .to_string()
        .into_bytes();
    line.push(b'\n');
    line
}

/// Two endpoints share one acceptor configuration, and with it its bulk credentials. A
/// credential issued on a link of one endpoint's connection is refused on the other's, also
/// when the two connections have the same number within their endpoints.
#[test]
fn a_bulk_credential_does_not_cross_to_another_endpoints_connection() {
    let server = Rendezvous::start();
    let (ka, k1, k2) = (key(68), key(69), key(70));
    let pairing = Arc::new(PairingList::new());
    pairing.add(k1.id());
    pairing.add(k2.id());
    let mut config = P2pConfig::new(ka.clone(), pairing);
    config.enabled = true;
    config.server = Some(server.info);
    let a = P2pEndpoint::bind(config).expect("bind");
    let (b1, b2) = (
        endpoint(&server, 69, ka.id()),
        endpoint(&server, 70, ka.id()),
    );
    let accept = AcceptConfig::new(k1, "acceptor-69", None, Arc::new(Policy::default()))
        .with_timeouts(timeouts());
    let kept: Arc<Mutex<Vec<Accepted>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let serving: Vec<_> = [&b1, &b2]
        .into_iter()
        .map(|b| {
            let listener = P2pListener::new(b.clone(), accept.clone()).expect("listener");
            let (kept, stop) = (Arc::clone(&kept), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(inbound) = listener.accept_timeout(Some(Duration::from_millis(100)))
                    else {
                        continue;
                    };
                    // Kept, so the link's credential stays live.
                    if let Ok(accepted) = inbound.handshake(&AttemptCancel::new()) {
                        lock_kept(&kept).push(accepted);
                    }
                }
            })
        })
        .collect();
    let dial = |b: &P2pEndpoint| {
        let started = Instant::now();
        loop {
            match a.connect(b.id()) {
                Ok(conn) => break conn,
                Err(_) if started.elapsed() < PATIENCE => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("connect: {error:?}"),
            }
        }
    };
    // An attach link on `b1`, by hand: the success ACK carries the bulk credential.
    let to_b1 = dial(&b1);
    let mut attach = to_b1.open_bi().expect("a stream");
    attach.set_read_timeout(Some(PATIENCE));
    attach
        .write_all(&open_line(json!({
            "proto": 2,
            "purpose": "attach",
            "device_id": ka.id().to_string(),
            "instance_id": "raw-dialer",
            "holder": "dialer",
            "caps": [],
        })))
        .expect("message 1");
    let ack = read_ack(&mut attach);
    assert_eq!(ack["ok"], json!(true), "{ack}");
    let credential = ack["bulk_credential"]
        .as_str()
        .expect("a credential")
        .to_owned();
    // The same credential on a connection to `b2`.
    let to_b2 = dial(&b2);
    let number = |b: &P2pEndpoint| {
        b.connections()
            .into_iter()
            .find(|info| info.peer == ka.id())
            .map(|info| info.id)
    };
    assert_eq!(
        number(&b1),
        number(&b2),
        "the case under test: both connections have the same number in their endpoint"
    );
    let mut bulk = to_b2.open_bi().expect("a stream");
    bulk.set_read_timeout(Some(PATIENCE));
    bulk.write_all(&open_line(json!({
        "proto": 2,
        "purpose": "bulk",
        "instance_id": "raw-dialer",
        "bulk_credential": credential,
    })))
    .expect("message 1");
    let ack = read_ack(&mut bulk);
    assert_eq!(ack["ok"], json!(false), "{ack}");
    assert_eq!(ack["reason"], json!("bulk_credential_invalid"), "{ack}");
    stop.store(true, Ordering::SeqCst);
    for serving in serving {
        serving.join().expect("acceptor");
    }
    drop((attach, bulk, to_b1, to_b2));
    lock_kept(&kept).clear();
    a.close();
    b1.close();
    b2.close();
}

fn lock_kept(kept: &Mutex<Vec<Accepted>>) -> std::sync::MutexGuard<'_, Vec<Accepted>> {
    kept.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// A stream open on the shared connection that waits (the peer allows one stream, which a
/// link holds) while the connection ends at the QUIC idle timeout fails without a close code;
/// the connection had ended, so the connect punches a new one instead of returning that
/// failure (docs/design/transport.md "연결 공유의 시작").
#[test]
fn a_shared_connection_that_idles_out_under_an_open_is_passed_over() {
    let (pair, gate) = Pair::start_full(
        48,
        Mode::Echo,
        true,
        &[("quic.max_bidi_streams", P2pValue::Count(1))],
        timeouts(),
    );
    let gate = gate.expect("gate");
    let mut link = pair.link(None).expect("link");
    let old = live(&pair.table_a)[0].id;
    let (mut reader, writer) = link.take_ordered().expect("halves");
    writer.write_frame(StreamTag::Data, b"one").expect("write");
    assert_eq!(reader.read_frame().expect("echo").payload, b"one");
    gate.shut.store(true, Ordering::SeqCst);
    // The peer comes back once the old connection ended, for the new punch.
    let reopen = std::thread::spawn({
        let gate = Arc::clone(&gate);
        move || {
            let failed = reader.read_frame().expect_err("the connection timed out");
            gate.shut.store(false, Ordering::SeqCst);
            failed.failure()
        }
    });
    let conn = pair.connect(&AttemptCancel::new(), &ConnectOptions::new());
    assert_eq!(
        reopen.join().expect("the reader"),
        ReadFailure::HeartbeatTimeout
    );
    let conn = conn.expect("a channel on a new connection");
    drop(conn);
    assert!(
        live(&pair.table_a).iter().all(|entry| entry.id != old),
        "{:?}",
        pair.table_a.connections()
    );
    drop((writer, link));
}

/// The cause, the ending transition's cause and the ending event (exactly one) of the dialer's
/// entry `id`, once it ended.
fn dialer_end(pair: &Pair, id: ConnectionId) -> [Option<Cause>; 3] {
    wait_until("the entry's end", || {
        entry(&pair.table_a, id).state == ConnectionState::Disconnected
    });
    let ended = entry(&pair.table_a, id);
    let history = ended.history.last().and_then(|h| h.cause.clone());
    let events: Vec<_> = pair
        .table_a
        .events()
        .into_iter()
        .filter_map(|event| match event {
            ConnectionEvent::Transition { id: at, entry }
                if at == id && entry.state == ConnectionState::Disconnected =>
            {
                Some(entry.cause)
            }
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    [
        ended.last_cause,
        history,
        events.into_iter().next().flatten(),
    ]
}

/// Two links on one connection. The first ends normally (both directions half-closed and the
/// peer's end read) while the consumer keeps its halves, so its stream keeps its place; the
/// second is then lost. When the consumer lets the first link's halves go, they give the
/// connection's last place back and the dialer closes it: the entry ends with the first
/// link's `ended`, the link that gave the last place back, though the second ended later
/// (docs/design/transport.md "필드", `last_cause`). With the halves let go before the second
/// link's loss, the second gives the last place back and the entry ends with its `lost`.
#[test]
fn the_link_that_gives_the_last_place_back_names_the_local_end() {
    for (seed, halves_first, want) in [(94, false, Cause::Ended), (96, true, Cause::Lost)] {
        // The accepting side waits long for the dialer's close, so its own close at the
        // drain cap does not come first.
        let pair = Pair::start_draining(seed, Mode::Echo, Duration::from_secs(10));
        let mut first = pair.link(None).expect("first link");
        let second = pair.link(None).expect("second link");
        let id = live(&pair.table_a)[0].id;
        let (mut reader, mut writer) = first.take_ordered().expect("halves");
        writer.half_close().expect("half close");
        assert_eq!(
            reader.read_frame().expect_err("EOF").failure(),
            ReadFailure::PeerClosed
        );
        reader.finish();
        let halves = (reader, writer, first);
        let kept = if halves_first {
            drop(halves);
            None
        } else {
            Some(halves)
        };
        second.closer().force_close(EndReason::Lost);
        if let Some(halves) = kept {
            assert_eq!(
                entry(&pair.table_a, id).state,
                ConnectionState::Connected,
                "the first link's stream keeps the connection"
            );
            drop(halves);
        }
        let want = Some(want);
        assert_eq!(
            dialer_end(&pair, id),
            [want.clone(), want.clone(), want],
            "halves first {halves_first}"
        );
        drop(second);
    }
}
