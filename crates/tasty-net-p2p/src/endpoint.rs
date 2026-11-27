//! The endpoint: configuration, binding, the owned runtime and the blocking API
//! (docs/design/p2p.md "endpoint와 런타임", "정책").

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use quinn::ConnectionError;
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_rendezvous::packet_io::{DemuxCounters, DemuxSocket, PacketIo};
use tasty_net_rendezvous::tls::{self, TlsOptions};
use tasty_net_rendezvous::wire::candidate::{CandidateKind, candidate_allowed};
use tasty_net_rendezvous::wire::message::{DeviceMessage, Limits};
use tasty_net_rendezvous::wire::{ALPN_P2P, ALPN_RENDEZVOUS};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::debug;

use crate::close::P2pCloseReason;
use crate::conn::{ConnInner, P2pCancel, P2pConnection};
use crate::error::{P2pConnectError, P2pError, StopCause};
use crate::events::{AttemptPhase, Cause, CauseDetail, EndpointPhase, Events, P2pEvent, Phase};
use crate::info::{P2pConnectionInfo, P2pStatus, RendezvousServer, Tracked, Tracker};
use crate::nat::NatJudgement;
use crate::pairing::PairingStore;
use crate::runtime::{Finished, Owner};
use crate::settings::{OsValues, P2pParams, P2pSettings, SettingEntry};
use crate::socket::{Gate, ProbeSession, StunCounters, StunRouter, canonical};

/// The name of the lock file in the key directory.
const LOCK_FILE: &str = "p2p.lock";

/// The endpoint's configuration (docs/design/p2p.md "endpoint와 런타임").
#[derive(Clone)]
pub struct P2pConfig {
    /// P2P is on. Off by default: an endpoint that is off opens no socket, does not connect to
    /// the server and does not take connections.
    pub enabled: bool,
    /// The device key.
    pub device_key: DeviceKey,
    /// The directory the device key is stored in. When given, `bind` holds its lock file so
    /// that one key has one P2P endpoint across processes ([`P2pError::EndpointInUse`]). In one
    /// process one key has one P2P endpoint that is on also without it.
    pub key_dir: Option<PathBuf>,
    /// The one rendezvous server. Required when `enabled`.
    pub server: Option<RendezvousServer>,
    /// The pairing allow list.
    pub pairing: Arc<dyn PairingStore>,
    /// Settings; keys not given take their default.
    pub settings: P2pSettings,
    /// Host addresses to report besides the local IP the OS picks towards the server (private
    /// IPv4 and global IPv6 are sent; others are dropped by the candidate rule).
    pub host_ips: Vec<IpAddr>,
    /// Packet I/O to run on instead of an OS UDP socket (a mock NAT in tests).
    pub packet_io: Option<Arc<dyn PacketIo>>,
}

impl fmt::Debug for P2pConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pConfig")
            .field("enabled", &self.enabled)
            .field("device", &self.device_key.id())
            .field("key_dir", &self.key_dir)
            .field("server", &self.server)
            .field("settings", &self.settings)
            .field("host_ips", &self.host_ips)
            .field("packet_io", &self.packet_io)
            .finish_non_exhaustive()
    }
}

impl P2pConfig {
    /// A configuration that is off, with default settings.
    pub fn new(device_key: DeviceKey, pairing: Arc<dyn PairingStore>) -> Self {
        P2pConfig {
            enabled: false,
            device_key,
            key_dir: None,
            server: None,
            pairing,
            settings: P2pSettings::new(),
            host_ips: Vec::new(),
            packet_io: None,
        }
    }
}

/// What the server answered a `punch_request`.
#[derive(Debug)]
pub(crate) enum RequestOutcome {
    Go(
        tasty_net_rendezvous::wire::message::PunchGo,
        mpsc::UnboundedReceiver<PunchSignal>,
    ),
    Refused(tasty_net_rendezvous::wire::message::RefuseReason),
}

/// What the server tells a punch in progress.
#[derive(Debug, Clone)]
pub(crate) enum PunchSignal {
    Go(tasty_net_rendezvous::wire::message::PunchGo),
    Cancel(tasty_net_rendezvous::wire::message::CancelReason),
    /// The coordination connection ended.
    Lost,
}

/// What the accept loop tells an acceptor's session.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// A QUIC handshake from this session address began.
    Started(SocketAddr),
    /// One of those handshakes ended without a connection for this session.
    Ended,
    /// One of those handshakes failed, and this session was the only one its address fitted:
    /// the failure is this session's, with this cause.
    Failed(Cause),
    /// The connection is up and its device ID is the session's peer.
    Established(quinn::Connection),
}

/// An acceptor's session as the accept loop sees it.
#[derive(Debug, Clone)]
pub(crate) struct AcceptSession {
    pub(crate) probe: Arc<ProbeSession>,
    pub(crate) to: mpsc::UnboundedSender<Incoming>,
}

/// The endpoint's registration state.
#[derive(Debug, Default)]
pub(crate) struct Registration {
    pub(crate) phase: Option<EndpointPhase>,
    pub(crate) attempt: u64,
    pub(crate) next_at: Option<Instant>,
    pub(crate) last_cause: Option<Cause>,
    pub(crate) stopped: Option<StopCause>,
    pub(crate) reflexive: Option<SocketAddr>,
    pub(crate) stun: Vec<SocketAddr>,
    pub(crate) limits: Option<Limits>,
    pub(crate) mismatch: Option<&'static str>,
    pub(crate) nat: Option<NatJudgement>,
    pub(crate) registered_at: Option<SystemTime>,
    /// When the current registration began, for how long it was held.
    pub(crate) registered_since: Option<Instant>,
}

/// The current coordination connection's writer, with the number of its registration.
#[derive(Debug, Clone)]
pub(crate) struct Link {
    pub(crate) tx: mpsc::UnboundedSender<DeviceMessage>,
    pub(crate) generation: u64,
}

/// A punch's key: the registration its server messages came on and the server's punch number.
/// A server that starts again numbers its punches anew.
pub(crate) type PunchKey = (u64, u64);

