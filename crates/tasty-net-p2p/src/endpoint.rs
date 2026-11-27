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
use crate::conn::{P2pCancel, P2pConnection};
use crate::error::{P2pConnectError, P2pError, StopCause};
use crate::events::{AttemptPhase, Cause, CauseDetail, EndpointPhase, Events, P2pEvent, Phase};
use crate::info::{P2pConnectionInfo, P2pStatus, RendezvousServer, Tracked, Tracker};
use crate::nat::NatJudgement;
use crate::pairing::PairingStore;
use crate::runtime::{Owner, block_on};
use crate::settings::{OsValues, P2pParams, P2pSettings, SettingEntry};
use crate::socket::{ProbeSession, StunCounters, StunRouter, canonical};

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
    /// that one key has one P2P endpoint ([`P2pError::EndpointInUse`]).
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
    /// A QUIC handshake from a session address began.
    Started,
    /// One of those handshakes ended without a connection for this session.
    Ended,
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
}

/// The current coordination connection's writer.
#[derive(Debug, Clone)]
pub(crate) struct Link {
    pub(crate) tx: mpsc::UnboundedSender<DeviceMessage>,
}

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
    pub(crate) io: Arc<dyn PacketIo>,
    pub(crate) router: Arc<StunRouter>,
    pub(crate) demux: Arc<DemuxCounters>,
    pub(crate) endpoint: quinn::Endpoint,
    pub(crate) handle: Handle,
    pub(crate) events: Arc<Events>,
    pub(crate) tracker: Arc<Tracker>,
    pub(crate) reg: Mutex<Registration>,
    /// Bumped at every registration change, for attempts that wait for one.
    pub(crate) reg_changed: watch::Sender<u64>,
    pub(crate) link: Mutex<Option<Link>>,
    pub(crate) requests: Mutex<HashMap<u64, oneshot::Sender<RequestOutcome>>>,
    pub(crate) punches: Mutex<HashMap<u64, mpsc::UnboundedSender<PunchSignal>>>,
    pub(crate) next_request: AtomicU64,
    pub(crate) accepting: Mutex<Vec<AcceptSession>>,
    pub(crate) accepted_tx: mpsc::UnboundedSender<P2pConnection>,
    pub(crate) accepted_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<P2pConnection>>,
    /// Set when the endpoint shuts down.
    pub(crate) shutdown: watch::Sender<bool>,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("id", &self.id)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Core {
    /// Move the endpoint state machine.
    pub(crate) fn endpoint_to(&self, to: EndpointPhase, cause: Cause) {
        let from = {
            let mut r = lock(&self.reg);
            // After the shutdown reported `Off`, `Off` stays the last state: a server loop that
            // was between two steps when the shutdown began does not move the endpoint again.
            if r.phase == Some(EndpointPhase::Off) && self.is_shutting_down() {
                return;
            }
            let from = r.phase.replace(to);
            r.last_cause = Some(cause.clone());
            from
        };
        self.events.push(P2pEvent::Transition {
            subject: crate::events::Subject::Endpoint,
            from: from.map(Phase::Endpoint),
            to: Phase::Endpoint(to),
            cause,
            at: SystemTime::now(),
        });
        self.reg_changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Send a message on the coordination connection; whether there is one.
    pub(crate) fn send(&self, msg: DeviceMessage) -> bool {
        lock(&self.link)
            .as_ref()
            .is_some_and(|l| l.tx.send(msg).is_ok())
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
/// [`P2pConnection`] are dropped, or at [`P2pEndpoint::close`].
#[derive(Clone)]
pub struct P2pEndpoint {
    inner: Arc<Inner>,
}

struct Inner {
    id: DeviceId,
    settings: P2pSettings,
    on: Option<On>,
}

struct On {
    core: Arc<Core>,
    owner: Mutex<Option<Owner>>,
    _lock: Option<File>,
}

impl fmt::Debug for P2pEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pEndpoint")
            .field("id", &self.inner.id)
            .field("enabled", &self.inner.on.is_some())
            .finish()
    }
}

/// The local IP the OS uses towards `server`, without sending anything.
fn route_ip(server: SocketAddr) -> Option<IpAddr> {
    let any: SocketAddr = match server {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, u16::MIN).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, u16::MIN).into(),
    };
    let s = UdpSocket::bind(any).ok()?;
    s.connect(server).ok()?;
    s.local_addr().ok().map(|a| a.ip())
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
        let lock_file = config.key_dir.as_deref().map(lock_key_dir).transpose()?;
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
        } else if let Some(ip) = route_ip(server.addr) {
            host_ips.push(ip);
        }
        host_ips.dedup();
        let router = Arc::new(StunRouter::default());
        let demux = Arc::new(DemuxCounters::default());
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
        let mut server_config = tasty_net_rendezvous::quic::server_config(&params.quic, crypto);
        server_config.migration(params.allow_migration);
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
            router,
            demux,
            endpoint,
            handle: handle.clone(),
            events,
            tracker,
            reg: Mutex::default(),
            reg_changed: watch::channel(u64::MIN).0,
            link: Mutex::new(None),
            requests: Mutex::default(),
            punches: Mutex::default(),
            next_request: AtomicU64::new(u64::MIN),
            accepting: Mutex::default(),
            accepted_tx,
            accepted_rx: tokio::sync::Mutex::new(accepted_rx),
            shutdown: watch::channel(false).0,
        });
        lock(&core.reg).phase = Some(EndpointPhase::Off);
        handle.spawn(crate::server::run(core.clone()));
        handle.spawn(accept_loop(core.clone()));
        Ok(P2pEndpoint {
            inner: Arc::new(Inner {
                id,
                settings: config.settings,
                on: Some(On {
                    core,
                    owner: Mutex::new(Some(owner)),
                    _lock: lock_file,
                }),
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
        let Some(core) = self.core() else {
            return Err(P2pConnectError::Disabled.into());
        };
        if peer == core.id {
            return Err(P2pConnectError::SelfAttach.into());
        }
        let core = core.clone();
        let cancel = cancel.clone();
        let fut = async move { crate::attempt::dial(core, peer, cancel).await };
        let handle = &self.core().expect("checked").handle;
        let task = block_on(handle, async { handle.spawn(fut).await })
            .map_err(|_| P2pError::InAsyncContext)?;
        match task {
            Ok(r) => r.map_err(P2pError::Connect),
            Err(_) => Err(P2pError::Closed),
        }
    }

    /// Wait for the next incoming connection.
    pub fn accept(&self) -> Result<P2pConnection, P2pError> {
        self.accept_timeout(None)
    }

    /// Wait for the next incoming connection up to `limit`.
    pub fn accept_timeout(&self, limit: Option<Duration>) -> Result<P2pConnection, P2pError> {
        let Some(core) = self.core() else {
            return Err(P2pError::Disabled);
        };
        let wait = async {
            let mut rx = core.accepted_rx.lock().await;
            match limit {
                Some(t) => tokio::time::timeout(t, rx.recv())
                    .await
                    .map_err(|_| P2pError::TimedOut)?
                    .ok_or(P2pError::Closed),
                None => rx.recv().await.ok_or(P2pError::Closed),
            }
        };
        block_on(&core.handle, wait).map_err(|_| P2pError::InAsyncContext)?
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

    /// The oldest queued event, waiting up to `limit` for one.
    pub fn next_event(&self, limit: Duration) -> Option<P2pEvent> {
        self.core().and_then(|c| c.events.next(limit))
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
        let Some(on) = &self.inner.on else {
            return;
        };
        let finished = {
            let mut owner = lock(&on.owner);
            let Some(mut o) = owner.take() else {
                return;
            };
            let finished = o.finished();
            o.stop(shutdown_work(on.core.clone()));
            finished
        };
        if block_on(&on.core.handle, async {}).is_ok() {
            finished.wait(on.core.params.shutdown_timeout);
        }
    }
}

fn shutdown_work(core: Arc<Core>) -> crate::runtime::Shutdown {
    Box::new(move |handle: &Handle| {
        handle.block_on(async move {
            core.shutdown.send_replace(true);
            core.endpoint_to(EndpointPhase::Off, Cause::named("disabled"));
            core.endpoint.close(
                P2pCloseReason::Closed.var_int(),
                P2pCloseReason::Closed.reason(),
            );
            let _done =
                tokio::time::timeout(core.params.shutdown_timeout, core.endpoint.wait_idle())
                    .await
                    .is_ok();
        });
    })
}

impl Drop for On {
    fn drop(&mut self) {
        if let Some(mut o) = lock(&self.owner).take() {
            o.stop(shutdown_work(self.core.clone()));
        }
    }
}

/// The session a connection from `addr` whose TLS device ID is `id` belongs to: among the
/// sessions that took its Initial, the one whose peer is `id` (docs/design/p2p.md "펀칭 절차" 9).
pub(crate) fn pick_session(sessions: &[AcceptSession], id: &DeviceId) -> Option<AcceptSession> {
    sessions.iter().find(|s| s.probe.peer == *id).cloned()
}

/// Take QUIC Initials only from addresses of active punch sessions; bind every connection to
/// its session by device ID.
async fn accept_loop(core: Arc<Core>) {
    let mut shutdown = core.shutdown.subscribe();
    loop {
        let incoming = tokio::select! {
            i = core.endpoint.accept() => i,
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
        let core = core.clone();
        tokio::spawn(async move {
            for s in &matching {
                let _sent = s.to.send(Incoming::Started).is_ok();
            }
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
                    ended(&matching);
                    return;
                }
                Err(_) => {
                    debug!(%addr, "incoming QUIC handshake timed out");
                    ended(&matching);
                    return;
                }
            };
            let Some(id) = tls::peer_device_id(&conn) else {
                conn.close(
                    P2pCloseReason::SessionMismatch.var_int(),
                    P2pCloseReason::SessionMismatch.reason(),
                );
                ended(&matching);
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
                    ended(&matching);
                }
            }
        });
    }
}

fn ended(sessions: &[AcceptSession]) {
    for s in sessions {
        let _sent = s.to.send(Incoming::Ended).is_ok();
    }
}

/// Watch a connection that is up: its end, and (acceptor) the remote address quinn uses.
pub(crate) async fn watch_connection(
    core: Arc<Core>,
    tracked: Tracked,
    conn: P2pConnection,
    quic: quinn::Connection,
    poll_address: bool,
) {
    let mut tick = tokio::time::interval(core.params.path_poll_interval);
    let mut last = canonical(quic.remote_address());
    let mut shutdown = core.shutdown.subscribe();
    let cause = loop {
        tokio::select! {
            e = quic.closed() => break close_cause(&e, &conn),
            _ = shutdown.changed() => {
                quic.close(P2pCloseReason::Closed.var_int(), P2pCloseReason::Closed.reason());
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

fn close_cause(e: &ConnectionError, conn: &P2pConnection) -> Cause {
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
