//! Tests of the bindings on loopback TCP channels, without P2P or SSH: links, bulk and
//! request channels, the error conversions, the trust store callbacks, the event stream and
//! the token boundary (docs/design/mobile.md "android 요구의 자리" #7, #7+, #7b, #8, #9).

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tasty_net_frame::{MAX_FRAME_LEN, ReadFailure};
use tasty_net_identity::{ConnectionToken, DeviceKey};
use tasty_net_p2p::{P2pCloseReason, P2pConnectError, P2pError, StopCause};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ConnectError, Connection,
    ConnectionTable, ConnectionTableConfig, DialAuth, DialConfig, DomainAccept, DomainPolicy,
    DomainReject, Endpoint, EndpointHandle, HandshakeError, MAX_HANDSHAKE_LINE, PeerInfo,
    RawChannel, RejectReason, RequestError, RequestHandler, TooOldEvidence, Transport,
    accept_handshake, read_first_line,
};

use crate::channel::{AttachLink, FrameTag, RequestChannel};
use crate::error::{HandshakeFailure, P2pConnectFailure, RequestFailure, TooOld};
use crate::net::{AttachOptions, Dialer, EventStream, RequestOptions, TastyNet, TastyNetConfig};
use crate::records::{ConnectionRecord, LinkHolder, NetEvent};
use crate::secrets::Secrets;
use crate::store::{PeerKeyStore, PeerStoreError, PinResult, reregister_with};

pub(crate) fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

/// A token whose text is easy to find in any output.
fn token() -> ConnectionToken {
    ConnectionToken::from_str(&"5e".repeat(32)).expect("token")
}

fn other_token() -> ConnectionToken {
    ConnectionToken::from_str(&"c7".repeat(32)).expect("token")
}

/// Fails when `text` shows either test token.
fn assert_no_token(what: &str, text: &str) {
    for t in [token(), other_token()] {
        assert!(
            !text.contains(t.expose_hex()),
            "{what} shows a connection token: {text}"
        );
    }
    assert!(
        !text.contains(&token_bytes()),
        "{what} shows the bytes of a connection token: {text}"
    );
}

/// The bytes of [`token`] as text: 32 times `0x5e`, which is `^` in UTF-8.
fn token_bytes() -> String {
    "^".repeat(32)
}

/// The dialing side of a loopback TCP channel, with the token of the peer's record.
struct Loopback {
    addr: SocketAddr,
    token: ConnectionToken,
}

struct LoopbackEndpoint {
    addr: SocketAddr,
    token: ConnectionToken,
}

impl EndpointHandle for LoopbackEndpoint {
    fn connect_more(&self, _cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        let stream = TcpStream::connect(self.addr).map_err(ConnectError::Io)?;
        RawChannel::from_tcp(stream).map_err(ConnectError::Io)
    }

    fn auth(&self) -> Option<DialAuth> {
        Some(DialAuth::Token(self.token.clone()))
    }
}

impl Transport for Loopback {
    type Target = ();

    fn connect(&self, _target: &(), cancel: &AttemptCancel) -> Result<Connection, ConnectError> {
        let endpoint = LoopbackEndpoint {
            addr: self.addr,
            token: self.token.clone(),
        };
        let channel = endpoint.connect_more(cancel)?;
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(endpoint)),
        })
    }
}

/// Accepts every attach, bulk and request channel.
struct Open;

impl DomainPolicy for Open {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept {
            ext: Some(r#"{"descriptor":"d"}"#.into()),
            client_id: Some(7),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }
}

/// Answers `echo` with its parameters, `big` with a result larger than one frame and `hang`
/// only after [`HANG`].
struct Echo;

/// How long `hang` keeps its caller waiting: far longer than a test waits for an end.
const HANG: Duration = Duration::from_secs(10);

impl RequestHandler for Echo {
    fn handle(&self, _peer: &PeerInfo, method: &str, params: Value) -> Result<Value, Value> {
        match method {
            "echo" => Ok(json!({ "echo": params })),
            "big" => Ok(Value::String("x".repeat(MAX_FRAME_LEN as usize))),
            "hang" => {
                std::thread::sleep(HANG);
                Ok(Value::Null)
            }
            _ => Err(json!({ "code": "unknown_method" })),
        }
    }
}

/// [`Echo`], and tells when a `hang` call came in before it keeps the caller waiting.
struct SignalsHang(Mutex<mpsc::Sender<()>>);

impl RequestHandler for SignalsHang {
    fn handle(&self, peer: &PeerInfo, method: &str, params: Value) -> Result<Value, Value> {
        if method == "hang" {
            let _told = self.0.lock().expect("sender").send(());
        }
        Echo.handle(peer, method, params)
    }
}

/// How long a test waits for something that comes in well under a second on loopback.
pub(crate) const PATIENCE: Duration = Duration::from_secs(20);

/// How often the accepting thread looks at its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// The accepting end on a loopback listener: echoes frames on links and bulk channels and
/// serves requests. It reports in `table` and sends each handshake result on. Dropping it
/// stops and joins the accepting thread.
struct Peer {
    addr: SocketAddr,
    table: ConnectionTable,
    results: mpsc::Receiver<Result<(), String>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Peer {
    /// The next handshake result, or a failure of the test after [`PATIENCE`].
    fn result(&self) -> Result<(), String> {
        self.results
            .recv_timeout(PATIENCE)
            .expect("a handshake result in time")
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

fn peer_with(table: ConnectionTable, token: ConnectionToken) -> Peer {
    peer_with_policy(table, token, Arc::new(Open))
}

fn peer_with_policy(
    table: ConnectionTable,
    token: ConnectionToken,
    policy: Arc<dyn DomainPolicy>,
) -> Peer {
    peer_as(table, token, policy, "acceptor-1", Arc::new(Echo))
}

/// A peer that answers with `instance_id` as its instance ID.
fn peer_as(
    table: ConnectionTable,
    token: ConnectionToken,
    policy: Arc<dyn DomainPolicy>,
    instance_id: &str,
    handler: Arc<dyn RequestHandler>,
) -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let config = AcceptConfig::new(key(2), instance_id, Some(token), policy)
        .with_caps(vec!["net.request".into()])
        .expect("caps")
        .with_connections(table.clone());
    let (tx, results) = mpsc::channel();
    listener.set_nonblocking(true).expect("nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Some systems hand a nonblocking listener's mode on to the stream.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let config = config.clone();
                        let tx = tx.clone();
                        let handler = Arc::clone(&handler);
                        std::thread::spawn(move || serve_one(stream, &config, &tx, &*handler));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_POLL);
                    }
                    Err(_) => return,
                }
            }
        })
    };
    Peer {
        addr,
        table,
        results,
        stop,
        thread: Some(thread),
    }
}

