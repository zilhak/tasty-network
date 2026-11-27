//! One QUIC stream as a [`RawChannel`] (docs/design/transport.md "채널 대응", "P2P의 닫기
//! 범위").
//!
//! The read half, the write half and the control share one [`Stream`]. The halves lock the
//! stream half they use for each call; the control first wakes a blocked call through the
//! stream's cancel signal, so a forced close never waits for one. A normal end finishes the
//! sending side (QUIC `finish`); a forced close resets the sending side and stops the
//! receiving side, dropping what was not read.
//!
//! On the side that accepted the QUIC connection, the finish waits until the reader read the
//! peer's end, or until the drain cap passed (docs/design/transport.md "연결을 닫는 때"): when
//! the dialer reads this end's EOF, this side's application already read everything the dialer
//! sent, so the dialer may close the connection. The refusing ACK is the exception: the
//! handshake finishes it at once ([`RawChannel::finish_at_once`]).

use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tasty_net_p2p::{
    P2pCancel, P2pCloseReason, P2pConnection, P2pRecvStream, P2pSendStream, P2pStream,
};

use crate::cancel::{AttemptCancel, Cancellable};
use crate::channel::{ChannelControl, RawChannel};
use crate::lock::lock;
use crate::transport::{ConnectError, TransportKind};

use super::conn::{ChannelSlot, ConnSide};

/// The application code a reset or a stop carries. The peer decides nothing by it: a reset
/// read is `ConnectionReset`, a stopped write `BrokenPipe`, whatever the code.
const STREAM_CLOSE_CODE: u32 = 0;

/// The state one stream's halves and its control share.
pub(crate) struct Stream {
    send: Mutex<P2pSendStream>,
    recv: Mutex<P2pRecvStream>,
    cancel: P2pCancel,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
    conn: P2pConnection,
    side: ConnSide,
    drain_cap: Duration,
    end: Mutex<EndState>,
    end_changed: Condvar,
}

/// Where the sending side is.
#[derive(Default)]
struct EndState {
    /// The reader read the peer's end of the stream.
    eof_read: bool,
    /// A finish was asked for and waits for `eof_read` (the side that accepted the connection).
    finish_wanted: bool,
    /// The sending side was finished.
    finished: bool,
    /// The stream was closed both ways.
    aborted: bool,
}

impl Stream {
    /// Ends the sending side now, once.
    fn finish_now(&self) -> io::Result<()> {
        {
            let mut end = lock(&self.end);
            if end.finished || end.aborted {
                return Ok(());
            }
            end.finished = true;
            end.finish_wanted = false;
        }
        self.end_changed.notify_all();
        lock(&self.send).finish()
    }

    /// The finish of a normal end ([`ChannelControl::shutdown_write`]): at once on the side
    /// that opened the connection, after the peer's end (or the drain cap) on the other side.
    fn request_finish(self: &Arc<Self>) -> io::Result<()> {
        if self.side == ConnSide::Dialer {
            return self.finish_now();
        }
        {
            let mut end = lock(&self.end);
            if end.finished || end.aborted || end.finish_wanted {
                return Ok(());
            }
            if !end.eof_read {
                end.finish_wanted = true;
                drop(end);
                self.finish_after_drain_cap();
                return Ok(());
            }
        }
        self.finish_now()
    }

    /// Finishes at the drain cap unless the peer's end, a forced close or the finish came first.
    fn finish_after_drain_cap(self: &Arc<Self>) {
        let stream = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("p2p-finish".into())
            .spawn(move || {
                let started = Instant::now();
                let mut end = lock(&stream.end);
                while end.finish_wanted && !end.finished && !end.aborted {
                    let Some(left) = stream
                        .drain_cap
                        .checked_sub(started.elapsed())
                        .filter(|d| !d.is_zero())
                    else {
                        drop(end);
                        if let Err(error) = stream.finish_now() {
                            tracing::debug!(%error, "finishing a P2P stream at the drain cap failed");
                        }
                        return;
                    };
                    end = stream
                        .end_changed
                        .wait_timeout(end, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            });
        if let Err(error) = spawned {
            // Without the timer the finish goes out at once rather than never.
            tracing::debug!(%error, "no thread for the P2P finish timer");
            if let Err(error) = self.finish_now() {
                tracing::debug!(%error, "finishing a P2P stream failed");
            }
        }
    }

    /// The reader read the peer's end: a finish that waited for it goes out now.
    fn saw_eof(&self) {
        let wanted = {
            let mut end = lock(&self.end);
            end.eof_read = true;
            end.finish_wanted
        };
        self.end_changed.notify_all();
        if wanted && let Err(error) = self.finish_now() {
            tracing::debug!(%error, "finishing a P2P stream after the peer's end failed");
        }
    }

    /// The forced close: wakes blocked calls, resets the sending side and stops the
    /// receiving side.
    pub(crate) fn abort(&self) {
        self.cancel.cancel();
        {
            let mut end = lock(&self.end);
            if end.aborted {
                return;
            }
            end.aborted = true;
        }
        self.end_changed.notify_all();
        // The cancel made blocked calls return, so the locks come free.
        if let Err(error) = lock(&self.send).reset(STREAM_CLOSE_CODE) {
            tracing::debug!(%error, "resetting a P2P stream failed");
        }
        if let Err(error) = lock(&self.recv).stop(STREAM_CLOSE_CODE) {
            tracing::debug!(%error, "stopping a P2P stream failed");
        }
    }
}

/// The read half of a stream channel.
struct StreamReader {
    stream: Arc<Stream>,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = *lock(&self.stream.read_timeout);
        let n = {
            let mut recv = lock(&self.stream.recv);
            recv.set_read_timeout(timeout);
            recv.read(buf)?
        };
        if n == 0 && !buf.is_empty() {
            self.stream.saw_eof();
        }
        Ok(n)
    }
}

/// The write half of a stream channel.
struct StreamWriter {
    stream: Arc<Stream>,
}

impl Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let timeout = *lock(&self.stream.write_timeout);
        let mut send = lock(&self.stream.send);
        send.set_write_timeout(timeout);
        send.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The control of a stream channel.
struct StreamControl {
    stream: Arc<Stream>,
}

impl ChannelControl for StreamControl {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.stream.read_timeout) = timeout;
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.stream.write_timeout) = timeout;
        Ok(())
    }

    fn shutdown_write(&self) -> io::Result<()> {
        self.stream.request_finish()
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        self.stream.abort();
        Ok(())
    }

    fn close_reason(&self) -> Option<P2pCloseReason> {
        self.stream.conn.close_reason()
    }
}

