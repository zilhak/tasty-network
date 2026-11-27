//! Packet I/O under quinn and the first-byte split of one UDP socket
//! (docs/design/p2p.md "소켓 하나와 패킷 나누기").
//!
//! [`PacketIo`] is the real input and output: send, receive, local address. The default
//! implementation is an OS UDP socket ([`UdpPacketIo`]); tests put a mock NAT behind it.
//! [`DemuxSocket`] gives quinn a socket ([`quinn::AsyncUdpSocket`]) over a `PacketIo`: QUIC
//! packets go to quinn, STUN packets go to a [`StunSink`], everything else is dropped and
//! counted. The server's primary port and the device's P2P socket use the same code.
//!
//! A send the OS refuses as larger than the path takes ([`send_error_is_too_large`]) is dropped
//! and counted like a lost datagram, as quinn's own UDP layer does: quinn's MTU discovery
//! sends such probes, and quinn ends the connection on any other send error but `WouldBlock`.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use tracing::debug;

use crate::wire::demux::{PacketClass, classify};

/// Datagram input and output.
pub trait PacketIo: Send + Sync + fmt::Debug + 'static {
    /// Receive one datagram into `buf`: its length and source.
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>>;

    /// Ready when a send may succeed.
    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    /// Send one datagram now. `WouldBlock` when the socket is full.
    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()>;

    /// The bound address.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Whether datagrams sent may be fragmented on the way. quinn runs MTU discovery only when
    /// this is false. The default is true: packet I/O that does not know (a mock, for
    /// example) does not take part in MTU discovery.
    fn may_fragment(&self) -> bool {
        true
    }

    /// Whether this IPv6 socket also takes IPv4 (`IPV6_V6ONLY` off), so that an IPv6 wildcard
    /// binding serves both families. The default is false: packet I/O that does not know is
    /// taken as IPv6 only when bound to IPv6.
    fn dual_stack(&self) -> bool {
        false
    }
}

/// [`PacketIo`] over a tokio UDP socket, received and set up through quinn's UDP layer
/// (`quinn::udp::UdpSocketState`): the socket asks the OS not to fragment where it can (then
/// [`PacketIo::may_fragment`] is false and quinn runs MTU discovery), and datagrams the OS
/// coalesces on receive (GRO) are handed out one by one.
pub struct UdpPacketIo {
    socket: tokio::net::UdpSocket,
    state: quinn::udp::UdpSocketState,
    dual_stack: bool,
    held: Mutex<Held>,
}

/// The datagrams of one receive not handed out yet.
struct Held {
    buf: Vec<u8>,
    from: SocketAddr,
    /// Bytes received.
    len: usize,
    /// The size of each datagram but the last.
    stride: usize,
    /// Bytes handed out.
    pos: usize,
}

/// The most the OS coalesces into one receive, 64 KiB.
const COALESCED_MAX: usize = 1 << 16;

impl fmt::Debug for UdpPacketIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpPacketIo")
            .field("socket", &self.socket)
            .field("may_fragment", &self.state.may_fragment())
            .field("dual_stack", &self.dual_stack)
            .finish_non_exhaustive()
    }
}

impl UdpPacketIo {
    /// Bind a UDP socket. An IPv6 socket is asked to take IPv4 too (`IPV6_V6ONLY` off) on
    /// every OS, since the default differs (Windows, and Linux with `net.ipv6.bindv6only=1`,
    /// start IPv6 only). When the OS refuses, the socket stays IPv6 only and
    /// [`PacketIo::dual_stack`] is false.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        if addr.is_ipv6()
            && let Err(e) = socket.set_only_v6(false)
        {
            debug!(error = %e, %addr, "the socket stays IPv6 only");
        }
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        Self::new(tokio::net::UdpSocket::from_std(socket.into())?)
    }

    /// Wrap a bound socket. This sets the socket options of quinn's UDP layer on it; whether an
    /// IPv6 socket takes IPv4 is read from the socket.
    pub fn new(socket: tokio::net::UdpSocket) -> io::Result<Self> {
        let state = quinn::udp::UdpSocketState::new((&socket).into())?;
        let dual_stack = socket.local_addr()?.is_ipv6()
            && socket2::SockRef::from(&socket)
                .only_v6()
                .is_ok_and(|only| !only);
        Ok(UdpPacketIo {
            socket,
            state,
            dual_stack,
            held: Mutex::new(Held {
                buf: vec![0; COALESCED_MAX],
                // Nothing held yet.
                from: SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0)),
                len: 0,
                stride: 0,
                pos: 0,
            }),
        })
    }
}