fn peer() -> Peer {
    peer_with(ConnectionTable::new(ConnectionTableConfig::new()), token())
}

fn serve_one(
    stream: TcpStream,
    config: &AcceptConfig,
    tx: &mpsc::Sender<Result<(), String>>,
    handler: &dyn RequestHandler,
) {
    let mut channel = RawChannel::from_tcp(stream).expect("channel");
    let deadline = Instant::now() + Duration::from_secs(5);
    let Ok(line) = read_first_line(&mut channel, MAX_HANDSHAKE_LINE, deadline) else {
        return;
    };
    match accept_handshake(channel, &line, Vec::new(), config, &AttemptCancel::new()) {
        Ok(Accepted::Link(mut link)) => {
            let _sent = tx.send(Ok(()));
            let (mut reader, writer) = link.take_ordered().expect("ordered");
            while let Ok(frame) = reader.read_frame() {
                if writer.write_frame(frame.tag, &frame.payload).is_err() {
                    break;
                }
            }
        }
        Ok(Accepted::Bulk(mut bulk)) => {
            let _sent = tx.send(Ok(()));
            while let Ok(frame) = bulk.reader.read_frame() {
                if bulk.writer.write_frame(frame.tag, &frame.payload).is_err() {
                    break;
                }
            }
            bulk.writer.finish();
        }
        Ok(Accepted::Request(server)) => {
            let _sent = tx.send(Ok(()));
            let _served = server.serve(handler);
        }
        Err(e) => {
            let _sent = tx.send(Err(e.to_string()));
        }
    }
}

fn dialer(store: Option<Arc<dyn PeerKeyStore>>) -> Dialer {
    Dialer {
        config: DialConfig::new(key(1), "dialer-1"),
        store,
        secrets: Secrets::default(),
    }
}

