//! Connections and streams with blocking I/O (docs/design/p2p.md "스트림과 연결 (어댑터와의
//! 경계)", docs/design/transport.md "동기·비동기 경계").
//!
//! This crate does not know the transport contract: it gives byte streams and the life of a
//! connection. Reads and writes block the calling thread on the endpoint's runtime, with an
//! optional time limit; a [`P2pCancel`] wakes a blocked call. QUIC errors become
//! [`std::io::ErrorKind`]s: the peer closing is `ConnectionReset`, this side closing is
//! `ConnectionAborted`, the idle timeout and a read or write time limit are `TimedOut`, a
//! stopped send is `BrokenPipe`.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use quinn::{ConnectionError, ReadError, VarInt, WriteError};
use tasty_net_identity::DeviceId;
use tokio::sync::Notify;

use crate::close::P2pCloseReason;
use crate::endpoint::Core;
use crate::error::P2pError;
use crate::info::P2pConnectionInfo;
use crate::runtime::block_on;

/// A signal that cancels a connection attempt or wakes the blocked calls of a stream. Clones
/// share the signal.
#[derive(Clone, Debug, Default)]
pub struct P2pCancel(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
    set: AtomicBool,
    notify: Notify,
}

impl P2pCancel {
    /// A new signal, not set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the signal. Blocked calls return.
    pub fn cancel(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Whether the signal is set.
    pub fn is_cancelled(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Ends when the signal is set.
    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

fn in_async() -> io::Error {
    io::Error::other(P2pError::InAsyncContext)
}

fn aborted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the stream was aborted")
}

/// The I/O class of a lost connection.
pub(crate) fn connection_error_kind(e: &ConnectionError) -> io::ErrorKind {
    match e {
        ConnectionError::TimedOut => io::ErrorKind::TimedOut,
        ConnectionError::LocallyClosed => io::ErrorKind::ConnectionAborted,
        ConnectionError::ApplicationClosed(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::Reset => io::ErrorKind::ConnectionReset,
        ConnectionError::VersionMismatch
        | ConnectionError::TransportError(_)
        | ConnectionError::CidsExhausted => io::ErrorKind::Other,
    }
}

fn read_error(e: ReadError) -> io::Error {
    let kind = match &e {
        ReadError::Reset(_) => io::ErrorKind::ConnectionReset,
        ReadError::ConnectionLost(c) => connection_error_kind(c),
        ReadError::ClosedStream => io::ErrorKind::NotConnected,
        ReadError::IllegalOrderedRead => io::ErrorKind::InvalidInput,
        ReadError::ZeroRttRejected => io::ErrorKind::ConnectionRefused,
    };
    io::Error::new(kind, e)
}

fn write_error(e: WriteError) -> io::Error {
    let kind = match &e {
        WriteError::Stopped(_) => io::ErrorKind::BrokenPipe,
        WriteError::ConnectionLost(c) => connection_error_kind(c),
        WriteError::ClosedStream => io::ErrorKind::NotConnected,
        WriteError::ZeroRttRejected => io::ErrorKind::ConnectionRefused,
    };
    io::Error::new(kind, e)
}

fn connection_io(e: ConnectionError) -> io::Error {
    io::Error::new(connection_error_kind(&e), e)
}

/// Run `fut` with an optional time limit, ended early by `cancel`.
async fn bounded<T>(
    fut: impl std::future::Future<Output = io::Result<T>>,
    limit: Option<Duration>,
    cancel: &P2pCancel,
) -> io::Result<T> {
    let limited = async {
        match limit {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the time limit passed"))?,
            None => fut.await,
        }
    };
    tokio::select! {
        r = limited => r,
        () = cancel.cancelled() => Err(aborted()),
    }
}

/// A device-to-device QUIC connection. Clones share the connection; it stays usable while any
/// clone lives, and keeps the endpoint's runtime alive.
#[derive(Clone)]
pub struct P2pConnection {
    inner: Arc<ConnInner>,
}

struct ConnInner {
    core: Arc<Core>,
    conn: quinn::Connection,
    id: u64,
    peer: DeviceId,
    local_close: Mutex<Option<P2pCloseReason>>,
}

impl fmt::Debug for P2pConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pConnection")
            .field("id", &self.inner.id)
            .field("peer", &self.inner.peer)
            .field("remote", &self.inner.conn.remote_address())
            .finish()
    }
}

impl P2pConnection {
    pub(crate) fn new(core: Arc<Core>, conn: quinn::Connection, id: u64, peer: DeviceId) -> Self {
        P2pConnection {
            inner: Arc::new(ConnInner {
                core,
                conn,
                id,
                peer,
                local_close: Mutex::new(None),
            }),
        }
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
        block_on(&self.inner.core.handle, fut).map_err(|_| in_async())?
    }

