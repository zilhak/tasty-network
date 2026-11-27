//! The mock network of the P2P tests (docs/design/testing.md "모의 NAT"): NATs with a mapping
//! rule (EIM, APDM), a filter rule (endpoint independent, address and port dependent), an
//! outbound rule per packet (pass, drop, delay), a mapping expiry and a latency, and a public
//! side where the rendezvous server sits.
#![allow(dead_code)]

pub mod fake;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::settings::Value;
use tasty_net_p2p::{
    AttemptPhase, EndpointPhase, P2pConfig, P2pEndpoint, P2pEvent, PairingList, Phase,
    RendezvousServer, Subject,
};
use tasty_net_rendezvous::packet_io::{DemuxSocket, PacketIo, StunSink};
use tasty_net_rendezvous::server::{Server, ServerConfig, ServerIo};
use tasty_net_rendezvous::settings::{ServerSettings, Value as ServerValue};

/// What an outbound rule does with one packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Drop,
    Delay(Duration),
}

/// An outbound rule: destination and packet.
pub type Rule = Arc<dyn Fn(SocketAddr, &[u8]) -> Verdict + Send + Sync>;

/// RFC 4787 §4.1 mapping behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// Endpoint-independent mapping.
    Independent,
    /// Address and port-dependent mapping.
    Dependent,
}

/// RFC 4787 §5 filtering behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Endpoint-independent filtering.
    Independent,
    /// Address and port-dependent filtering.
    AddressPort,
}

/// A NAT's rules.
#[derive(Clone, Copy, Debug)]
pub struct NatSpec {
    pub mapping: Mapping,
    pub filter: Filter,
    /// A mapping with no outbound packet for this long is gone.
    pub expiry: Duration,
    /// Added to every packet that goes in or out through this NAT.
    pub latency: Duration,
    /// Packets between two devices of this LAN are source-translated to the device's public
    /// mapping (as if the two were on different inside networks of one router).
    pub lan_snat: bool,
    /// The LAN is 100.64.<n>.0/24 (shared address space): no address passes the host
    /// candidate rule.
    pub cgnat: bool,
}

impl NatSpec {
    pub fn new(mapping: Mapping, filter: Filter) -> Self {
        NatSpec {
            mapping,
            filter,
            expiry: Duration::from_secs(120),
            latency: Duration::ZERO,
            lan_snat: false,
            cgnat: false,
        }
    }

    /// Port-restricted cone: EIM with address and port-dependent filtering.
    pub fn eim() -> Self {
        NatSpec::new(Mapping::Independent, Filter::AddressPort)
    }

    /// Full cone: EIM with endpoint-independent filtering.
    pub fn full_cone() -> Self {
        NatSpec::new(Mapping::Independent, Filter::Independent)
    }

    /// Symmetric: APDM with address and port-dependent filtering.
    pub fn symmetric() -> Self {
        NatSpec::new(Mapping::Dependent, Filter::AddressPort)
    }
}

fn is_inside(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 10 || v4.octets()[..2] == [100, 64])
}

/// An address with an IPv4-mapped IPv6 address as IPv4.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// QUIC packets have the fixed bit (0x40) set; STUN packets start with 0b00.
pub fn is_quic(packet: &[u8]) -> bool {
    packet.first().is_some_and(|b| b & 0x40 != 0)
}

#[derive(Debug)]
struct MapEntry {
    internal: SocketAddr,
    /// The destination for APDM; none for EIM.
    remote: Option<SocketAddr>,
    public_port: u16,
    sent_to: HashSet<SocketAddr>,
    last: Instant,
}

struct Nat {
    public_ip: Ipv4Addr,
    lan: u8,
    spec: NatSpec,
    rule: Mutex<Option<Rule>>,
    state: Mutex<NatState>,
}

#[derive(Debug, Default)]
struct NatState {
    next_port: u16,
    maps: Vec<MapEntry>,
}

