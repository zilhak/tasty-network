//! The one P2P UDP socket and the STUN side of its first-byte split
//! (docs/design/p2p.md "소켓 하나와 패킷 나누기").
//!
//! The socket carries the rendezvous QUIC connection, the STUN queries of the NAT judgement,
//! the punch probes and the device-to-device QUIC connections. quinn gets a
//! [`DemuxSocket`] over it; STUN packets come to the [`StunRouter`] here: Binding Success
//! Responses go to whoever sent the request with that transaction ID, punch probes (Binding
//! Requests with `TASTY-PUNCH`) are answered for the punch session whose peer token they carry.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tasty_net_identity::DeviceId;
use tasty_net_rendezvous::packet_io::{PacketIo, StunSink, UdpPacketIo};
use tasty_net_rendezvous::wire::candidate::CandidateKind;
use tasty_net_rendezvous::wire::message::{PunchMode, PunchToken};
use tasty_net_rendezvous::wire::stun::{self, StunClass, StunMessage, TransactionId};
use tokio::sync::{Notify, mpsc};
use tracing::debug;

use crate::settings::{OsValues, P2pParams};

/// The kind of a candidate a device probes, with the kind it adds itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteKind {
    /// The address the server observed.
    ServerReflexive,
    /// A reflexive address the peer reported.
    ReportedReflexive,
    /// A host address the peer reported.
    Host,
    /// An address a probe with the right token came from that was not a candidate
    /// (RFC 8445 §7.3.1.3).
    PeerReflexive,
}

impl RemoteKind {
    /// The name in connection information.
    pub fn name(self) -> &'static str {
        match self {
            RemoteKind::ServerReflexive => "server_reflexive",
            RemoteKind::ReportedReflexive => "reported_reflexive",
            RemoteKind::Host => "host",
            RemoteKind::PeerReflexive => "peer_reflexive",
        }
    }
}

impl From<CandidateKind> for RemoteKind {
    fn from(k: CandidateKind) -> Self {
        match k {
            CandidateKind::ServerReflexive => RemoteKind::ServerReflexive,
            CandidateKind::ReportedReflexive => RemoteKind::ReportedReflexive,
            CandidateKind::Host => RemoteKind::Host,
        }
    }
}

/// A probe target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RemoteCandidate {
    /// The address.
    pub addr: SocketAddr,
    /// Its kind.
    pub kind: RemoteKind,
}

/// The address as compared: IPv4-mapped IPv6 becomes IPv4.
pub(crate) fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// The address to send to from a socket bound to `local`: an IPv4 address becomes IPv4-mapped
/// IPv6 on an IPv6 (dual-stack) socket.
pub(crate) fn wire_addr(local: SocketAddr, addr: SocketAddr) -> SocketAddr {
    match (local, addr) {
        (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        _ => addr,
    }
}

/// Open the P2P socket: `[::]:0` dual stack, or `0.0.0.0:0` when the OS does not take IPv6
/// with IPv4. The buffer sizes are set when configured and read back either way.
pub(crate) fn open_udp(params: &P2pParams) -> io::Result<(Arc<dyn PacketIo>, OsValues)> {
    let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), u16::MIN);
    let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), u16::MIN);
    let socket = match bind(v6, true, params) {
        Ok(s) => s,
        Err(e) => {
            debug!(error = %e, "no dual-stack IPv6 socket, IPv4 only");
            bind(v4, false, params)?
        }
    };
    let sock = socket2::SockRef::from(&socket);
    let os = OsValues {
        recv_buffer: sock
            .recv_buffer_size()
            .ok()
            .and_then(|n| u64::try_from(n).ok()),
        send_buffer: sock
            .send_buffer_size()
            .ok()
            .and_then(|n| u64::try_from(n).ok()),
    };
    let io = UdpPacketIo::new(tokio::net::UdpSocket::from_std(socket)?)?;
    Ok((Arc::new(io), os))
}

