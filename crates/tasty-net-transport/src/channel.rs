//! One channel: the raw byte halves a transport gives ([`RawChannel`]) and the framed halves
//! a consumer reads and writes ([`ChannelReader`], [`ChannelWriter`]).

use std::any::Any;
use std::error::Error;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tasty_net_frame::{
    HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT, ReadFailure, StreamFrame, StreamTag, read_frame,
    write_frame,
};

use tasty_net_ssh::{ListenerVerdict, RemoteListenerAbsent};

use crate::end::{EndReason, LinkCloser};
use crate::heartbeat::{Heartbeat, HeartbeatPolicy};
use crate::lock::lock;
use crate::reject::P2pCloseReason;
use crate::transport::TransportKind;

/// Control of one channel, shared by its reader, its writer and whoever closes it.
///
/// A transport implements it for its channel type. Timeouts apply to every half of the
/// channel. A read that times out returns `WouldBlock` or `TimedOut`, the kinds
/// [`ReadFailure::HeartbeatTimeout`] stands for.
pub trait ChannelControl: Send + Sync {
    /// Sets the timeout of every later read on this channel. `None` blocks without a limit.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// Sets the timeout of every later write on this channel. `None` blocks without a limit.
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// Ends the sending direction: the peer reads what was sent and then sees EOF (TCP
    /// `shutdown(Write)`, QUIC `finish`).
    fn shutdown_write(&self) -> io::Result<()>;
    /// Ends both directions and wakes a read blocked on this channel in another thread. Bytes
    /// not yet read are dropped.
    fn shutdown_and_wake(&self) -> io::Result<()>;

    /// Why the whole QUIC connection under this channel was closed, once it was closed with a
    /// code. Only a P2P channel has one ([`P2pCloseReason`]).
    fn close_reason(&self) -> Option<P2pCloseReason> {
        None
    }
}

/// One channel as a transport gives it: a read half, a write half and its control.
///
/// It carries no framing yet. The handshake runs on it and then turns it into a
/// [`ChannelReader`] and a [`ChannelWriter`]. It may hold resources that must live as long as
/// the channel, such as the SSH tunnel it goes through; they drop with the last half.
pub struct RawChannel {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    control: Arc<dyn ChannelControl>,
    kind: TransportKind,
    keep: Vec<Arc<dyn Any + Send + Sync>>,
    /// The absent-listener verdict of a channel through an SSH tunnel.
    verdict: Option<ListenerVerdict>,
}