fn attach_options() -> AttachOptions {
    AttachOptions {
        holder: LinkHolder::Dialer,
        ext: Some(r#"{"target_workspace":1}"#.into()),
        name: Some("desk".into()),
        peer_name: None,
    }
}

fn attach(
    dialer: &Dialer,
    peer: &Peer,
    token: ConnectionToken,
    peer_name: &str,
) -> Result<AttachLink, HandshakeFailure> {
    let transport = Loopback {
        addr: peer.addr,
        token,
    };
    dialer.attach(
        &transport,
        &(),
        peer_name,
        &attach_options(),
        &AttemptCancel::new(),
    )
}

fn request(dialer: &Dialer, peer: &Peer) -> RequestChannel {
    let transport = Loopback {
        addr: peer.addr,
        token: token(),
    };
    let options = RequestOptions {
        name: None,
        peer_name: None,
    };
    dialer
        .request(&transport, &(), "desk", &options, &AttemptCancel::new())
        .expect("request channel")
}

#[test]
fn an_attach_link_carries_frames_both_ways_and_opens_bulk_channels() {
    let peer = peer();
    let link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));

    let record = link.peer();
    assert_eq!(record.device_id, key(2).id().to_string());
    assert_eq!(record.instance_id, "acceptor-1");
    assert_eq!(record.holder, Some(LinkHolder::Dialer));
    assert!(record.local_holds);
    assert!(link.has_cap("net.request".into()));
    assert!(!link.has_cap("net.other".into()));
    assert_eq!(link.client_id(), Some(7));
    assert_eq!(link.ext().as_deref(), Some(r#"{"descriptor":"d"}"#));
    assert!(!link.link_id().is_empty());

    let ordered = link.ordered();
    ordered
        .write_frame(FrameTag::Data, b"keys".to_vec())
        .expect("write");
    let frame = ordered.read_frame().expect("read");
    assert_eq!(
        (frame.tag, frame.payload),
        (FrameTag::Data, b"keys".to_vec())
    );

    let bulk = link.open_bulk(None).expect("bulk");
    assert_eq!(peer.result(), Ok(()));
    let channel = bulk.channel();
    channel
        .write_frame(FrameTag::Data, b"chunk".to_vec())
        .expect("write");
    assert_eq!(channel.read_frame().expect("read").payload, b"chunk");

    link.force_close();
    assert!(ordered.read_frame().is_err(), "a closed link reads no more");
    assert!(
        ordered
            .write_frame(FrameTag::Data, b"late".to_vec())
            .is_err()
    );
}

/// The ordered channel alone keeps the link: the attach link object is dropped in the same
/// expression (Swift's `try net.openAttach(...).ordered()`), and the bulk channel opened
/// before is dropped too.
#[test]
fn the_ordered_channel_alone_keeps_the_link() {
    let peer = peer();
    let dialer = dialer(None);
    let ordered = attach(&dialer, &peer, token(), "desk")
        .expect("attach")
        .ordered();
    assert_eq!(peer.result(), Ok(()));
    ordered
        .write_frame(FrameTag::Data, b"alone".to_vec())
        .expect("write without the link object");
    assert_eq!(ordered.read_frame().expect("read").payload, b"alone");

    let link = attach(&dialer, &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    let bulk = link.open_bulk(None).expect("bulk");
    assert_eq!(peer.result(), Ok(()));
    let ordered = link.ordered();
    let bulk_channel = bulk.channel();
    drop((link, bulk, bulk_channel));
    ordered
        .write_frame(FrameTag::Data, b"after".to_vec())
        .expect("write with only the ordered channel");
    assert_eq!(ordered.read_frame().expect("read").payload, b"after");
}

/// The frame channel of a bulk channel alone keeps the link: the attach link, its ordered
/// channel and the bulk channel object are all dropped.
#[test]
fn the_bulk_frame_channel_alone_keeps_the_link() {
    let peer = peer();
    let link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    let bulk = link.open_bulk(None).expect("bulk");
    assert_eq!(peer.result(), Ok(()));
    let channel = bulk.channel();
    let ordered = link.ordered();
    drop((link, bulk, ordered));
    channel
        .write_frame(FrameTag::Data, b"bulk".to_vec())
        .expect("write with only the bulk channel");
    assert_eq!(channel.read_frame().expect("read").payload, b"bulk");
}

/// After a `Detach` the app marks its reading half as ended, and the normal end then does not
/// wait out the drain cap.
#[test]
fn finish_reading_lets_the_normal_end_finish_without_the_drain_cap() {
    let peer = peer();
    let link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    let ordered = link.ordered();
    ordered
        .write_frame(FrameTag::Detach, Vec::new())
        .expect("write");
    assert_eq!(ordered.read_frame().expect("read").tag, FrameTag::Detach);
    ordered.finish_reading();
    assert!(matches!(
        ordered.read_frame(),
        Err(crate::error::ChannelFailure::AlreadyClosed)
    ));
    let started = Instant::now();
    assert_eq!(ordered.finish(), crate::channel::FinishResult::ReaderEnded);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_request_channel_shows_the_peer_caps_and_keeps_response_too_large() {
    let peer = peer();
    let channel = request(&dialer(None), &peer);
    assert_eq!(peer.result(), Ok(()));
    assert_eq!(channel.caps(), vec!["net.request".to_string()]);
    assert!(channel.has_cap("net.request".into()));

    let answer = channel
        .call("echo".into(), r#"{"a":1}"#.into(), Duration::from_secs(5))
        .expect("echo");
    assert_eq!(
        serde_json::from_str::<Value>(&answer).expect("json"),
        json!({ "echo": { "a": 1 } })
    );
    match channel.call("nope".into(), "null".into(), Duration::from_secs(5)) {
        Err(RequestFailure::Remote { error_json }) => {
            assert_eq!(
                serde_json::from_str::<Value>(&error_json).expect("json"),
                json!({ "code": "unknown_method" })
            );
        }
        other => panic!("expected a remote error: {other:?}"),
    }
    assert!(matches!(
        channel.call("big".into(), "null".into(), Duration::from_secs(5)),
        Err(RequestFailure::ResponseTooLarge)
    ));
    assert!(matches!(
        channel.call("echo".into(), "not json".into(), Duration::from_secs(5)),
        Err(RequestFailure::InvalidParams { .. })
    ));
    channel.finish();
    assert!(matches!(
        channel.call("echo".into(), "1".into(), Duration::from_secs(5)),
        Err(RequestFailure::AlreadyClosed)
    ));
}

/// `finish` with a call running on another thread ends that call at once, and the channel
/// takes no call after it.
#[test]
fn finish_ends_a_running_call_and_refuses_new_ones() {
    let (started_tx, started) = mpsc::channel();
    let peer = peer_as(
        ConnectionTable::new(ConnectionTableConfig::new()),
        token(),
        Arc::new(Open),
        "acceptor-1",
        Arc::new(SignalsHang(Mutex::new(started_tx))),
    );
    let channel = Arc::new(request(&dialer(None), &peer));
    assert_eq!(peer.result(), Ok(()));
    let running = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || {
            let started = Instant::now();
            let answer = channel.call("hang".into(), "null".into(), Duration::from_secs(30));
            (answer, started.elapsed())
        })
    };
    // The peer has the request: the call wrote it and waits for the answer.
    started
        .recv_timeout(PATIENCE)
        .expect("the peer got the call");
    let started = Instant::now();
    channel.finish();
    assert!(started.elapsed() < HANG / 2, "{:?}", started.elapsed());
    let (answer, waited) = running.join().expect("call thread");
    assert!(
        matches!(answer, Err(RequestFailure::Closed { .. })),
        "{answer:?}"
    );
    assert!(waited < HANG / 2, "the call waited {waited:?}");
    assert!(matches!(
        channel.call("echo".into(), "1".into(), Duration::from_secs(5)),
        Err(RequestFailure::AlreadyClosed)
    ));
    channel.finish();
}

/// Both paths of a peer too old for v2 keep their evidence in one FFI variant.
#[test]
fn peer_too_old_keeps_its_evidence_on_both_paths() {
    let discovery = ConnectError::PeerTooOld {
        evidence: TooOldEvidence::PortOnlyDiscovery,
    };
    let cases = [
        (
            HandshakeFailure::from(&discovery),
            TooOld::PortOnlyDiscovery,
        ),
        (
            HandshakeFailure::from(&HandshakeError::Connect(discovery)),
            TooOld::PortOnlyDiscovery,
        ),
        (
            HandshakeFailure::from(&HandshakeError::PeerTooOld {
                evidence: TooOldEvidence::V1Ack,
            }),
            TooOld::V1Ack,
        ),
    ];
    for (failure, want) in cases {
        match failure {
            HandshakeFailure::PeerTooOld { evidence } => assert_eq!(evidence, want),
            other => panic!("expected PeerTooOld: {other:?}"),
        }
    }
}

fn every_p2p_connect_error() -> Vec<P2pConnectError> {
    let mut all = vec![
        P2pConnectError::Disabled,
        P2pConnectError::RendezvousUnavailable { stopped: None },
        P2pConnectError::RendezvousLost,
        P2pConnectError::RequestTimeout,
        P2pConnectError::PeerNotFound,
        P2pConnectError::Declined,
        P2pConnectError::NatUnsupported,
        P2pConnectError::PeerLimitsInvalid,
        P2pConnectError::PunchFailed,
        P2pConnectError::Busy,
        P2pConnectError::ServerLimitsInvalid { key: "punch.x" },
        P2pConnectError::PeerIdentityMismatch,
        P2pConnectError::SelfAttach,
        P2pConnectError::AlpnMismatch,
        P2pConnectError::Cancelled,
    ];
    for stop in [
        StopCause::Superseded,
        StopCause::AccessDenied,
        StopCause::UnsupportedProto,
        StopCause::AlpnMismatch,
        StopCause::ServerKeyMismatch,
        StopCause::ReconnectExhausted,
    ] {
        all.push(P2pConnectError::RendezvousUnavailable {
            stopped: Some(stop),
        });
    }
    // The reason is a rendezvous wire type; it is read here from its wire form.
    for reason in ["peer_gone", "expired", "superseded", "peer_limits_invalid"] {
        all.push(P2pConnectError::PunchCancelled {
            reason: serde_json::from_value(json!(reason)).expect("reason"),
        });
    }
    all
}

/// `is_retryable()` of every FFI error equals the core's for the error it came from.
#[test]
fn is_retryable_equals_the_core_answer() {
    let mut errors: Vec<HandshakeError> = vec![
        HandshakeError::Rejected {
            reason: RejectReason::Busy,
            error: "busy".into(),
            ext: None,
        },
        HandshakeError::Rejected {
            reason: RejectReason::TokenInvalid,
            error: "no".into(),
            ext: None,
        },
        HandshakeError::Rejected {
            reason: RejectReason::Unknown("new".into()),
            error: "?".into(),
            ext: None,
        },
        HandshakeError::Refused(RejectReason::Busy),
        HandshakeError::PeerTooOld {
            evidence: TooOldEvidence::V1Ack,
        },
        HandshakeError::SignatureInvalid,
        HandshakeError::SelfAttach,
        HandshakeError::PeerKeyMismatch {
            expected: key(3).id(),
            got: key(4).id(),
        },
        HandshakeError::PeerMismatch,
        HandshakeError::Protocol("x"),
        HandshakeError::TimedOut,
        HandshakeError::Cancelled,
        HandshakeError::PeerClosed,
        HandshakeError::InvalidTimeouts,
        HandshakeError::MessageTooLong,
        HandshakeError::NoCredential,
        HandshakeError::NotBulkCapable,
        HandshakeError::ParentEnded,
        HandshakeError::Store(tasty_net_identity::StoreError::new("disk full")),
        HandshakeError::Io(io::Error::other("io")),
        HandshakeError::Connect(ConnectError::Cancelled),
        HandshakeError::Connect(ConnectError::Io(io::Error::other("refused"))),
        HandshakeError::Connect(ConnectError::SelfAttach),
        HandshakeError::Connect(ConnectError::P2p(P2pError::TimedOut)),
        HandshakeError::Connect(ConnectError::P2p(P2pError::Io(io::Error::other("udp")))),
        HandshakeError::Connect(ConnectError::P2p(P2pError::Disabled)),
        HandshakeError::Connect(ConnectError::P2p(P2pError::Closed)),
    ];
    for code in [0, 1, 4, 99] {
        errors.push(HandshakeError::Closed(P2pCloseReason::from_code(code)));
    }
    for e in every_p2p_connect_error() {
        errors.push(HandshakeError::Connect(ConnectError::P2p(
            P2pError::Connect(e),
        )));
        let ffi = P2pConnectFailure::from(&e);
        assert_eq!(
            HandshakeFailure::P2pConnect {
                failure: ffi,
                detail: String::new()
            }
            .is_retryable(),
            e.is_retryable(),
            "{e:?}"
        );
    }
    for e in &errors {
        let ffi = HandshakeFailure::from(e);
        assert_eq!(ffi.is_retryable(), e.is_retryable(), "{e:?} -> {ffi:?}");
        assert!(
            !matches!(ffi, HandshakeFailure::Other { .. }),
            "{e:?} has its own variant"
        );
    }
}

/// Failures the core tells apart stay apart as values: the P2P endpoint failures by kind and
/// the server's reason for a cancelled punch.
#[test]
fn p2p_failures_keep_their_kind_and_reason() {
    // The crate root names every type the public errors carry.
    use crate::{P2pFailureKind, PunchCancelReason, SetupFailure};

    let kinds = [
        (P2pError::Disabled, P2pFailureKind::Disabled),
        (P2pError::NoServer, P2pFailureKind::NoServer),
        (P2pError::EndpointInUse, P2pFailureKind::EndpointInUse),
        (P2pError::InAsyncContext, P2pFailureKind::InAsyncContext),
        (P2pError::Closed, P2pFailureKind::Closed),
    ];
    for (core, want) in kinds {
        match HandshakeFailure::from(&ConnectError::P2p(core)) {
            HandshakeFailure::P2pUnavailable { kind, .. } => assert_eq!(kind, want),
            other => panic!("expected P2pUnavailable {want:?}: {other:?}"),
        }
    }
    let in_use = SetupFailure::from(&P2pError::EndpointInUse);
    let io = SetupFailure::from(&P2pError::Io(io::Error::other("bind")));
    assert!(matches!(
        in_use,
        SetupFailure::P2p {
            kind: P2pFailureKind::EndpointInUse,
            ..
        }
    ));
    assert!(matches!(
        io,
        SetupFailure::P2p {
            kind: P2pFailureKind::Io,
            ..
        }
    ));

    let cancelled = P2pConnectError::PunchCancelled {
        reason: serde_json::from_value(json!("superseded")).expect("reason"),
    };
    assert_eq!(
        P2pConnectFailure::from(&cancelled),
        P2pConnectFailure::PunchCancelled {
            reason: PunchCancelReason::Superseded
        }
    );
}

/// The text of an error is the core sentence of the error it came from; the I/O text stays
/// in the `detail` field and out of the text.
#[test]
fn error_texts_are_core_sentences_without_io_text() {
    let io_text = "raw os error 111 at 10.0.0.9";
    let handshake = [
        HandshakeError::Io(io::Error::other(io_text)),
        HandshakeError::Connect(ConnectError::Io(io::Error::other(io_text))),
        HandshakeError::Store(tasty_net_identity::StoreError::new(io_text)),
    ];
    for core in &handshake {
        let ffi = HandshakeFailure::from(core);
        assert_eq!(ffi.to_string(), core.to_string(), "{ffi:?}");
        assert!(!ffi.to_string().contains(io_text));
        assert!(format!("{ffi:?}").contains(io_text), "the detail keeps it");
    }
    let p2p_io = ConnectError::P2p(P2pError::Io(io::Error::other(io_text)));
    let ffi = HandshakeFailure::from(&p2p_io);
    assert!(matches!(ffi, HandshakeFailure::P2pIo { .. }));
    assert_eq!(ffi.to_string(), p2p_io.to_string());
    assert!(format!("{ffi:?}").contains(io_text), "the detail keeps it");
    let request = RequestError::Io(io::Error::other(io_text));
    let ffi = RequestFailure::from(&request);
    assert_eq!(ffi.to_string(), request.to_string());
    assert!(!ffi.to_string().contains(io_text));
    for core in [
        HandshakeError::TimedOut,
        HandshakeError::Connect(ConnectError::P2p(P2pError::TimedOut)),
        HandshakeError::Rejected {
            reason: RejectReason::Busy,
            error: "busy".into(),
            ext: None,
        },
    ] {
        let ffi = HandshakeFailure::from(&core);
        assert!(!ffi.to_string().is_empty(), "{ffi:?}");
    }
}

/// Every P2P endpoint error that `open_attach` and `open_request` can get.
fn every_p2p_error() -> Vec<P2pError> {
    let mut all = vec![
        P2pError::Disabled,
        P2pError::Settings(tasty_net_p2p::settings::SettingsError::UnknownKey(
            "p2p.nothing".into(),
        )),
        P2pError::NoServer,
        P2pError::EndpointInUse,
        P2pError::InAsyncContext,
        P2pError::Closed,
        P2pError::TimedOut,
        P2pError::Io(io::Error::other("udp send failed")),
        P2pError::Tls(tasty_net_rendezvous::tls::TlsError::InvalidKey),
    ];
    all.extend(every_p2p_connect_error().into_iter().map(P2pError::Connect));
    all
}

/// A P2P failure of the dial path shows the core sentence of `ConnectError::P2p`, whatever
/// the P2P error, also inside `HandshakeError::Connect`; the kind and the core text stay in
/// the fields.
#[test]
fn p2p_dial_failures_show_the_dial_sentence() {
    for (core, again) in every_p2p_error().into_iter().zip(every_p2p_error()) {
        let text = core.to_string();
        let kind = crate::error::P2pFailureKind::from(&core);
        let dial = ConnectError::P2p(core);
        let ffi = HandshakeFailure::from(&dial);
        assert_eq!(ffi.to_string(), dial.to_string(), "{ffi:?}");
        let wrapped = HandshakeError::Connect(ConnectError::P2p(again));
        assert_eq!(
            HandshakeFailure::from(&wrapped).to_string(),
            dial.to_string()
        );
        match &ffi {
            HandshakeFailure::P2pConnect { detail, .. } => {
                assert_eq!(kind, crate::error::P2pFailureKind::Connect);
                assert_eq!(*detail, text.trim_start_matches("P2P connection failed: "));
            }
            HandshakeFailure::P2pUnavailable { kind: got, detail } => {
                assert_eq!(*got, kind);
                assert_eq!(*detail, text);
            }
            HandshakeFailure::P2pIo { detail } => assert!(text.contains(detail.as_str())),
            HandshakeFailure::P2pTimedOut => {
                assert_eq!(kind, crate::error::P2pFailureKind::TimedOut);
            }
            other => panic!("not a P2P variant: {other:?}"),
        }
    }
}

/// The app's store error text comes back inside the handshake failure.
#[test]
fn a_store_failure_keeps_the_app_text() {
    let ffi = HandshakeFailure::from(&HandshakeError::Store(tasty_net_identity::StoreError::new(
        PeerStoreError::Failed {
            reason: "the database is locked".into(),
        },
    )));
    match &ffi {
        HandshakeFailure::Store { detail } => assert_eq!(detail, "the database is locked"),
        other => panic!("expected Store: {other:?}"),
    }
}

#[test]
fn request_errors_keep_their_variants() {
    let cases = [
        (RequestError::ResponseTooLarge, "ResponseTooLarge"),
        (RequestError::TimedOut, "TimedOut"),
        (RequestError::Closed(ReadFailure::PeerClosed), "Closed"),
        (RequestError::Protocol("both"), "Protocol"),
        (RequestError::Remote(json!({"code": 1})), "Remote"),
        (RequestError::Io(io::Error::other("w")), "Io"),
    ];
    for (core, want) in cases {
        let ffi = RequestFailure::from(&core);
        assert!(format!("{ffi:?}").starts_with(want), "{core:?} -> {ffi:?}");
    }
}

/// The app's store, recording every call. `fail` makes every call fail.
#[derive(Default)]
struct Recorder {
    records: Mutex<HashMap<String, String>>,
    calls: Mutex<Vec<String>>,
    fail: bool,
}

impl Recorder {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn failure(&self) -> Result<(), PeerStoreError> {
        if self.fail {
            Err(PeerStoreError::Failed {
                reason: "store offline".into(),
            })
        } else {
            Ok(())
        }
    }
}

impl PeerKeyStore for Recorder {
    fn get(&self, peer: String) -> Result<Option<String>, PeerStoreError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("get {peer}"));
        self.failure()?;
        Ok(self.records.lock().expect("records").get(&peer).cloned())
    }

    fn pin_if_absent(&self, peer: String, device_id: String) -> Result<PinResult, PeerStoreError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("pin {peer} {device_id}"));
        self.failure()?;
        let mut records = self.records.lock().expect("records");
        Ok(match records.get(&peer) {
            Some(existing) => PinResult::Existing {
                device_id: existing.clone(),
            },
            None => {
                records.insert(peer, device_id);
                PinResult::Inserted
            }
        })
    }

    fn replace(&self, peer: String, device_id: String) -> Result<(), PeerStoreError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("replace {peer} {device_id}"));
        self.failure()?;
        self.records
            .lock()
            .expect("records")
            .insert(peer, device_id);
        Ok(())
    }
}