fn bind(addr: SocketAddr, dual: bool, params: &P2pParams) -> io::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if dual {
        socket.set_only_v6(false)?;
    }
    // While the endpoint is on, the port is its alone: on Windows another socket could
    // otherwise bind it too and take its datagrams (docs/design/p2p.md "소켓 하나와 패킷 나누기").
    tasty_net_rendezvous::packet_io::set_exclusive_address_use(&socket)?;
    if let Some(n) = params.recv_buffer {
        socket.set_recv_buffer_size(n)?;
    }
    if let Some(n) = params.send_buffer {
        socket.set_send_buffer_size(n)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// The endpoint's packet I/O, given up when the endpoint turns off: `Off` has no socket
/// (docs/design/p2p.md "상태 기계"). quinn, its connections and the STUN side hold this gate
/// rather than the socket, so that the socket closes at `Off` even while endpoint and
/// connection handles live on. Once released, receives and sends fail with `NotConnected`.
#[derive(Debug)]
pub(crate) struct Gate {
    io: Mutex<Option<Arc<dyn PacketIo>>>,
    may_fragment: bool,
    dual_stack: bool,
}

fn released() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the P2P socket is closed")
}

impl Gate {
    pub(crate) fn new(io: Arc<dyn PacketIo>) -> Self {
        Gate {
            may_fragment: io.may_fragment(),
            dual_stack: io.dual_stack(),
            io: Mutex::new(Some(io)),
        }
    }

    fn io(&self) -> io::Result<Arc<dyn PacketIo>> {
        self.io
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(released)
    }

    /// Give up the packet I/O; whether it was still held.
    pub(crate) fn release(&self) -> bool {
        self.io
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .is_some()
    }
}