impl std::fmt::Debug for Nat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nat")
            .field("public_ip", &self.public_ip)
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl Nat {
    fn in_lan(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.octets()[..3] == self.prefix(),
            IpAddr::V6(_) => false,
        }
    }

    fn prefix(&self) -> [u8; 3] {
        if self.spec.cgnat {
            [100, 64, self.lan]
        } else {
            [10, self.lan, 0]
        }
    }

    /// The public address of `internal` towards `dest`, made or refreshed.
    fn map_out(&self, internal: SocketAddr, dest: SocketAddr) -> SocketAddr {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let expiry = self.spec.expiry;
        st.maps.retain(|m| now.duration_since(m.last) < expiry);
        let remote = match self.spec.mapping {
            Mapping::Independent => None,
            Mapping::Dependent => Some(dest),
        };
        let found = st
            .maps
            .iter()
            .position(|m| m.internal == internal && m.remote == remote);
        let i = match found {
            Some(i) => i,
            None => {
                st.next_port += 1;
                let public_port = 40_000 + st.next_port;
                st.maps.push(MapEntry {
                    internal,
                    remote,
                    public_port,
                    sent_to: HashSet::new(),
                    last: now,
                });
                st.maps.len() - 1
            }
        };
        let m = &mut st.maps[i];
        m.last = now;
        m.sent_to.insert(dest);
        SocketAddr::new(IpAddr::V4(self.public_ip), m.public_port)
    }

    /// The inside address a packet from `src` to public port `port` goes to, if any.
    fn map_in(&self, port: u16, src: SocketAddr) -> Option<SocketAddr> {
        let now = Instant::now();
        let st = self.state.lock().unwrap();
        let m = st
            .maps
            .iter()
            .find(|m| m.public_port == port && now.duration_since(m.last) < self.spec.expiry)?;
        match self.spec.filter {
            Filter::Independent => Some(m.internal),
            Filter::AddressPort => m.sent_to.contains(&src).then_some(m.internal),
        }
    }

    fn verdict(&self, dest: SocketAddr, packet: &[u8]) -> Verdict {
        match self.rule.lock().unwrap().as_ref() {
            Some(rule) => rule(dest, packet),
            None => Verdict::Pass,
        }
    }
}

type Inbox = (VecDeque<(Vec<u8>, SocketAddr)>, Option<Waker>);

/// Delayed packets in time order, their contents, the last ID.
type Queue = (
    BinaryHeap<Reverse<(Instant, u64)>>,
    HashMap<u64, Delayed>,
    u64,
);

/// One socket of the mock network.
pub struct Port {
    addr: SocketAddr,
    nat: Option<Arc<Nat>>,
    net: Arc<NetInner>,
    inbox: Mutex<Inbox>,
    sent: Mutex<Vec<SocketAddr>>,
    received: AtomicU64,
}

impl std::fmt::Debug for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Port")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Port {
    fn put(&self, packet: Vec<u8>, from: SocketAddr) {
        self.received.fetch_add(1, Ordering::Relaxed);
        let mut inbox = self.inbox.lock().unwrap();
        inbox.0.push_back((packet, from));
        if let Some(w) = inbox.1.take() {
            w.wake();
        }
    }

    /// The address this socket is bound to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every destination this socket sent to, in order.
    pub fn sent(&self) -> Vec<SocketAddr> {
        self.sent.lock().unwrap().clone()
    }

    /// Datagrams handed to this socket.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

/// [`PacketIo`] over a [`Port`].
#[derive(Debug, Clone)]
pub struct MockIo(pub Arc<Port>);