/// Wraps `stream` of `conn` as a channel that holds `slot` (its place among the connection's
/// channels) until both halves are dropped.
pub(crate) fn channel(
    stream: P2pStream,
    conn: &P2pConnection,
    side: ConnSide,
    drain_cap: Duration,
    slot: ChannelSlot,
) -> RawChannel {
    let cancel = stream.canceller();
    let (send, recv) = stream.split();
    let shared = Arc::new(Stream {
        send: Mutex::new(send),
        recv: Mutex::new(recv),
        cancel,
        read_timeout: Mutex::new(None),
        write_timeout: Mutex::new(None),
        conn: conn.clone(),
        side,
        drain_cap,
        end: Mutex::new(EndState::default()),
        end_changed: Condvar::new(),
    });
    RawChannel::new(
        Box::new(StreamReader {
            stream: Arc::clone(&shared),
        }),
        Box::new(StreamWriter {
            stream: Arc::clone(&shared),
        }),
        Arc::new(StreamControl {
            stream: Arc::clone(&shared),
        }),
        TransportKind::P2p,
    )
    .with_finish_now(Arc::new(move || shared.finish_now()))
    .keep_alive(Arc::new(slot))
}

/// Opens a stream on `conn`, under `cancel`.
///
/// `tasty-net-p2p`'s `open_bi` waits without a limit while the peer allows no more streams
/// (`quic.max_bidi_streams`) and takes no cancel, so it runs on a thread of its own and the
/// cancel ends the wait here. A stream that opens after its attempt was cancelled is refused at
/// once; the thread ends with the open, which the connection's close ends too.
pub(crate) fn open(
    conn: &P2pConnection,
    cancel: &AttemptCancel,
) -> Result<P2pStream, ConnectError> {
    let opening = Arc::new(Opening::default());
    let _registration = cancel
        .register(Arc::clone(&opening) as Arc<dyn Cancellable>)
        .map_err(|_| ConnectError::Cancelled)?;
    let spawned = {
        let opening = Arc::clone(&opening);
        let conn = conn.clone();
        std::thread::Builder::new()
            .name("p2p-open".into())
            .spawn(move || opening.opened(conn.open_bi()))
    };
    if let Err(error) = spawned {
        // Without the thread the open waits here, as `open_bi` does.
        tracing::debug!(%error, "no thread for a P2P stream open");
        return conn.open_bi().map_err(ConnectError::Io);
    }
    opening.wait()
}

/// One stream open waiting on its thread.
#[derive(Default)]
struct Opening {
    state: Mutex<OpenState>,
    changed: Condvar,
}

#[derive(Default)]
struct OpenState {
    result: Option<io::Result<P2pStream>>,
    /// The attempt was cancelled: nobody takes the result any more.
    cancelled: bool,
}

impl Opening {
    /// The open's result, from its thread.
    fn opened(&self, result: io::Result<P2pStream>) {
        let mut state = lock(&self.state);
        if state.cancelled {
            drop(state);
            if let Ok(stream) = result {
                refuse(stream);
            }
            return;
        }
        state.result = Some(result);
        drop(state);
        self.changed.notify_all();
    }

    /// Waits for the open or the cancel, whichever comes first.
    fn wait(&self) -> Result<P2pStream, ConnectError> {
        let mut state = lock(&self.state);
        loop {
            if state.cancelled {
                let late = state.result.take();
                drop(state);
                if let Some(Ok(stream)) = late {
                    refuse(stream);
                }
                return Err(ConnectError::Cancelled);
            }
            if let Some(result) = state.result.take() {
                return result.map_err(ConnectError::Io);
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Cancellable for Opening {
    fn cancel(&self) {
        lock(&self.state).cancelled = true;
        self.changed.notify_all();
    }
}

/// Refuses a stream this end does not take: resets its sending side and stops its receiving
/// side.
pub(crate) fn refuse(stream: P2pStream) {
    let (mut send, mut recv) = stream.split();
    if let Err(error) = send.reset(STREAM_CLOSE_CODE) {
        tracing::debug!(%error, "resetting a refused P2P stream failed");
    }
    if let Err(error) = recv.stop(STREAM_CLOSE_CODE) {
        tracing::debug!(%error, "stopping a refused P2P stream failed");
    }
}