impl fmt::Debug for RawChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawChannel")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl RawChannel {
    /// A channel from its halves. A transport implementation builds its channels with this.
    pub fn new(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        control: Arc<dyn ChannelControl>,
        kind: TransportKind,
    ) -> Self {
        Self {
            reader,
            writer,
            control,
            kind,
            keep: Vec::new(),
            verdict: None,
        }
    }

    /// Wraps a TCP connection, such as one a consumer's loopback listener accepted. Nagle is
    /// turned off (`set_nodelay(true)`) so a small frame is not held back.
    pub fn from_tcp(stream: TcpStream) -> io::Result<Self> {
        Self::from_tcp_as(stream, TransportKind::Tcp)
    }

    /// The reader, the writer and the control share one socket handle: the timeouts the
    /// control sets are on the handle every read and write goes through. A Windows CI run
    /// once saw a refusal's linger read wait 120 s past its drain cap when each part had its
    /// own `try_clone` copy; that the copies did not share the timeout is a hypothesis, not a
    /// confirmed cause.
    pub(crate) fn from_tcp_as(stream: TcpStream, kind: TransportKind) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        let stream = Arc::new(stream);
        Ok(Self::new(
            Box::new(SharedTcp(Arc::clone(&stream))),
            Box::new(SharedTcp(Arc::clone(&stream))),
            Arc::new(TcpControl { stream }),
            kind,
        ))
    }

    /// The control of this channel.
    pub fn control(&self) -> Arc<dyn ChannelControl> {
        Arc::clone(&self.control)
    }

    /// The transport the channel came from.
    pub fn kind(&self) -> TransportKind {
        self.kind
    }

    /// Keeps `resource` alive until both halves of the channel are dropped.
    pub fn keep_alive(mut self, resource: Arc<dyn Any + Send + Sync>) -> Self {
        self.keep.push(resource);
        self
    }

    pub(crate) fn with_verdict(mut self, verdict: ListenerVerdict) -> Self {
        self.verdict = Some(verdict);
        self
    }

    /// Whether this channel failed with `failure` because nothing listens behind its SSH
    /// tunnel. Waits up to half a second for ssh's report when the channel has a verdict.
    pub(crate) fn listener_absent(
        &self,
        failure: &(dyn std::error::Error + 'static),
    ) -> Option<RemoteListenerAbsent> {
        self.verdict.as_ref()?.explain_failure(failure)
    }

    /// Puts `bytes` in front of the read half: they are read before anything from the
    /// transport. The receiving side passes what its first-line reader already buffered.
    pub fn with_buffered(mut self, bytes: Vec<u8>) -> Self {
        if !bytes.is_empty() {
            self.reader = Box::new(Cursor::new(bytes).chain(self.reader));
        }
        self
    }

    /// Frames the channel without a handshake: the read and write timeouts and the heartbeat
    /// start now. The handshake functions call this after a successful ACK; a consumer calls
    /// it only for a channel it authenticates itself (tests, tools).
    pub fn framed(self, config: &ChannelConfig) -> io::Result<Framed> {
        let closer = LinkCloser::new(Arc::clone(&self.control));
        self.framed_under(config, closer)
    }

    /// [`RawChannel::framed`] with the closer of an existing link (a bulk channel). A drain
    /// cap too large to add to the current time is refused with `InvalidInput`.
    pub(crate) fn framed_under(
        self,
        config: &ChannelConfig,
        closer: LinkCloser,
    ) -> io::Result<Framed> {
        // Every timeout must make a deadline: one that does not is a configuration error,
        // never a wait without a limit.
        let now = Instant::now();
        for (limit, what) in [
            (
                config.read_timeout,
                "the read timeout is too large to make a deadline",
            ),
            (
                config.write_timeout,
                "the write timeout is too large to make a deadline",
            ),
            (
                config.drain_cap,
                "the drain cap is too large to make a deadline",
            ),
        ] {
            if now.checked_add(limit).is_none() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, what));
            }
        }
        let read_timeout = nonzero(config.read_timeout);
        self.control.set_read_timeout(Some(read_timeout))?;
        self.control
            .set_write_timeout(Some(nonzero(config.write_timeout)))?;
        let keep = Arc::new(self.keep);
        let shared = Arc::new(ChannelShared {
            control: Arc::clone(&self.control),
            reader_end: Mutex::new(false),
            reader_end_cv: Condvar::new(),
            writer_closed: AtomicBool::new(false),
            on_normal_end: Mutex::new(NormalEnd::default()),
            heartbeat: Mutex::new(HeartbeatSlot::Stopped),
            heartbeat_stopped: Condvar::new(),
        });
        let out = Arc::new(Outbox {
            state: Mutex::new(OutState {
                writer: self.writer,
                last_write: Instant::now(),
                failed: false,
                closed: false,
            }),
        });
        let heartbeat = Heartbeat::start(
            Arc::clone(&out),
            config.heartbeat,
            heartbeat_interval(read_timeout),
        )?;
        *lock(&shared.heartbeat) = HeartbeatSlot::Running(heartbeat);
        Ok(Framed {
            reader: ChannelReader {
                inner: self.reader,
                failed: None,
                shared: Arc::clone(&shared),
                read_timeout,
                _keep: Arc::clone(&keep),
            },
            writer: ChannelWriter {
                out,
                shared,
                closer: closer.clone(),
                drain_cap: config.drain_cap,
                _keep: keep,
            },
            closer,
        })
    }
}

/// Reads the raw bytes of the channel (the handshake reads its first messages this way).
impl Read for RawChannel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Writes raw bytes to the channel (the handshake writes its first messages this way).
impl Write for RawChannel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A zero timeout means "no timeout" to the socket API and is refused there; the smallest
/// timeout is used instead.
fn nonzero(timeout: Duration) -> Duration {
    timeout.max(Duration::from_millis(1))
}