impl Held {
    /// The next held datagram into `buf`, if any.
    fn next(&mut self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        if self.pos >= self.len {
            return None;
        }
        let size = self.stride.min(self.len - self.pos);
        let n = size.min(buf.len());
        buf[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += size;
        Some((n, self.from))
    }
}

impl PacketIo for UdpPacketIo {
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(got) = held.next(buf) {
            return Poll::Ready(Ok(got));
        }
        loop {
            match self.socket.poll_recv_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
            let held = &mut *held;
            let got = self.socket.try_io(tokio::io::Interest::READABLE, || {
                let mut meta = [RecvMeta::default()];
                let mut bufs = [IoSliceMut::new(&mut held.buf)];
                self.state
                    .recv((&self.socket).into(), &mut bufs, &mut meta)
                    .map(|_| meta[0])
            });
            match got {
                Ok(meta) => {
                    held.from = meta.addr;
                    held.len = meta.len;
                    // Without coalescing the stride is the length.
                    held.stride = match meta.stride {
                        0 => meta.len,
                        stride => stride,
                    };
                    held.pos = 0;
                    let got = held.next(buf).unwrap_or((0, meta.addr));
                    return Poll::Ready(Ok(got));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket.poll_send_ready(cx)
    }

    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.socket.try_send_to(buf, dest).map(|_| ())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }

    fn dual_stack(&self) -> bool {
        self.dual_stack
    }
}

/// Where STUN packets of a [`DemuxSocket`] go. It is called on the receive path and must not
/// block; it may answer through `io`.
pub trait StunSink: Send + Sync + fmt::Debug + 'static {
    /// One STUN packet from `from`.
    fn on_stun(&self, packet: &[u8], from: SocketAddr, io: &dyn PacketIo);
}

/// Whether a receive error means the socket is broken, so that no later receive can succeed.
/// Every other receive error belongs to one datagram (an ICMP error reported as
/// `ConnectionReset`·`ConnectionRefused`, a datagram larger than the buffer on Windows
/// `WSAEMSGSIZE`, and the like): the datagram is dropped and counted, and receiving goes on.
pub fn receive_error_is_fatal(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Unsupported
    )
}

/// Whether a send error means the datagram is larger than the path takes (`EMSGSIZE`, Windows
/// `WSAEMSGSIZE`). The OS gives it at once for an MTU discovery probe larger than the
/// interface MTU.
pub fn send_error_is_too_large(e: &io::Error) -> bool {
    #[cfg(unix)]
    let too_large = Some(libc::EMSGSIZE);
    #[cfg(windows)]
    let too_large = Some(windows_sys::Win32::Networking::WinSock::WSAEMSGSIZE);
    #[cfg(not(any(unix, windows)))]
    let too_large = None;
    too_large.is_some() && e.raw_os_error() == too_large
}

/// Counts of a [`DemuxSocket`].
#[derive(Debug, Default)]
pub struct DemuxCounters {
    /// STUN packets handed to the sink.
    pub stun: AtomicU64,
    /// QUIC packets handed to quinn.
    pub quic: AtomicU64,
    /// Packets of no known class, dropped.
    pub dropped: AtomicU64,
    /// Receive errors of one datagram, dropped ([`receive_error_is_fatal`] is false).
    pub receive_errors: AtomicU64,
    /// Sends the OS refused as larger than the path ([`send_error_is_too_large`]), dropped.
    pub send_too_large: AtomicU64,
}