impl PacketIo for MockIo {
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut inbox = self.0.inbox.lock().unwrap();
        match inbox.0.pop_front() {
            Some((packet, from)) => {
                let n = packet.len().min(buf.len());
                buf[..n].copy_from_slice(&packet[..n]);
                Poll::Ready(Ok((n, from)))
            }
            None => {
                inbox.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn poll_send_ready(&self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.0.sent.lock().unwrap().push(dest);
        self.0.net.route(&self.0, buf, dest);
        Ok(())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr)
    }
}

struct Delayed {
    port: Arc<Port>,
    packet: Vec<u8>,
    from: SocketAddr,
}

/// One thread for every delayed packet of the process.
#[derive(Default)]
struct DelayLine {
    queue: Mutex<Queue>,
    wake: Condvar,
}

fn delay_line() -> &'static DelayLine {
    static LINE: OnceLock<&'static DelayLine> = OnceLock::new();
    LINE.get_or_init(|| {
        let line: &'static DelayLine = Box::leak(Box::default());
        std::thread::spawn(move || {
            let mut q = line.queue.lock().unwrap();
            loop {
                let now = Instant::now();
                match q.0.peek().copied() {
                    Some(Reverse((at, id))) if at <= now => {
                        q.0.pop();
                        if let Some(d) = q.1.remove(&id) {
                            d.port.put(d.packet, d.from);
                        }
                    }
                    Some(Reverse((at, _))) => {
                        q = line.wake.wait_timeout(q, at - now).unwrap().0;
                    }
                    None => q = line.wake.wait(q).unwrap(),
                }
            }
        });
        line
    })
}

fn deliver(port: Arc<Port>, packet: Vec<u8>, from: SocketAddr, delay: Duration) {
    if delay.is_zero() {
        port.put(packet, from);
        return;
    }
    let line = delay_line();
    let mut q = line.queue.lock().unwrap();
    q.2 += 1;
    let id = q.2;
    q.0.push(Reverse((Instant::now() + delay, id)));
    q.1.insert(id, Delayed { port, packet, from });
    line.wake.notify_all();
}

#[derive(Clone)]
struct Intercept {
    port: Arc<Port>,
    quic_only: bool,
    except: Vec<IpAddr>,
}

#[derive(Default)]
struct NetInner {
    ports: Mutex<HashMap<SocketAddr, Arc<Port>>>,
    nats: Mutex<Vec<Arc<Nat>>>,
    /// Sockets that take what is sent to an address before anything else.
    overrides: Mutex<HashMap<SocketAddr, Intercept>>,
}

impl NetInner {
    fn route(&self, from: &Port, packet: &[u8], dest: SocketAddr) {
        let dest = SocketAddr::new(dest.ip().to_canonical(), dest.port());
        let mut delay = Duration::ZERO;
        let src = match &from.nat {
            Some(nat) => {
                match nat.verdict(dest, packet) {
                    Verdict::Pass => {}
                    Verdict::Drop => return,
                    Verdict::Delay(d) => delay += d,
                }
                delay += nat.spec.latency;
                if nat.in_lan(dest.ip()) {
                    let src = if nat.spec.lan_snat {
                        nat.map_out(from.addr, dest)
                    } else {
                        from.addr
                    };
                    let port = self.ports.lock().unwrap().get(&dest).cloned();
                    if let Some(port) = port {
                        deliver(port, packet.to_vec(), src, delay);
                    }
                    return;
                }
                if is_inside(dest.ip()) {
                    // Another LAN's address: not routed.
                    return;
                }
                if dest.ip() == IpAddr::V4(nat.public_ip) {
                    // No hairpinning.
                    return;
                }
                nat.map_out(from.addr, dest)
            }
            None => from.addr,
        };
        self.deliver_public(packet, src, dest, delay);
    }