impl PacketIo for Gate {
    fn poll_recv_from(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<(usize, SocketAddr)>> {
        match self.io() {
            Ok(io) => io.poll_recv_from(cx, buf),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_recv_with_dst(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<(usize, SocketAddr, Option<IpAddr>)>> {
        match self.io() {
            Ok(io) => io.poll_recv_with_dst(cx, buf),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_send_ready(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        match self.io() {
            Ok(io) => io.poll_send_ready(cx),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.io()?.try_send_to(buf, dest)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io()?.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.may_fragment
    }

    fn dual_stack(&self) -> bool {
        self.dual_stack
    }
}

/// A STUN response delivered to the sender of the request.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub(crate) msg: StunMessage,
    pub(crate) from: SocketAddr,
}

/// Counts of the STUN side.
#[derive(Debug, Default)]
pub struct StunCounters {
    /// Probes answered.
    pub probes_answered: AtomicU64,
    /// Probes not answered: no session with that token, or (`host_only`) not from a host
    /// candidate.
    pub probes_dropped: AtomicU64,
    /// Probe sources not added as peer reflexive because the candidates were full.
    pub peer_reflexive_full: AtomicU64,
    /// Responses with a transaction ID nobody waits for.
    pub responses_unmatched: AtomicU64,
    /// STUN packets that do not parse, plain Binding Requests and other classes.
    pub dropped: AtomicU64,
    /// QUIC Initials refused before TLS: their address is in no active punch session.
    pub quic_refused: AtomicU64,
    /// QUIC connections closed with `session_mismatch`.
    pub session_mismatch: AtomicU64,
}

/// A punch session's probe side, shared by the router (answers) and the attempt (sends).
#[derive(Debug)]
pub(crate) struct ProbeSession {
    pub(crate) peer: DeviceId,
    pub(crate) token_local: PunchToken,
    pub(crate) token_peer: PunchToken,
    pub(crate) mode: PunchMode,
    pub(crate) handshake: std::time::Duration,
    max_candidates: usize,
    state: Mutex<SessionState>,
    /// A candidate was added.
    pub(crate) added: Notify,
}

#[derive(Debug, Default)]
struct SessionState {
    candidates: Vec<RemoteCandidate>,
    /// Addresses a probe with the peer's token came from (`host_only`: host candidates only).
    probed_from: HashSet<SocketAddr>,
    /// B: new QUIC Initials are taken until this time.
    accept_until: Option<Instant>,
}

impl ProbeSession {
    pub(crate) fn new(
        peer: DeviceId,
        token_local: PunchToken,
        token_peer: PunchToken,
        mode: PunchMode,
        handshake: std::time::Duration,
        candidates: Vec<RemoteCandidate>,
        max_candidates: usize,
    ) -> Self {
        // `host_only` probes host candidates only, also when the server sends others
        // (docs/design/p2p.md "펀칭 절차" 4, 6).
        let candidates = candidates
            .into_iter()
            .filter(|c| mode != PunchMode::HostOnly || c.kind == RemoteKind::Host)
            .collect();
        ProbeSession {
            peer,
            token_local,
            token_peer,
            mode,
            handshake,
            max_candidates,
            state: Mutex::new(SessionState {
                candidates,
                ..SessionState::default()
            }),
            added: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The probe targets now.
    pub(crate) fn candidates(&self) -> Vec<RemoteCandidate> {
        self.lock().candidates.clone()
    }

    /// The candidate with this address, if any.
    pub(crate) fn candidate(&self, addr: SocketAddr) -> Option<RemoteCandidate> {
        let addr = canonical(addr);
        self.lock()
            .candidates
            .iter()
            .find(|c| canonical(c.addr) == addr)
            .copied()
    }

    /// B: take new QUIC Initials until `until`.
    pub(crate) fn accept_until(&self, until: Instant) {
        self.lock().accept_until = Some(until);
    }

    /// Whether a QUIC Initial from `addr` belongs to this session now.
    pub(crate) fn takes_initial(&self, addr: SocketAddr, now: Instant) -> bool {
        let s = self.lock();
        s.accept_until.is_some_and(|until| now < until) && s.probed_from.contains(&canonical(addr))
    }

    /// The router saw a probe with the peer's token from `from`. Returns whether it is
    /// answered.
    fn on_probe(&self, from: SocketAddr, counters: &StunCounters) -> bool {
        let from = canonical(from);
        let mut s = self.lock();
        let known = s.candidates.iter().any(|c| canonical(c.addr) == from);
        if self.mode == PunchMode::HostOnly {
            let host = s
                .candidates
                .iter()
                .any(|c| c.kind == RemoteKind::Host && canonical(c.addr) == from);
            if !host {
                return false;
            }
        } else if !known {
            if s.candidates.len() < self.max_candidates {
                s.candidates.push(RemoteCandidate {
                    addr: from,
                    kind: RemoteKind::PeerReflexive,
                });
                self.added.notify_one();
            } else {
                counters.peer_reflexive_full.fetch_add(1, Ordering::Relaxed);
            }
        }
        s.probed_from.insert(from);
        true
    }
}

/// The transaction IDs one task waits for, with where each request went. They are forgotten
/// when the task ends, also when its future is dropped (a cancel), so no waiter is left.
pub(crate) struct Waiting {
    router: Arc<StunRouter>,
    to: mpsc::UnboundedSender<Reply>,
    sent: HashMap<TransactionId, SocketAddr>,
}

impl Waiting {
    pub(crate) fn new(router: Arc<StunRouter>, to: mpsc::UnboundedSender<Reply>) -> Self {
        Waiting {
            router,
            to,
            sent: HashMap::new(),
        }
    }

    /// Deliver responses with `txid`, a request sent to `to`.
    pub(crate) fn add(&mut self, txid: TransactionId, to: SocketAddr) {
        self.router.wait_for(txid, self.to.clone());
        self.sent.insert(txid, to);
    }

    /// Where the request with `txid` went.
    pub(crate) fn sent(&self) -> &HashMap<TransactionId, SocketAddr> {
        &self.sent
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.router.forget(self.sent.keys());
    }
}

/// Where the socket's STUN packets go.
#[derive(Debug, Default)]
pub(crate) struct StunRouter {
    state: Mutex<RouterState>,
    pub(crate) counters: Arc<StunCounters>,
}

#[derive(Debug, Default)]
struct RouterState {
    waiters: HashMap<TransactionId, mpsc::UnboundedSender<Reply>>,
    sessions: Vec<Arc<ProbeSession>>,
}

impl StunRouter {
    fn lock(&self) -> MutexGuard<'_, RouterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Deliver responses with `txid` to `to`.
    fn wait_for(&self, txid: TransactionId, to: mpsc::UnboundedSender<Reply>) {
        self.lock().waiters.insert(txid, to);
    }

    /// How many transaction IDs are waited for.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.lock().waiters.len()
    }

    /// Stop delivering responses with these transaction IDs.
    fn forget<'a>(&self, txids: impl IntoIterator<Item = &'a TransactionId>) {
        let mut s = self.lock();
        for t in txids {
            s.waiters.remove(t);
        }
    }

    /// Answer probes for `session`.
    pub(crate) fn add_session(&self, session: Arc<ProbeSession>) {
        self.lock().sessions.push(session);
    }

    /// Stop answering probes for `session`.
    pub(crate) fn remove_session(&self, session: &Arc<ProbeSession>) {
        self.lock().sessions.retain(|s| !Arc::ptr_eq(s, session));
    }

    fn on_request(&self, msg: &StunMessage, from: SocketAddr, io: &dyn PacketIo) {
        let Some(token) = msg.punch_token else {
            // Devices answer probes only; a plain Binding Request is not for them.
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let session = self
            .lock()
            .sessions
            .iter()
            .find(|s| s.token_peer == token)
            .cloned();
        let Some(session) = session.filter(|_| !msg.unknown_required) else {
            self.counters.probes_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if !session.on_probe(from, &self.counters) {
            self.counters.probes_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let answer = stun::binding_success(
            &msg.transaction_id,
            canonical(from),
            Some(&session.token_local),
        );
        match io.try_send_to(&answer, from) {
            Ok(()) => {
                self.counters
                    .probes_answered
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => debug!(error = %e, %from, "probe answer not sent"),
        }
    }
}

impl StunSink for StunRouter {
    fn on_stun(&self, packet: &[u8], from: SocketAddr, io: &dyn PacketIo) {
        let msg = match stun::parse(packet) {
            Ok(m) => m,
            Err(_) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        match msg.class {
            StunClass::BindingRequest => self.on_request(&msg, from, io),
            StunClass::BindingSuccess => {
                let to = self.lock().waiters.get(&msg.transaction_id).cloned();
                match to {
                    Some(to) if to.send(Reply { msg, from }).is_ok() => {}
                    _ => {
                        self.counters
                            .responses_unmatched
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// Send one STUN message, mapping the address for the socket's family.
pub(crate) fn send(io: &dyn PacketIo, packet: &[u8], to: SocketAddr) -> io::Result<()> {
    let local = io.local_addr()?;
    io.try_send_to(packet, wire_addr(local, to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::task::{Context, Poll, Waker};

    use quinn::AsyncUdpSocket;
    use tasty_net_rendezvous::packet_io::DemuxSocket;

    /// Packet I/O that hands out queued packets and records sends.
    #[derive(Debug, Default)]
    struct Queued {
        inbox: Mutex<VecDeque<Vec<u8>>>,
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    fn peer() -> SocketAddr {
        "192.0.2.7:4000".parse().unwrap()
    }

    impl PacketIo for Queued {
        fn poll_recv_from(
            &self,
            _: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<(usize, SocketAddr)>> {
            match self.inbox.lock().unwrap().pop_front() {
                Some(p) => {
                    buf[..p.len()].copy_from_slice(&p);
                    Poll::Ready(Ok((p.len(), peer())))
                }
                None => Poll::Pending,
            }
        }
        fn poll_send_ready(&self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
            self.sent.lock().unwrap().push((buf.to_vec(), dest));
            Ok(())
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("10.0.0.2:5000".parse().unwrap())
        }
    }

    #[test]
    fn demux_classifies_first_byte() {
        let io = Arc::new(Queued::default());
        let router = Arc::new(StunRouter::default());
        let socket = DemuxSocket::new(io.clone(), router.clone());
        let counters = socket.counters();
        // A STUN response for a waiter, so a STUN packet is seen arriving at the router.
        let txid = [9; 12];
        let (tx, mut rx) = mpsc::unbounded_channel();
        router.wait_for(txid, tx);
        let stun_packet = stun::binding_success(&txid, peer(), None);
        let cases: [(u8, &str); 11] = [
            (0, "stun"),
            (1, "stun"),
            (3, "stun"),
            (4, "dropped"),
            (63, "dropped"),
            (64, "quic"),
            (127, "quic"),
            (128, "dropped"),
            (191, "dropped"),
            (192, "quic"),
            (255, "quic"),
        ];
        let mut want = HashMap::new();
        for (first, class) in cases {
            let mut p = if class == "stun" {
                stun_packet.clone()
            } else {
                vec![0; 32]
            };
            // The response's own first byte is 1. With 0 it reads as a plain Binding Request
            // and with 3 as a class the codec does not take: both are counted as STUN by the
            // split and dropped by the router.
            p[0] = first;
            io.inbox.lock().unwrap().push_back(p);
            *want.entry(class).or_insert(0u64) += 1;
        }
        let mut cx = Context::from_waker(Waker::noop());
        let mut quic = 0;
        loop {
            let mut buf = [0u8; 64];
            let mut bufs = [io::IoSliceMut::new(&mut buf)];
            let mut meta = [quinn::udp::RecvMeta::default()];
            match socket.poll_recv(&mut cx, &mut bufs, &mut meta) {
                Poll::Ready(Ok(n)) => quic += n,
                _ => break,
            }
        }
        assert_eq!(quic as u64, want["quic"]);
        assert_eq!(counters.quic.load(Ordering::Relaxed), want["quic"]);
        assert_eq!(counters.stun.load(Ordering::Relaxed), want["stun"]);
        assert_eq!(counters.dropped.load(Ordering::Relaxed), want["dropped"]);
        assert!(rx.try_recv().is_ok(), "the response reached its waiter");
        assert_eq!(router.counters.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn probes_are_answered_only_for_a_session_token() {
        let io = Queued::default();
        let router = StunRouter::default();
        let peer_id = tasty_net_identity::DeviceKey::from_seed([3; 32]).id();
        let host: SocketAddr = "10.0.0.9:6000".parse().unwrap();
        let session = Arc::new(ProbeSession::new(
            peer_id,
            PunchToken([1; 16]),
            PunchToken([2; 16]),
            PunchMode::HostOnly,
            std::time::Duration::from_secs(1),
            vec![RemoteCandidate {
                addr: host,
                kind: RemoteKind::Host,
            }],
            8,
        ));
        router.add_session(session.clone());
        let probe = stun::binding_request(&[5; 12], Some(&PunchToken([2; 16])));
        let wrong = stun::binding_request(&[6; 12], Some(&PunchToken([7; 16])));
        router.on_stun(&wrong, host, &io);
        // host_only: a probe from an address that is not a host candidate gets no answer.
        router.on_stun(&probe, peer(), &io);
        router.on_stun(&probe, host, &io);
        let sent = io.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let answer = stun::parse(&sent[0].0).unwrap();
        assert_eq!(answer.class, StunClass::BindingSuccess);
        assert_eq!(answer.mapped, Some(host));
        assert_eq!(answer.punch_token, Some(PunchToken([1; 16])));
        assert_eq!(router.counters.probes_dropped.load(Ordering::Relaxed), 2);
        assert_eq!(
            session.candidates().len(),
            1,
            "no peer reflexive in host_only"
        );
    }

    #[test]
    fn host_only_probes_host_candidates_only() {
        let peer_id = tasty_net_identity::DeviceKey::from_seed([3; 32]).id();
        let host: SocketAddr = "10.0.0.9:6000".parse().unwrap();
        let reflexive: SocketAddr = "203.0.113.9:6000".parse().unwrap();
        let given = vec![
            RemoteCandidate {
                addr: reflexive,
                kind: RemoteKind::ServerReflexive,
            },
            RemoteCandidate {
                addr: host,
                kind: RemoteKind::Host,
            },
            RemoteCandidate {
                addr: reflexive,
                kind: RemoteKind::ReportedReflexive,
            },
        ];
        let session = |mode| {
            ProbeSession::new(
                peer_id,
                PunchToken([1; 16]),
                PunchToken([2; 16]),
                mode,
                std::time::Duration::from_secs(1),
                given.clone(),
                8,
            )
        };
        let host_only = session(PunchMode::HostOnly);
        assert_eq!(
            host_only.candidates(),
            vec![RemoteCandidate {
                addr: host,
                kind: RemoteKind::Host,
            }]
        );
        assert!(host_only.candidate(reflexive).is_none());
        assert_eq!(session(PunchMode::All).candidates(), given);
    }
}