/// A quinn socket over a [`PacketIo`] that splits received packets by their first byte.
pub struct DemuxSocket {
    io: Arc<dyn PacketIo>,
    stun: Arc<dyn StunSink>,
    counters: Arc<DemuxCounters>,
}

impl fmt::Debug for DemuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DemuxSocket")
            .field("io", &self.io)
            .finish_non_exhaustive()
    }
}

impl DemuxSocket {
    /// A socket over `io` that gives STUN packets to `stun`.
    pub fn new(io: Arc<dyn PacketIo>, stun: Arc<dyn StunSink>) -> Self {
        DemuxSocket {
            io,
            stun,
            counters: Arc::default(),
        }
    }

    /// A socket that counts into `counters`.
    pub fn with_counters(
        io: Arc<dyn PacketIo>,
        stun: Arc<dyn StunSink>,
        counters: Arc<DemuxCounters>,
    ) -> Self {
        DemuxSocket { io, stun, counters }
    }

    /// The counts.
    pub fn counters(&self) -> Arc<DemuxCounters> {
        self.counters.clone()
    }

    /// The packet I/O under this socket.
    pub fn io(&self) -> Arc<dyn PacketIo> {
        self.io.clone()
    }
}

#[derive(Debug)]
struct Poller(Arc<dyn PacketIo>);

impl quinn::UdpPoller for Poller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_send_ready(cx)
    }
}

impl quinn::AsyncUdpSocket for DemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(Poller(self.io.clone()))
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        match self.io.try_send_to(transmit.contents, transmit.destination) {
            Err(e) if send_error_is_too_large(&e) => {
                // An MTU probe too large for the path: quinn takes it as lost.
                self.counters.send_too_large.fetch_add(1, Ordering::Relaxed);
                debug!(error = %e, len = transmit.contents.len(), "datagram too large dropped");
                Ok(())
            }
            other => other,
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let (Some(buf), Some(slot)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Ok(0));
        };
        loop {
            let (len, from) = match self.io.poll_recv_from(cx, buf) {
                Poll::Ready(Ok(got)) => got,
                Poll::Ready(Err(e)) if receive_error_is_fatal(&e) => return Poll::Ready(Err(e)),
                Poll::Ready(Err(e)) => {
                    // quinn ends its driver on any error but `ConnectionReset`: drop this one
                    // and give the executor back before the next receive.
                    self.counters.receive_errors.fetch_add(1, Ordering::Relaxed);
                    debug!(error = %e, "datagram receive error dropped");
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Pending => return Poll::Pending,
            };
            let packet = &buf[..len];
            match classify(packet) {
                PacketClass::Quic => {
                    self.counters.quic.fetch_add(1, Ordering::Relaxed);
                    *slot = RecvMeta {
                        addr: from,
                        len,
                        stride: len,
                        ecn: None,
                        dst_ip: None,
                    };
                    return Poll::Ready(Ok(1));
                }
                PacketClass::Stun => {
                    self.counters.stun.fetch_add(1, Ordering::Relaxed);
                    self.stun.on_stun(packet, from, self.io.as_ref());
                }
                PacketClass::Other => {
                    self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.io.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coalesced_receive_splits_into_stride_sized_datagrams() {
        let from: SocketAddr = "192.0.2.1:4000".parse().unwrap();
        // Two full segments of 4 bytes and a short last one of 2.
        let mut held = Held {
            buf: (0..10).collect(),
            from,
            len: 10,
            stride: 4,
            pos: 0,
        };
        let mut out = [0u8; 8];
        let mut got = Vec::new();
        while let Some((n, addr)) = held.next(&mut out) {
            assert_eq!(addr, from);
            got.push(out[..n].to_vec());
        }
        assert_eq!(got, [vec![0, 1, 2, 3], vec![4, 5, 6, 7], vec![8, 9]]);
        assert!(held.next(&mut out).is_none());
    }
}
