//! The rendezvous server (docs/design/rendezvous.md).
//!
//! [`Server::start`] binds the primary port (QUIC coordination + STUN, split by the first
//! byte), the alternate port and, when set, the second IP's two ports (STUN only). It needs a
//! tokio runtime. The send function of a connection takes [`ServerMessage`] values only, and
//! the STUN path answers only the sender: there is no path that passes a device's bytes on.

mod state;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use quinn::crypto::rustls::QuicServerConfig;
use tasty_net_identity::{DeviceId, DeviceKey};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::access::{AccessPolicy, Everyone};
use crate::packet_io::{DemuxCounters, DemuxSocket, PacketIo, StunSink, UdpPacketIo};
use crate::settings::{ServerParams, ServerSettings, SettingEntry, SettingsError};
use crate::tls::{self, TlsError, TlsOptions};
use crate::wire::demux::{PacketClass, classify};
use crate::wire::frame::{FrameError, read_frame, write_frame};
use crate::wire::message::{
    CancelReason, DeviceMessage, Limits, ReflexiveChanged, ServerMessage, Welcome,
};
use crate::wire::stun::{ServerStunAction, server_answer};
use crate::wire::{ALPN_RENDEZVOUS, CloseCode, PROTO};

use state::{After, Counters, RegisterRefusal, Registration, State, filter_host};

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
    /// STUN packets dropped (not a plain Binding Request, or not STUN on a STUN-only port).
    pub stun_dropped: u64,
    /// Packets on the primary port of no known first-byte class.
    pub packets_dropped: u64,
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
    addrs: ServerAddrs,
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
        let (registrations, punches) = {
            let s = self.state();
            (s.devices.len(), s.punches.len())
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
            stun_dropped: get(&c.stun_dropped),
            packets_dropped: get(&self.demux.dropped),
            candidates_rejected: get(&c.candidates_rejected),
            candidates_truncated: get(&c.candidates_truncated),
            registrations_total: get(&c.registrations_total),
            superseded: get(&c.superseded),
            access_denied: get(&c.access_denied),
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
        let primary_ip = p
            .advertise_primary_ip
            .or(Some(p.listen_primary.ip()).filter(|ip| !ip.is_unspecified()));
        if let Some(ip) = primary_ip {
            out.push(SocketAddr::new(ip, alt_port));
        }
        let second_ip = p
            .advertise_alternate_ip
            .or(p.listen_alternate_ip.filter(|ip| !ip.is_unspecified()));
        if let Some(ip) = second_ip {
            out.push(SocketAddr::new(ip, a.primary.port()));
            out.push(SocketAddr::new(ip, alt_port));
        }
        let v4 = remote.ip().to_canonical().is_ipv4();
        out.retain(|s| s.ip().to_canonical().is_ipv4() == v4 && !s.ip().is_unspecified());
        out
    }

    fn after(self: &Arc<Self>, after: After) {
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
            self.state().set_timer(punch_id, handle.abort_handle());
        }
    }
}

impl StunSink for Inner {
    fn on_stun(&self, packet: &[u8], from: SocketAddr, io: &dyn PacketIo) {
        let reply = match server_answer(packet, from) {
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
                w.start = now;
                w.counts.clear();
            }
            let n = w.counts.entry(from).or_default();
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

    /// Check the settings and start serving on the given packet I/O (`listen.*` is not used
    /// for binding).
    pub async fn start_with_io(config: ServerConfig, io: ServerIo) -> Result<Server, StartError> {
        let params = config.settings.resolve().map_err(StartError::Settings)?;
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

        let entries = config.settings.entries();
        let demux_counters = Arc::<DemuxCounters>::default();
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
            addrs,
        });
        let socket = DemuxSocket::with_counters(io.primary.clone(), inner.clone(), demux_counters);
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            endpoint_config,
            Some(server_config),
            Arc::new(socket),
            Arc::new(quinn::TokioRuntime),
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

    /// Every setting with its value and source (`default`, `configured`, `fixed`).
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

    /// Close every connection with `closed` and stop.
    pub async fn shutdown(mut self) {
        self.endpoint
            .close(CloseCode::Closed.var_int(), CloseCode::Closed.reason());
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.endpoint.wait_idle().await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.endpoint
            .close(CloseCode::Closed.var_int(), CloseCode::Closed.reason());
        for t in &self.tasks {
            t.abort();
        }
    }
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
            Err(e) => debug!(error = %e, "STUN port receive failed"),
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
            stun_dropped = s.stun_dropped,
            packets_dropped = s.packets_dropped,
            "status"
        );
    }
}

async fn accept_loop(inner: Arc<Inner>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(connection(inner.clone(), incoming));
    }
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

async fn connection(inner: Arc<Inner>, incoming: quinn::Incoming) {
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
    let hello = until(deadline, async {
        let (send, mut recv) = conn
            .accept_bi()
            .await
            .map_err(|_| HelloFailure::Connection)?;
        match read_frame(&mut recv, p.max_message_bytes, p.frame_timeout).await {
            Ok(Some(body)) => Ok((send, recv, body)),
            Ok(None) => Err(HelloFailure::Ended),
            Err(e) => Err(HelloFailure::Frame(e.to_string())),
        }
    })
    .await
    .unwrap_or(Err(HelloFailure::Timeout));
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

    let reflexive = conn.remote_address();
    let advertised = inner.advertised(reflexive);
    let (host, dropped) = filter_host(&hello.host_candidates);
    inner.counters.candidates_rejected.fetch_add(
        u64::try_from(dropped).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let registered = {
        let mut state = inner.state();
        let conn_id = state.new_conn_id();
        let reg = Registration {
            conn_id,
            conn: conn.clone(),
            tx: tx.clone(),
            reflexive,
            advertised: advertised.clone(),
            reported_reflexive: Vec::new(),
            host,
            nat: Default::default(),
            registered_at: now,
            request_times: VecDeque::new(),
        };
        state
            .register(p, &inner.counters, device, reg)
            .map(|old| (conn_id, old))
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
    info!(device = %device, "registered");
    debug!(device = %device, reflexive = %reflexive, stun = ?advertised, "registered addresses");
    let welcome = ServerMessage::Welcome(Welcome {
        reflexive,
        stun: advertised,
        limits: inner.limits(),
    });
    if tx.send(welcome).is_err() {
        return;
    }
    drop(tx);

    tokio::spawn(writer(conn.clone(), send, rx, p.max_message_bytes));
    tokio::spawn(reader(inner.clone(), conn.clone(), device, recv));
    monitor(inner.clone(), conn, device, conn_id, reflexive).await;
}

/// The only send path of a connection: it takes [`ServerMessage`] values.
async fn writer(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut rx: mpsc::UnboundedReceiver<ServerMessage>,
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
                debug!(device = %device, error = %e, "coordination stream failed");
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
        let after = {
            let mut state = inner.state();
            match msg {
                DeviceMessage::Hello(_) => After::Nothing,
                DeviceMessage::NatReport(r) => {
                    state.nat_report(&inner.counters, device, r);
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
            }
        };
        inner.after(after);
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
                let now = conn.remote_address();
                if now != reflexive {
                    reflexive = now;
                    let mut state = inner.state();
                    if let Some(reg) = state.devices.get_mut(&device).filter(|r| r.conn_id == conn_id) {
                        reg.reflexive = now;
                        info!(device = %device, "reflexive address changed");
                        debug!(device = %device, reflexive = %now, "new reflexive address");
                        if reg.tx.send(ServerMessage::ReflexiveChanged(ReflexiveChanged { reflexive: now })).is_err() {
                            debug!(device = %device, "writer gone");
                        }
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