/// First sight pins through the app's store; a changed key ends the connection without
/// `replace`; only the user's re-registration replaces, and then the link opens.
#[test]
fn the_trust_store_pins_once_and_replaces_only_on_reregistration() {
    let peer = peer();
    let store = Arc::new(Recorder::default());
    let dyn_store: Arc<dyn PeerKeyStore> = store.clone();
    let real = key(2).id().to_string();
    let d = dialer(Some(Arc::clone(&dyn_store)));

    attach(&d, &peer, token(), "desk").expect("first sight");
    assert_eq!(store.calls(), vec![format!("pin desk {real}")]);

    let stale = key(9).id().to_string();
    store
        .records
        .lock()
        .expect("records")
        .insert("desk".into(), stale.clone());
    match attach(&d, &peer, token(), "desk") {
        Err(HandshakeFailure::PeerKeyMismatch { expected, got }) => {
            assert_eq!((expected, got), (stale.clone(), real.clone()));
        }
        other => panic!("expected PeerKeyMismatch: {other:?}"),
    }
    assert!(
        !store.calls().iter().any(|c| c.starts_with("replace")),
        "the library never replaces by itself: {:?}",
        store.calls()
    );

    reregister_with(&dyn_store, "desk", &real).expect("reregister");
    assert_eq!(
        store.calls().last().map(String::as_str),
        Some(format!("replace desk {real}").as_str())
    );
    attach(&d, &peer, token(), "desk").expect("after re-registration");

    let failing: Arc<dyn PeerKeyStore> = Arc::new(Recorder {
        fail: true,
        ..Recorder::default()
    });
    match attach(&dialer(Some(failing)), &peer, token(), "desk") {
        Err(HandshakeFailure::Store { detail }) => assert_eq!(detail, "store offline"),
        other => panic!("expected Store: {other:?}"),
    }
}