/// The endpoint's shared state.
pub(crate) struct Core {
    pub(crate) params: P2pParams,
    pub(crate) settings: P2pSettings,
    pub(crate) os: OsValues,
    pub(crate) key: DeviceKey,
    pub(crate) id: DeviceId,
    pub(crate) server: RendezvousServer,
    pub(crate) pairing: Arc<dyn PairingStore>,
    pub(crate) host_ips: Vec<IpAddr>,
    /// The socket through its gate.
    pub(crate) io: Arc<dyn PacketIo>,
    /// The gate, closed at `Off`.
    gate: Arc<Gate>,
    pub(crate) router: Arc<StunRouter>,
    pub(crate) demux: Arc<DemuxCounters>,
    /// The QUIC endpoint, given up at `Off`.
    quic: Mutex<Option<quinn::Endpoint>>,
    pub(crate) handle: Handle,
    pub(crate) events: Arc<Events>,
    pub(crate) tracker: Arc<Tracker>,
    pub(crate) reg: Mutex<Registration>,
    /// Bumped at every registration change, for attempts that wait for one.
    pub(crate) reg_changed: watch::Sender<u64>,
    pub(crate) link: Mutex<Option<Link>>,
    pub(crate) requests: Mutex<HashMap<u64, oneshot::Sender<RequestOutcome>>>,
    pub(crate) punches: Mutex<HashMap<PunchKey, mpsc::UnboundedSender<PunchSignal>>>,
    /// The number of the last registration.
    generations: AtomicU64,
    pub(crate) next_request: AtomicU64,
    pub(crate) accepting: Mutex<Vec<AcceptSession>>,
    pub(crate) accepted_tx: mpsc::UnboundedSender<Arc<ConnInner>>,
    pub(crate) accepted_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Arc<ConnInner>>>,
    /// Set when the endpoint shuts down.
    pub(crate) shutdown: watch::Sender<bool>,
    /// The blocking calls in the runtime now, and whether new ones are refused.
    calls: Mutex<Calls>,
    /// A blocking call left the runtime.
    call_left: tokio::sync::Notify,
    /// The device key's single endpoint, given up at `Off`.
    key_hold: Mutex<Option<KeyHold>>,
}

#[derive(Debug, Default)]
struct Calls {
    closing: bool,
    active: usize,
}

/// Why [`Core::block`] did not run a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocked {
    /// The caller runs an async task.
    InAsyncContext,
    /// The endpoint is closed or closing.
    Closed,
}

impl From<Blocked> for P2pError {
    fn from(b: Blocked) -> Self {
        match b {
            Blocked::InAsyncContext => P2pError::InAsyncContext,
            Blocked::Closed => P2pError::Closed,
        }
    }
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("id", &self.id)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// Move the endpoint state in `reg` to `to` and queue its event; whether it moved. The event
/// is queued under the state's lock, so the events come in the order of the transitions (the
/// lock order is the state, then the queue). After the shutdown reported `Off`, `Off` stays the
/// last state: a server loop that was between two steps when the shutdown began does not move
/// the endpoint again.
fn move_endpoint(
    reg: &Mutex<Registration>,
    events: &Events,
    shutting_down: impl FnOnce() -> bool,
    to: EndpointPhase,
    cause: Cause,
) -> bool {
    let mut r = lock(reg);
    if r.phase == Some(EndpointPhase::Off) && shutting_down() {
        return false;
    }
    let from = r.phase.replace(to);
    r.last_cause = Some(cause.clone());
    events.push(P2pEvent::Transition {
        subject: crate::events::Subject::Endpoint,
        from: from.map(Phase::Endpoint),
        to: Phase::Endpoint(to),
        cause,
        at: SystemTime::now(),
    });
    true
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Core {
    /// Run `fut` on the runtime and block the calling thread until it ends: the bridge of every
    /// blocking call. Refused inside an async task, and once the endpoint is closing, before
    /// the runtime is entered. A call in the runtime when the shutdown begins returns
    /// [`Blocked::Closed`] at once, and the shutdown waits for it to leave before it ends the
    /// runtime, so no call polls the runtime's timers after the runtime is gone.
    pub(crate) fn block<F: std::future::Future>(&self, fut: F) -> Result<F::Output, Blocked> {
        if crate::runtime::in_async_task() {
            return Err(Blocked::InAsyncContext);
        }
        {
            let mut c = lock(&self.calls);
            if c.closing {
                return Err(Blocked::Closed);
            }
            c.active += 1;
        }
        struct Leave<'a>(&'a Core);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                lock(&self.0.calls).active -= 1;
                self.0.call_left.notify_one();
            }
        }
        let _leave = Leave(self);
        let mut shutdown = self.shutdown.subscribe();
        crate::runtime::block_on(&self.handle, async move {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|down| *down) => None,
                out = fut => Some(out),
            }
        })
        .ok_or(Blocked::Closed)
    }

    /// The QUIC endpoint, until `Off`.
    pub(crate) fn quic(&self) -> Option<quinn::Endpoint> {
        lock(&self.quic).clone()
    }

    /// Move the endpoint state machine.
    pub(crate) fn endpoint_to(&self, to: EndpointPhase, cause: Cause) {
        if move_endpoint(
            &self.reg,
            &self.events,
            || self.is_shutting_down(),
            to,
            cause,
        ) {
            self.reg_changed.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// Send a message on the coordination connection of registration `generation` only: work
    /// begun on an earlier registration does not reach the next one.
    pub(crate) fn send_on(&self, generation: u64, msg: DeviceMessage) -> bool {
        lock(&self.link)
            .as_ref()
            .is_some_and(|l| l.generation == generation && l.tx.send(msg).is_ok())
    }

    /// The number of the current registration, if there is one.
    pub(crate) fn generation(&self) -> Option<u64> {
        lock(&self.link).as_ref().map(|l| l.generation)
    }

    /// Make `tx` the writer of a new registration; its number.
    pub(crate) fn open_link(&self, tx: mpsc::UnboundedSender<DeviceMessage>) -> u64 {
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        *lock(&self.link) = Some(Link { tx, generation });
        generation
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// The QUIC client configuration towards a key: the server's for the coordination
    /// connection, the peer's for a device connection.
    pub(crate) fn client_config(
        &self,
        expected: DeviceId,
        alpn: &[u8],
        quic: &tasty_net_rendezvous::settings::QuicParams,
    ) -> Result<quinn::ClientConfig, P2pError> {
        let tls = tls::client_config(
            &self.key,
            expected,
            alpn,
            TlsOptions {
                max_fragment_size: quic.tls_max_fragment_size,
            },
        )
        .map_err(P2pError::Tls)?;
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| P2pError::Io(std::io::Error::other(e)))?;
        let mut c = quinn::ClientConfig::new(Arc::new(crypto));
        c.transport_config(Arc::new(tasty_net_rendezvous::quic::transport_config(quic)));
        Ok(c)
    }
}

/// The P2P endpoint. Clones share it; it ends when the last clone and the last
/// [`P2pConnection`] and stream are dropped, or at [`P2pEndpoint::close`].
#[derive(Clone)]
pub struct P2pEndpoint {
    inner: Arc<Inner>,
}

struct Inner {
    id: DeviceId,
    settings: P2pSettings,
    on: Option<Arc<Life>>,
}

/// The life of an endpoint that is on: the runtime's owner. The
/// endpoint's clones, its connections and their streams hold it; the endpoint shuts down when
/// the last of them is dropped, or at [`P2pEndpoint::close`].
pub(crate) struct Life {
    pub(crate) core: Arc<Core>,
    owner: Mutex<Option<Owner>>,
}

impl Life {
    /// Hand the shutdown to the owner thread, once; the signal of that thread's end.
    fn stop(&self) -> Option<Arc<Finished>> {
        let mut o = lock(&self.owner).take()?;
        let finished = o.finished();
        o.stop(shutdown_work(self.core.clone()));
        Some(finished)
    }
}

impl Drop for Life {
    fn drop(&mut self) {
        // Only signals: the last handle may drop inside an async task.
        let _stopping = self.stop();
    }
}

impl fmt::Debug for P2pEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pEndpoint")
            .field("id", &self.inner.id)
            .field("enabled", &self.inner.on.is_some())
            .finish()
    }
}