/// The Ping interval for a read timeout: [`HEARTBEAT_INTERVAL`], or a sixth of a shorter read
/// timeout so the peer still sees several Pings inside its own timeout (the default 30 s
/// timeout gives the 5 s interval).
pub(crate) fn heartbeat_interval(read_timeout: Duration) -> Duration {
    HEARTBEAT_INTERVAL
        .min(read_timeout / 6)
        .max(Duration::from_millis(1))
}

/// Timeouts and heartbeat of one framed channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
    /// Read timeout. Any frame (a Ping included) renews it.
    pub read_timeout: Duration,
    /// Timeout of a blocking write.
    pub write_timeout: Duration,
    /// When this side sends Pings.
    pub heartbeat: HeartbeatPolicy,
    /// How long [`ChannelWriter::finish`] waits for the reader to end.
    pub drain_cap: Duration,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            read_timeout: HEARTBEAT_TIMEOUT,
            write_timeout: HEARTBEAT_TIMEOUT,
            heartbeat: HeartbeatPolicy::WhenIdle,
            drain_cap: DEFAULT_DRAIN_CAP,
        }
    }
}

/// Default [`ChannelConfig::drain_cap`].
pub const DEFAULT_DRAIN_CAP: Duration = Duration::from_secs(2);

/// The framed halves of one channel and the closer they share.
#[derive(Debug)]
pub struct Framed {
    /// Read half. Give it to the reading thread.
    pub reader: ChannelReader,
    /// Write half. Give it to the writing thread.
    pub writer: ChannelWriter,
    /// Closer of the link the channel belongs to.
    pub closer: LinkCloser,
}

impl Framed {
    /// Runs `hook` once when the reader ended and the write half was closed.
    pub(crate) fn on_normal_end(&self, hook: NormalEndHook) {
        lock(&self.writer.shared.on_normal_end).hook = Some(hook);
        self.writer.shared.check_normal_end();
    }

    /// The state the halves share, for an owner that must stop the heartbeat itself (a
    /// [`Link`](crate::Link) whose writer the consumer may keep).
    pub(crate) fn shared(&self) -> Arc<ChannelShared> {
        Arc::clone(&self.writer.shared)
    }
}

/// State the reader and the writer of one channel share.
pub(crate) struct ChannelShared {
    control: Arc<dyn ChannelControl>,
    reader_end: Mutex<bool>,
    reader_end_cv: Condvar,
    writer_closed: AtomicBool,
    /// The normal end: run once when the reader ended and the write half is closed, unless
    /// the link's drop claimed the end first. The one lock decides which of the two wins.
    on_normal_end: Mutex<NormalEnd>,
    /// The heartbeat thread, stopped and joined by whoever comes first: the writer's half
    /// close, the writer's drop, or the link's drop. A later caller waits for that join.
    heartbeat: Mutex<HeartbeatSlot>,
    heartbeat_stopped: Condvar,
}

/// Where the heartbeat thread of a channel is: running, being joined by one caller (taken
/// out, not yet joined), or joined.
pub(crate) enum HeartbeatSlot {
    Running(Heartbeat),
    Stopping,
    Stopped,
}

type NormalEndHook = Box<dyn FnOnce() + Send>;

/// Who claimed the end of the channel: the normal end (both directions done) or the link's
/// drop (a forced close). The first to claim it under the lock wins.
#[derive(Default)]
pub(crate) struct NormalEnd {
    hook: Option<NormalEndHook>,
    claimed_normal: bool,
    claimed_forced: bool,
}