/// Two first connections to one peer name at once: the library asks the app's store once per
/// connection and holds no lock of its own around it, so with a store whose `pin_if_absent` is
/// atomic exactly one of them records the key and both links come up.
#[test]
fn two_first_connections_at_once_record_the_key_once() {
    #[derive(Default)]
    struct Counting {
        inner: Recorder,
        inserted: AtomicU64,
    }
    impl PeerKeyStore for Counting {
        fn get(&self, peer: String) -> Result<Option<String>, PeerStoreError> {
            self.inner.get(peer)
        }
        fn pin_if_absent(
            &self,
            peer: String,
            device_id: String,
        ) -> Result<PinResult, PeerStoreError> {
            // Widen the window in which the two calls overlap.
            std::thread::sleep(Duration::from_millis(50));
            let result = self.inner.pin_if_absent(peer, device_id)?;
            if result == PinResult::Inserted {
                self.inserted.fetch_add(1, Ordering::SeqCst);
            }
            Ok(result)
        }
        fn replace(&self, peer: String, device_id: String) -> Result<(), PeerStoreError> {
            self.inner.replace(peer, device_id)
        }
    }

    let peer = peer();
    let store = Arc::new(Counting::default());
    let dyn_store: Arc<dyn PeerKeyStore> = store.clone();
    let d = Arc::new(dialer(Some(dyn_store)));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (d, addr) = (Arc::clone(&d), peer.addr);
            std::thread::spawn(move || {
                let transport = Loopback {
                    addr,
                    token: token(),
                };
                d.attach(
                    &transport,
                    &(),
                    "desk",
                    &attach_options(),
                    &AttemptCancel::new(),
                )
                .map(|_| ())
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("thread").expect("attach");
    }
    assert_eq!(store.inserted.load(Ordering::SeqCst), 1);
    let pins = store
        .inner
        .calls()
        .iter()
        .filter(|c| c.starts_with("pin desk"))
        .count();
    assert_eq!(pins, 2, "{:?}", store.inner.calls());
}

/// A callback that fails in an unexpected way on the foreign side (UniFFI's
/// `UnexpectedUniFFICallbackError`, such as a Kotlin exception of another type) comes back as
/// the store's failure with its text, and the connection ends with `Store`.
#[test]
fn an_unexpected_callback_error_becomes_a_store_failure() {
    let unexpected = uniffi::UnexpectedUniFFICallbackError::new("IllegalStateException: closed");
    match PeerStoreError::from(unexpected) {
        PeerStoreError::Failed { reason } => assert_eq!(reason, "IllegalStateException: closed"),
    }
    let ffi = HandshakeFailure::from(&HandshakeError::Store(tasty_net_identity::StoreError::new(
        PeerStoreError::from(uniffi::UnexpectedUniFFICallbackError::new("boom")),
    )));
    assert!(matches!(&ffi, HandshakeFailure::Store { detail } if detail == "boom"));
}

fn stream(table: &ConnectionTable, poll: Duration) -> EventStream {
    EventStream::new(table.clone(), poll, Secrets::default())
}

/// The accepting end's table reports through the stream; an empty stream waits out the
/// timeout and gives none.
#[test]
fn the_event_stream_gives_the_table_events_and_waits_out_the_timeout() {
    let peer = peer();
    let events = stream(&peer.table, Duration::from_millis(5));
    let _link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));

    let first = events.next(Duration::from_secs(5)).expect("an event");
    let id = match &first {
        NetEvent::Changed { id, .. } => *id,
        other => panic!("expected a transition: {other:?}"),
    };
    while events.next(Duration::from_millis(200)).is_some() {}
    let started = Instant::now();
    assert!(events.next(Duration::from_millis(100)).is_none());
    assert!(started.elapsed() >= Duration::from_millis(100));

    let records: Vec<ConnectionRecord> = peer
        .table
        .connections()
        .iter()
        .map(ConnectionRecord::from)
        .collect();
    let record = records.iter().find(|r| r.id == id).expect("the entry");
    assert_eq!(record.links.len(), 1);
    assert_eq!(
        record.peer.device_id.as_deref(),
        Some(key(1).id().to_string().as_str())
    );
}