    fn deliver_public(&self, packet: &[u8], src: SocketAddr, dest: SocketAddr, delay: Duration) {
        if let Some(i) = self.overrides.lock().unwrap().get(&dest).cloned()
            && (!i.quic_only || is_quic(packet))
            && !i.except.contains(&src.ip())
        {
            deliver(i.port, packet.to_vec(), src, delay);
            return;
        }
        let public = self
            .ports
            .lock()
            .unwrap()
            .get(&dest)
            .filter(|p| p.nat.is_none())
            .cloned();
        if let Some(port) = public {
            deliver(port, packet.to_vec(), src, delay);
            return;
        }
        let nat = self
            .nats
            .lock()
            .unwrap()
            .iter()
            .find(|n| IpAddr::V4(n.public_ip) == dest.ip())
            .cloned();
        let Some(nat) = nat else { return };
        let Some(internal) = nat.map_in(dest.port(), src) else {
            return;
        };
        let port = self.ports.lock().unwrap().get(&internal).cloned();
        if let Some(port) = port {
            deliver(port, packet.to_vec(), src, delay + nat.spec.latency);
        }
    }
}

/// The mock network.
#[derive(Clone, Default)]
pub struct Net(Arc<NetInner>);

impl Net {
    pub fn new() -> Self {
        Net::default()
    }

    fn port(&self, addr: SocketAddr, nat: Option<Arc<Nat>>) -> Arc<Port> {
        let port = Arc::new(Port {
            addr,
            nat,
            net: self.0.clone(),
            inbox: Mutex::default(),
            sent: Mutex::default(),
            received: AtomicU64::new(0),
        });
        self.0.ports.lock().unwrap().insert(addr, port.clone());
        port
    }

    /// A socket on the public side.
    pub fn public(&self, addr: SocketAddr) -> Arc<Port> {
        self.port(addr, None)
    }

    /// A socket that takes what is sent to `addr` (`quic_only`: only QUIC packets) from any
    /// source but `except`, ahead of the socket or NAT that owns the address. It sends from
    /// `addr`.
    pub fn intercept(&self, addr: SocketAddr, quic_only: bool, except: Vec<IpAddr>) -> Arc<Port> {
        let port = Arc::new(Port {
            addr,
            nat: None,
            net: self.0.clone(),
            inbox: Mutex::default(),
            sent: Mutex::default(),
            received: AtomicU64::new(0),
        });
        self.0.overrides.lock().unwrap().insert(
            addr,
            Intercept {
                port: port.clone(),
                quic_only,
                except,
            },
        );
        port
    }

    /// A NAT with public IP 203.0.113.<n> in front of LAN 10.<n>.0.0/24.
    pub fn nat(&self, n: u8, spec: NatSpec) -> NatHandle {
        let nat = Arc::new(Nat {
            public_ip: Ipv4Addr::new(203, 0, 113, n),
            lan: n,
            spec,
            rule: Mutex::new(None),
            state: Mutex::default(),
        });
        self.0.nats.lock().unwrap().push(nat.clone());
        NatHandle {
            net: self.clone(),
            nat,
        }
    }
}

/// A NAT of the mock network.
#[derive(Clone)]
pub struct NatHandle {
    net: Net,
    nat: Arc<Nat>,
}

impl NatHandle {
    /// A device socket 10.<n>.0.<host>:5000 behind this NAT.
    pub fn socket(&self, host: u8) -> Arc<Port> {
        let [a, b, c] = self.nat.prefix();
        let addr = SocketAddr::from(([a, b, c, host], 5000));
        self.net.port(addr, Some(self.nat.clone()))
    }

    /// Set the outbound rule.
    pub fn set_rule(&self, rule: Rule) {
        *self.nat.rule.lock().unwrap() = Some(rule);
    }

    /// Drop the outbound rule.
    pub fn clear_rule(&self) {
        *self.nat.rule.lock().unwrap() = None;
    }

    /// Forget every mapping: the next outbound packet of each socket gets a new public port.
    pub fn remap(&self) {
        self.nat.state.lock().unwrap().maps.clear();
    }

    /// The public IP.
    pub fn public_ip(&self) -> IpAddr {
        IpAddr::V4(self.nat.public_ip)
    }
}