impl ChannelShared {
    /// Stops and joins the heartbeat thread. The first caller takes it out and joins it
    /// outside the lock; a caller that comes while that join runs waits until it completed, so
    /// every caller returns only once the thread is gone.
    pub(crate) fn stop_heartbeat(&self) {
        let mut slot = lock(&self.heartbeat);
        loop {
            match std::mem::replace(&mut *slot, HeartbeatSlot::Stopping) {
                HeartbeatSlot::Running(heartbeat) => {
                    drop(slot);
                    drop(heartbeat);
                    *lock(&self.heartbeat) = HeartbeatSlot::Stopped;
                    self.heartbeat_stopped.notify_all();
                    return;
                }
                HeartbeatSlot::Stopping => {
                    slot = self
                        .heartbeat_stopped
                        .wait(slot)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                HeartbeatSlot::Stopped => {
                    *slot = HeartbeatSlot::Stopped;
                    return;
                }
            }
        }
    }

    /// True until the heartbeat thread was joined (also while one caller is joining it).
    #[cfg(test)]
    pub(crate) fn heartbeat_running(&self) -> bool {
        !matches!(*lock(&self.heartbeat), HeartbeatSlot::Stopped)
    }

    /// Makes the running heartbeat thread run `hook` right before it ends, for tests that
    /// hold its join in progress.
    #[cfg(test)]
    pub(crate) fn heartbeat_before_exit(&self, hook: Box<dyn FnOnce() + Send>) {
        if let HeartbeatSlot::Running(heartbeat) = &*lock(&self.heartbeat) {
            heartbeat.before_exit(hook);
        }
    }

    fn mark_reader_end(&self) {
        *lock(&self.reader_end) = true;
        self.reader_end_cv.notify_all();
        self.check_normal_end();
    }

    fn mark_writer_closed(&self) {
        self.writer_closed.store(true, Ordering::Release);
        self.check_normal_end();
    }

    fn check_normal_end(&self) {
        let ended = *lock(&self.reader_end) && self.writer_closed.load(Ordering::Acquire);
        if !ended {
            return;
        }
        let hook = {
            let mut end = lock(&self.on_normal_end);
            if end.claimed_forced || end.hook.is_none() {
                return;
            }
            end.claimed_normal = true;
            end.hook.take()
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Claims the end for a forced close (the link's drop). False if the normal end already
    /// claimed it: its hook runs (or ran) and the channel must not be reset on top of it.
    pub(crate) fn claim_forced_end(&self) -> bool {
        let mut end = lock(&self.on_normal_end);
        if end.claimed_normal {
            return false;
        }
        end.claimed_forced = true;
        end.hook = None;
        true
    }

    /// Waits until the reader ended or `limit` passed. True if the reader ended.
    /// ([`RawChannel::framed`] refuses a drain cap that cannot make a deadline.)
    fn wait_reader_end(&self, limit: Duration) -> bool {
        // Measured as time elapsed, so no deadline is added to an Instant.
        let started = Instant::now();
        let mut ended = lock(&self.reader_end);
        while !*ended {
            let Some(left) = limit
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
            else {
                return false;
            };
            ended = self
                .reader_end_cv
                .wait_timeout(ended, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// A read failure: its class and the I/O error behind it (as `source()`).
#[derive(Clone, Debug)]
pub struct ReadError {
    failure: ReadFailure,
    error: Arc<io::Error>,
}

impl ReadError {
    pub(crate) fn from_io(error: io::Error) -> Self {
        Self {
            failure: ReadFailure::of_io(&error),
            error: Arc::new(error),
        }
    }

    /// The class of the failure.
    pub fn failure(&self) -> ReadFailure {
        self.failure
    }

    /// The kind of the I/O error behind it.
    pub fn io_kind(&self) -> io::ErrorKind {
        self.error.kind()
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "channel read failed: {}", self.failure)
    }
}

impl Error for ReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// The read half of a framed channel.
///
/// [`ChannelReader::read_frame`] skips Pings: they only renew the read timeout. After one
/// failure every later read returns the same failure, because the failed read may have
/// consumed part of a frame and the frame boundary is lost. Dropping the reader (a panic
/// unwinding through its thread included) marks the reader as ended for
/// [`ChannelWriter::finish`].
pub struct ChannelReader {
    inner: Box<dyn Read + Send>,
    failed: Option<ReadError>,
    shared: Arc<ChannelShared>,
    read_timeout: Duration,
    _keep: Arc<Vec<Arc<dyn Any + Send + Sync>>>,
}

impl fmt::Debug for ChannelReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelReader")
            .field("failed", &self.failed)
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl ChannelReader {
    /// Reads the next frame that is not a Ping.
    pub fn read_frame(&mut self) -> Result<StreamFrame, ReadError> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        loop {
            match read_frame(&mut self.inner) {
                Ok(frame) if frame.tag == StreamTag::Ping => continue,
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    let failed = ReadError::from_io(error);
                    self.failed = Some(failed.clone());
                    return Err(failed);
                }
            }
        }
    }

    /// The read timeout set on this channel (for [`ReadFailure::describe`]).
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// Marks the reader as ended without dropping it, for a consumer that read a frame which
    /// ends the connection and reads nothing more.
    pub fn finish(&mut self) {
        self.shared.mark_reader_end();
    }
}

impl Drop for ChannelReader {
    fn drop(&mut self) {
        self.shared.mark_reader_end();
    }
}

/// The writer and the time of its last frame, shared with the heartbeat thread.
pub(crate) struct Outbox {
    state: Mutex<OutState>,
}

struct OutState {
    writer: Box<dyn Write + Send>,
    last_write: Instant,
    failed: bool,
    closed: bool,
}

/// What [`Outbox::write_ping_if`] did.
pub(crate) enum PingResult {
    Sent,
    NotDue(Duration),
    Stopped,
}

impl Outbox {
    fn write(&self, tag: StreamTag, payload: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the write half of the channel is closed",
            ));
        }
        if state.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "an earlier write on the channel failed",
            ));
        }
        match write_frame(&mut state.writer, tag, payload) {
            Ok(()) => {
                state.last_write = Instant::now();
                Ok(())
            }
            // Nothing was written: the frame is refused before any byte goes out.
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Err(e),
            Err(e) => {
                state.failed = true;
                Err(e)
            }
        }
    }

    /// Writes a Ping if `due(last_write)` says so; otherwise says how long to wait.
    pub(crate) fn write_ping_if(
        &self,
        due: impl FnOnce(Instant) -> Option<Duration>,
    ) -> PingResult {
        let mut state = lock(&self.state);
        if state.closed || state.failed {
            return PingResult::Stopped;
        }
        if let Some(wait) = due(state.last_write) {
            return PingResult::NotDue(wait);
        }
        match write_frame(&mut state.writer, StreamTag::Ping, &[]) {
            Ok(()) => {
                state.last_write = Instant::now();
                PingResult::Sent
            }
            Err(error) => {
                tracing::debug!(%error, "heartbeat write failed");
                state.failed = true;
                PingResult::Stopped
            }
        }
    }

    /// Closes the write half for frames; true the first time.
    fn close(&self) -> bool {
        let mut state = lock(&self.state);
        !std::mem::replace(&mut state.closed, true)
    }
}

