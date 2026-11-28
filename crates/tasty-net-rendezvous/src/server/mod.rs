//! The rendezvous server (docs/design/rendezvous.md).
//!
//! [`Server::start`] binds the primary port (QUIC coordination + STUN, split by the first
//! byte), the alternate port and, when set, the second IP's two ports (STUN only). It needs a
//! tokio runtime. The send function of a connection takes [`ServerMessage`] values only, and
//! the STUN path answers only the sender: there is no path that passes a device's bytes on.

mod state;

use std::collections::HashMap;
use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use quinn::crypto::rustls::QuicServerConfig;
use tasty_net_identity::{DeviceId, DeviceKey};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::access::{AccessPolicy, Everyone};
use crate::packet_io::{
    DemuxCounters, DemuxSocket, PacketIo, StunSink, UdpPacketIo, receive_error_is_fatal,
};
use crate::settings::{
    Absent, ServerParams, ServerSettings, SettingEntry, SettingsError, Source, Value,
    advertise_fits,
};
use crate::tls::{self, TlsError, TlsOptions};
use crate::wire::demux::{PacketClass, classify};
use crate::wire::frame::{FrameError, read_frame, read_frame_by, write_frame};
use crate::wire::message::{
    CancelReason, DeviceMessage, Limits, ReflexiveChanged, ServerMessage, Welcome,
};
use crate::wire::stun::{ServerStunAction, server_answer};
use crate::wire::{ALPN_RENDEZVOUS, CloseCode, PROTO};

use state::{After, Counters, RegisterRefusal, Registration, State, filter_host, stored_host};

/// The query line that tells whether QUIC MTU discovery runs on the primary socket.
pub const MTU_DISCOVERY: &str = "quic.mtu_discovery";

/// What a server is started with.
#[derive(Clone)]
pub struct ServerConfig {
    /// The server key. Devices pin its public key.
    pub key: DeviceKey,
    /// The settings.
    pub settings: ServerSettings,
    /// Who may register.
    pub access: Arc<dyn AccessPolicy>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("key", &self.key.id())
            .field("settings", &self.settings)
            .field("access", &self.access)
            .finish()
    }
}

impl ServerConfig {
    /// A configuration with default settings that lets every device in.
    pub fn new(key: DeviceKey) -> Self {
        ServerConfig {
            key,
            settings: ServerSettings::new(),
            access: Arc::new(Everyone),
        }
    }
}

/// Packet I/O the server runs on instead of binding its own sockets (for example a mock NAT).
#[derive(Debug, Clone)]
pub struct ServerIo {
    /// The primary address: QUIC and STUN.
    pub primary: Arc<dyn PacketIo>,
    /// The alternate port on the primary IP: STUN only.
    pub alternate_port: Arc<dyn PacketIo>,
    /// The second IP's primary and alternate port, when there is a second IP: STUN only.
    pub alternate_ip: Option<(Arc<dyn PacketIo>, Arc<dyn PacketIo>)>,
}

/// The addresses the server is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerAddrs {
    /// The primary address.
    pub primary: SocketAddr,
    /// The alternate port on the primary IP.
    pub alternate_port: SocketAddr,
    /// The second IP's two addresses.
    pub alternate_ip: Option<(SocketAddr, SocketAddr)>,
}

/// Why the server did not start.
#[derive(Debug)]
pub enum StartError {
    /// The settings are not consistent. Nothing was bound.
    Settings(SettingsError),
    /// A socket could not be bound.
    Bind(io::Error),
    /// The TLS configuration could not be made.
    Tls(TlsError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Settings(e) => write!(f, "server settings are invalid: {e}"),
            StartError::Bind(e) => write!(f, "server socket could not be bound: {e}"),
            StartError::Tls(e) => write!(f, "server TLS could not be set up: {e}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartError::Settings(e) => Some(e),
            StartError::Bind(e) => Some(e),
            StartError::Tls(e) => Some(e),
        }
    }
}