/// Drop every packet whose destination is not one of `allowed`.
pub fn only_to(allowed: Vec<IpAddr>) -> Rule {
    Arc::new(move |dest, _| {
        if allowed.contains(&dest.ip()) {
            Verdict::Pass
        } else {
            Verdict::Drop
        }
    })
}

/// Drop everything.
pub fn block_all() -> Rule {
    Arc::new(|_, _| Verdict::Drop)
}

/// A device key from a seed.
pub fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

/// A running rendezvous server on the public side.
pub struct TestServer {
    pub info: RendezvousServer,
    pub primary: Arc<Port>,
    server: Option<Server>,
}

/// Server settings whose listen addresses do not matter (the server runs on mock sockets).
pub fn server_settings() -> ServerSettings {
    let mut s = ServerSettings::new();
    s.set(
        "listen.primary",
        ServerValue::Address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
    )
    .unwrap();
    s.set("listen.alternate_port", ServerValue::Port(0))
        .unwrap();
    s
}

/// The test world: the mock network and a runtime for the servers. Devices use their own
/// runtimes (each endpoint owns one).
pub struct World {
    pub net: Net,
    pub rt: tokio::runtime::Runtime,
}

impl Default for World {
    fn default() -> Self {
        World::new()
    }
}

impl World {
    pub fn new() -> Self {
        World {
            net: Net::new(),
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        }
    }

    /// Server number `n` (1, 2, ...) at 198.51.100.<2n-1> and 198.51.100.<2n>, ports 3478 and
    /// 3479.
    pub fn server(&self, n: u8, settings: ServerSettings) -> TestServer {
        let ip1 = Ipv4Addr::new(198, 51, 100, 2 * n - 1);
        let ip2 = Ipv4Addr::new(198, 51, 100, 2 * n);
        let io = |ip: Ipv4Addr, port: u16| -> (Arc<Port>, Arc<dyn PacketIo>) {
            let p = self.net.public(SocketAddr::from((ip, port)));
            (p.clone(), Arc::new(MockIo(p)))
        };
        let (primary, primary_io) = io(ip1, 3478);
        let mut config = ServerConfig::new(key(200 + n));
        config.settings = settings;
        let public_key = config.key.id();
        let server = self
            .rt
            .block_on(Server::start_with_io(
                config,
                ServerIo {
                    primary: primary_io,
                    alternate_port: io(ip1, 3479).1,
                    alternate_ip: Some((io(ip2, 3478).1, io(ip2, 3479).1)),
                },
            ))
            .expect("server starts");
        TestServer {
            info: RendezvousServer {
                addr: SocketAddr::from((ip1, 3478)),
                public_key,
            },
            primary,
            server: Some(server),
        }
    }

    /// Stop a server: its connections close.
    pub fn stop(&self, server: &mut TestServer) {
        if let Some(s) = server.server.take() {
            self.rt.block_on(s.shutdown());
        }
    }

    /// The running server.
    pub fn running<'a>(&self, server: &'a TestServer) -> &'a Server {
        server.server.as_ref().expect("server is running")
    }
}

/// A device: its endpoint, its socket and its pairing list.
pub struct Device {
    pub ep: P2pEndpoint,
    pub port: Arc<Port>,
    pub pairing: Arc<PairingList>,
    pub key_seed: u8,
}

impl Device {
    pub fn id(&self) -> DeviceId {
        self.ep.id()
    }
}

/// Settings tuned for tests: nothing beyond the defaults unless a test sets it.
pub fn settings(pairs: &[(&str, Value)]) -> tasty_net_p2p::settings::P2pSettings {
    let mut s = tasty_net_p2p::settings::P2pSettings::new();
    for (k, v) in pairs {
        s.set(k, *v);
    }
    s
}

/// A device behind `nat` at host `host`, on `server`, with key seed `seed`.
pub fn device(
    nat: &NatHandle,
    host: u8,
    seed: u8,
    server: &TestServer,
    s: tasty_net_p2p::settings::P2pSettings,
) -> Device {
    device_with(nat.socket(host), seed, server.info, s, true)
}

