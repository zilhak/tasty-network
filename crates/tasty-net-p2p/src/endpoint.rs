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
        PunchHold,
    ),
    Refused(tasty_net_rendezvous::wire::message::RefuseReason),
}

/// The owner of a punch's server channel in [`Core::punches`]: dropping it removes the entry.
/// The dialer's channel rides in its `punch_go` answer, so the entry has an owner from the
/// moment it is made, also when the dialer gives the request up after the answer was queued
/// and drops it unread.
#[derive(Debug)]
pub(crate) struct PunchHold {
    core: std::sync::Weak<Core>,
    key: PunchKey,
}

impl PunchHold {
    /// Put the channel of punch `key` in the table, owned by the returned hold.
    pub(crate) fn insert(
        core: &Arc<Core>,
        key: PunchKey,
        tx: mpsc::UnboundedSender<PunchSignal>,
    ) -> Self {
        lock(&core.punches).insert(key, tx);
        PunchHold {
            core: Arc::downgrade(core),
            key,
        }
    }
}

impl Drop for PunchHold {
    fn drop(&mut self) {
        if let Some(core) = self.core.upgrade() {
            lock(&core.punches).remove(&self.key);
        }
    }
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
    move_endpoint_in(&mut lock(reg), events, shutting_down, to, cause)
}