/// How [`ChannelWriter::finish`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// The reader ended within the drain cap. The reader side settles the end.
    ReaderEnded,
    /// The drain cap ran out. The writer raised [`EndReason::Lost`], tried to settle
    /// (`settled` says whether it won) and closed the link both ways.
    DrainCapReached {
        /// Whether this call won the settle.
        settled: bool,
    },
}

/// The write half of a framed channel.
///
/// [`ChannelWriter::write_frame`] gathers one frame into one buffer, writes it with one
/// `write_all` and flushes. The heartbeat thread of the channel writes Pings between frames.
/// After a failed write every later write fails, because part of a frame may be on the wire.
pub struct ChannelWriter {
    out: Arc<Outbox>,
    shared: Arc<ChannelShared>,
    closer: LinkCloser,
    drain_cap: Duration,
    _keep: Arc<Vec<Arc<dyn Any + Send + Sync>>>,
}

impl fmt::Debug for ChannelWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelWriter")
            .field("closed", &self.shared.writer_closed.load(Ordering::Acquire))
            .field("drain_cap", &self.drain_cap)
            .finish_non_exhaustive()
    }
}

impl ChannelWriter {
    /// Writes one frame and flushes.
    pub fn write_frame(&self, tag: StreamTag, payload: &[u8]) -> io::Result<()> {
        self.out.write(tag, payload)
    }

    /// Closes the sending direction after the frames written so far: the heartbeat stops and
    /// the peer reads every frame before it sees EOF. Later writes fail. Calling it again does
    /// nothing.
    pub fn half_close(&mut self) -> io::Result<()> {
        if !self.out.close() {
            return Ok(());
        }
        // The heartbeat thread stops before the shutdown, so no Ping follows it.
        self.shared.stop_heartbeat();
        let shut = self.shared.control.shutdown_write();
        self.shared.mark_writer_closed();
        shut
    }