pub fn device_with(
    port: Arc<Port>,
    seed: u8,
    server: RendezvousServer,
    s: tasty_net_p2p::settings::P2pSettings,
    enabled: bool,
) -> Device {
    let pairing = Arc::new(PairingList::new());
    let mut config = P2pConfig::new(key(seed), pairing.clone());
    config.enabled = enabled;
    config.server = Some(server);
    config.settings = s;
    config.packet_io = Some(Arc::new(MockIo(port.clone())));
    Device {
        ep: P2pEndpoint::bind(config).expect("bind"),
        port,
        pairing,
        key_seed: seed,
    }
}

/// Pair two devices both ways.
pub fn pair(a: &Device, b: &Device) {
    a.pairing.add(b.id());
    b.pairing.add(a.id());
}

/// Wait until `f` holds, at most `limit`.
pub fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until the endpoint is registered and has judged its NAT.
pub fn registered(d: &Device) {
    assert!(
        wait_until(Duration::from_secs(20), || {
            let s = d.ep.status();
            s.state == EndpointPhase::Registered && !s.nat_answers.is_empty()
        }),
        "registered: {:?}",
        d.ep.status()
    );
}

/// The events of an endpoint, collected as they are read.
#[derive(Default)]
pub struct Log(pub Vec<P2pEvent>);

impl Log {
    /// Read events until `f` matches one (returned) or `limit` passes.
    pub fn wait(
        &mut self,
        ep: &P2pEndpoint,
        limit: Duration,
        mut f: impl FnMut(&P2pEvent) -> bool,
    ) -> Option<P2pEvent> {
        if let Some(e) = self.0.iter().find(|e| f(e)) {
            return Some(e.clone());
        }
        let end = Instant::now() + limit;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Some(e) = ep.next_event(left) else { break };
            self.0.push(e.clone());
            if f(&e) {
                return Some(e);
            }
        }
        None
    }

    /// Everything read so far plus what is queued now.
    pub fn drain(&mut self, ep: &P2pEndpoint) -> &[P2pEvent] {
        self.0.extend(ep.events());
        &self.0
    }

    /// The attempt transitions read so far, as (connection id, to, cause name).
    pub fn attempt_path(&self) -> Vec<(u64, AttemptPhase, &'static str)> {
        self.0
            .iter()
            .filter_map(|e| match e {
                P2pEvent::Transition {
                    subject: Subject::Connection(id),
                    to: Phase::Attempt(to),
                    cause,
                    ..
                } => Some((*id, *to, cause.name)),
                _ => None,
            })
            .collect()
    }
}

/// An event that moves an attempt to `to`.
pub fn attempt_to(to: AttemptPhase) -> impl FnMut(&P2pEvent) -> bool {
    move |e| matches!(e, P2pEvent::Transition { to: Phase::Attempt(t), .. } if *t == to)
}

/// A STUN sink that drops everything, for test-owned quinn endpoints.
#[derive(Debug)]
pub struct NoStun;

impl StunSink for NoStun {
    fn on_stun(&self, _: &[u8], _: SocketAddr, _: &dyn PacketIo) {}
}

/// A quinn endpoint of the test on `port` (inside `rt`).
pub fn raw_endpoint(
    rt: &tokio::runtime::Runtime,
    port: Arc<Port>,
    server: Option<quinn::ServerConfig>,
) -> quinn::Endpoint {
    let _enter = rt.enter();
    let socket = Arc::new(DemuxSocket::new(Arc::new(MockIo(port)), Arc::new(NoStun)));
    let mut config = quinn::EndpointConfig::default();
    config.grease_quic_bit(false);
    quinn::Endpoint::new_with_abstract_socket(config, server, socket, Arc::new(quinn::TokioRuntime))
        .expect("raw endpoint")
}
