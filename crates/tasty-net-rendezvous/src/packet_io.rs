//! Packet I/O under quinn and the first-byte split of one UDP socket
//! (docs/design/p2p.md "소켓 하나와 패킷 나누기").
//!
//! [`PacketIo`] is the real input and output: send, receive, local address. The default
//! implementation is an OS UDP socket ([`UdpPacketIo`]); tests put a mock NAT behind it.
//! [`DemuxSocket`] gives quinn a socket ([`quinn::AsyncUdpSocket`]) over a `PacketIo`: QUIC
//! packets go to quinn, STUN packets go to a [`StunSink`], everything else is dropped and
//! counted. The server's primary port and the device's P2P socket use the same code.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
}

/// [`PacketIo`] over a tokio UDP socket.
#[derive(Debug)]
pub struct UdpPacketIo(tokio::net::UdpSocket);

impl UdpPacketIo {
    /// Bind a UDP socket.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        tokio::net::UdpSocket::bind(addr).await.map(UdpPacketIo)
    }

    /// Wrap a bound socket.
    pub fn new(socket: tokio::net::UdpSocket) -> Self {
        UdpPacketIo(socket)
    }
}

impl PacketIo for UdpPacketIo {
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut read = tokio::io::ReadBuf::new(buf);
        match self.0.poll_recv_from(cx, &mut read) {
            Poll::Ready(Ok(from)) => Poll::Ready(Ok((read.filled().len(), from))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_send_ready(cx)
    }

    fn try_send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.0.try_send_to(buf, dest).map(|_| ())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
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
        self.io.try_send_to(transmit.contents, transmit.destination)
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
}
