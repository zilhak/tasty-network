//! The public [`TastyNet`] over P2P (docs/design/mobile.md "바인딩 API (1단계)"): a rendezvous
//! server and the accepting device's P2P endpoint run in this process on loopback; the
//! bindings dial through `TastyNet::open_attach` and `TastyNet::open_request`, and their own
//! connection list and events show the connection. The app's trust store calls back into the
//! same `TastyNet` while the handshake waits on it.

use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::settings::{P2pSettings, Value as P2pValue};
use tasty_net_p2p::{P2pConfig, P2pEndpoint, PairingList};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelReader,
    ChannelWriter, ConnectionTable, DomainAccept, DomainPolicy, DomainReject, HandshakeError,
    P2pListener, PeerInfo, RequestHandler, RequestServer,
};

use crate::channel::FrameTag;
use crate::error::{HandshakeFailure, RequestFailure};
use crate::net::{AttachOptions, P2pSetup, RequestOptions, TastyNet, TastyNetConfig};
use crate::records::{ConnectionPath, LinkHolder, NetEvent};
use crate::store::{PeerKeyStore, PeerStoreError, PinResult};
use crate::tests::{PATIENCE, TempDir, key};

/// How often the accepting thread looks at its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// How long to wait between two tries while the peer is not yet registered on the server.
const RETRY_WAIT: Duration = Duration::from_millis(200);

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
        let mut config = ServerConfig::new(DeviceKey::from_seed([240; 32]));
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

/// Accepts every attach and bulk channel.
struct Open;

impl DomainPolicy for Open {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }
}

/// Answers `echo` with its parameters.
struct Echo;

impl RequestHandler for Echo {
    fn handle(&self, _peer: &PeerInfo, method: &str, params: Value) -> Result<Value, Value> {
        match method {
            "echo" => Ok(json!({ "echo": params })),
            _ => Err(json!({ "code": "unknown_method" })),
        }
    }
}

/// The accepting device: its own P2P endpoint and a listener that echoes frames and serves
/// requests. Dropping it stops and joins the accepting thread and the serving threads.
struct Acceptor {
    endpoint: P2pEndpoint,
    stop: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
    served: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Acceptor {
    fn start(server: &Rendezvous, seed: u8, dialer: DeviceId) -> Self {
        Self::start_with(server, seed, dialer, P2pSettings::new(), None)
    }

    /// An acceptor with P2P `settings`. With `hold`, an accepted request channel is handed
    /// to the test instead of being served, so nothing reads it.
    fn start_with(
        server: &Rendezvous,
        seed: u8,
        dialer: DeviceId,
        settings: P2pSettings,
        hold: Option<mpsc::Sender<RequestServer>>,
    ) -> Self {
        let pairing = Arc::new(PairingList::new());
        pairing.add(dialer);
        let mut config = P2pConfig::new(key(seed), pairing);
        config.enabled = true;
        config.server = Some(server.info);
        config.settings = settings;
        let endpoint = P2pEndpoint::bind(config).expect("bind");
        let accept = AcceptConfig::new(key(seed), "acceptor-p2p", None, Arc::new(Open))
            .with_caps(vec!["net.request".into()])
            .expect("caps")
            .with_connections(ConnectionTable::default());
        let listener = P2pListener::new(endpoint.clone(), accept).expect("listener");
        let stop = Arc::new(AtomicBool::new(false));
        let served: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let accepting = {
            let (stop, served) = (Arc::clone(&stop), Arc::clone(&served));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(inbound) = listener.accept_timeout(Some(ACCEPT_POLL)) else {
                        continue;
                    };
                    let hold = hold.clone();
                    let thread = std::thread::spawn(move || {
                        serve(inbound.handshake(&AttemptCancel::new()), hold.as_ref());
                    });
                    served.lock().expect("served").push(thread);
                }
            })
        };
        Self {
            endpoint,
            stop,
            accepting: Some(accepting),
            served,
        }
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.endpoint.close();
        if let Some(accepting) = self.accepting.take() {
            let _joined = accepting.join();
        }
        for thread in std::mem::take(&mut *self.served.lock().expect("served")) {
            let _joined = thread.join();
        }
    }
}

