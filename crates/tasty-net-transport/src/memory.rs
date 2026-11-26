//! The in-process transport for tests: channels are pipe pairs inside one process.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cancel::AttemptCancel;
use crate::channel::{ChannelControl, RawChannel};
use crate::lock::lock;
use crate::transport::{
    AcceptError, ConnectError, Connection, Endpoint, EndpointHandle, Listener, Transport,
    TransportKind,
};

/// One direction of a memory channel.
#[derive(Default)]
struct Pipe {
    state: Mutex<PipeState>,
    cv: Condvar,
}

#[derive(Default)]
struct PipeState {
    buf: VecDeque<u8>,
    /// The writing end closed: the reader sees EOF after the buffered bytes.
    write_closed: bool,
    /// The reading end closed: buffered bytes are dropped and writes fail like a reset.
    read_closed: bool,
}

/// Timeouts of one end, shared by its halves and its control.
#[derive(Default)]
struct Timeouts {
    read: Mutex<Option<Duration>>,
}

struct PipeReader {
    pipe: Arc<Pipe>,
    timeouts: Arc<Timeouts>,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let timeout = *lock(&self.timeouts.read);
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut state = lock(&self.pipe.state);
        loop {
            if state.read_closed {
                return Ok(0);
            }
            if !state.buf.is_empty() {
                let n = out.len().min(state.buf.len());
                for (slot, byte) in out.iter_mut().zip(state.buf.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            if state.write_closed {
                return Ok(0);
            }
            state = match deadline {
                None => self
                    .pipe
                    .cv
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "memory channel read timed out",
                        ));
                    }
                    self.pipe
                        .cv
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
            };
        }
    }
}

struct PipeWriter {
    pipe: Arc<Pipe>,
}

impl Write for PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = lock(&self.pipe.state);
        if state.read_closed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "the memory channel was closed by its reader",
            ));
        }
        if state.write_closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the memory channel write half is closed",
            ));
        }
        state.buf.extend(bytes);
        self.pipe.cv.notify_all();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Control of one end: `outgoing` is the pipe this end writes, `incoming` the one it reads.
struct MemoryControl {
    outgoing: Arc<Pipe>,
    incoming: Arc<Pipe>,
    timeouts: Arc<Timeouts>,
}

impl ChannelControl for MemoryControl {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if timeout == Some(Duration::ZERO) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a zero timeout is not allowed",
            ));
        }
        *lock(&self.timeouts.read) = timeout;
        Ok(())
    }

    fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        // Writes into a memory pipe never block.
        Ok(())
    }

    fn shutdown_write(&self) -> io::Result<()> {
        lock(&self.outgoing.state).write_closed = true;
        self.outgoing.cv.notify_all();
        Ok(())
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        lock(&self.outgoing.state).write_closed = true;
        self.outgoing.cv.notify_all();
        {
            let mut incoming = lock(&self.incoming.state);
            incoming.read_closed = true;
            incoming.buf.clear();
        }
        self.incoming.cv.notify_all();
        Ok(())
    }
}

fn end(outgoing: &Arc<Pipe>, incoming: &Arc<Pipe>) -> RawChannel {
    let timeouts = Arc::new(Timeouts::default());
    RawChannel::new(
        Box::new(PipeReader {
            pipe: Arc::clone(incoming),
            timeouts: Arc::clone(&timeouts),
        }),
        Box::new(PipeWriter {
            pipe: Arc::clone(outgoing),
        }),
        Arc::new(MemoryControl {
            outgoing: Arc::clone(outgoing),
            incoming: Arc::clone(incoming),
            timeouts,
        }),
        TransportKind::Memory,
    )
}

/// Two connected ends of a memory channel: what one writes, the other reads.
pub fn memory_channel_pair() -> (RawChannel, RawChannel) {
    let a_to_b = Arc::new(Pipe::default());
    let b_to_a = Arc::new(Pipe::default());
    (end(&a_to_b, &b_to_a), end(&b_to_a, &a_to_b))
}

/// A memory transport and the listener that receives its channels.
pub fn memory_transport() -> (MemoryTransport, MemoryListener) {
    let (tx, rx) = mpsc::channel();
    (
        MemoryTransport { tx },
        MemoryListener { rx: Mutex::new(rx) },
    )
}

/// The dialing side of [`memory_transport`]. Every channel it opens arrives at the paired
/// [`MemoryListener`].
#[derive(Clone)]
pub struct MemoryTransport {
    tx: Sender<RawChannel>,
}

impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTransport").finish_non_exhaustive()
    }
}

impl MemoryTransport {
    fn open(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let (dialer, acceptor) = memory_channel_pair();
        self.tx.send(acceptor).map_err(|_| {
            ConnectError::Io(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "the memory listener is gone",
            ))
        })?;
        Ok(dialer)
    }
}

struct MemoryEndpoint {
    transport: MemoryTransport,
}

impl EndpointHandle for MemoryEndpoint {
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        self.transport.open(cancel)
    }
}

impl Transport for MemoryTransport {
    type Target = ();

    fn connect(&self, _target: &(), cancel: &AttemptCancel) -> Result<Connection, ConnectError> {
        let channel = self.open(cancel)?;
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(MemoryEndpoint {
                transport: self.clone(),
            })),
        })
    }
}

/// The receiving side of [`memory_transport`].
pub struct MemoryListener {
    rx: Mutex<Receiver<RawChannel>>,
}

impl std::fmt::Debug for MemoryListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryListener").finish_non_exhaustive()
    }
}

impl Listener for MemoryListener {
    /// Waits for the next channel. Fails once every [`MemoryTransport`] clone is dropped.
    fn accept(&self) -> Result<RawChannel, AcceptError> {
        lock(&self.rx).recv().map_err(|_| {
            AcceptError::new(io::Error::new(
                io::ErrorKind::NotConnected,
                "every memory transport is gone",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_read_times_out_and_a_shutdown_wakes_it() {
        let (mut a, b) = memory_channel_pair();
        a.control()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        let mut byte = [0u8; 1];
        let e = a.read(&mut byte).expect_err("timeout");
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);

        a.control().set_read_timeout(None).expect("no timeout");
        let control = a.control();
        let blocked = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            a.read(&mut byte)
        });
        std::thread::sleep(Duration::from_millis(50));
        control.shutdown_and_wake().expect("wake");
        assert_eq!(blocked.join().expect("reader").expect("eof"), 0);
        drop(b);
    }

    #[test]
    fn a_closed_reader_makes_the_peer_write_fail() {
        let (a, mut b) = memory_channel_pair();
        a.control().shutdown_and_wake().expect("close");
        let e = b.write(b"x").expect_err("reset");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn memory_transport_delivers_each_channel_to_the_listener() {
        let (transport, listener) = memory_transport();
        let cancel = AttemptCancel::new();
        let mut first = transport.connect(&(), &cancel).expect("connect");
        let mut more = first.endpoint.connect_more(&cancel).expect("more");
        let mut a1 = listener.accept().expect("accept 1");
        let mut a2 = listener.accept().expect("accept 2");
        first.channel.write_all(b"1").expect("write");
        more.write_all(b"2").expect("write");
        let mut byte = [0u8; 1];
        a1.read_exact(&mut byte).expect("read");
        assert_eq!(&byte, b"1");
        a2.read_exact(&mut byte).expect("read");
        assert_eq!(&byte, b"2");
        drop((first, more, transport));
        assert!(listener.accept().is_err());
    }
}