    /// The writer's end of a connection (docs/design/transport.md "종료 순서"): closes the
    /// sending direction, then waits up to the drain cap for the reader of the same channel
    /// to end (EOF, a frame the consumer treats as the end, a read failure, or the reader
    /// dropped). If the cap runs out, raises [`EndReason::Lost`], tries to settle and closes
    /// the link both ways, which also wakes the reader.
    pub fn finish(&mut self) -> Finish {
        if let Err(error) = self.half_close() {
            tracing::debug!(%error, "half-closing the channel failed");
        }
        if self.shared.wait_reader_end(self.drain_cap) {
            return Finish::ReaderEnded;
        }
        self.closer.raise(EndReason::Lost);
        let settled = self.closer.settle();
        self.closer.wake();
        Finish::DrainCapReached { settled }
    }

    /// The closer of the link this channel belongs to.
    pub fn closer(&self) -> &LinkCloser {
        &self.closer
    }
}

impl Drop for ChannelWriter {
    fn drop(&mut self) {
        // Stop and join the heartbeat thread; the socket closes with the last half.
        self.shared.stop_heartbeat();
    }
}

/// Control of a TCP channel.
pub(crate) struct TcpControl {
    stream: Arc<TcpStream>,
}

/// A half of a TCP channel on the handle its [`TcpControl`] sets the timeouts of.
struct SharedTcp(Arc<TcpStream>);

impl Read for SharedTcp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Write for SharedTcp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.0).flush()
    }
}