    /// The connection number ([`P2pConnectionInfo::id`]).
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The peer's device ID, checked by TLS.
    pub fn remote_device_id(&self) -> DeviceId {
        self.inner.peer
    }

    /// The peer address quinn uses now.
    pub fn remote_address(&self) -> SocketAddr {
        crate::socket::canonical(self.inner.conn.remote_address())
    }

    /// quinn's RTT estimate.
    pub fn rtt(&self) -> Duration {
        self.inner.conn.rtt()
    }

    /// The connection's information, while the endpoint tracks it.
    pub fn info(&self) -> Option<P2pConnectionInfo> {
        self.inner
            .core
            .tracker
            .snapshot()
            .into_iter()
            .find(|i| i.id == self.inner.id)
    }

    /// Open a bidirectional stream. Only the side that opened the connection opens streams
    /// (docs/design/transport.md "P2P의 닫기 범위").
    pub fn open_bi(&self) -> io::Result<P2pStream> {
        let (send, recv) = self.block(async {
            self.inner
                .conn
                .open_bi()
                .await
                .map_err(|e| io::Error::new(connection_error_kind(&e), e))
        })?;
        Ok(self.stream(send, recv))
    }

    /// Wait for the peer's next bidirectional stream, with an optional time limit.
    pub fn accept_bi(&self, limit: Option<Duration>) -> io::Result<P2pStream> {
        let cancel = P2pCancel::new();
        let (send, recv) = self.block(bounded(
            async {
                self.inner
                    .conn
                    .accept_bi()
                    .await
                    .map_err(|e| io::Error::new(connection_error_kind(&e), e))
            },
            limit,
            &cancel,
        ))?;
        Ok(self.stream(send, recv))
    }

    fn stream(&self, send: quinn::SendStream, recv: quinn::RecvStream) -> P2pStream {
        let cancel = P2pCancel::new();
        P2pStream {
            send: P2pSendStream {
                core: self.inner.core.clone(),
                stream: send,
                limit: None,
                cancel: cancel.clone(),
            },
            recv: P2pRecvStream {
                core: self.inner.core.clone(),
                stream: recv,
                limit: None,
                cancel,
            },
        }
    }

    /// Close the whole connection with `reason` (QUIC CONNECTION_CLOSE with the code of
    /// docs/design/handshake.md "QUIC 종료 코드").
    pub fn close(&self, reason: P2pCloseReason) {
        *self
            .inner
            .local_close
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reason);
        self.inner.conn.close(reason.var_int(), reason.reason());
    }

    /// Why the connection closed: the reason this side sent, or the code the peer sent.
    /// `None` while it is open, and when it ended without a CONNECTION_CLOSE (idle timeout).
    pub fn close_reason(&self) -> Option<P2pCloseReason> {
        match self.inner.conn.close_reason()? {
            ConnectionError::ApplicationClosed(c) => {
                Some(P2pCloseReason::from_code(c.error_code.into_inner()))
            }
            ConnectionError::LocallyClosed => *self
                .inner
                .local_close
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            _ => None,
        }
    }