/// Where the route of the address family the server is not in is asked: a documentation
/// address of that family (RFC 3849, RFC 5737), never sent to (docs/design/p2p.md "소켓 하나와
/// 패킷 나누기").
const ROUTE_PROBE_V6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
const ROUTE_PROBE_V4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 0, 2, 1);

/// The local IP the OS uses towards `to`, without sending anything.
fn route_ip(to: SocketAddr) -> Option<IpAddr> {
    let any: SocketAddr = match to {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, u16::MIN).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, u16::MIN).into(),
    };
    let s = UdpSocket::bind(any).ok()?;
    s.connect(to).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// The local IPs of a wildcard socket, one per address family the socket serves: the route
/// towards the server for its family, and for the other family the route towards a
/// documentation address of it. A global IPv6 host candidate is found so also with an IPv4
/// server. The candidate rule drops what may not be sent.
fn route_ips(server: SocketAddr, local: SocketAddr, dual_stack: bool) -> Vec<IpAddr> {
    let other: SocketAddr = match server {
        SocketAddr::V4(_) => (ROUTE_PROBE_V6, server.port()).into(),
        SocketAddr::V6(_) => (ROUTE_PROBE_V4, server.port()).into(),
    };
    // An IPv4 socket serves IPv4 only; an IPv6 one IPv4 too when dual stack.
    let serves = |a: &SocketAddr| match (local, a) {
        (SocketAddr::V4(_), SocketAddr::V4(_)) | (SocketAddr::V6(_), SocketAddr::V6(_)) => true,
        (SocketAddr::V6(_), SocketAddr::V4(_)) => dual_stack,
        (SocketAddr::V4(_), SocketAddr::V6(_)) => false,
    };
    [server, other]
        .iter()
        .filter(|a| serves(a))
        .filter_map(|a| route_ip(*a))
        .collect()
}

/// The device IDs of this process's endpoints that are on.
static ACTIVE_KEYS: Mutex<std::collections::BTreeSet<DeviceId>> =
    Mutex::new(std::collections::BTreeSet::new());

/// One device key's single endpoint: its place among this process's endpoints that are on and,
/// with a key directory, that directory's lock for other processes. Given up at `Off`.
struct KeyHold {
    id: DeviceId,
    _file: Option<File>,
}

impl KeyHold {
    fn take(id: DeviceId, dir: Option<&std::path::Path>) -> Result<Self, P2pError> {
        if !lock(&ACTIVE_KEYS).insert(id) {
            return Err(P2pError::EndpointInUse);
        }
        // From here on the hold gives the ID back when dropped, also on a lock failure.
        let mut hold = KeyHold { id, _file: None };
        hold._file = dir.map(lock_key_dir).transpose()?;
        Ok(hold)
    }
}

impl Drop for KeyHold {
    fn drop(&mut self) {
        lock(&ACTIVE_KEYS).remove(&self.id);
    }
}

fn lock_key_dir(dir: &std::path::Path) -> Result<File, P2pError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .map_err(P2pError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(P2pError::EndpointInUse),
        Err(std::fs::TryLockError::Error(e)) => Err(P2pError::Io(e)),
    }
}

/// The QUIC server configuration of the P2P socket: the shared one with `quic.allow_migration`.
pub(crate) fn p2p_server_config(
    params: &P2pParams,
    crypto: quinn::crypto::rustls::QuicServerConfig,
) -> quinn::ServerConfig {
    let mut c = tasty_net_rendezvous::quic::server_config(&params.quic, crypto);
    c.migration(params.allow_migration);
    c
}