fn serve(accepted: Result<Accepted, HandshakeError>, hold: Option<&mpsc::Sender<RequestServer>>) {
    match accepted {
        Ok(Accepted::Link(mut link)) => {
            if let Some((reader, writer)) = link.take_ordered() {
                echo(reader, writer);
            }
        }
        Ok(Accepted::Bulk(bulk)) => echo(bulk.reader, bulk.writer),
        Ok(Accepted::Request(server)) => match hold {
            Some(hold) => drop(hold.send(server)),
            None => drop(server.serve(&Echo)),
        },
        Err(_) => {}
    }
}

fn echo(mut reader: ChannelReader, mut writer: ChannelWriter) {
    while let Ok(frame) = reader.read_frame() {
        if writer.write_frame(frame.tag, &frame.payload).is_err() {
            return;
        }
    }
    drop(writer.half_close());
}

/// The app's store: records atomically and, inside every call, reads the connection list of
/// the `TastyNet` that is waiting on it.
#[derive(Default)]
struct Reentrant {
    net: OnceLock<Weak<TastyNet>>,
    records: Mutex<Vec<(String, String)>>,
    pins: AtomicU64,
    lists_read: AtomicU64,
}

impl Reentrant {
    fn read_back(&self) {
        if let Some(net) = self.net.get().and_then(Weak::upgrade) {
            let _list = net.connections();
            self.lists_read.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl PeerKeyStore for Reentrant {
    fn get(&self, peer: String) -> Result<Option<String>, PeerStoreError> {
        self.read_back();
        let records = self.records.lock().expect("records");
        Ok(records
            .iter()
            .find(|(p, _)| *p == peer)
            .map(|(_, id)| id.clone()))
    }

    fn pin_if_absent(&self, peer: String, device_id: String) -> Result<PinResult, PeerStoreError> {
        self.read_back();
        self.pins.fetch_add(1, Ordering::SeqCst);
        let mut records = self.records.lock().expect("records");
        if let Some((_, existing)) = records.iter().find(|(p, _)| *p == peer) {
            return Ok(PinResult::Existing {
                device_id: existing.clone(),
            });
        }
        records.push((peer, device_id));
        Ok(PinResult::Inserted)
    }

    fn replace(&self, peer: String, device_id: String) -> Result<(), PeerStoreError> {
        self.read_back();
        let mut records = self.records.lock().expect("records");
        records.retain(|(p, _)| *p != peer);
        records.push((peer, device_id));
        Ok(())
    }
}

/// Tries `open` until it succeeds, fails in a way connecting again does not change, or
/// [`PATIENCE`] passed: the accepting device registers on the server shortly after it starts.
fn retry<T>(mut open: impl FnMut() -> Result<T, HandshakeFailure>) -> T {
    let started = Instant::now();
    loop {
        match open() {
            Ok(value) => return value,
            Err(e) if e.is_retryable() && started.elapsed() < PATIENCE => {
                std::thread::sleep(RETRY_WAIT);
            }
            Err(e) => panic!("open failed: {e:?}"),
        }
    }
}

/// A `TastyNet` setup with P2P on through `server`, paired with `peer`.
fn p2p_config(dir: &TempDir, server: &Rendezvous, peer: DeviceId) -> TastyNetConfig {
    TastyNetConfig {
        key_dir: dir.0.join("keys").to_string_lossy().into_owned(),
        instance_id: "phone-p2p".into(),
        caps: Vec::new(),
        p2p: Some(P2pSetup {
            server_addr: server.info.addr.to_string(),
            server_key: server.info.public_key.to_string(),
            paired_devices: vec![peer.to_string()],
        }),
        settings: Vec::new(),
    }
}

fn request_options() -> RequestOptions {
    RequestOptions {
        name: Some("desk".into()),
        peer_name: Some("desk".into()),
    }
}

#[test]
fn tasty_net_opens_attach_and_request_over_p2p_and_lists_them() {
    let server = Rendezvous::start();
    let seed = 230;
    let peer_id = key(seed).id();
    let dir = TempDir::new();
    let store = Arc::new(Reentrant::default());
    let config = p2p_config(&dir, &server, peer_id);
    let dyn_store: Arc<dyn PeerKeyStore> = store.clone();
    let net = TastyNet::new(config, Some(dyn_store)).expect("new");
    store
        .net
        .set(Arc::downgrade(&net))
        .expect("the store's TastyNet");
    let own_id = DeviceId::from_str(&net.device_id()).expect("device ID");
    let _acceptor = Acceptor::start(&server, seed, own_id);

    let options = AttachOptions {
        holder: LinkHolder::Dialer,
        ext: None,
        name: Some("desk".into()),
        peer_name: Some("desk".into()),
    };
    let link = retry(|| net.open_attach(peer_id.to_string(), options.clone(), None));
    assert_eq!(link.peer().device_id, peer_id.to_string());
    let ordered = link.ordered();
    ordered
        .write_frame(FrameTag::Data, b"over p2p".to_vec())
        .expect("write");
    assert_eq!(ordered.read_frame().expect("read").payload, b"over p2p");

    let request = retry(|| net.open_request(peer_id.to_string(), request_options(), None));
    assert!(request.has_cap("net.request".into()));
    let answer = request
        .call("echo".into(), r#"{"n":1}"#.into(), PATIENCE)
        .expect("echo");
    assert_eq!(
        serde_json::from_str::<Value>(&answer).expect("json"),
        json!({ "echo": { "n": 1 } })
    );

    // The trust store was asked once per connection and could read the list meanwhile.
    assert_eq!(store.pins.load(Ordering::SeqCst), 2);
    assert!(store.lists_read.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        store.records.lock().expect("records").clone(),
        vec![("desk".to_string(), peer_id.to_string())]
    );

    // The dialer's own connection list and events show the P2P connection with its link.
    let entry = net
        .connections()
        .into_iter()
        .find(|c| c.path == ConnectionPath::P2pUdp && !c.links.is_empty())
        .expect("a p2p_udp entry with the link");
    assert_eq!(
        entry.peer.device_id.as_deref(),
        Some(peer_id.to_string().as_str())
    );
    assert_eq!(net.connection(entry.id), Some(entry.clone()));
    let events = net.events();
    let mut changed = false;
    while let Some(event) = events.next(Duration::from_millis(200)) {
        if let NetEvent::Changed { id, .. } = event
            && id == entry.id
        {
            changed = true;
        }
    }
    assert!(changed, "the stream reported the entry");

    request.finish();
    // The app reads nothing more, so it marks its reading half as ended before the normal end.
    ordered.finish_reading();
    assert_eq!(ordered.finish(), crate::channel::FinishResult::ReaderEnded);
    net.shutdown();
}

/// The accepting end's QUIC receive window of one stream in [`finish_wakes_a_call_blocked_in_its_write`].
const SMALL_WINDOW: u64 = 64 * 1024;

/// A request larger than [`SMALL_WINDOW`] and smaller than one frame.
const LARGE_PARAMS: usize = 512 * 1024;

/// A call whose request does not fit in the peer's stream window, on a channel the peer does
/// not read, cannot get past its write; finishing the channel then wakes it with `Closed`.
#[test]
fn finish_wakes_a_call_blocked_in_its_write() {
    let server = Rendezvous::start();
    let seed = 231;
    let peer_id = key(seed).id();
    let dir = TempDir::new();
    let net = TastyNet::new(p2p_config(&dir, &server, peer_id), None).expect("new");
    let own_id = DeviceId::from_str(&net.device_id()).expect("device ID");
    let mut settings = P2pSettings::new();
    settings.set("quic.stream_receive_window", P2pValue::Bytes(SMALL_WINDOW));
    let (hold, held) = mpsc::channel();
    let _acceptor = Acceptor::start_with(&server, seed, own_id, settings, Some(hold));

    let channel = Arc::new(retry(|| {
        net.open_request(peer_id.to_string(), request_options(), None)
    }));
    // The peer keeps the channel and never reads it.
    let _unread = held
        .recv_timeout(PATIENCE)
        .expect("the peer's request channel");
    let params = Value::String("x".repeat(LARGE_PARAMS)).to_string();
    let running = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || channel.call("echo".into(), params, PATIENCE))
    };
    // Once the call holds the channel it is in or before its write, which cannot end: the
    // request is larger than the window the peer gives and the peer reads nothing.
    let started = Instant::now();
    while channel.calls_running() == 0 {
        assert!(started.elapsed() < PATIENCE, "the call did not start");
        std::thread::yield_now();
    }
    channel.finish();
    let answer = running.join().expect("call thread");
    assert!(
        matches!(answer, Err(RequestFailure::Closed { .. })),
        "{answer:?}"
    );
    assert!(started.elapsed() < PATIENCE, "{:?}", started.elapsed());
    net.shutdown();
}