impl ChannelControl for TcpControl {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(timeout)
    }

    fn shutdown_write(&self) -> io::Result<()> {
        self.stream.shutdown(Shutdown::Write)
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        crate::wake::shutdown_both_and_wake(&self.stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memory_channel_pair;
    use std::net::TcpListener;

    fn quick() -> ChannelConfig {
        ChannelConfig {
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            heartbeat: HeartbeatPolicy::WhenIdle,
            drain_cap: Duration::from_millis(300),
        }
    }

    fn tcp_pair() -> (RawChannel, RawChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let dialed = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (accepted, _) = listener.accept().expect("accept");
        (
            RawChannel::from_tcp(dialed).expect("dialer"),
            RawChannel::from_tcp(accepted).expect("acceptor"),
        )
    }

    /// Sends Data, Control, MeshData and Detach frames mixed and checks the order and the
    /// payloads on the other side.
    fn check_mixed_tag_order(a: RawChannel, b: RawChannel) {
        let mut a = a.framed(&quick()).expect("framed a");
        let mut b = b.framed(&quick()).expect("framed b");
        let tags = [
            StreamTag::Data,
            StreamTag::Control,
            StreamTag::MeshData,
            StreamTag::Data,
            StreamTag::Detach,
        ];
        const N: usize = 10_000;
        let writer = std::thread::spawn(move || {
            for i in 0..N {
                let tag = tags[i % tags.len()];
                a.writer
                    .write_frame(tag, &(i as u32).to_be_bytes())
                    .expect("write");
            }
            a.writer.half_close().expect("half close");
            a
        });
        for i in 0..N {
            let frame = b.reader.read_frame().expect("read");
            assert_eq!(frame.tag, tags[i % tags.len()], "frame {i}");
            assert_eq!(frame.payload, (i as u32).to_be_bytes(), "frame {i}");
        }
        let end = b.reader.read_frame().expect_err("EOF after the last frame");
        assert_eq!(end.failure(), ReadFailure::PeerClosed);
        drop(writer.join().expect("writer"));
    }

    #[test]
    fn ordered_channel_preserves_mixed_tag_order() {
        let (a, b) = memory_channel_pair();
        check_mixed_tag_order(a, b);
        let (a, b) = tcp_pair();
        check_mixed_tag_order(a, b);
    }

    /// A read on a TCP channel whose peer stays open and silent ends on the timeout set
    /// through the channel's control alone: nothing wakes it.
    #[test]
    fn a_tcp_read_ends_on_the_timeout_its_control_set() {
        let (mut a, _b) = tcp_pair();
        a.control()
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("read timeout");
        let started = Instant::now();
        let mut byte = [0u8; 1];
        let error = a.read(&mut byte).expect_err("the peer sends nothing");
        assert_eq!(ReadFailure::of_io(&error), ReadFailure::HeartbeatTimeout);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_failed_reader_keeps_returning_the_same_failure() {
        let (a, b) = memory_channel_pair();
        let mut b = b.framed(&quick()).expect("framed");
        // A header that announces 10 bytes, then EOF after 3 of them.
        let mut raw = a;
        raw.write_all(&[0, 0, 0, 0, 10, 1, 2, 3]).expect("write");
        raw.control().shutdown_write().expect("shutdown");
        let first = b.reader.read_frame().expect_err("truncated frame");
        assert_eq!(first.failure(), ReadFailure::PeerClosed);
        // The next read does not touch the stream again.
        let second = b.reader.read_frame().expect_err("still failed");
        assert_eq!(second.failure(), first.failure());
        assert_eq!(second.io_kind(), first.io_kind());
    }

    #[test]
    fn a_failed_reader_classifies_through_of_error() {
        let (a, b) = memory_channel_pair();
        let mut b = b.framed(&quick()).expect("framed");
        let mut raw = a;
        raw.write_all(&[9, 0, 0, 0, 0]).expect("write");
        let error = b.reader.read_frame().expect_err("unknown tag");
        assert_eq!(error.failure(), ReadFailure::Protocol);
        assert_eq!(ReadFailure::of_error(&error), ReadFailure::Protocol);
    }

    #[test]
    fn half_close_lets_the_peer_read_every_frame_before_eof() {
        let (a, b) = tcp_pair();
        let mut a = a.framed(&quick()).expect("framed a");
        let mut b = b.framed(&quick()).expect("framed b");
        a.writer
            .write_frame(StreamTag::Data, b"last")
            .expect("write");
        a.writer.half_close().expect("half close");
        a.writer
            .half_close()
            .expect("second half close does nothing");
        assert!(a.writer.write_frame(StreamTag::Data, b"late").is_err());
        assert_eq!(b.reader.read_frame().expect("frame").payload, b"last");
        assert_eq!(
            b.reader.read_frame().expect_err("eof").failure(),
            ReadFailure::PeerClosed
        );
        // The other direction still works after a half close.
        b.writer
            .write_frame(StreamTag::Data, b"back")
            .expect("write back");
        assert_eq!(a.reader.read_frame().expect("frame").payload, b"back");
    }

    #[test]
    fn finish_returns_when_the_reader_ends_first() {
        let (a, b) = memory_channel_pair();
        let mut a = a.framed(&quick()).expect("framed a");
        let mut b = b.framed(&quick()).expect("framed b");
        let reader = std::thread::spawn(move || {
            let mut r = a.reader;
            // Ends on the peer's EOF.
            let end = r.read_frame().expect_err("eof");
            assert_eq!(end.failure(), ReadFailure::PeerClosed);
        });
        // The peer answers our half close with its own.
        let peer = std::thread::spawn(move || {
            let end = b.reader.read_frame().expect_err("eof");
            assert_eq!(end.failure(), ReadFailure::PeerClosed);
            b.writer.half_close().expect("peer half close");
            b
        });
        assert_eq!(a.writer.finish(), Finish::ReaderEnded);
        reader.join().expect("reader");
        drop(peer.join().expect("peer"));
        assert!(!a.closer.is_ending());
    }

    #[test]
    fn finish_settles_lost_and_wakes_the_reader_after_the_drain_cap() {
        let (a, b) = tcp_pair();
        let mut a = a.framed(&quick()).expect("framed a");
        let _peer = b.framed(&quick()).expect("framed b"); // never half-closes
        let mut reader = a.reader;
        let woken = std::thread::spawn(move || reader.read_frame().map(|_| ()));
        let started = Instant::now();
        assert_eq!(a.writer.finish(), Finish::DrainCapReached { settled: true });
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(a.closer.settled(), Some(EndReason::Lost));
        let closed = Instant::now();
        let result = woken.join().expect("reader thread");
        assert!(result.is_err(), "the forced close woke the blocked read");
        // Woken by the close, not ended by the 5 s read timeout.
        assert!(
            closed.elapsed() < Duration::from_secs(1),
            "the reader ended {:?} after the close",
            closed.elapsed()
        );
    }
}