impl P2pEndpoint {
    /// Make the endpoint. Off: no socket, nothing sent. On: the settings are checked, the key
    /// directory's lock is taken, the runtime and the socket are made and the endpoint starts
    /// registering on the server.
    pub fn bind(config: P2pConfig) -> Result<Self, P2pError> {
        let params = config.settings.resolve().map_err(P2pError::Settings)?;
        let id = config.device_key.id();
        if !config.enabled {
            return Ok(P2pEndpoint {
                inner: Arc::new(Inner {
                    id,
                    settings: config.settings,
                    on: None,
                }),
            });
        }
        let server = config.server.ok_or(P2pError::NoServer)?;
        let key_hold = KeyHold::take(id, config.key_dir.as_deref())?;
        let owner =
            Owner::start(params.worker_threads, params.shutdown_timeout).map_err(P2pError::Io)?;
        let handle = owner.handle().clone();
        let (io, os) = match config.packet_io {
            Some(io) => (io, OsValues::default()),
            None => {
                let _enter = handle.enter();
                crate::socket::open_udp(&params).map_err(P2pError::Io)?
            }
        };
        let mut host_ips = config.host_ips.clone();
        let local = io.local_addr().map_err(P2pError::Io)?;
        if !local.ip().is_unspecified() {
            host_ips.push(local.ip());
        } else {
            host_ips.extend(route_ips(server.addr, local, io.dual_stack()));
        }
        host_ips.dedup();
        let router = Arc::new(StunRouter::default());
        let demux = Arc::new(DemuxCounters::default());
        let gate = Arc::new(Gate::new(io));
        let io: Arc<dyn PacketIo> = gate.clone();
        let socket = Arc::new(DemuxSocket::with_counters(
            io.clone(),
            router.clone(),
            demux.clone(),
        ));
        let server_tls = tls::server_config(
            &config.device_key,
            ALPN_P2P,
            TlsOptions {
                max_fragment_size: params.quic.tls_max_fragment_size,
            },
        )
        .map_err(P2pError::Tls)?;
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)
            .map_err(|e| P2pError::Io(std::io::Error::other(e)))?;
        let server_config = p2p_server_config(&params, crypto);
        let endpoint = {
            let _enter = handle.enter();
            quinn::Endpoint::new_with_abstract_socket(
                tasty_net_rendezvous::quic::endpoint_config(&params.quic),
                Some(server_config),
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .map_err(P2pError::Io)?
        };
        let events = Arc::new(Events::new(params.events_queue_limit));
        let tracker = Arc::new(Tracker::new(events.clone(), params.history_limit));
        let (accepted_tx, accepted_rx) = mpsc::unbounded_channel();
        let core = Arc::new(Core {
            params,
            settings: config.settings.clone(),
            os,
            id,
            key: config.device_key,
            server,
            pairing: config.pairing,
            host_ips,
            io,
            gate,
            router,
            demux,
            quic: Mutex::new(Some(endpoint)),
            handle: handle.clone(),
            events,
            tracker,
            reg: Mutex::default(),
            reg_changed: watch::channel(u64::MIN).0,
            link: Mutex::new(None),
            requests: Mutex::default(),
            punches: Mutex::default(),
            generations: AtomicU64::new(u64::MIN),
            next_request: AtomicU64::new(u64::MIN),
            accepting: Mutex::default(),
            accepted_tx,
            accepted_rx: tokio::sync::Mutex::new(accepted_rx),
            shutdown: watch::channel(false).0,
            calls: Mutex::default(),
            call_left: tokio::sync::Notify::new(),
            key_hold: Mutex::new(Some(key_hold)),
        });
        lock(&core.reg).phase = Some(EndpointPhase::Off);
        handle.spawn(crate::server::run(core.clone()));
        handle.spawn(accept_loop(core.clone()));
        Ok(P2pEndpoint {
            inner: Arc::new(Inner {
                id,
                settings: config.settings,
                on: Some(Arc::new(Life {
                    core,
                    owner: Mutex::new(Some(owner)),
                })),
            }),
        })
    }

    fn core(&self) -> Option<&Arc<Core>> {
        self.inner.on.as_ref().map(|o| &o.core)
    }

    /// This device's ID.
    pub fn id(&self) -> DeviceId {
        self.inner.id
    }

    /// Whether P2P is on.
    pub fn is_enabled(&self) -> bool {
        self.inner.on.is_some()
    }

    /// The local socket address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.core().and_then(|c| c.io.local_addr().ok())
    }

    /// Connect to `peer` (punching through the server). Blocks until the connection is up or
    /// the attempt ends.
    pub fn connect(&self, peer: DeviceId) -> Result<P2pConnection, P2pError> {
        self.connect_with_cancel(peer, &P2pCancel::new())
    }

    /// [`Self::connect`], ended early with [`P2pConnectError::Cancelled`] by `cancel`.
    pub fn connect_with_cancel(
        &self,
        peer: DeviceId,
        cancel: &P2pCancel,
    ) -> Result<P2pConnection, P2pError> {
        let Some(life) = &self.inner.on else {
            return Err(P2pConnectError::Disabled.into());
        };
        let core = &life.core;
        if peer == core.id {
            return Err(P2pConnectError::SelfAttach.into());
        }
        let fut = crate::attempt::dial(core.clone(), peer, cancel.clone());
        let task = core.block(async { core.handle.spawn(fut).await })?;
        match task {
            Ok(r) => Ok(P2pConnection::new(
                life.clone(),
                r.map_err(P2pError::Connect)?,
            )),
            Err(_) => Err(P2pError::Closed),
        }
    }

    /// Wait for the next incoming connection.
    pub fn accept(&self) -> Result<P2pConnection, P2pError> {
        self.accept_timeout(None)
    }

    /// Wait for the next incoming connection up to `limit` (the wait for another caller's
    /// accept counts in it). A close wakes it with [`P2pError::Closed`].
    pub fn accept_timeout(&self, limit: Option<Duration>) -> Result<P2pConnection, P2pError> {
        let Some(life) = &self.inner.on else {
            return Err(P2pError::Disabled);
        };
        let core = &life.core;
        let take = async {
            let mut rx = core.accepted_rx.lock().await;
            rx.recv().await.ok_or(P2pError::Closed)
        };
        let wait = async {
            match limit {
                Some(t) => tokio::time::timeout(t, take)
                    .await
                    .map_err(|_| P2pError::TimedOut)?,
                None => take.await,
            }
        };
        let inner = core.block(wait)??;
        Ok(P2pConnection::new(life.clone(), inner))
    }

    /// The endpoint's state and registration.
    pub fn status(&self) -> P2pStatus {
        let Some(core) = self.core() else {
            return P2pStatus {
                state: EndpointPhase::Off,
                server: None,
                local_addr: None,
                reflexive: None,
                nat: Default::default(),
                nat_answers: Vec::new(),
                limits: None,
                limits_mismatch: None,
                registered_at: None,
                reconnect_attempt: u64::MIN,
                next_attempt_in: None,
                last_cause: None,
                stopped: None,
                attempt_bound: None,
            };
        };
        let r = lock(&core.reg);
        let p = &core.params;
        P2pStatus {
            state: r.phase.unwrap_or(EndpointPhase::Off),
            server: Some(core.server),
            local_addr: core.io.local_addr().ok(),
            reflexive: r.reflexive,
            nat: r.nat.as_ref().map(|n| n.mapping).unwrap_or_default(),
            nat_answers: r
                .nat
                .as_ref()
                .map(|n| n.answers.clone())
                .unwrap_or_default(),
            limits: r.limits,
            limits_mismatch: r.mismatch,
            registered_at: r.registered_at,
            reconnect_attempt: r.attempt,
            next_attempt_in: r
                .next_at
                .map(|t| t.saturating_duration_since(Instant::now())),
            last_cause: r.last_cause.clone(),
            stopped: r.stopped,
            attempt_bound: r.limits.map(|l| crate::attempt::attempt_bound(p, &l)),
        }
    }

    /// Every connection and attempt in progress.
    pub fn connections(&self) -> Vec<P2pConnectionInfo> {
        self.core()
            .map(|c| c.tracker.snapshot())
            .unwrap_or_default()
    }

    /// Every queued event, oldest first.
    pub fn events(&self) -> Vec<P2pEvent> {
        self.core().map(|c| c.events.drain()).unwrap_or_default()
    }

    /// The oldest queued event, waiting up to `limit` for one. It blocks, so inside an async
    /// task it is refused with [`P2pError::InAsyncContext`] ([`P2pEndpoint::events`] does not
    /// wait).
    pub fn next_event(&self, limit: Duration) -> Result<Option<P2pEvent>, P2pError> {
        if crate::runtime::in_async_task() {
            return Err(P2pError::InAsyncContext);
        }
        Ok(self.core().and_then(|c| c.events.next(limit)))
    }

    /// Every setting with the value in effect and its source.
    pub fn settings(&self) -> Vec<SettingEntry> {
        let os = self.core().map(|c| c.os).unwrap_or_default();
        self.inner.settings.entries_with(os)
    }

    /// The STUN and QUIC demux counts.
    pub fn counters(&self) -> Option<(Arc<StunCounters>, Arc<DemuxCounters>)> {
        self.core()
            .map(|c| (c.router.counters.clone(), c.demux.clone()))
    }

    /// Turn P2P off: close the server connection with code 0, close every connection with
    /// `closed`, close the socket and end the runtime. Waits up to `runtime.shutdown_timeout`
    /// for the end.
    pub fn close(&self) {
        let Some(life) = &self.inner.on else {
            return;
        };
        let Some(finished) = life.stop() else {
            return;
        };
        if !crate::runtime::in_async_task() {
            finished.wait(life.core.params.shutdown_timeout);
        }
    }
}