    /// Wait until the connection is closed, up to `limit`; the I/O class of the end
    /// (`TimedOut` for the idle timeout). `None` when it is still open at `limit`.
    pub fn wait_closed(&self, limit: Duration) -> io::Result<Option<io::Error>> {
        self.block(async {
            match tokio::time::timeout(limit, self.inner.conn.closed()).await {
                Ok(e) => Ok(Some(connection_io(e))),
                Err(_) => Ok(None),
            }
        })
    }
}

/// A bidirectional stream with blocking reads and writes. [`P2pStream::split`] gives the two
/// halves to two threads.
#[derive(Debug)]
pub struct P2pStream {
    send: P2pSendStream,
    recv: P2pRecvStream,
}

impl P2pStream {
    /// The two halves.
    pub fn split(self) -> (P2pSendStream, P2pRecvStream) {
        (self.send, self.recv)
    }

    /// The signal that wakes this stream's blocked calls (both halves).
    pub fn canceller(&self) -> P2pCancel {
        self.recv.cancel.clone()
    }

    /// A time limit for each read; `None` waits without limit.
    pub fn set_read_timeout(&mut self, limit: Option<Duration>) {
        self.recv.limit = limit;
    }

    /// A time limit for each write; `None` waits without limit.
    pub fn set_write_timeout(&mut self, limit: Option<Duration>) {
        self.send.limit = limit;
    }

    /// Finish the sending side ([`P2pSendStream::finish`]).
    pub fn finish(&mut self) -> io::Result<()> {
        self.send.finish()
    }
}

impl io::Read for P2pStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv.read(buf)
    }
}

impl io::Write for P2pStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send.flush()
    }
}

/// The sending half.
pub struct P2pSendStream {
    core: Arc<Core>,
    stream: quinn::SendStream,
    limit: Option<Duration>,
    cancel: P2pCancel,
}

impl fmt::Debug for P2pSendStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pSendStream")
            .field("id", &self.stream.id())
            .finish()
    }
}

impl P2pSendStream {
    /// A time limit for each write; `None` waits without limit.
    pub fn set_write_timeout(&mut self, limit: Option<Duration>) {
        self.limit = limit;
    }

    /// The signal that wakes this half's blocked calls.
    pub fn canceller(&self) -> P2pCancel {
        self.cancel.clone()
    }

    /// No more data: the peer reads the end after the data sent.
    pub fn finish(&mut self) -> io::Result<()> {
        self.stream
            .finish()
            .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e))
    }

    /// Abandon the sending side with an application code.
    pub fn reset(&mut self, code: u32) -> io::Result<()> {
        self.stream
            .reset(VarInt::from_u32(code))
            .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e))
    }
}

impl io::Write for P2pSendStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(aborted());
        }
        let stream = &mut self.stream;
        let fut = bounded(
            async { stream.write(buf).await.map_err(write_error) },
            self.limit,
            &self.cancel,
        );
        block_on(&self.core.handle, fut).map_err(|_| in_async())?
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The receiving half.
pub struct P2pRecvStream {
    core: Arc<Core>,
    stream: quinn::RecvStream,
    limit: Option<Duration>,
    cancel: P2pCancel,
}

impl fmt::Debug for P2pRecvStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pRecvStream")
            .field("id", &self.stream.id())
            .finish()
    }
}

impl P2pRecvStream {
    /// A time limit for each read; `None` waits without limit.
    pub fn set_read_timeout(&mut self, limit: Option<Duration>) {
        self.limit = limit;
    }

    /// The signal that wakes this half's blocked calls.
    pub fn canceller(&self) -> P2pCancel {
        self.cancel.clone()
    }

    /// Tell the peer to stop sending, with an application code.
    pub fn stop(&mut self, code: u32) -> io::Result<()> {
        self.stream
            .stop(VarInt::from_u32(code))
            .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e))
    }
}

impl io::Read for P2pRecvStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(aborted());
        }
        let stream = &mut self.stream;
        let fut = bounded(
            async {
                stream
                    .read(buf)
                    .await
                    .map(Option::unwrap_or_default)
                    .map_err(read_error)
            },
            self.limit,
            &self.cancel,
        );
        block_on(&self.core.handle, fut).map_err(|_| in_async())?
    }
}