/// Running state of the server (docs/design/rendezvous.md "서버 설정값" "실행 중 조회").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerStatus {
    /// Registered devices.
    pub registrations: usize,
    /// Punches in progress.
    pub punches: usize,
    /// Registrations refused by `max_devices`.
    pub busy_max_devices: u64,
    /// Requests refused by `max_punches`.
    pub busy_max_punches: u64,
    /// Requests refused by `max_punches_per_device`.
    pub busy_per_device: u64,
    /// Requests refused by `punch_requests_per_minute`.
    pub busy_request_rate: u64,
    /// Requests refused because the pair has a session in the other direction.
    pub busy_pair: u64,
    /// STUN requests answered.
    pub stun_answered: u64,
    /// STUN requests not answered because of `stun_per_source_per_second`.
    pub stun_rate_limited: u64,
    /// STUN requests not answered because the window already counts `max_stun_sources`
    /// other source addresses.
    pub stun_sources_full: u64,
    /// STUN packets dropped (not a plain Binding Request, or not STUN on a STUN-only port).
    pub stun_dropped: u64,
    /// Packets on the primary port of no known first-byte class.
    pub packets_dropped: u64,
    /// Receive errors of one datagram on any port, dropped.
    pub receive_errors: u64,
    /// Sends on the primary port the OS refused as larger than the path (MTU discovery
    /// probes), dropped.
    pub send_too_large: u64,
    /// Candidate addresses the rule refused.
    pub candidates_rejected: u64,
    /// Candidate addresses left out by the per-kind caps.
    pub candidates_truncated: u64,
    /// Registrations made since start.
    pub registrations_total: u64,
    /// Registrations replaced by a new one of the same device.
    pub superseded: u64,
    /// Connections the access policy refused.
    pub access_denied: u64,
    /// Connections closed as `busy` because their send queue (`max_queued_messages`) was full.
    pub queue_full: u64,
    /// Connections not registered yet (handshake or `hello` pending).
    pub pending: usize,
    /// Connection attempts refused because `max_pending_connections` were pending.
    pub pending_refused: u64,
    /// `punch_cancel{expired}` messages sent (a punch's deadline passed before its results).
    pub expired_cancels: u64,
}

struct Inner {
    params: ServerParams,
    entries: Vec<SettingEntry>,
    key_id: DeviceId,
    access: Arc<dyn AccessPolicy>,
    state: Mutex<State>,
    counters: Counters,
    stun_window: Mutex<StunWindow>,
    demux: Arc<DemuxCounters>,
    queue_full: Arc<std::sync::atomic::AtomicU64>,
    pending: AtomicUsize,
    addrs: ServerAddrs,
    connection_tasks: Arc<Tasks>,
}

/// The tasks of the connections: the server's own (handshake and `hello`, reader, writer,
/// monitor) and the drivers quinn spawns for the endpoint and each connection, which hold the
/// sockets. [`Server::shutdown`] ends what is left of them once `shutdown_timeout` passed.
struct Tasks(Mutex<Option<JoinSet<()>>>);

impl fmt::Debug for Tasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tasks").finish_non_exhaustive()
    }
}

impl Tasks {
    fn new() -> Self {
        Tasks(Mutex::new(Some(JoinSet::new())))
    }

    /// Run `task`; after [`Tasks::end`] it is dropped without running.
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut set = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = set.as_mut() {
            // Finished tasks leave the set here, so it holds only the running ones.
            while set.try_join_next().is_some() {}
            set.spawn(task);
        }
    }

    /// Abort every task and wait until each has stopped.
    async fn end(&self) {
        let set = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut set) = set {
            set.abort_all();
            while set.join_next().await.is_some() {}
        }
    }
}

/// quinn's tokio runtime with the tasks it spawns kept in [`Tasks`].
#[derive(Debug)]
struct QuinnRuntime(Arc<Tasks>);

impl quinn::Runtime for QuinnRuntime {
    fn new_timer(&self, at: std::time::Instant) -> Pin<Box<dyn quinn::AsyncTimer>> {
        quinn::TokioRuntime.new_timer(at)
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.0.spawn(future);
    }

    fn wrap_udp_socket(
        &self,
        socket: std::net::UdpSocket,
    ) -> io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
        quinn::TokioRuntime.wrap_udp_socket(socket)
    }

    fn now(&self) -> std::time::Instant {
        quinn::TokioRuntime.now()
    }
}