/// The shutdown the owner thread runs before it ends the runtime: refuse new blocking calls
/// and wake the ones in the runtime, close QUIC with `closed`, then give up the QUIC endpoint,
/// the connections nobody took, the socket (`Off`: no socket) and the device key's hold, and
/// report `Off`.
fn shutdown_work(core: Arc<Core>) -> crate::runtime::Shutdown {
    Box::new(move |handle: &Handle| {
        handle.block_on(async move {
            lock(&core.calls).closing = true;
            core.shutdown.send_replace(true);
            let limit = core.params.shutdown_timeout;
            let calls_left = async {
                loop {
                    let left = core.call_left.notified();
                    if lock(&core.calls).active == usize::MIN {
                        return;
                    }
                    left.await;
                }
            };
            let _left = tokio::time::timeout(limit, calls_left).await.is_ok();
            let quic = lock(&core.quic).take();
            if let Some(quic) = quic {
                quic.close(
                    P2pCloseReason::Closed.var_int(),
                    P2pCloseReason::Closed.reason(),
                );
                let _idle = tokio::time::timeout(limit, quic.wait_idle()).await.is_ok();
            }
            if let Ok(mut queued) = core.accepted_rx.try_lock() {
                queued.close();
                while queued.try_recv().is_ok() {}
            }
            let _released = core.gate.release();
            drop(lock(&core.key_hold).take());
            core.endpoint_to(EndpointPhase::Off, Cause::named("disabled"));
        });
    })
}

/// The session a connection from `addr` whose TLS device ID is `id` belongs to: among the
/// sessions that took its Initial, the one whose peer is `id` (docs/design/p2p.md "펀칭 절차" 9).
/// When several sessions with that peer fit (a retry of the same dialer), the newest one: the
/// older ones are superseded (docs/design/p2p.md "진행 중 `punch_cancel`").
pub(crate) fn pick_session(sessions: &[AcceptSession], id: &DeviceId) -> Option<AcceptSession> {
    sessions.iter().rev().find(|s| s.probe.peer == *id).cloned()
}

/// Take QUIC Initials only from addresses of active punch sessions; bind every connection to
/// its session by device ID.
async fn accept_loop(core: Arc<Core>) {
    let Some(quic) = core.quic() else {
        return;
    };
    let mut shutdown = core.shutdown.subscribe();
    loop {
        let incoming = tokio::select! {
            i = quic.accept() => i,
            _ = shutdown.changed() => None,
        };
        let Some(incoming) = incoming else { break };
        let addr = canonical(incoming.remote_address());
        let now = Instant::now();
        let matching: Vec<AcceptSession> = lock(&core.accepting)
            .iter()
            .filter(|s| s.probe.takes_initial(addr, now))
            .cloned()
            .collect();
        if matching.is_empty() {
            core.router
                .counters
                .quic_refused
                .fetch_add(1, Ordering::Relaxed);
            debug!(%addr, "QUIC Initial outside every punch session refused");
            incoming.refuse();
            continue;
        }
        // The sessions learn of the Initial before the loop takes the next one, so an Initial
        // taken before a session's deadline is counted before that deadline is judged.
        for s in &matching {
            let _sent = s.to.send(Incoming::Started(addr)).is_ok();
        }
        let core = core.clone();
        tokio::spawn(async move {
            let limit = matching
                .iter()
                .map(|s| s.probe.handshake)
                .max()
                .unwrap_or_default();
            let conn = match incoming.accept() {
                Ok(connecting) => tokio::time::timeout(limit, connecting).await,
                Err(e) => Ok(Err(e)),
            };
            let conn = match conn {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    debug!(error = %e, %addr, "incoming QUIC handshake failed");
                    let cause = match crate::server::incoming_tls_stage(&e) {
                        Some(crate::server::TlsStage::KeyRefused) => {
                            Some(Cause::named("peer_identity_mismatch"))
                        }
                        Some(crate::server::TlsStage::NoAlpn) => {
                            Some(Cause::named("alpn_mismatch"))
                        }
                        None => None,
                    };
                    failed(&matching, cause);
                    return;
                }
                Err(_) => {
                    debug!(%addr, "incoming QUIC handshake timed out");
                    failed(&matching, Some(Cause::named("handshake_timeout")));
                    return;
                }
            };
            let Some(id) = tls::peer_device_id(&conn) else {
                conn.close(
                    P2pCloseReason::SessionMismatch.var_int(),
                    P2pCloseReason::SessionMismatch.reason(),
                );
                failed(&matching, Some(session_mismatch()));
                return;
            };
            match pick_session(&matching, &id) {
                Some(s) => {
                    for other in matching.iter().filter(|o| !Arc::ptr_eq(&o.probe, &s.probe)) {
                        let _sent = other.to.send(Incoming::Ended).is_ok();
                    }
                    if s.to.send(Incoming::Established(conn.clone())).is_err() {
                        // The session ended meanwhile.
                        conn.close(
                            P2pCloseReason::SessionMismatch.var_int(),
                            P2pCloseReason::SessionMismatch.reason(),
                        );
                    }
                }
                None => {
                    core.router
                        .counters
                        .session_mismatch
                        .fetch_add(1, Ordering::Relaxed);
                    conn.close(
                        P2pCloseReason::SessionMismatch.var_int(),
                        P2pCloseReason::SessionMismatch.reason(),
                    );
                    failed(&matching, Some(session_mismatch()));
                }
            }
        });
    }
}