/// A timeout too large for a point in time keeps the wait going until an event comes, and a
/// timeout just over the time already waited ends the call.
#[test]
fn a_huge_timeout_still_returns_the_next_event() {
    let peer = peer();
    let events = Arc::new(stream(&peer.table, Duration::from_millis(5)));
    assert!(Instant::now().checked_add(Duration::MAX).is_none());
    let waiting = {
        let events = Arc::clone(&events);
        std::thread::spawn(move || events.next(Duration::MAX))
    };
    std::thread::sleep(Duration::from_millis(100));
    let _link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    assert!(matches!(
        waiting.join().expect("waiting thread"),
        Some(NetEvent::Changed { .. })
    ));
    while events.next(Duration::from_millis(200)).is_some() {}
    let started = Instant::now();
    assert!(events.next(Duration::from_millis(30)).is_none());
    assert!(started.elapsed() >= Duration::from_millis(30));
}

/// A clock that does not wait: a sleep moves its time on.
struct FakeClock {
    now: Duration,
    sleeps: Vec<Duration>,
}

/// More sleeps than any test here needs: a wait that does not count down ends the test.
const MAX_SLEEPS: usize = 1000;

impl crate::net::Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        self.now
    }

    fn sleep(&mut self, duration: Duration) {
        assert!(
            self.sleeps.len() < MAX_SLEEPS,
            "the wait does not count down: {:?}",
            &self.sleeps[..3]
        );
        self.sleeps.push(duration);
        self.now = self.now.saturating_add(duration);
    }
}

/// With a timeout too large for a point in time, the time left still shrinks by the time
/// waited and reaches zero: the stream sleeps the poll interval while more is left, then
/// only what is left, and returns none at the bound.
#[test]
fn a_huge_timeout_counts_down_to_zero() {
    let peer = peer();
    let poll = Duration::from_millis(5);
    let events = stream(&peer.table, poll);
    let half = poll / 2;
    let mut clock = FakeClock {
        now: Duration::MAX - poll * 2 - half,
        sleeps: Vec::new(),
    };
    assert!(events.next_with(Duration::MAX, &mut clock).is_none());
    assert_eq!(clock.sleeps, vec![poll, poll, half]);
    assert_eq!(clock.now, Duration::MAX);

    let mut fresh = FakeClock {
        now: Duration::ZERO,
        sleeps: Vec::new(),
    };
    let timeout = poll * 3;
    assert!(events.next_with(timeout, &mut fresh).is_none());
    assert_eq!(fresh.sleeps, vec![poll, poll, poll]);
}

/// Events nobody read are dropped and the stream says how many.
#[test]
fn unread_events_come_back_as_lagged() {
    let peer = peer_with(
        ConnectionTable::new(ConnectionTableConfig::new().with_event_queue_limit(1)),
        token(),
    );
    let events = stream(&peer.table, Duration::from_millis(5));
    for _ in 0..3 {
        attach(&dialer(None), &peer, token(), "desk").expect("attach");
        assert_eq!(peer.result(), Ok(()));
    }
    match events.next(Duration::from_secs(5)) {
        Some(NetEvent::Lagged { dropped }) => assert!(dropped > 0),
        other => panic!("expected Lagged: {other:?}"),
    }
}