/// One connection counted in `Inner::pending` until it is dropped.
struct Pending(Arc<Inner>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

struct StunWindow {
    start: Instant,
    counts: HashMap<SocketAddr, u64>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(&self) -> ServerStatus {
        let (registrations, punches, expired_cancels) = {
            let s = self.state();
            (s.devices.len(), s.punches.len(), s.expired_cancels)
        };
        let c = &self.counters;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        ServerStatus {
            registrations,
            punches,
            busy_max_devices: get(&c.busy_max_devices),
            busy_max_punches: get(&c.busy_max_punches),
            busy_per_device: get(&c.busy_per_device),
            busy_request_rate: get(&c.busy_request_rate),
            busy_pair: get(&c.busy_pair),
            stun_answered: get(&c.stun_answered),
            stun_rate_limited: get(&c.stun_rate_limited),
            stun_sources_full: get(&c.stun_sources_full),
            stun_dropped: get(&c.stun_dropped),
            packets_dropped: get(&self.demux.dropped),
            receive_errors: get(&self.demux.receive_errors),
            send_too_large: get(&self.demux.send_too_large),
            candidates_rejected: get(&c.candidates_rejected),
            candidates_truncated: get(&c.candidates_truncated),
            registrations_total: get(&c.registrations_total),
            superseded: get(&c.superseded),
            access_denied: get(&c.access_denied),
            queue_full: get(&self.queue_full),
            pending: self.pending.load(Ordering::Relaxed),
            pending_refused: get(&c.pending_refused),
            expired_cancels,
        }
    }

    fn limits(&self) -> Limits {
        let p = &self.params;
        let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Limits {
            offer_timeout_ms: ms(p.offer_timeout),
            start_delay_max_ms: ms(p.start_delay_max),
            punch_window_ms: ms(p.punch_window),
            quic_handshake_ms: ms(p.quic_handshake_timeout),
            punch_session_ttl_ms: ms(p.punch_session_ttl()),
            max_host_candidates: n(p.max_host_candidates),
            max_reported_reflexive: n(p.max_reported_reflexive),
            max_message_bytes: p.max_message_bytes,
        }
    }

    /// The alternate STUN addresses for a device that reached the server from `remote`: only
    /// addresses of the same family, never a wildcard.
    fn advertised(&self, remote: SocketAddr) -> Vec<SocketAddr> {
        let p = &self.params;
        let a = &self.addrs;
        let mut out = Vec::new();
        let alt_port = p
            .advertise_alternate_port
            .unwrap_or_else(|| a.alternate_port.port());
        // Without an advertised IP, the IP the socket is really bound to.
        let primary_ip = p
            .advertise_primary_ip
            .or(Some(a.primary.ip()).filter(|ip| !ip.is_unspecified()));
        if let Some(ip) = primary_ip {
            out.push(SocketAddr::new(ip, alt_port));
        }
        let second_ip = p.advertise_alternate_ip.or(a
            .alternate_ip
            .map(|(primary, _)| primary.ip())
            .filter(|ip| !ip.is_unspecified()));
        if let Some(ip) = second_ip {
            out.push(SocketAddr::new(ip, a.primary.port()));
            out.push(SocketAddr::new(ip, alt_port));
        }
        let v4 = remote.ip().to_canonical().is_ipv4();
        out.retain(|s| s.ip().to_canonical().is_ipv4() == v4 && !s.ip().is_unspecified());
        out
    }

    /// Arrange what a state change asks for. It runs under the same lock as the change: a
    /// timer is spawned and its handle stored with the stage it is for before any other
    /// message can move the punch on, so no handle is overwritten or lost.
    fn after(self: &Arc<Self>, state: &mut State, after: After) {
        if let After::Timer {
            punch_id,
            stage,
            at,
        } = after
        {
            let inner = self.clone();
            let handle = tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                inner.state().expire(punch_id, stage);
            });
            state.set_timer(punch_id, stage, handle.abort_handle());
        }
    }
}