fn session_mismatch() -> Cause {
    Cause::with(
        "session_mismatch",
        CauseDetail::Close(P2pCloseReason::SessionMismatch),
    )
}

/// A handshake from an address several sessions fit failed: which session it was for is not
/// known, so each only counts it ended. With one session, the failure and its cause are that
/// session's.
fn failed(sessions: &[AcceptSession], cause: Option<Cause>) {
    match (sessions, cause) {
        ([only], Some(cause)) => {
            let _sent = only.to.send(Incoming::Failed(cause)).is_ok();
        }
        _ => {
            for s in sessions {
                let _sent = s.to.send(Incoming::Ended).is_ok();
            }
        }
    }
}

/// Watch a connection that is up: its end, and (acceptor) the remote address quinn uses.
pub(crate) async fn watch_connection(
    core: Arc<Core>,
    tracked: Tracked,
    conn: Arc<ConnInner>,
    quic: quinn::Connection,
    poll_address: bool,
) {
    tokio::spawn(refuse_uni_streams(quic.clone()));
    let mut tick = tokio::time::interval(core.params.path_poll_interval);
    let mut last = canonical(quic.remote_address());
    let mut shutdown = core.shutdown.subscribe();
    let cause = loop {
        tokio::select! {
            e = quic.closed() => break close_cause(&e, &conn),
            _ = shutdown.changed() => {
                conn.close(P2pCloseReason::Closed);
                break Cause::named("endpoint_off");
            }
            _ = tick.tick(), if poll_address => {
                let now = canonical(quic.remote_address());
                if now != last {
                    tracked.to(
                        AttemptPhase::Connected,
                        Cause::with(
                            "remote_address_observed_change",
                            CauseDetail::Address { from: last, to: now },
                        ),
                    );
                    last = now;
                }
            }
        }
    };
    let cause = if core.is_shutting_down() {
        Cause::named("endpoint_off")
    } else {
        cause
    };
    tracked.to(AttemptPhase::Closed, cause);
}

/// Close every unidirectional stream the peer opens without reading it, until the connection
/// ends: this crate uses none, and a `quic.max_uni_streams` above 0 only lets quinn take them
/// (docs/design/p2p.md "설정값"). Stopping a stream gives its credit back to the peer.
pub(crate) async fn refuse_uni_streams(quic: quinn::Connection) {
    while let Ok(mut recv) = quic.accept_uni().await {
        // Application code 0, the default value.
        let _stopped = recv.stop(quinn::VarInt::default()).is_ok();
    }
}

fn close_cause(e: &ConnectionError, conn: &ConnInner) -> Cause {
    match e {
        ConnectionError::ApplicationClosed(c) => Cause::with(
            "closed_by_peer",
            CauseDetail::Close(P2pCloseReason::from_code(c.error_code.into_inner())),
        ),
        ConnectionError::LocallyClosed => Cause::with(
            "closed_by_local",
            CauseDetail::Close(conn.close_reason().unwrap_or(P2pCloseReason::Closed)),
        ),
        ConnectionError::TimedOut => Cause::named("idle_timeout"),
        other => Cause::with(
            "closed_by_peer",
            CauseDetail::Io {
                kind: crate::conn::connection_error_kind(other),
                what: other.to_string(),
            },
        ),
    }
}

/// Host candidate texts for `hello` and `nat_report`: private IPv4 first, then global IPv6,
/// with the socket's port.
pub(crate) fn host_candidates(core: &Core) -> Vec<String> {
    let Ok(local) = core.io.local_addr() else {
        return Vec::new();
    };
    let mut addrs: Vec<SocketAddr> = core
        .host_ips
        .iter()
        .map(|ip| SocketAddr::new(ip.to_canonical(), local.port()))
        .filter(|a| candidate_allowed(CandidateKind::Host, *a))
        .collect();
    addrs.sort_by_key(|a| a.is_ipv6());
    addrs.dedup();
    addrs.iter().map(ToString::to_string).collect()
}

/// The ALPN of the coordination connection, for [`Core::client_config`].
pub(crate) const RENDEZVOUS_ALPN: &[u8] = ALPN_RENDEZVOUS;

#[cfg(test)]
mod tests {
    use super::*;