/// No record, error, event, `Debug` or `Display` output of the bindings shows the token: on
/// a link that came up and on a refusal for a wrong token.
#[test]
fn no_output_of_the_bindings_shows_the_connection_token() {
    let peer = peer();
    let events = stream(&peer.table, Duration::from_millis(5));
    let link = attach(&dialer(None), &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    let request = request(&dialer(None), &peer);
    assert_eq!(peer.result(), Ok(()));

    let refused = attach(&dialer(None), &peer, other_token(), "desk").expect_err("wrong token");
    assert!(matches!(
        &refused,
        HandshakeFailure::Rejected {
            reason: crate::error::Rejection::TokenInvalid,
            ..
        }
    ));
    let _refusal = peer.result();

    let mut texts = vec![
        ("link", format!("{link:?}")),
        ("link peer", format!("{:?}", link.peer())),
        ("ordered", format!("{:?}", link.ordered())),
        ("request", format!("{request:?}")),
        ("refusal debug", format!("{refused:?}")),
        ("refusal text", refused.to_string()),
    ];
    while let Some(event) = events.next(Duration::from_millis(200)) {
        texts.push(("event", format!("{event:?}")));
    }
    for info in peer.table.connections() {
        texts.push(("record", format!("{:?}", ConnectionRecord::from(&info))));
    }
    assert!(texts.iter().any(|(what, _)| *what == "record"));
    for (what, text) in &texts {
        assert_no_token(what, text);
    }
}

/// The P2P attempt and the NAT judgement of an entry come through to its record.
#[test]
fn a_p2p_entry_shows_its_attempt_and_nat_judgement() {
    // The crate root names every type the public records carry.
    use crate::{
        AttemptRecord, CandidateRecord, ConnectionVia, NatMappingKind, NatRecord, PunchModeKind,
    };
    use tasty_net_p2p::{AddressPair, AttemptInfo, NatInfo, RemoteCandidate, RemoteKind};
    use tasty_net_transport::Via;

    let remote: SocketAddr = "192.0.2.7:4100".parse().expect("addr");
    let local: SocketAddr = "0.0.0.0:4200".parse().expect("addr");
    let attempt = AttemptInfo {
        number: 2,
        retry_in: Some(Duration::from_millis(1500)),
        candidates: vec![RemoteCandidate {
            addr: remote,
            kind: RemoteKind::PeerReflexive,
        }],
        handshake_pair: Some(AddressPair {
            local: Some(local),
            remote,
        }),
        handshake_deadline_in: Some(Duration::from_secs(3)),
        ..AttemptInfo::default()
    };
    assert_eq!(
        AttemptRecord::from(&attempt),
        AttemptRecord {
            number: 2,
            retry_in: Some(Duration::from_millis(1500)),
            bound_in: None,
            candidates: vec![CandidateRecord {
                addr: "192.0.2.7:4100".into(),
                kind: "peer_reflexive".into(),
            }],
            window_end_in: None,
            handshake_local: Some("0.0.0.0:4200".into()),
            handshake_remote: Some("192.0.2.7:4100".into()),
            handshake_deadline_in: Some(Duration::from_secs(3)),
        }
    );

    // The NAT values are the rendezvous wire types; they are read here from their wire form.
    let nat = NatInfo {
        local: serde_json::from_value(json!("independent_port_only")).expect("mapping"),
        peer: serde_json::from_value(json!("dependent")).expect("mapping"),
        mode: serde_json::from_value(json!("host_only")).expect("mode"),
    };
    let via = Via::Rendezvous {
        server: None,
        candidate: Some(RemoteKind::Host),
        nat: Some(nat),
    };
    assert_eq!(
        ConnectionVia::from(&via),
        ConnectionVia::Rendezvous {
            server_addr: None,
            server_key: None,
            candidate: Some("host".into()),
            nat: Some(NatRecord {
                local: NatMappingKind::IndependentPortOnly,
                peer: NatMappingKind::Dependent,
                mode: PunchModeKind::HostOnly,
            }),
        }
    );
}

/// Accepts attach links with an `ext` that carries the dialer's token back.
struct EchoesToken;

impl DomainPolicy for EchoesToken {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept {
            ext: Some(format!(r#"{{"seen":"{}"}}"#, token().expose_hex())),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Err(DomainReject::new(token().expose_hex().to_uppercase()))
    }
}

/// A peer that reads the dialer's first message and answers with a refusing ACK whose
/// sentence, reason and `ext` all carry the dialer's token. It ends as the shared [`Peer`]
/// does: a stop flag, a nonblocking accept and a join on drop; every read has a deadline.
struct RefusingPeer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RefusingPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

fn refuses_with_the_token() -> RefusingPeer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let stream = loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_POLL);
                    }
                    Err(_) => return,
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(PATIENCE)).is_err()
            {
                return;
            }
            refuse_with_the_token(stream);
        })
    };
    RefusingPeer {
        addr,
        stop,
        thread: Some(thread),
    }
}

/// Reads the first line, writes the refusing ACK and reads until the dialer closes.
fn refuse_with_the_token(stream: TcpStream) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = io::BufReader::new(clone);
    let mut line = String::new();
    if io::BufRead::read_line(&mut reader, &mut line).is_err() {
        return;
    }
    let hex = token().expose_hex().to_string();
    let ack = json!({
        "ok": false,
        "proto": 2,
        "error": format!("your token {hex} is no good"),
        "reason": format!("bad_{hex}"),
        "ext": format!(r#"{{"token":"{}"}}"#, hex.to_uppercase()),
    });
    let mut stream = stream;
    if tasty_net_frame::write_frame(
        &mut stream,
        tasty_net_frame::StreamTag::Control,
        ack.to_string().as_bytes(),
    )
    .is_err()
    {
        return;
    }
    // Keep the connection until the dialer has read the refusal and closed; the read
    // timeout bounds it.
    let _rest = io::Read::read_to_end(&mut reader, &mut Vec::new());
}