impl StunSink for Inner {
    fn on_stun(&self, packet: &[u8], from: SocketAddr, io: &dyn PacketIo) {
        // On a dual-stack socket an IPv4 device shows up as ::ffff:a.b.c.d; it is told its
        // IPv4 address. The answer still goes to the address the request came from.
        let seen = canonical(from);
        let reply = match server_answer(packet, seen) {
            ServerStunAction::Answer(reply) => reply,
            ServerStunAction::Drop => {
                self.counters.stun_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        {
            let mut w = self.stun_window.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if now.duration_since(w.start) >= self.params.stun_rate_window {
                // A new map gives the memory of a full window back.
                w.start = now;
                w.counts = HashMap::new();
            }
            let sources = w.counts.len();
            let n = match w.counts.get_mut(&seen) {
                Some(n) => n,
                None if sources >= self.params.max_stun_sources => {
                    self.counters
                        .stun_sources_full
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                None => w.counts.entry(seen).or_default(),
            };
            *n += 1;
            if *n > self.params.stun_per_source_per_second {
                self.counters
                    .stun_rate_limited
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        match io.try_send_to(&reply, from) {
            Ok(()) => {
                self.counters.stun_answered.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => debug!(error = %e, "STUN answer not sent"),
        }
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerInner")
            .field("key", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// A running rendezvous server.
pub struct Server {
    inner: Arc<Inner>,
    endpoint: quinn::Endpoint,
    tasks: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("key", &self.inner.key_id)
            .field("addrs", &self.inner.addrs)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Check the settings, bind the sockets of `listen.*` and start serving.
    pub async fn start(config: ServerConfig) -> Result<Server, StartError> {
        let params = config.settings.resolve().map_err(StartError::Settings)?;
        let primary = bind(params.listen_primary).await?;
        let primary_ip = primary.local_addr().map_err(StartError::Bind)?.ip();
        let alternate_port =
            bind(SocketAddr::new(primary_ip, params.listen_alternate_port)).await?;
        let primary_port = primary.local_addr().map_err(StartError::Bind)?.port();
        let alternate_ip = match params.listen_alternate_ip {
            None => None,
            Some(ip) => {
                let alt_port = alternate_port
                    .local_addr()
                    .map_err(StartError::Bind)?
                    .port();
                Some((
                    bind(SocketAddr::new(ip, primary_port)).await?,
                    bind(SocketAddr::new(ip, alt_port)).await?,
                ))
            }
        };
        Self::run(
            config,
            params,
            ServerIo {
                primary,
                alternate_port,
                alternate_ip,
            },
        )
    }

    /// Check the settings and start serving on the given packet I/O. `listen.*` is not used
    /// for binding, so the advertised defaults and the checks of `advertise.*` follow the
    /// addresses the injected sockets are really bound to, not `listen.*`.
    pub async fn start_with_io(config: ServerConfig, io: ServerIo) -> Result<Server, StartError> {
        let params = config
            .settings
            .resolve_for_injected_io()
            .map_err(StartError::Settings)?;
        Self::run(config, params, io)
    }

    fn run(config: ServerConfig, params: ServerParams, io: ServerIo) -> Result<Server, StartError> {
        let local = |io: &Arc<dyn PacketIo>| io.local_addr().map_err(StartError::Bind);
        let addrs = ServerAddrs {
            primary: local(&io.primary)?,
            alternate_port: local(&io.alternate_port)?,
            alternate_ip: match &io.alternate_ip {
                Some((a, b)) => Some((local(a)?, local(b)?)),
                None => None,
            },
        };
        let dual_stack = Sockets {
            primary: io.primary.dual_stack(),
            alternate_port: io.alternate_port.dual_stack(),
            alternate_ip: io
                .alternate_ip
                .as_ref()
                .map(|(a, b)| (a.dual_stack(), b.dual_stack())),
        };
        check_advertise(&params, &addrs, dual_stack).map_err(StartError::Settings)?;
        let tls_config = tls::server_config(
            &config.key,
            ALPN_RENDEZVOUS,
            TlsOptions {
                max_fragment_size: params.quic.tls_max_fragment_size,
            },
        )
        .map_err(StartError::Tls)?;
        let crypto = QuicServerConfig::try_from(tls_config).map_err(|e| {
            StartError::Tls(TlsError::Rustls(rustls::Error::General(e.to_string())))
        })?;
        let server_config = crate::quic::server_config(&params.quic, crypto);
        let endpoint_config = crate::quic::endpoint_config(&params.quic);

        let mut entries = config.settings.entries();
        // quinn runs MTU discovery only on a socket that does not fragment; packet I/O that
        // does not know says it may fragment.
        let mtu_discovery = !io.primary.may_fragment();
        entries.push(SettingEntry {
            key: MTU_DISCOVERY,
            value: Value::Switch(mtu_discovery),
            source: Source::Computed,
        });
        let demux_counters = Arc::<DemuxCounters>::default();
        let connection_tasks = Arc::new(Tasks::new());
        let inner = Arc::new(Inner {
            params,
            entries,
            key_id: config.key.id(),
            access: config.access.clone(),
            state: Mutex::new(State::new()),
            counters: Counters::default(),
            stun_window: Mutex::new(StunWindow {
                start: Instant::now(),
                counts: HashMap::new(),
            }),
            demux: demux_counters.clone(),
            queue_full: Arc::default(),
            pending: AtomicUsize::default(),
            addrs,
            connection_tasks: connection_tasks.clone(),
        });
        let socket = DemuxSocket::with_counters(io.primary.clone(), inner.clone(), demux_counters);
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            endpoint_config,
            Some(server_config),
            Arc::new(socket),
            Arc::new(QuinnRuntime(connection_tasks)),
        )
        .map_err(StartError::Bind)?;

        info!(
            public_key = %inner.key_id,
            primary = %addrs.primary,
            alternate_port = %addrs.alternate_port,
            alternate_ip = ?addrs.alternate_ip,
            "rendezvous server started"
        );
        for e in &inner.entries {
            info!(key = e.key, value = %e.value, source = %e.source, "setting");
        }
        let wildcard = std::iter::once(addrs.primary)
            .chain(std::iter::once(addrs.alternate_port))
            .chain(addrs.alternate_ip.into_iter().flat_map(|(a, b)| [a, b]))
            .any(|a| a.ip().is_unspecified());
        if wildcard {
            warn!(
                "a socket is bound to a wildcard address: on a host with several local IPs a STUN \
                 answer may leave from another IP than the one asked, and the device drops it; \
                 bind listen.* to a specific IP there"
            );
        }

        let mut tasks = Vec::new();
        let stun_only = std::iter::once(io.alternate_port.clone()).chain(
            io.alternate_ip
                .iter()
                .flat_map(|(a, b)| [a.clone(), b.clone()]),
        );
        for port in stun_only {
            tasks.push(tokio::spawn(stun_only_loop(inner.clone(), port)));
        }
        tasks.push(tokio::spawn(accept_loop(inner.clone(), endpoint.clone())));
        tasks.push(tokio::spawn(status_loop(inner.clone())));
        Ok(Server {
            inner,
            endpoint,
            tasks,
        })
    }

    /// Every setting with its value and source (`default`, `configured`, `fixed`), then the
    /// computed values (source `computed`): `punch_session_ttl` and [`MTU_DISCOVERY`], on when
    /// the primary socket does not fragment.
    pub fn settings(&self) -> Vec<SettingEntry> {
        self.inner.entries.clone()
    }

    /// Registrations, punches in progress and how often limits were hit.
    pub fn status(&self) -> ServerStatus {
        self.inner.status()
    }

    /// The bound addresses.
    pub fn addrs(&self) -> ServerAddrs {
        self.inner.addrs
    }

    /// The server's public key (what devices pin).
    pub fn public_key(&self) -> DeviceId {
        self.inner.key_id
    }

    /// Close every connection with `closed` and stop. The connections get `shutdown_timeout`
    /// to finish closing (docs/design/rendezvous.md "서버 종료"); then what is left of them and
    /// of their tasks ends, and the server's sockets are given up when this returns.
    pub async fn shutdown(mut self) {
        self.stop();
        self.tasks.clear();
        let limit = self.inner.params.shutdown_timeout;
        if tokio::time::timeout(limit, self.endpoint.wait_idle())
            .await
            .is_err()
        {
            info!("connections still closing at shutdown_timeout were dropped");
        }
        self.inner.connection_tasks.end().await;
        // The registrations hold their connections, and through them the primary socket,
        // which holds the server state: an ended monitor no longer drops its registration.
        *self.inner.state() = State::new();
    }

    /// Close every connection and abort the server's tasks and the punch timers.
    fn stop(&self) {
        self.endpoint
            .close(CloseCode::Closed.var_int(), CloseCode::Closed.reason());
        for t in &self.tasks {
            t.abort();
        }
        self.inner.state().abort_timers();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether each socket takes both families (an IPv6 socket with `IPV6_V6ONLY` off).
#[derive(Clone, Copy)]
struct Sockets {
    primary: bool,
    alternate_port: bool,
    alternate_ip: Option<(bool, bool)>,
}

/// The checks of docs/design/rendezvous.md "광고 주소와 바인딩 주소" against the addresses the
/// sockets are really bound to (they differ from `listen.*` with injected packet I/O) and the
/// families they take. Every socket an advertised IP is sent for must take its family: the
/// primary IP stands for the primary and the alternate port socket, the second IP for both
/// sockets of the second IP. An IPv6 wildcard socket the OS keeps IPv6 only takes no IPv4.
fn check_advertise(
    p: &ServerParams,
    a: &ServerAddrs,
    dual_stack: Sockets,
) -> Result<(), SettingsError> {
    let relation = |rule, values| Err(SettingsError::Relation { rule, values });
    let primary_ip = match p.advertise_primary_ip {
        None if a.primary.ip().is_unspecified() => {
            return relation(
                "a wildcard primary socket needs advertise.primary_ip for the alternate STUN \
                 address",
                vec![
                    ("listen.primary", Value::Address(a.primary)),
                    ("advertise.primary_ip", Value::Absent(Absent::None)),
                ],
            );
        }
        Some(ip) => ip,
        None => a.primary.ip(),
    };
    let fits = |key: &'static str, ip, sockets: &[(&'static str, SocketAddr, bool)]| {
        for &(socket_key, addr, dual) in sockets {
            if !advertise_fits(addr.ip(), dual, ip) {
                return relation(
                    "an advertised IP has an address family every socket it stands for takes \
                     (an IPv6 socket the OS keeps IPv6 only takes no IPv4)",
                    vec![(key, Value::Ip(ip)), (socket_key, Value::Address(addr))],
                );
            }
        }
        Ok(())
    };
    fits(
        "advertise.primary_ip",
        primary_ip,
        &[
            ("listen.primary", a.primary, dual_stack.primary),
            (
                "listen.alternate_port",
                a.alternate_port,
                dual_stack.alternate_port,
            ),
        ],
    )?;
    if let (Some((first, second)), Some((first_dual, second_dual))) =
        (a.alternate_ip, dual_stack.alternate_ip)
    {
        let ip = p.advertise_alternate_ip.unwrap_or(first.ip());
        fits(
            "advertise.alternate_ip",
            ip,
            &[
                ("listen.alternate_ip", first, first_dual),
                ("listen.alternate_ip", second, second_dual),
            ],
        )?;
    }
    Ok(())
}

async fn bind(addr: SocketAddr) -> Result<Arc<dyn PacketIo>, StartError> {
    let io = UdpPacketIo::bind(addr).await.map_err(StartError::Bind)?;
    Ok(Arc::new(io))
}

async fn stun_only_loop(inner: Arc<Inner>, io: Arc<dyn PacketIo>) {
    let mut buf = vec![0u8; usize::from(u16::MAX)];
    loop {
        let got = poll_fn(|cx| io.poll_recv_from(cx, &mut buf)).await;
        match got {
            Ok((len, from)) => {
                let packet = &buf[..len];
                if classify(packet) == PacketClass::Stun {
                    inner.on_stun(packet, from, io.as_ref());
                } else {
                    inner.counters.stun_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(e) if receive_error_is_fatal(&e) => {
                warn!(error = %e, "STUN port socket is broken, the port stops");
                return;
            }
            Err(e) => {
                // One datagram's error: count it and let other tasks run (and this task be
                // aborted) before the next receive, even when every receive fails.
                inner.demux.receive_errors.fetch_add(1, Ordering::Relaxed);
                debug!(error = %e, "STUN port receive failed");
                tokio::task::yield_now().await;
            }
        }
    }
}

async fn status_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(inner.params.status_log_interval);
    tick.tick().await;
    loop {
        tick.tick().await;
        let s = inner.status();
        info!(
            registrations = s.registrations,
            punches = s.punches,
            busy_max_devices = s.busy_max_devices,
            busy_max_punches = s.busy_max_punches,
            busy_per_device = s.busy_per_device,
            busy_request_rate = s.busy_request_rate,
            busy_pair = s.busy_pair,
            stun_answered = s.stun_answered,
            stun_rate_limited = s.stun_rate_limited,
            stun_sources_full = s.stun_sources_full,
            stun_dropped = s.stun_dropped,
            packets_dropped = s.packets_dropped,
            receive_errors = s.receive_errors,
            send_too_large = s.send_too_large,
            queue_full = s.queue_full,
            pending = s.pending,
            pending_refused = s.pending_refused,
            expired_cancels = s.expired_cancels,
            "status"
        );
    }
}

async fn accept_loop(inner: Arc<Inner>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        // Connections before registration are capped: a refused attempt costs no task.
        let p = &inner.params;
        let taken = inner
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < p.max_pending_connections).then_some(n + 1)
            });
        if taken.is_err() {
            inner
                .counters
                .pending_refused
                .fetch_add(1, Ordering::Relaxed);
            debug!("connection refused: max_pending_connections reached");
            incoming.refuse();
            continue;
        }
        let pending = Pending(inner.clone());
        inner
            .connection_tasks
            .spawn(connection(inner.clone(), incoming, pending));
    }
}

/// `addr` with an IPv4-mapped IPv6 address (a dual-stack socket's view of an IPv4 peer) as
/// the IPv4 address: what the server stores and sends.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// `fut` until `deadline`: `None` when the deadline passed first. A deadline of `None` (past
/// the clock's range) never passes.
async fn until<F: std::future::IntoFuture>(deadline: Option<Instant>, fut: F) -> Option<F::Output> {
    match deadline {
        Some(at) => tokio::time::timeout_at(at, fut).await.ok(),
        None => Some(fut.await),
    }
}

fn close(conn: &quinn::Connection, code: CloseCode) {
    conn.close(code.var_int(), code.reason());
}

enum HelloFailure {
    Timeout,
    Frame(String),
    Ended,
    Connection,
}

async fn connection(inner: Arc<Inner>, incoming: quinn::Incoming, pending: Pending) {
    let p = &inner.params;
    let deadline = Instant::now().checked_add(p.hello_timeout);
    let conn = match until(deadline, incoming).await {
        Some(Ok(conn)) => conn,
        Some(Err(e)) => {
            debug!(error = %e, "handshake failed");
            return;
        }
        None => {
            debug!("handshake did not finish within hello_timeout");
            return;
        }
    };
    let Some(device) = tls::peer_device_id(&conn) else {
        close(&conn, CloseCode::Malformed);
        return;
    };
    if !inner.access.allows(&device) {
        inner.counters.access_denied.fetch_add(1, Ordering::Relaxed);
        info!(device = %device, code = CloseCode::AccessDenied.name(), "connection closed");
        close(&conn, CloseCode::AccessDenied);
        return;
    }
    // hello_timeout bounds the time to the first byte of the hello; a hello that started is
    // bounded by frame_timeout, and one that does not end in time is malformed.
    let hello = async {
        let (send, mut recv) = until(deadline, conn.accept_bi())
            .await
            .ok_or(HelloFailure::Timeout)?
            .map_err(|_| HelloFailure::Connection)?;
        match read_frame_by(&mut recv, p.max_message_bytes, p.frame_timeout, deadline).await {
            Ok(Some(body)) => Ok((send, recv, body)),
            Ok(None) => Err(HelloFailure::Ended),
            Err(FrameError::Idle) => Err(HelloFailure::Timeout),
            Err(e) => Err(HelloFailure::Frame(e.to_string())),
        }
    }
    .await;
    let (send, recv, body) = match hello {
        Ok(got) => got,
        Err(HelloFailure::Timeout) => {
            info!(device = %device, code = CloseCode::HelloTimeout.name(), "connection closed");
            close(&conn, CloseCode::HelloTimeout);
            return;
        }
        Err(HelloFailure::Connection) => return,
        Err(HelloFailure::Ended) => {
            info!(device = %device, code = CloseCode::Malformed.name(), "connection closed");
            close(&conn, CloseCode::Malformed);
            return;
        }
        Err(HelloFailure::Frame(error)) => {
            info!(device = %device, error, code = CloseCode::Malformed.name(), "connection closed");
            close(&conn, CloseCode::Malformed);
            return;
        }
    };
    let hello = match DeviceMessage::parse(&body) {
        Ok(DeviceMessage::Hello(h)) => h,
        _ => {
            info!(device = %device, code = CloseCode::Malformed.name(), "connection closed");
            close(&conn, CloseCode::Malformed);
            return;
        }
    };
    if hello.proto != PROTO {
        info!(device = %device, proto = hello.proto, code = CloseCode::UnsupportedProto.name(), "connection closed");
        close(&conn, CloseCode::UnsupportedProto);
        return;
    }

    let reflexive = canonical(conn.remote_address());
    let advertised = inner.advertised(reflexive);
    let host = filter_host(&hello.host_candidates, stored_host(p));
    let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    inner
        .counters
        .candidates_rejected
        .fetch_add(count(host.rejected), Ordering::Relaxed);
    inner
        .counters
        .candidates_truncated
        .fetch_add(count(host.truncated), Ordering::Relaxed);
    let host = host.addrs;
    let (tx, rx) = mpsc::channel(p.max_queued_messages);
    let now = Instant::now();
    let welcome = ServerMessage::Welcome(Welcome {
        reflexive,
        stun: advertised.clone(),
        limits: inner.limits(),
    });
    let registered = {
        let mut state = inner.state();
        let conn_id = state.new_conn_id();
        let reg = Registration {
            conn_id,
            conn: conn.clone(),
            tx: tx.clone(),
            queue_full: inner.queue_full.clone(),
            reflexive,
            advertised: advertised.clone(),
            reported_reflexive: Vec::new(),
            host,
            nat: Default::default(),
            registered_at: now,
            last_frame_at: now,
        };
        let registered = state
            .register(p, &inner.counters, device, reg)
            .map(|old| (conn_id, old));
        // The welcome is queued under the same lock as the registration: no message of
        // another connection (a punch_offer) can come before it.
        if registered.is_ok() && tx.try_send(welcome).is_err() {
            debug!(device = %device, "welcome not queued");
        }
        registered
    };
    let conn_id = match registered {
        Ok((conn_id, old)) => {
            if let Some(old) = old {
                info!(device = %device, code = CloseCode::Superseded.name(), "older connection closed");
                close(&old, CloseCode::Superseded);
            }
            conn_id
        }
        Err(RegisterRefusal::Busy) => {
            info!(device = %device, code = CloseCode::Busy.name(), "connection closed");
            close(&conn, CloseCode::Busy);
            return;
        }
    };
    drop(pending);
    info!(device = %device, "registered");
    debug!(device = %device, reflexive = %reflexive, stun = ?advertised, "registered addresses");
    drop(tx);

    inner
        .connection_tasks
        .spawn(writer(conn.clone(), send, rx, p.max_message_bytes));
    inner
        .connection_tasks
        .spawn(reader(inner.clone(), conn.clone(), device, conn_id, recv));
    monitor(inner.clone(), conn, device, conn_id, reflexive).await;
}

/// The only send path of a connection: it takes [`ServerMessage`] values.
async fn writer(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut rx: mpsc::Receiver<ServerMessage>,
    limit: u32,
) {
    while let Some(msg) = rx.recv().await {
        let body = match msg.to_json() {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "message could not be encoded");
                continue;
            }
        };
        if let Err(e) = write_frame(&mut send, &body, limit).await {
            debug!(error = %e, kind = msg.type_name(), "frame not written");
            close(&conn, CloseCode::Malformed);
            return;
        }
    }
}

async fn reader(
    inner: Arc<Inner>,
    conn: quinn::Connection,
    device: DeviceId,
    conn_id: u64,
    mut recv: quinn::RecvStream,
) {
    let p = &inner.params;
    loop {
        let body = match read_frame(&mut recv, p.max_message_bytes, p.frame_timeout).await {
            Ok(Some(body)) => body,
            Ok(None) => {
                info!(device = %device, code = CloseCode::Closed.name(), "coordination stream ended");
                close(&conn, CloseCode::Closed);
                return;
            }
            Err(FrameError::Io(e)) => {
                // A reset or failed coordination stream ends the registration: the connection
                // is closed (a no-op when it already is) and the monitor removes the entry.
                info!(device = %device, code = CloseCode::Malformed.name(), "coordination stream failed");
                debug!(device = %device, error = %e, "coordination stream error");
                close(&conn, CloseCode::Malformed);
                return;
            }
            Err(e) => {
                info!(device = %device, error = %e, code = CloseCode::Malformed.name(), "connection closed");
                close(&conn, CloseCode::Malformed);
                return;
            }
        };
        let msg = match DeviceMessage::parse(&body) {
            Ok(DeviceMessage::Hello(_)) | Err(_) => {
                info!(device = %device, code = CloseCode::Malformed.name(), "connection closed");
                close(&conn, CloseCode::Malformed);
                return;
            }
            Ok(m) => m,
        };
        let now = Instant::now();
        {
            let mut state = inner.state();
            // A connection that a new registration of the same device replaced is being
            // closed: what it still sends does not touch the new registration.
            match state.devices.get_mut(&device) {
                Some(reg) if reg.conn_id == conn_id => reg.last_frame_at = now,
                _ => {
                    debug!(device = %device, "message of a replaced connection dropped");
                    continue;
                }
            }
            let after = match msg {
                DeviceMessage::Hello(_) => After::Nothing,
                DeviceMessage::NatReport(r) => {
                    state.nat_report(p, &inner.counters, device, r);
                    After::Nothing
                }
                DeviceMessage::PunchRequest(r) => {
                    state.punch_request(p, &inner.counters, device, r, now)
                }
                DeviceMessage::PunchAnswer(a) => {
                    state.punch_answer(p, &inner.counters, device, a, now)
                }
                DeviceMessage::PunchResult(r) => {
                    state.punch_result(device, r);
                    After::Nothing
                }
            };
            inner.after(&mut state, after);
        }
    }
}

async fn monitor(
    inner: Arc<Inner>,
    conn: quinn::Connection,
    device: DeviceId,
    conn_id: u64,
    mut reflexive: SocketAddr,
) {
    let mut poll = tokio::time::interval(inner.params.reflexive_poll_interval);
    loop {
        tokio::select! {
            stream = conn.accept_bi() => {
                if stream.is_ok() {
                    info!(device = %device, code = CloseCode::Malformed.name(), "second stream refused");
                    close(&conn, CloseCode::Malformed);
                }
                break;
            }
            stream = conn.accept_uni() => {
                if stream.is_ok() {
                    info!(device = %device, code = CloseCode::Malformed.name(), "unidirectional stream refused");
                    close(&conn, CloseCode::Malformed);
                }
                break;
            }
            _ = poll.tick() => {
                let now = canonical(conn.remote_address());
                if now != reflexive {
                    reflexive = now;
                    let mut state = inner.state();
                    if let Some(reg) = state.devices.get_mut(&device).filter(|r| r.conn_id == conn_id) {
                        reg.reflexive = now;
                        info!(device = %device, "reflexive address changed");
                        debug!(device = %device, reflexive = %now, "new reflexive address");
                        state::send(reg, ServerMessage::ReflexiveChanged(ReflexiveChanged { reflexive: now }));
                    }
                }
            }
            reason = conn.closed() => {
                debug!(device = %device, reason = %reason, "connection ended");
                break;
            }
        }
    }
    conn.closed().await;
    inner
        .state()
        .remove_device(device, Some(conn_id), CancelReason::PeerGone);
}