    /// Phases one after another as (from, to) of the queued endpoint events.
    fn chain(events: &Events) -> Vec<(Option<Phase>, Phase)> {
        events
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                P2pEvent::Transition { from, to, .. } => Some((from, to)),
                P2pEvent::Lagged { .. } => None,
            })
            .collect()
    }

    /// Threads moving the endpoint at once: each event starts where the one before it ended,
    /// and the last event is the state now.
    #[test]
    fn endpoint_events_follow_the_order_of_the_transitions() {
        let reg = Mutex::new(Registration {
            phase: Some(EndpointPhase::Off),
            ..Registration::default()
        });
        let events = Events::new(100_000);
        let phases = [
            EndpointPhase::Connecting,
            EndpointPhase::Registered,
            EndpointPhase::Backoff,
        ];
        std::thread::scope(|s| {
            for to in phases {
                let (reg, events) = (&reg, &events);
                s.spawn(move || {
                    for _ in 0..2_000 {
                        move_endpoint(reg, events, || false, to, Cause::named("enabled"));
                    }
                });
            }
        });
        let got = chain(&events);
        assert_eq!(got.len(), 6_000);
        assert_eq!(got[0].0, Some(Phase::Endpoint(EndpointPhase::Off)));
        for w in got.windows(2) {
            assert_eq!(w[1].0, Some(w[0].1), "{w:?}");
        }
        let now = lock(&reg).phase.map(Phase::Endpoint);
        assert_eq!(got.last().map(|e| Some(e.1)), Some(now));
    }

    #[test]
    fn off_stays_the_last_state_during_the_shutdown() {
        let reg = Mutex::new(Registration {
            phase: Some(EndpointPhase::Off),
            ..Registration::default()
        });
        let events = Events::new(16);
        let moved = move_endpoint(
            &reg,
            &events,
            || true,
            EndpointPhase::Backoff,
            Cause::named("io"),
        );
        assert!(!moved);
        assert!(chain(&events).is_empty());
        assert_eq!(lock(&reg).phase, Some(EndpointPhase::Off));
    }

    #[test]
    fn route_ips_ask_each_family_the_socket_serves() {
        let local_v6: SocketAddr = "[::]:5000".parse().unwrap();
        let local_v4: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let server: SocketAddr = "127.0.0.1:3478".parse().unwrap();
        // An IPv4 server: the route towards it, and an IPv6 route asked besides when the
        // socket is dual stack. Hosts without IPv6 routes give no IPv6 address.
        let dual = route_ips(server, local_v6, true);
        assert!(
            dual.contains(&IpAddr::from(std::net::Ipv4Addr::LOCALHOST)),
            "{dual:?}"
        );
        let other = route_ip(SocketAddr::from((ROUTE_PROBE_V6, 3478)));
        assert_eq!(
            dual.iter().any(IpAddr::is_ipv6),
            other.is_some(),
            "{dual:?}"
        );
        // An IPv4 socket asks IPv4 only, an IPv6-only socket IPv6 only.
        assert!(
            route_ips(server, local_v4, false)
                .iter()
                .all(IpAddr::is_ipv4)
        );
        assert!(
            route_ips(server, local_v6, false)
                .iter()
                .all(IpAddr::is_ipv6)
        );
    }

    /// A peer's unidirectional streams are stopped unread: the peer sees the stop, and the
    /// credit comes back, so with a limit of 1 the peer opens one stream after another.
    #[test]
    fn unidirectional_streams_of_the_peer_are_stopped_unread() {
        use tasty_net_rendezvous::quic;
        let mut s = P2pSettings::new();
        s.set("quic.max_uni_streams", crate::settings::Value::Count(1));
        let p = s.resolve().unwrap();
        let options = TlsOptions {
            max_fragment_size: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let server_key = DeviceKey::from_seed([11; 32]);
            let client_key = DeviceKey::from_seed([12; 32]);
            let tls = tls::server_config(&server_key, ALPN_P2P, options).unwrap();
            let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
            let server = quinn::Endpoint::server(
                quic::server_config(&p.quic, crypto),
                "127.0.0.1:0".parse().unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let accepting = tokio::spawn(async move {
                let c = server.accept().await.unwrap().await.unwrap();
                refuse_uni_streams(c).await;
            });
            let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let tls = tls::client_config(&client_key, server_key.id(), ALPN_P2P, options).unwrap();
            let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
            let mut config = quinn::ClientConfig::new(Arc::new(crypto));
            config.transport_config(Arc::new(quic::transport_config(&p.quic)));
            let conn = client
                .connect_with(config, addr, tls::SERVER_NAME)
                .unwrap()
                .await
                .unwrap();
            for n in 0..3 {
                let mut send = tokio::time::timeout(Duration::from_secs(5), conn.open_uni())
                    .await
                    .unwrap_or_else(|_| panic!("stream {n} waits for credit"))
                    .unwrap();
                send.write_all(b"unread").await.unwrap();
                let stopped = tokio::time::timeout(Duration::from_secs(5), send.stopped())
                    .await
                    .expect("the peer stops the stream");
                assert_eq!(stopped.unwrap(), Some(quinn::VarInt::default()));
            }
            conn.close(quinn::VarInt::default(), b"");
            let _done = accepting.await;
        });
    }

    /// Sessions of one peer that fit the same address (a retry of the same dialer): the
    /// connection goes to the newest.
    #[test]
    fn the_newest_session_of_a_peer_takes_its_connection() {
        let peer = DeviceKey::from_seed([31; 32]).id();
        let other = DeviceKey::from_seed([32; 32]).id();
        let session = |peer: DeviceId| {
            let (to, _rx) = mpsc::unbounded_channel();
            AcceptSession {
                probe: Arc::new(ProbeSession::new(
                    peer,
                    PunchToken([1; 16]),
                    PunchToken([2; 16]),
                    PunchMode::All,
                    Duration::from_secs(1),
                    Vec::new(),
                    8,
                )),
                to,
            }
        };
        let sessions = [session(peer), session(other), session(peer)];
        let picked = pick_session(&sessions, &peer).expect("a session");
        assert!(Arc::ptr_eq(&picked.probe, &sessions[2].probe));
        assert!(Arc::ptr_eq(
            &pick_session(&sessions, &other).unwrap().probe,
            &sessions[1].probe
        ));
        let stranger = DeviceKey::from_seed([33; 32]).id();
        assert!(pick_session(&sessions, &stranger).is_none());
    }

    /// A failed handshake is a session's only when its address fitted that session alone;
    /// with several, each only counts the handshake as ended (docs/design/testing.md "같은
    /// 주소가 두 상대의 세션에 맞을 때").
    #[test]
    fn a_failure_is_pinned_on_a_session_only_when_it_is_the_only_one() {
        let session = |seed: u8| {
            let (to, rx) = mpsc::unbounded_channel();
            let s = AcceptSession {
                probe: Arc::new(ProbeSession::new(
                    DeviceKey::from_seed([seed; 32]).id(),
                    PunchToken([1; 16]),
                    PunchToken([2; 16]),
                    PunchMode::All,
                    Duration::from_secs(1),
                    Vec::new(),
                    8,
                )),
                to,
            };
            (s, rx)
        };
        let (one, mut one_rx) = session(34);
        failed(std::slice::from_ref(&one), Some(session_mismatch()));
        assert!(matches!(
            one_rx.try_recv(),
            Ok(Incoming::Failed(c)) if c.name == "session_mismatch"
        ));
        let (first, mut first_rx) = session(35);
        let (second, mut second_rx) = session(36);
        failed(&[first, second], Some(session_mismatch()));
        assert!(matches!(first_rx.try_recv(), Ok(Incoming::Ended)));
        assert!(matches!(second_rx.try_recv(), Ok(Incoming::Ended)));
        // No cause known: ended even for one session.
        failed(&[one], None);
        assert!(matches!(one_rx.try_recv(), Ok(Incoming::Ended)));
    }

    /// An endpoint whose server is never reached, with the test standing in for its
    /// registration: the coordination messages it sends come out of the receiver.
    struct StandIn {
        ep: P2pEndpoint,
        core: Arc<Core>,
        generation: u64,
        rx: mpsc::UnboundedReceiver<DeviceMessage>,
    }

    fn stand_in(seed: u8, peer: DeviceId, settings: &[(&str, crate::settings::Value)]) -> StandIn {
        let pairing = Arc::new(crate::pairing::PairingList::new());
        pairing.add(peer);
        let mut config = P2pConfig::new(DeviceKey::from_seed([seed; 32]), pairing);
        config.enabled = true;
        config.server = Some(RendezvousServer {
            addr: "192.0.2.10:3478".parse().unwrap(),
            public_key: DeviceKey::from_seed([seed.wrapping_add(100); 32]).id(),
        });
        config.packet_io = Some(Arc::new(crate::probe::tests_support::Sink));
        // Registration stays Connecting while the test stands in for it.
        config.settings.set(
            "rendezvous.connect_timeout",
            crate::settings::Value::Duration(Duration::from_secs(60)),
        );
        for (key, value) in settings {
            config.settings.set(key, *value);
        }
        let ep = P2pEndpoint::bind(config).expect("bind");
        let core = ep.core().unwrap().clone();
        let until = Instant::now() + Duration::from_secs(10);
        while lock(&core.reg).phase != Some(EndpointPhase::Connecting) {
            assert!(Instant::now() < until, "connecting");
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let mut r = lock(&core.reg);
            r.phase = Some(EndpointPhase::Registered);
            r.limits = Some(Limits {
                offer_timeout_ms: 5_000,
                start_delay_max_ms: 1_000,
                punch_window_ms: 5_000,
                quic_handshake_ms: 5_000,
                punch_session_ttl_ms: 30_000,
                max_host_candidates: 8,
                max_reported_reflexive: 4,
                max_message_bytes: 65_536,
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let generation = core.open_link(tx);
        StandIn {
            ep,
            core,
            generation,
            rx,
        }
    }

    /// The next coordination message within ten seconds.
    fn next_sent(rx: &mut mpsc::UnboundedReceiver<DeviceMessage>) -> DeviceMessage {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(m) = rx.try_recv() {
                return m;
            }
            assert!(Instant::now() < until, "no message");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn punch_go(punch_id: u64, request_id: Option<u64>, peer: DeviceId) -> PunchGo {
        PunchGo {
            punch_id,
            request_id,
            peer,
            role: if request_id.is_some() {
                Role::Controlling
            } else {
                Role::Controlled
            },
            mode: PunchMode::All,
            candidates: Vec::new(),
            peer_nat: NatMapping::default(),
            token_local: PunchToken([1; 16]),
            token_peer: PunchToken([2; 16]),
            start_after_ms: 0,
            window_ms: 300,
            handshake_ms: 300,
        }
    }

    use tasty_net_rendezvous::wire::message::{
        CancelReason, NatMapping, PunchCancel, PunchGo, PunchMode, PunchOffer, PunchToken, Role,
        ServerMessage,
    };

    /// A dial cancelled while it waits for `punch_go` leaves no request waiter behind, and a
    /// `punch_go` for it that comes after the cancel leaves no punch channel behind.
    #[test]
    fn a_cancelled_request_leaves_no_waiter_behind() {
        let peer = DeviceKey::from_seed([22; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(21, peer, &[]);
        let cancel = P2pCancel::new();
        let request_id = std::thread::scope(|s| {
            let dial = s.spawn(|| ep.connect_with_cancel(peer, &cancel));
            let DeviceMessage::PunchRequest(asked) = next_sent(&mut rx) else {
                panic!("punch_request first");
            };
            assert_eq!(lock(&core.requests).len(), 1);
            cancel.cancel();
            let r = dial.join().unwrap();
            assert_eq!(
                r.err().and_then(|e| e.connect_error().copied()),
                Some(P2pConnectError::Cancelled)
            );
            asked.request_id
        });
        assert!(lock(&core.requests).is_empty());
        let _enter = core.handle.enter();
        crate::server::dispatch(
            &core,
            generation,
            ServerMessage::PunchGo(punch_go(7, Some(request_id), peer)),
        );
        assert!(lock(&core.punches).is_empty());
        drop(_enter);
        ep.close();
    }

    /// A punch the acceptor took from one registration belongs to it: after the endpoint
    /// registers again, a cancel with the same punch number from the new server does not end
    /// it, and its result is not sent to the new server (docs/design/p2p.md "조정 연결을
    /// 잃으면").
    #[test]
    fn a_punch_of_an_earlier_registration_stays_with_it() {
        let peer = DeviceKey::from_seed([25; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(24, peer, &[]);
        {
            let _enter = core.handle.enter();
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchOffer(PunchOffer {
                    punch_id: 5,
                    from: peer,
                }),
            );
        }
        let DeviceMessage::PunchAnswer(answer) = next_sent(&mut rx) else {
            panic!("punch_answer first");
        };
        assert!(answer.accept);
        {
            let _enter = core.handle.enter();
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchGo(punch_go(5, None, peer)),
            );
        }
        // Registered again: the server numbers its punches anew.
        let (tx, mut again) = mpsc::unbounded_channel();
        let next = core.open_link(tx);
        assert_ne!(next, generation);
        crate::server::dispatch(
            &core,
            next,
            ServerMessage::PunchCancel(PunchCancel {
                punch_id: 5,
                reason: CancelReason::Expired,
            }),
        );
        let until = Instant::now() + Duration::from_secs(10);
        let closed = loop {
            assert!(Instant::now() < until, "the attempt ends");
            let ended = ep.events().into_iter().find_map(|e| match e {
                P2pEvent::Transition {
                    subject: crate::events::Subject::Connection(_),
                    to: Phase::Attempt(AttemptPhase::Closed),
                    cause,
                    ..
                } => Some(cause.name),
                _ => None,
            });
            if let Some(name) = ended {
                break name;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(closed, "accept_deadline_elapsed");
        assert!(again.try_recv().is_err(), "nothing sent to the new server");
        ep.close();
    }
}