/// [`move_endpoint`] on a state whose lock the caller already holds.
fn move_endpoint_in(
    r: &mut Registration,
    events: &Events,
    shutting_down: impl FnOnce() -> bool,
    to: EndpointPhase,
    cause: Cause,
) -> bool {
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
        nat: None,
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
            self.reg_moved();
        }
    }

    /// Move the endpoint state machine inside a section that already holds the state's lock,
    /// so the move goes with what that section checked; whether it moved. The caller calls
    /// [`Core::reg_moved`] after it releases the lock when it moved.
    pub(crate) fn endpoint_to_in(
        &self,
        r: &mut Registration,
        to: EndpointPhase,
        cause: Cause,
    ) -> bool {
        move_endpoint_in(r, &self.events, || self.is_shutting_down(), to, cause)
    }

    /// Wake the waiters of a registration change.
    pub(crate) fn reg_moved(&self) {
        self.reg_changed.send_modify(|n| *n = n.wrapping_add(1));
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
    /// wait). After the endpoint ended the queued events (`Off` among them) still come first;
    /// once the queue is empty the call returns [`P2pError::Closed`] at once, and the end wakes
    /// every caller that waits.
    pub fn next_event(&self, limit: Duration) -> Result<Option<P2pEvent>, P2pError> {
        if crate::runtime::in_async_task() {
            return Err(P2pError::InAsyncContext);
        }
        match self.core() {
            None => Ok(None),
            Some(c) => c.events.next(limit).map_err(|_| P2pError::Closed),
        }
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
            // The attempts in progress end now, each with its `Closed` transition: their
            // tasks see the shutdown and stop, and the list does not keep them.
            let off = Cause::named("endpoint_off");
            core.tracker.close_attempts(&off);
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
            // An attempt the server loop began while the shutdown started ends here too.
            core.tracker.close_attempts(&off);
            core.endpoint_to(EndpointPhase::Off, Cause::named("disabled"));
            core.events.end();
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
        stand_in_with(
            seed,
            peer,
            settings,
            Arc::new(crate::probe::tests_support::Sink),
        )
    }

    /// [`stand_in`] on the packet I/O `io`.
    fn stand_in_with(
        seed: u8,
        peer: DeviceId,
        settings: &[(&str, crate::settings::Value)],
        io: Arc<dyn tasty_net_rendezvous::packet_io::PacketIo>,
    ) -> StandIn {
        let pairing = Arc::new(crate::pairing::PairingList::new());
        pairing.add(peer);
        let mut config = P2pConfig::new(DeviceKey::from_seed([seed; 32]), pairing);
        config.enabled = true;
        config.server = Some(RendezvousServer {
            addr: "192.0.2.10:3478".parse().unwrap(),
            public_key: DeviceKey::from_seed([seed.wrapping_add(100); 32]).id(),
        });
        config.packet_io = Some(io);
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
            &mut tokio::task::JoinSet::new(),
        );
        assert!(lock(&core.punches).is_empty());
        drop(_enter);
        ep.close();
    }

    /// A `punch_go` answer that was queued for the dialer and then given up unread, or taken
    /// and dropped before the punch began, leaves no punch channel behind: the channel's hold
    /// rides in the answer.
    #[test]
    fn a_punch_go_queued_and_then_given_up_leaves_no_channel() {
        let peer = DeviceKey::from_seed([27; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            rx: _rx,
        } = stand_in(26, peer, &[]);
        let _enter = core.handle.enter();
        let ask = |request_id: u64| {
            let (tx, rx) = oneshot::channel();
            lock(&core.requests).insert(request_id, tx);
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchGo(punch_go(request_id, Some(request_id), peer)),
                &mut tokio::task::JoinSet::new(),
            );
            assert!(lock(&core.requests).is_empty());
            assert_eq!(lock(&core.punches).len(), 1, "the answer is queued");
            rx
        };
        // Queued, then the receiver is dropped unread (a cancel or the request timeout).
        drop(ask(8));
        assert!(lock(&core.punches).is_empty());
        // Taken, then dropped before the punch's own cleanup existed.
        let taken = ask(9).try_recv().expect("answer");
        assert_eq!(lock(&core.punches).len(), 1);
        drop(taken);
        assert!(lock(&core.punches).is_empty());
        drop(_enter);
        ep.close();
    }

    /// A NAT judgement scheduled on one registration that first runs after the next one began
    /// reads no STUN server, writes no judgement and reports nothing to either server; the same
    /// judgement scheduled on the current registration reports to it.
    #[test]
    fn a_judgement_scheduled_on_an_earlier_registration_does_nothing() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([29; 32]).id();
        let io = Arc::new(crate::probe::tests_support::CountingSink::default());
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in_with(
            28,
            peer,
            &[
                ("nat.stun_rto", Value::Duration(Duration::from_millis(10))),
                ("nat.stun_attempts", Value::Count(1)),
            ],
            io.clone(),
        );
        let stun_sent = || io.stun_sent.load(std::sync::atomic::Ordering::SeqCst);
        // Registered again before the judgement of the first registration runs.
        let (tx, mut again) = mpsc::unbounded_channel();
        let next = core.open_link(tx);
        assert_ne!(next, generation);
        let sent_before = stun_sent();
        core.handle
            .block_on(crate::server::rejudge(core.clone(), generation));
        assert_eq!(stun_sent(), sent_before, "no STUN request");
        assert!(lock(&core.reg).nat.is_none());
        assert!(rx.try_recv().is_err());
        assert!(again.try_recv().is_err());
        // The control: scheduled on the current registration, it reports there.
        core.handle
            .block_on(crate::server::rejudge(core.clone(), next));
        assert!(
            stun_sent() > sent_before,
            "the control asks the STUN servers"
        );
        assert!(matches!(again.try_recv(), Ok(DeviceMessage::NatReport(_))));
        assert!(lock(&core.reg).nat.is_some());
        ep.close();
    }

    /// The `Closed` transition of an attempt is its end record: one that ends at once on a
    /// `punch_go` past `punch.window_max` carries the NAT combination of that `punch_go`, and
    /// one that ends before `punch_go` carries none. The attempt has left the connection list
    /// by then, so nothing else could tell it.
    #[test]
    fn the_end_of_an_attempt_records_its_nat_combination() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([31; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(
            30,
            peer,
            &[(
                "punch.window_max",
                Value::Duration(Duration::from_millis(100)),
            )],
        );
        let ended = |ep: &P2pEndpoint| {
            ep.events().into_iter().find_map(|e| match e {
                P2pEvent::Transition {
                    subject: crate::events::Subject::Connection(id),
                    to: Phase::Attempt(AttemptPhase::Closed),
                    nat,
                    ..
                } => Some((id, nat)),
                _ => None,
            })
        };
        let _before = ep.events();
        let r = std::thread::scope(|s| {
            let dial = s.spawn(|| ep.connect(peer));
            let DeviceMessage::PunchRequest(asked) = next_sent(&mut rx) else {
                panic!("punch_request first");
            };
            // The punch_go window (300 ms) is past punch.window_max (100 ms).
            let _enter = core.handle.enter();
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchGo(punch_go(11, Some(asked.request_id), peer)),
                &mut tokio::task::JoinSet::new(),
            );
            drop(_enter);
            dial.join().unwrap()
        });
        assert_eq!(
            r.err().and_then(|e| e.connect_error().copied()),
            Some(P2pConnectError::ServerLimitsInvalid {
                key: "punch.window_max"
            })
        );
        let (first, nat) = ended(&ep).expect("closed");
        assert_eq!(
            nat,
            Some(crate::info::NatInfo {
                local: NatMapping::default(),
                peer: NatMapping::default(),
                mode: PunchMode::All,
            })
        );
        assert!(ep.connections().is_empty());
        // Ended before punch_go: no combination.
        let cancel = P2pCancel::new();
        std::thread::scope(|s| {
            let dial = s.spawn(|| ep.connect_with_cancel(peer, &cancel));
            // The first attempt's punch_result may come before the new request.
            while !matches!(next_sent(&mut rx), DeviceMessage::PunchRequest(_)) {}
            cancel.cancel();
            assert!(dial.join().unwrap().is_err());
        });
        let (second, nat) = ended(&ep).expect("closed");
        assert_ne!(second, first);
        assert_eq!(nat, None);
        ep.close();
    }

    /// `close` while an attempt waits in `RetryWait` (a long `punch.retry_delay`) ends that
    /// attempt with a `Closed` transition caused by `endpoint_off` and takes it off the list:
    /// after `Off`, `connections()` has no attempt.
    #[test]
    fn close_ends_an_attempt_in_progress_and_the_list_has_none() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([33; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(
            32,
            peer,
            &[(
                "punch.retry_delay",
                Value::Duration(Duration::from_secs(60)),
            )],
        );
        let _before = ep.events();
        let r = std::thread::scope(|s| {
            let dial = s.spawn(|| ep.connect(peer));
            let DeviceMessage::PunchRequest(asked) = next_sent(&mut rx) else {
                panic!("punch_request first");
            };
            {
                // Nobody answers the probes: the 300 ms window passes and a retry waits.
                let _enter = core.handle.enter();
                crate::server::dispatch(
                    &core,
                    generation,
                    ServerMessage::PunchGo(punch_go(13, Some(asked.request_id), peer)),
                    &mut tokio::task::JoinSet::new(),
                );
            }
            let until = Instant::now() + Duration::from_secs(10);
            while ep
                .connections()
                .iter()
                .all(|c| c.state != AttemptPhase::RetryWait)
            {
                assert!(Instant::now() < until, "RetryWait: {:?}", ep.connections());
                std::thread::sleep(Duration::from_millis(10));
            }
            ep.close();
            dial.join().unwrap()
        });
        assert!(matches!(r, Err(P2pError::Closed)), "{:?}", r.map(|_| ()));
        assert_eq!(ep.status().state, EndpointPhase::Off);
        assert!(ep.connections().is_empty(), "{:?}", ep.connections());
        let closed: Vec<_> = ep
            .events()
            .into_iter()
            .filter_map(|e| match e {
                P2pEvent::Transition {
                    subject: crate::events::Subject::Connection(_),
                    from,
                    to: Phase::Attempt(AttemptPhase::Closed),
                    cause,
                    ..
                } => Some((from, cause.name)),
                _ => None,
            })
            .collect();
        assert_eq!(
            closed,
            vec![(
                Some(Phase::Attempt(AttemptPhase::RetryWait)),
                "endpoint_off"
            )]
        );
    }

    /// A NAT judgement shows in the status only after its report is in the coordination
    /// link's queue: a punch_request the device sends after seeing the judgement reaches the
    /// server after the report, and the server picks the mode with the reported mapping.
    #[test]
    fn a_judgement_shows_in_the_status_after_its_report_is_queued() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([39; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(
            38,
            peer,
            &[
                ("nat.stun_rto", Value::Duration(Duration::from_millis(1))),
                ("nat.stun_attempts", Value::Count(1)),
            ],
        );
        for round in 0..100 {
            lock(&core.reg).nat = None;
            let _judging = core
                .handle
                .spawn(crate::server::rejudge(core.clone(), generation));
            let until = Instant::now() + Duration::from_secs(10);
            while ep.status().nat_answers.is_empty() {
                assert!(Instant::now() < until, "judged");
                std::hint::spin_loop();
            }
            assert!(
                matches!(rx.try_recv(), Ok(DeviceMessage::NatReport(_))),
                "round {round}: the report is not queued yet"
            );
        }
        ep.close();
    }

    /// The judgement of a registration leaves the status when that registration ends: the
    /// next registration shows none until its own report is in its link's queue, so a
    /// punch_request sent on seeing a judgement never goes ahead of the report the server
    /// judges the mode with (the server starts every registration at `unknown`).
    #[test]
    fn a_new_registration_shows_no_judgement_before_its_report_is_queued() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([41; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in(
            40,
            peer,
            &[
                ("nat.stun_rto", Value::Duration(Duration::from_millis(1))),
                ("nat.stun_attempts", Value::Count(1)),
            ],
        );
        core.handle
            .block_on(crate::server::rejudge(core.clone(), generation));
        assert!(matches!(rx.try_recv(), Ok(DeviceMessage::NatReport(_))));
        assert!(!ep.status().nat_answers.is_empty(), "the first one judged");
        crate::server::clear_registration(&core);
        let (tx, mut again) = mpsc::unbounded_channel();
        let next = core.open_link(tx);
        let s = ep.status();
        assert!(
            s.nat_answers.is_empty() && s.nat == NatMapping::Unknown,
            "the earlier judgement still shows: {s:?}"
        );
        let _judging = core
            .handle
            .spawn(crate::server::rejudge(core.clone(), next));
        let until = Instant::now() + Duration::from_secs(10);
        while ep.status().nat_answers.is_empty() {
            assert!(Instant::now() < until, "judged again");
            std::hint::spin_loop();
        }
        assert!(
            matches!(again.try_recv(), Ok(DeviceMessage::NatReport(_))),
            "the report of the new registration is not queued yet"
        );
        ep.close();
    }

    /// A judgement whose report the coordination link no longer takes (its receiver is gone
    /// while the session ends) does not show in the status: only a queued report does.
    #[test]
    fn a_judgement_the_link_does_not_take_stays_out_of_the_status() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([43; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            rx,
        } = stand_in(
            42,
            peer,
            &[
                ("nat.stun_rto", Value::Duration(Duration::from_millis(1))),
                ("nat.stun_attempts", Value::Count(1)),
            ],
        );
        let nat_changed = |events: Vec<P2pEvent>| {
            events.iter().any(
                |e| matches!(e, P2pEvent::Transition { cause, .. } if cause.name == "nat_changed"),
            )
        };
        let _before = ep.events();
        drop(rx);
        core.handle
            .block_on(crate::server::rejudge(core.clone(), generation));
        assert!(lock(&core.reg).nat.is_none(), "an unqueued judgement shows");
        assert!(ep.status().nat_answers.is_empty());
        assert!(!nat_changed(ep.events()), "no nat_changed for it");
        // The control: on a link that takes the report, the judgement shows.
        let (tx, mut again) = mpsc::unbounded_channel();
        let next = core.open_link(tx);
        core.handle
            .block_on(crate::server::rejudge(core.clone(), next));
        assert!(matches!(again.try_recv(), Ok(DeviceMessage::NatReport(_))));
        assert!(!ep.status().nat_answers.is_empty());
        assert!(nat_changed(ep.events()), "the control reports nat_changed");
        ep.close();
    }

    /// The `nat_changed` move of a judgement goes with its check of the registration: once
    /// the registration is cleared and the state left `Registered` (as the server loop does
    /// before Backoff or Stopped), that judgement does not move the state back. Each round
    /// takes the state's lock the moment the judgement shows and ends the registration there,
    /// the narrowest place a server loop on another worker can take it.
    #[test]
    fn a_judgement_does_not_move_an_ended_registration_back() {
        use crate::settings::Value;
        let peer = DeviceKey::from_seed([45; 32]).id();
        let StandIn { ep, core, .. } = stand_in(
            44,
            peer,
            &[
                ("nat.stun_rto", Value::Duration(Duration::from_millis(1))),
                ("nat.stun_attempts", Value::Count(1)),
            ],
        );
        for round in 0..2_000 {
            let (tx, _queue) = mpsc::unbounded_channel();
            let generation = core.open_link(tx);
            {
                let mut r = lock(&core.reg);
                r.nat = None;
                r.phase = Some(EndpointPhase::Registered);
            }
            let judging = core
                .handle
                .spawn(crate::server::rejudge(core.clone(), generation));
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(Instant::now() < until, "round {round}: judged");
                let Ok(mut r) = core.reg.try_lock() else {
                    std::hint::spin_loop();
                    continue;
                };
                if r.nat.is_some() {
                    // The server loop's end of the registration, in one step.
                    *lock(&core.link) = None;
                    r.nat = None;
                    r.phase = Some(EndpointPhase::Backoff);
                    break;
                }
            }
            core.handle.block_on(judging).expect("judged");
            assert_eq!(
                lock(&core.reg).phase,
                Some(EndpointPhase::Backoff),
                "round {round}: the judgement moved the ended registration back"
            );
        }
        ep.close();
    }

    /// The endpoint's end drops a dialer's attempt while it waits in `RetryWait`, and the NAT
    /// judgement that wait started ends with it: no STUN request goes out after that and no
    /// transaction ID waits. Only the shutdown signal is sent, so the runtime still runs and a
    /// detached judgement would go on asking until its attempts run out.
    #[test]
    fn the_end_of_the_endpoint_stops_the_judgement_of_a_retry_wait() {
        use crate::settings::Value;
        let io = Arc::new(crate::probe::tests_support::CountingSink::default());
        let peer = DeviceKey::from_seed([37; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in_with(
            36,
            peer,
            &[
                (
                    "punch.retry_delay",
                    Value::Duration(Duration::from_secs(60)),
                ),
                // Two hundred requests 50 ms apart: the judgement asks for the whole test.
                ("nat.stun_rto", Value::Duration(Duration::from_millis(50))),
                ("nat.stun_attempts", Value::Count(200)),
            ],
            io.clone(),
        );
        let stun_sent = || io.stun_sent.load(std::sync::atomic::Ordering::SeqCst);
        let r = std::thread::scope(|s| {
            let dial = s.spawn(|| ep.connect(peer));
            let DeviceMessage::PunchRequest(asked) = next_sent(&mut rx) else {
                panic!("punch_request first");
            };
            {
                // Nobody answers the probes: the 300 ms window passes and a retry waits.
                let _enter = core.handle.enter();
                crate::server::dispatch(
                    &core,
                    generation,
                    ServerMessage::PunchGo(punch_go(14, Some(asked.request_id), peer)),
                    &mut tokio::task::JoinSet::new(),
                );
            }
            let until = Instant::now() + Duration::from_secs(10);
            while ep
                .connections()
                .iter()
                .all(|c| c.state != AttemptPhase::RetryWait)
            {
                assert!(Instant::now() < until, "RetryWait: {:?}", ep.connections());
                std::thread::sleep(Duration::from_millis(5));
            }
            // The probes have ended; what goes out now is the judgement's.
            let waiting = stun_sent();
            while !(stun_sent() > waiting && core.router.waiting() > 0) {
                assert!(Instant::now() < until, "judging: {:?}", ep.connections());
                std::thread::sleep(Duration::from_millis(5));
            }
            core.shutdown.send_replace(true);
            let r = dial.join().unwrap();
            // The attempt sees the signal at once; a request in flight may still go out.
            std::thread::sleep(Duration::from_millis(100));
            let after = stun_sent();
            // Six request intervals.
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(stun_sent(), after, "STUN requests sent after the end");
            assert_eq!(core.router.waiting(), 0, "transaction IDs left");
            r
        });
        assert!(matches!(r, Err(P2pError::Closed)), "{:?}", r.map(|_| ()));
        ep.close();
    }

    /// The endpoint's end drops the acceptor's attempt while it punches, and its probes end
    /// with it: no STUN probe goes out after that and no transaction ID waits. The shutdown
    /// signal alone ends the attempt here, so the runtime still runs and a detached probe task
    /// would go on sending until its window ends.
    #[test]
    fn the_end_of_the_endpoint_stops_the_probes_of_a_punching_acceptor() {
        use tasty_net_rendezvous::wire::candidate::CandidateKind;
        use tasty_net_rendezvous::wire::message::Candidate;
        let io = Arc::new(crate::probe::tests_support::CountingSink::default());
        let peer = DeviceKey::from_seed([35; 32]).id();
        let StandIn {
            ep,
            core,
            generation,
            mut rx,
        } = stand_in_with(34, peer, &[], io.clone());
        let stun_sent = || io.stun_sent.load(std::sync::atomic::Ordering::SeqCst);
        {
            let _enter = core.handle.enter();
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchOffer(PunchOffer {
                    punch_id: 15,
                    from: peer,
                }),
                &mut tokio::task::JoinSet::new(),
            );
        }
        let DeviceMessage::PunchAnswer(answer) = next_sent(&mut rx) else {
            panic!("punch_answer first");
        };
        assert!(answer.accept);
        // A window long enough to punch through the whole test.
        let mut go = punch_go(15, None, peer);
        go.window_ms = 4_000;
        go.candidates = vec![Candidate {
            addr: "203.0.113.9:40000".parse().unwrap(),
            kind: CandidateKind::ServerReflexive,
        }];
        {
            let _enter = core.handle.enter();
            crate::server::dispatch(
                &core,
                generation,
                ServerMessage::PunchGo(go),
                &mut tokio::task::JoinSet::new(),
            );
        }
        let until = Instant::now() + Duration::from_secs(10);
        while !(ep
            .connections()
            .iter()
            .any(|c| c.state == AttemptPhase::Punching)
            && stun_sent() > 0
            && core.router.waiting() > 0)
        {
            assert!(Instant::now() < until, "punching: {:?}", ep.connections());
            std::thread::sleep(Duration::from_millis(5));
        }
        core.shutdown.send_replace(true);
        // The attempt sees the signal at once; a probe in flight may still finish its send.
        std::thread::sleep(Duration::from_millis(100));
        let after = stun_sent();
        // Six probe intervals (punch.probe_interval 50 ms).
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(stun_sent(), after, "probes sent after the end");
        assert_eq!(core.router.waiting(), 0, "transaction IDs left");
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
                &mut tokio::task::JoinSet::new(),
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
                &mut tokio::task::JoinSet::new(),
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
            &mut tokio::task::JoinSet::new(),
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

    /// Packet I/O over a loopback UDP socket that counts the STUN requests it sends and drops
    /// every STUN message it receives: a NAT judgement never gets its answers and asks on.
    #[derive(Debug)]
    struct UnansweredStun {
        udp: tasty_net_rendezvous::packet_io::UdpPacketIo,
        stun_sent: std::sync::atomic::AtomicU64,
    }

    fn is_stun(packet: &[u8]) -> bool {
        let cookie = tasty_net_rendezvous::wire::stun::MAGIC_COOKIE.to_be_bytes();
        packet.get(4..8) == Some(&cookie[..])
    }

    impl tasty_net_rendezvous::packet_io::PacketIo for UnansweredStun {
        fn poll_recv_from(
            &self,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<(usize, std::net::SocketAddr)>> {
            self.poll_recv_with_dst(cx, buf)
                .map_ok(|(len, from, _)| (len, from))
        }
        fn poll_recv_with_dst(
            &self,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<(usize, std::net::SocketAddr, Option<std::net::IpAddr>)>>
        {
            loop {
                match self.udp.poll_recv_with_dst(cx, buf) {
                    std::task::Poll::Ready(Ok((len, _, _))) if is_stun(&buf[..len]) => {}
                    other => return other,
                }
            }
        }
        fn poll_send_ready(
            &self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.udp.poll_send_ready(cx)
        }
        fn try_send_to(&self, buf: &[u8], to: std::net::SocketAddr) -> std::io::Result<()> {
            if is_stun(buf) {
                self.stun_sent
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.udp.try_send_to(buf, to)
        }
        fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            self.udp.local_addr()
        }
        fn may_fragment(&self) -> bool {
            self.udp.may_fragment()
        }
        fn dual_stack(&self) -> bool {
            self.udp.dual_stack()
        }
    }

    /// The endpoint's end stops the NAT judgement its registration started: no STUN request
    /// goes out after that and no transaction ID waits. The endpoint registers with a real
    /// server on loopback, whose STUN answers never reach it, so the judgement asks on. Only
    /// the shutdown signal is sent, so the runtime still runs and a detached judgement would go
    /// on asking until its attempts run out.
    #[test]
    fn the_end_of_the_endpoint_stops_the_judgement_its_registration_started() {
        use crate::settings::Value;
        use tasty_net_identity::DeviceKey;
        use tasty_net_rendezvous::server::{Server, ServerConfig};
        use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut settings = ServerSettings::new();
        settings
            .set(
                "listen.primary",
                ServerValue::Address("127.0.0.1:0".parse().unwrap()),
            )
            .unwrap();
        settings
            .set("listen.alternate_port", ServerValue::Port(0))
            .unwrap();
        let mut config = ServerConfig::new(DeviceKey::from_seed([40; 32]));
        config.settings = settings;
        let server = rt.block_on(Server::start(config)).expect("server starts");
        let udp = rt
            .block_on(tasty_net_rendezvous::packet_io::UdpPacketIo::bind(
                "127.0.0.1:0".parse().unwrap(),
            ))
            .unwrap();
        let io = Arc::new(UnansweredStun {
            udp,
            stun_sent: std::sync::atomic::AtomicU64::new(0),
        });
        let stun_sent = || io.stun_sent.load(std::sync::atomic::Ordering::SeqCst);
        let mut config = crate::P2pConfig::new(
            DeviceKey::from_seed([41; 32]),
            Arc::new(crate::PairingList::new()),
        );
        config.enabled = true;
        config.server = Some(crate::RendezvousServer {
            addr: server.addrs().primary,
            public_key: server.public_key(),
        });
        config.packet_io = Some(io.clone());
        // Two hundred requests 50 ms apart: the judgement asks for the whole test.
        config
            .settings
            .set("nat.stun_rto", Value::Duration(Duration::from_millis(50)));
        config.settings.set("nat.stun_attempts", Value::Count(200));
        let ep = crate::P2pEndpoint::bind(config).expect("bind");
        let core = ep.core().unwrap().clone();
        let until = Instant::now() + Duration::from_secs(10);
        while !(lock(&core.reg).phase == Some(EndpointPhase::Registered)
            && stun_sent() > 0
            && core.router.waiting() > 0)
        {
            assert!(Instant::now() < until, "judging: {:?}", ep.status());
            std::thread::sleep(Duration::from_millis(5));
        }
        core.shutdown.send_replace(true);
        // The registration sees the signal at once; a request in flight may still go out.
        std::thread::sleep(Duration::from_millis(100));
        let after = stun_sent();
        // Six request intervals.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(stun_sent(), after, "STUN requests sent after the end");
        assert_eq!(core.router.waiting(), 0, "transaction IDs left");
        ep.close();
        rt.block_on(server.shutdown());
    }
}