/// A peer that sends the dialer's token back, in a refusal or in the `ext` of a success, does
/// not get it into an error, its `Display`, a link's `ext`, a cause or an event.
#[test]
fn a_token_the_peer_sends_back_is_masked() {
    let secrets = Secrets::default();
    let dialer = Dialer {
        config: DialConfig::new(key(1), "dialer-1"),
        store: None,
        secrets: secrets.clone(),
    };
    let refusing_peer = refuses_with_the_token();
    let refusing = Loopback {
        addr: refusing_peer.addr,
        token: token(),
    };
    let refused = dialer
        .attach(
            &refusing,
            &(),
            "desk",
            &attach_options(),
            &AttemptCancel::new(),
        )
        .expect_err("refused");
    match &refused {
        HandshakeFailure::Rejected {
            reason: crate::error::Rejection::Unknown { code },
            peer_text,
            ext,
        } => {
            assert_eq!(code, "bad_[token]");
            assert_eq!(peer_text, "your token [token] is no good");
            assert_eq!(ext.as_deref(), Some(r#"{"token":"[token]"}"#));
        }
        other => panic!("expected an unknown refusal: {other:?}"),
    }

    let peer = peer_with_policy(
        ConnectionTable::new(ConnectionTableConfig::new()),
        token(),
        Arc::new(EchoesToken),
    );
    let link = attach(&dialer, &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    assert_eq!(link.ext().as_deref(), Some(r#"{"seen":"[token]"}"#));
    let bulk = link.open_bulk(None).expect_err("bulk refused");

    let cause = crate::records::ConnectionCause::from(&tasty_net_transport::Cause::Rejected(
        RejectReason::Unknown(format!("bad_{}", token().expose_hex())),
    ))
    .scrubbed(&secrets);
    let transition = crate::records::Transition::from(&tasty_net_transport::HistoryEntry {
        at: std::time::SystemTime::now(),
        state: tasty_net_transport::ConnectionState::Disconnected,
        phase: None,
        cause: Some(tasty_net_transport::Cause::Rejected(RejectReason::Unknown(
            token().expose_hex().to_string(),
        ))),
    })
    .scrubbed(&secrets);

    let texts = [
        ("refusal debug", format!("{refused:?}")),
        ("refusal text", refused.to_string()),
        ("link", format!("{link:?}")),
        ("bulk refusal debug", format!("{bulk:?}")),
        ("bulk refusal text", bulk.to_string()),
        ("cause", format!("{cause:?}")),
        ("event", format!("{transition:?}")),
    ];
    for (what, text) in &texts {
        assert_no_token(what, text);
        assert_no_token(what, &text.to_lowercase());
    }
}

/// A peer that proves its key in a successful handshake but gives the dialer's token as its
/// instance ID, in hex or as the token's bytes, does not get it into a peer record, a
/// connection record or a `Debug` text.
#[test]
fn a_token_the_peer_gives_as_its_instance_id_is_masked() {
    for instance_id in [token().expose_hex().to_string(), token_bytes()] {
        peer_instance_id_is_masked(&instance_id);
    }
}

fn peer_instance_id_is_masked(instance_id: &str) {
    let secrets = Secrets::default();
    let dialer = Dialer {
        config: DialConfig::new(key(1), "dialer-1"),
        store: None,
        secrets: secrets.clone(),
    };
    let peer = peer_as(
        ConnectionTable::new(ConnectionTableConfig::new()),
        token(),
        Arc::new(Open),
        instance_id,
        Arc::new(Echo),
    );
    let link = attach(&dialer, &peer, token(), "desk").expect("attach");
    assert_eq!(peer.result(), Ok(()));
    assert_eq!(link.peer().instance_id, Secrets::MASK);
    let bulk = link.open_bulk(None).expect("bulk");
    assert_eq!(peer.result(), Ok(()));
    assert_eq!(bulk.peer().instance_id, Secrets::MASK);
    let request = request(&dialer, &peer);
    assert_eq!(peer.result(), Ok(()));
    assert_eq!(request.peer().instance_id, Secrets::MASK);

    // The connection list copies the instance ID the handshake gave; the record of a
    // `TastyNet` goes through the same scrub.
    let info = peer
        .table
        .connections()
        .into_iter()
        .next()
        .expect("an entry");
    let mut record = ConnectionRecord::from(&info);
    record.peer.instance_id = Some(format!(
        "{instance_id} {}",
        token().expose_hex().to_uppercase()
    ));
    let record = record.scrubbed(&secrets);
    assert_eq!(record.peer.instance_id.as_deref(), Some("[token] [token]"));

    let texts = [
        ("link", format!("{link:?}")),
        ("ordered", format!("{:?}", link.ordered())),
        ("bulk", format!("{bulk:?}")),
        ("bulk channel", format!("{:?}", bulk.channel())),
        ("request", format!("{request:?}")),
        ("record", format!("{record:?}")),
    ];
    for (what, text) in &texts {
        assert_no_token(what, text);
        assert_no_token(what, &text.to_lowercase());
    }
}

static DIRS: AtomicU64 = AtomicU64::new(0);

/// A new directory under the system temporary directory, removed on drop.
pub(crate) struct TempDir(pub(crate) std::path::PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("tasty-net-ffi-test-{}-{n}", std::process::id()));
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _removed = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &TempDir) -> TastyNetConfig {
    TastyNetConfig {
        key_dir: dir.0.join("keys").to_string_lossy().into_owned(),
        instance_id: "phone-1".into(),
        caps: vec!["net.request".into()],
        p2p: None,
        settings: Vec::new(),
    }
}

/// The device key is made in the app's folder and read again; only its ID is shown.
#[test]
fn tasty_net_makes_the_device_key_once_and_shows_only_its_id() {
    let dir = TempDir::new();
    let first = TastyNet::new(config(&dir), None).expect("new");
    let second = TastyNet::new(config(&dir), None).expect("again");
    assert_eq!(first.device_id(), second.device_id());
    assert!(first.connections().is_empty());
    assert!(first.events().next(Duration::from_millis(10)).is_none());
    assert!(matches!(
        first.open_attach(key(2).id().to_string(), attach_options(), None),
        Err(HandshakeFailure::NoPath)
    ));
    assert!(
        first
            .reregister_peer("desk".into(), key(2).id().to_string())
            .is_err()
    );

    let settings = first.settings();
    let poll = settings
        .iter()
        .find(|s| s.key == "events.poll_interval")
        .expect("the FFI setting");
    assert_eq!(
        (poll.value.as_str(), poll.source.as_str()),
        ("50 ms", "default")
    );
    assert!(
        settings
            .iter()
            .any(|s| s.key == "connections.event_queue_limit")
    );
}

#[test]
fn tasty_net_refuses_a_bad_setting_or_capability() {
    let dir = TempDir::new();
    let mut bad = config(&dir);
    bad.settings = vec![crate::net::DurationSetting {
        key: "events.poll_interval".into(),
        value: Duration::ZERO,
    }];
    assert!(matches!(
        TastyNet::new(bad, None),
        Err(crate::error::SetupFailure::InvalidSetting { .. })
    ));
    let mut empty = config(&dir);
    empty.instance_id = String::new();
    assert!(matches!(
        TastyNet::new(empty, None),
        Err(crate::error::SetupFailure::InvalidInstanceId)
    ));
    assert!(
        !dir.0.join("keys").exists(),
        "a refused setup makes no device key"
    );
    let mut caps = config(&dir);
    caps.caps = vec!["Not A Cap".into()];
    assert!(matches!(
        TastyNet::new(caps, None),
        Err(crate::error::SetupFailure::InvalidCapabilities)
    ));
    let mut good = config(&dir);
    good.settings = vec![crate::net::DurationSetting {
        key: "events.poll_interval".into(),
        value: Duration::from_millis(5),
    }];
    let net = TastyNet::new(good, None).expect("new");
    let poll = net
        .settings()
        .into_iter()
        .find(|s| s.key == "events.poll_interval")
        .expect("setting");
    assert_eq!(
        (poll.value.as_str(), poll.source.as_str()),
        ("5 ms", "configured")
    );
}
