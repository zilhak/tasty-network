//! Whether nothing listens on the remote port behind a tunnel.
//!
//! `ssh -L` opens the local listener first, so the tunnel comes up even if the remote port is
//! empty. That shows only at the first connection: ssh cannot open the channel, writes
//! `channel N: open failed: connect failed: <remote reason>` to stderr, and closes the local
//! connection. Locally that looks like EOF (Linux, macOS) or a reset (Windows os error 10054).
//!
//! Both signals are needed. The `open failed: connect failed` part of the stderr line is the
//! fixed text the ssh client writes for the channel refusal code (SSH2_OPEN_CONNECT_FAILED),
//! independent of OS and locale. The reason after it (`Connection refused` and so on) comes
//! from the remote OS and is not read. The forward target is the remote loopback, so a failed
//! connect means no process accepts on that port. A listener that answers oddly writes no such
//! line and is not judged absent.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
#[cfg(feature = "system-ssh")]
use std::io::{Read, Write};
use std::net::SocketAddr;
#[cfg(feature = "system-ssh")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::settings::SshSettings;

/// How long to wait after a failure for the stderr line (`tunnel.verdict_grace`), and how often
/// to look meanwhile (`tunnel.verdict_poll`). ssh writes the refusal line and then closes the
/// local connection, but the caller can see the failure before the relay thread has handled the
/// line.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VerdictTiming {
    grace: Duration,
    poll: Duration,
}

impl VerdictTiming {
    pub(crate) fn new(settings: &SshSettings) -> Self {
        Self {
            grace: settings.verdict_grace(),
            poll: settings.verdict_poll(),
        }
    }
}

/// Whether an ssh stderr line reports a channel that could not open because the connect to the
/// remote port failed.
#[cfg(feature = "system-ssh")]
pub(crate) fn is_channel_connect_failure(line: &str) -> bool {
    line.trim_start().starts_with("channel ") && line.contains("open failed: connect failed")
}

/// Whether an ssh stderr line says this ssh does not know the `ForkAfterAuthentication` option
/// (OpenSSH before 8.7: `command-line: line 0: Bad configuration option: forkafterauthentication`).
/// The option name must end there: a newer ssh refusing a longer name that starts the same
/// (`forkafterauthenticationextra`) is a different error.
#[cfg(feature = "system-ssh")]
pub(crate) fn is_fork_option_refusal(line: &str) -> bool {
    const REFUSAL: &str = "bad configuration option: forkafterauthentication";
    let line = line.to_ascii_lowercase();
    line.find(REFUSAL).is_some_and(|at| {
        line[at + REFUSAL.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    })
}

/// What the stderr relay of one tunnel marked.
#[derive(Debug, Default)]
pub(crate) struct StderrMarks {
    /// How many channel connect failure lines came ([`is_channel_connect_failure`]). A count,
    /// so the marks of a tunnel keep the same size however long it lives.
    connect_failures: AtomicU64,
    /// The local connections of an embedded tunnel and the verdicts that name them
    /// ([`Connections`]).
    connections: Mutex<Connections>,
    /// ssh refused `ForkAfterAuthentication` ([`is_fork_option_refusal`]).
    #[cfg(feature = "system-ssh")]
    pub(crate) fork_option_refused: AtomicBool,
}

impl StderrMarks {
    /// Records a channel connect failure line.
    pub(crate) fn record_connect_failure(&self) {
        self.connect_failures.fetch_add(1, Ordering::AcqRel);
    }

    /// How many channel connect failure lines came so far.
    pub(crate) fn connect_failures(&self) -> u64 {
        self.connect_failures.load(Ordering::Acquire)
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, Connections> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The marks of an embedded tunnel, which knows each local connection's channel: a verdict
    /// that names its connection is judged by that connection's channel alone.
    #[cfg(any(test, feature = "embedded-ssh"))]
    pub(crate) fn per_connection() -> Self {
        let marks = Self::default();
        marks.connections().per_connection = true;
        marks
    }

    /// The local connection from `peer` the tunnel accepted: its slot, which the verdict that
    /// names this connection shares ([`ListenerVerdict::for_local`]). The tunnel gives it back
    /// with [`Self::release`] when the connection has ended.
    #[cfg(any(test, feature = "embedded-ssh"))]
    pub(crate) fn accept(&self, peer: SocketAddr) -> Arc<ChannelSlot> {
        let mut c = self.connections();
        c.accepted += 1;
        let generation = c.accepted;
        c.forget_dropped_claims();
        if let Some((after, slot)) = c.claims.remove(&peer)
            && after < generation
            && let Some(slot) = slot.upgrade()
        {
            return slot;
        }
        let slot = Arc::new(ChannelSlot::default());
        c.unclaimed.insert(peer, (generation, Arc::clone(&slot)));
        slot
    }

    /// The local connection from `peer` with `slot` has ended (both sides closed): a slot no
    /// verdict has taken is dropped, so a verdict that names that connection later knows
    /// nothing of it (no code, no absent listener).
    #[cfg(any(test, feature = "embedded-ssh"))]
    pub(crate) fn release(&self, peer: SocketAddr, slot: &Arc<ChannelSlot>) {
        let mut c = self.connections();
        if c.unclaimed
            .get(&peer)
            .is_some_and(|(_, kept)| Arc::ptr_eq(kept, slot))
        {
            c.unclaimed.remove(&peer);
        }
    }

    /// The entries the marks keep: accepted connections no verdict has taken, and verdicts
    /// waiting for their connection.
    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        let c = self.connections();
        c.unclaimed.len() + c.claims.len()
    }

    /// The local connections accepted so far.
    fn accepted(&self) -> u64 {
        self.connections().accepted
    }

    /// The slot of the first connection from `local` accepted after `after` connections: the
    /// one already accepted, or the one the tunnel accepts next from that address.
    fn claim(&self, local: SocketAddr, after: u64) -> Option<Arc<ChannelSlot>> {
        let mut c = self.connections();
        if !c.per_connection {
            return None;
        }
        c.forget_dropped_claims();
        if let Some((generation, slot)) = c.unclaimed.remove(&local)
            && generation > after
        {
            return Some(slot);
        }
        let slot = Arc::new(ChannelSlot::default());
        c.claims.insert(local, (after, Arc::downgrade(&slot)));
        Some(slot)
    }
}

impl Connections {
    /// Drops the claims whose verdicts are gone.
    fn forget_dropped_claims(&mut self) {
        self.claims.retain(|_, (_, slot)| slot.upgrade().is_some());
    }
}

/// The local connections of an embedded tunnel, each with a generation (the count of
/// connections accepted up to it), so a verdict finds its own connection and not a later one
/// from the same address. A verdict takes the count when it is taken, before its connection
/// exists, and names the address once connected, while its connection is open; the tunnel may
/// accept the connection before or after that.
///
/// What is kept is bounded by the open connections and the live verdicts, not by the
/// connections a tunnel has forwarded: an accepted connection's slot waits for its verdict
/// only while the connection is open (the tunnel releases it when both sides have closed, so
/// a connection no verdict names, such as one cancelled before it was named, leaves nothing),
/// a claim is dropped when its verdict is, and each map keeps one entry per address. A verdict
/// that names a connection which has already ended gets an empty slot: no code.
#[derive(Debug, Default)]
struct Connections {
    /// The marks know each connection's channel (an embedded tunnel).
    per_connection: bool,
    accepted: u64,
    /// Accepted connections no verdict has named yet: generation and slot.
    unclaimed: HashMap<SocketAddr, (u64, Arc<ChannelSlot>)>,
    /// Verdicts whose connection the tunnel has not accepted yet: the count when the verdict
    /// was taken, and the slot the verdict holds.
    claims: HashMap<SocketAddr, (u64, std::sync::Weak<ChannelSlot>)>,
}

/// How the server answered the channel open of one local connection, if it refused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChannelRefusal {
    /// `SSH_OPEN_CONNECT_FAILED` (reason 2): nothing listens on the remote port.
    ConnectFailed,
    /// Another reason code. Only the embedded tunnel refuses with one.
    #[cfg_attr(not(any(test, feature = "embedded-ssh")), allow(dead_code))]
    Other(u32),
}

/// What became of the tunnel channel of one local connection. The tunnel writes the refusal
/// before it closes the local connection.
#[derive(Debug, Default)]
pub(crate) struct ChannelSlot {
    refusal: Mutex<Option<ChannelRefusal>>,
}

impl ChannelSlot {
    #[cfg(any(test, feature = "embedded-ssh"))]
    pub(crate) fn refuse(&self, refusal: ChannelRefusal) {
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refusal);
    }

    fn refusal(&self) -> Option<ChannelRefusal> {
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether an I/O error kind means the connection ended without an answer.
fn is_dropped_kind(kind: io::ErrorKind) -> bool {
    use io::ErrorKind as K;
    matches!(
        kind,
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe
    )
}

/// Whether `error` or one of its `source()` causes is an [`io::Error`] for a dropped connection
/// (EOF, reset, abort, broken pipe). An `anyhow::Error` can be passed as `&*error`.
pub fn is_dropped_connection(error: &(dyn Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(e) = cause {
        if let Some(io) = e.downcast_ref::<io::Error>()
            && is_dropped_kind(io.kind())
        {
            return true;
        }
        cause = e.source();
    }
    false
}

/// Two signals decide that the listener is absent: the connection dropped, and ssh reported a
/// channel connect failure.
pub(crate) fn listener_absent(channel_connect_failed: bool, connection_dropped: bool) -> bool {
    channel_connect_failed && connection_dropped
}

/// The error for a connection that closed without an answer. [`is_dropped_connection`]
/// recognizes it.
pub fn connection_closed_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message.into())
}

/// Collects stderr bytes into lines and looks for the channel connect failure line.
#[cfg(feature = "system-ssh")]
struct LineScanner {
    line: Vec<u8>,
    /// A line longer than this loses its tail (`tunnel.stderr_max_line`). The lines to judge
    /// are short.
    max_line: usize,
    /// The current line lost bytes past `max_line`.
    truncated: bool,
    /// A `\r` was read and not yet kept: it ends the line if `\n` follows (CRLF), so it does
    /// not count toward the line's length.
    pending_cr: bool,
}

#[cfg(feature = "system-ssh")]
impl LineScanner {
    fn new(max_line: usize) -> Self {
        Self {
            line: Vec::new(),
            max_line,
            truncated: false,
            pending_cr: false,
        }
    }

    fn feed(&mut self, bytes: &[u8], seen: &StderrMarks) {
        for &b in bytes {
            if b == b'\n' {
                self.finish_line(seen);
                continue;
            }
            if std::mem::take(&mut self.pending_cr) {
                self.keep(b'\r');
            }
            if b == b'\r' {
                self.pending_cr = true;
            } else {
                self.keep(b);
            }
        }
    }

    fn keep(&mut self, b: u8) {
        if self.line.len() < self.max_line {
            self.line.push(b);
        } else {
            self.truncated = true;
        }
    }

    fn finish_line(&mut self, seen: &StderrMarks) {
        let line = String::from_utf8_lossy(&self.line);
        if is_channel_connect_failure(&line) {
            seen.record_connect_failure();
        }
        // A cut line could end right after `forkafterauthentication` while ssh wrote a longer
        // name; the exact-name check needs the whole line.
        if !self.truncated && is_fork_option_refusal(&line) {
            seen.fork_option_refused.store(true, Ordering::Release);
        }
        self.line.clear();
        self.truncated = false;
        self.pending_cr = false;
    }
}

/// Passes the tunnel ssh's stderr on unchanged and marks the channel connect failure line. It
/// reads until stderr closes (ssh exits). Reading goes on even if passing on fails; if it
/// stopped, ssh would block on the pipe. Lines are judged up to `max_line` bytes.
#[cfg(feature = "system-ssh")]
pub(crate) fn relay_stderr(
    mut from: impl Read,
    mut to: impl Write,
    seen: &StderrMarks,
    max_line: usize,
) {
    let mut buf = [0u8; 4096];
    let mut scanner = LineScanner::new(max_line);
    let mut relaying = true;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!(%e, "ssh tunnel stderr read ended");
                break;
            }
        };
        if relaying && let Err(e) = to.write_all(&buf[..n]).and_then(|()| to.flush()) {
            tracing::debug!(%e, "ssh tunnel stderr relay stopped");
            relaying = false;
        }
        scanner.feed(&buf[..n], seen);
    }
    scanner.finish_line(seen);
}

/// If the failure is a dropped connection, waits up to the grace for a failure line after
/// `baseline` and decides.
fn judge_after_failure(
    marks: &StderrMarks,
    baseline: u64,
    failure: &(dyn Error + 'static),
    timing: VerdictTiming,
) -> bool {
    let dropped = is_dropped_connection(failure);
    if !dropped {
        return false;
    }
    let refused = refused_until(marks, baseline, Instant::now() + timing.grace, timing.poll);
    listener_absent(refused, dropped)
}

/// Whether a failure line after `baseline` was seen by `deadline`, looking every `poll` (cut to
/// the time left). Only a count seen at a moment not after `deadline` counts: a line that comes
/// right after the deadline, even before the last look, does not.
fn refused_until(marks: &StderrMarks, baseline: u64, deadline: Instant, poll: Duration) -> bool {
    loop {
        // The count first, then the time: a count read before a time not after the deadline was
        // there by the deadline.
        let seen = marks.connect_failures();
        let now = Instant::now();
        if now <= deadline && seen > baseline {
            return true;
        }
        let left = deadline.saturating_duration_since(now);
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(poll.min(left));
    }
}

/// Judges the failure of one channel through a tunnel.
///
/// Take it with `SshTunnel::listener_verdict` right before opening the channel. It counts only
/// the channel connect failure lines ssh writes after that moment, so an earlier refusal on the
/// same tunnel (the ready probe, another channel, or a time when the remote listener was down)
/// does not make a later channel's drop look like an absent listener. ssh's stderr does not say
/// which local connection a line belongs to, so a refusal of another channel opened at the same
/// time still counts. The embedded backend knows each local connection's channel: once
/// [`ListenerVerdict::for_local`] names the connection, both the absent listener (reason 2) and
/// another refusal code are judged by that connection's channel alone.
#[derive(Clone, Debug)]
pub struct ListenerVerdict {
    remote_port: u16,
    marks: Arc<StderrMarks>,
    /// Failure lines seen before this verdict was taken.
    baseline: u64,
    /// Local connections the tunnel had accepted before this verdict was taken.
    accepted: u64,
    /// The channel of the local connection the verdict judges ([`Self::for_local`], embedded
    /// tunnels only).
    slot: Option<Arc<ChannelSlot>>,
    timing: VerdictTiming,
}

impl ListenerVerdict {
    pub(crate) fn new(remote_port: u16, marks: Arc<StderrMarks>, timing: VerdictTiming) -> Self {
        let baseline = marks.connect_failures();
        let accepted = marks.accepted();
        Self {
            remote_port,
            marks,
            baseline,
            accepted,
            slot: None,
            timing,
        }
    }

    /// The entries the tunnel's marks keep ([`StderrMarks::kept`]).
    #[cfg(all(test, feature = "embedded-ssh"))]
    pub(crate) fn kept(&self) -> usize {
        self.marks.kept()
    }

    /// The verdict for the local connection from `local` (the local address of the socket
    /// connected to the tunnel's forwarding end), made right after the connection: the first
    /// connection from that address the tunnel accepts after the verdict was taken. On an
    /// embedded tunnel only that connection's channel gives a code
    /// ([`Self::channel_open_refused`]) or an absent listener; a later connection from the same
    /// address is another one. Name one connection per verdict.
    #[must_use]
    pub fn for_local(mut self, local: SocketAddr) -> Self {
        self.slot = self.marks.claim(local, self.accepted);
        self
    }

    /// A handle with a fixed observation: whether ssh reported a channel connect failure after
    /// the verdict was taken. For testing code that uses the verdict without a tunnel. It waits
    /// with the default `tunnel.verdict_grace`.
    pub fn with_observation(remote_port: u16, channel_connect_failed: bool) -> Self {
        let marks = Arc::new(StderrMarks::default());
        let timing = VerdictTiming::new(&SshSettings::default());
        let mut verdict = Self::new(remote_port, Arc::clone(&marks), timing);
        if channel_connect_failed {
            marks.record_connect_failure();
            verdict.baseline = 0;
        }
        verdict
    }

    /// Whether a request through the tunnel failed with `failure` because nothing listens on the
    /// remote port. The connection must have dropped without an answer and ssh must have
    /// reported a channel connect failure since this verdict was taken. It waits up to
    /// `tunnel.verdict_grace` of the tunnel's launcher (0.5 s by default) for that line.
    ///
    /// On an embedded tunnel a verdict that names its connection ([`Self::for_local`]) is judged
    /// by that connection's channel alone, without waiting: the server refused it with reason 2
    /// (`SSH_OPEN_CONNECT_FAILED`).
    pub fn remote_listener_absent(&self, failure: &(dyn Error + 'static)) -> bool {
        if let Some(slot) = &self.slot {
            return is_dropped_connection(failure)
                && slot.refusal() == Some(ChannelRefusal::ConnectFailed);
        }
        judge_after_failure(&self.marks, self.baseline, failure, self.timing)
    }

    /// Whether a request through the tunnel failed with `failure` because the server refused to
    /// open its channel for another reason than a failed connect (docs/design/ssh-backends.md
    /// "내장 백엔드: 세션 하나에 채널 여럿"): the connection must have dropped, and the server
    /// must have refused the channel of the local connection the verdict names
    /// ([`Self::for_local`]). A refusal of another connection on the same tunnel, or a drop
    /// after the channel opened, gives none. The embedded backend records the refusal before
    /// it closes the local connection, so this does not wait. The system backend's `ssh`
    /// reports no such code, so its verdicts never give one.
    pub fn channel_open_refused(
        &self,
        failure: &(dyn Error + 'static),
    ) -> Option<ChannelOpenRefused> {
        if !is_dropped_connection(failure) {
            return None;
        }
        match self.slot.as_ref()?.refusal()? {
            ChannelRefusal::Other(code) => Some(ChannelOpenRefused { code }),
            ChannelRefusal::ConnectFailed => None,
        }
    }

    /// [`RemoteListenerAbsent`] if the listener is absent ([`Self::remote_listener_absent`]).
    /// The caller attaches it to its own error.
    pub fn explain_failure(&self, failure: &(dyn Error + 'static)) -> Option<RemoteListenerAbsent> {
        self.remote_listener_absent(failure)
            .then_some(RemoteListenerAbsent {
                remote_port: self.remote_port,
            })
    }
}

/// The SSH server refused to open the tunnel channel (`SSH_MSG_CHANNEL_OPEN_FAILURE`) with a
/// reason other than a failed connect: 1 `ADMINISTRATIVELY_PROHIBITED`, 3
/// `UNKNOWN_CHANNEL_TYPE`, 4 `RESOURCE_SHORTAGE` (such as sshd `MaxSessions`), or another code.
/// The session stays up; only that channel failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelOpenRefused {
    /// The reason code of the refusal (RFC 4254 5.1).
    pub code: u32,
}

impl fmt::Display for ChannelOpenRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The SSH server refused to open the tunnel channel (reason code {}).",
            self.code
        )
    }
}

impl Error for ChannelOpenRefused {}

/// Nothing listens on the remote port. The message names the likely causes (the remote
/// instance is not running, or its record is stale).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteListenerAbsent {
    pub remote_port: u16,
}

impl fmt::Display for RemoteListenerAbsent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Nothing is listening on remote port {}; the remote instance is not running, or its port file is stale.",
            self.remote_port
        )
    }
}

impl Error for RemoteListenerAbsent {}

// These tests drive the system backend's helpers; the default build runs them.
#[cfg(all(test, feature = "system-ssh"))]
mod tests {
    use super::*;

    /// The default `tunnel.stderr_max_line`.
    const MAX_LINE: usize = 1024;

    fn defaults() -> SshSettings {
        SshSettings::default()
    }

    // The functions below run with the default settings.

    fn relay_stderr(from: impl Read, to: impl Write, seen: &StderrMarks) {
        super::relay_stderr(from, to, seen, defaults().stderr_max_line());
    }

    fn judge_after_failure(
        marks: &StderrMarks,
        baseline: u64,
        failure: &(dyn Error + 'static),
    ) -> bool {
        super::judge_after_failure(marks, baseline, failure, VerdictTiming::new(&defaults()))
    }

    /// Marks with `n` failure lines.
    fn marks_with(n: u64) -> StderrMarks {
        let marks = StderrMarks::default();
        for _ in 0..n {
            marks.record_connect_failure();
        }
        marks
    }

    fn verdict(remote_port: u16, marks: Arc<StderrMarks>) -> ListenerVerdict {
        ListenerVerdict::new(remote_port, marks, VerdictTiming::new(&defaults()))
    }

    #[test]
    fn the_test_line_limit_is_the_default() {
        assert_eq!(defaults().stderr_max_line(), MAX_LINE);
    }

    /// An error with a cause, like a context layer of `anyhow`.
    #[derive(Debug)]
    struct Context(&'static str, Box<dyn Error + 'static>);

    impl fmt::Display for Context {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Context {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.1.as_ref())
        }
    }

    #[derive(Debug)]
    struct Plain;

    impl fmt::Display for Plain {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("IPC error response (code=-32601)")
        }
    }

    impl Error for Plain {}

    #[test]
    fn the_client_side_part_of_the_open_failure_line_is_matched() {
        for line in [
            "channel 1: open failed: connect failed: Connection refused",
            "channel 2: open failed: connect failed: Connection refused\r",
            "channel 3: open failed: connect failed: No connection could be made",
            "channel 4: open failed: connect failed: ",
        ] {
            assert!(is_channel_connect_failure(line), "{line}");
        }
        for line in [
            "channel 1: open failed: administratively prohibited: open failed",
            "Warning: Permanently added 'host' to the list of known hosts.",
            "connect failed: Connection refused",
            "",
        ] {
            assert!(!is_channel_connect_failure(line), "{line}");
        }
    }

    #[test]
    fn dropped_connections_include_eof_reset_abort_and_broken_pipe() {
        use io::ErrorKind as K;
        for kind in [
            K::UnexpectedEof,
            K::ConnectionReset,
            K::ConnectionAborted,
            K::BrokenPipe,
        ] {
            let e = Context("read failed", Box::new(io::Error::from(kind)));
            assert!(is_dropped_connection(&e), "{kind:?}");
            assert!(is_dropped_connection(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [K::TimedOut, K::WouldBlock, K::ConnectionRefused] {
            let e = Context("read failed", Box::new(io::Error::from(kind)));
            assert!(!is_dropped_connection(&e), "{kind:?}");
        }
        assert!(is_dropped_connection(&connection_closed_error("eof")));
        assert!(!is_dropped_connection(&Plain));
    }

    #[test]
    fn a_listener_is_absent_only_with_both_signals() {
        assert!(listener_absent(true, true));
        assert!(!listener_absent(true, false));
        assert!(!listener_absent(false, true));
        assert!(!listener_absent(false, false));
    }

    /// The bytes pass unchanged, and a line split over reads is still recognized.
    #[test]
    fn the_relay_forwards_everything_and_marks_a_split_failure_line() {
        struct Chunks(Vec<&'static [u8]>);
        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                buf[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }
        let input: Vec<&'static [u8]> = vec![
            b"Warning: added host\nchannel 1: open fa",
            b"iled: connect failed: Connection refused\n",
        ];
        let expected: Vec<u8> = input.concat();
        let seen = StderrMarks::default();
        let mut out = Vec::new();
        relay_stderr(Chunks(input), &mut out, &seen);
        assert_eq!(out, expected);
        assert_eq!(seen.connect_failures(), 1);
        assert!(!seen.fork_option_refused.load(Ordering::Acquire));

        let seen = StderrMarks::default();
        let mut out = Vec::new();
        relay_stderr(
            Chunks(vec![b"Warning: added host\n" as &[u8]]),
            &mut out,
            &seen,
        );
        assert_eq!(seen.connect_failures(), 0);

        let seen = StderrMarks::default();
        relay_stderr(
            Chunks(vec![
                b"command-line: line 0: Bad configuration option: forkafterauthentication\n"
                    as &[u8],
            ]),
            &mut Vec::new(),
            &seen,
        );
        assert!(seen.fork_option_refused.load(Ordering::Acquire));
        assert_eq!(seen.connect_failures(), 0);
        assert!(!is_fork_option_refusal("Bad configuration option: foo"));
        assert!(is_fork_option_refusal(
            "command-line: line 0: Bad configuration option: forkafterauthentication\r"
        ));
        assert!(!is_fork_option_refusal(
            "command-line: line 0: Bad configuration option: forkafterauthenticationextra"
        ));
        assert!(!is_fork_option_refusal(
            "Bad configuration option: forkafterauthentication_x"
        ));
    }

    /// A refusal that ends exactly at the line limit is judged; one whose option name goes on
    /// past the limit is not, since the cut would hide the rest of the name.
    #[test]
    fn a_fork_refusal_cut_at_the_line_limit_is_not_judged() {
        const DIAG: &str =
            "command-line: line 0: Bad configuration option: forkafterauthentication";
        let full_line = format!("{}{DIAG}", "x".repeat(MAX_LINE - DIAG.len()));
        assert_eq!(full_line.len(), MAX_LINE);
        let judge = |stderr: String| {
            let seen = StderrMarks::default();
            relay_stderr(io::Cursor::new(stderr.into_bytes()), io::sink(), &seen);
            seen.fork_option_refused.load(Ordering::Acquire)
        };
        assert!(judge(format!("{full_line}\n")), "ends at the limit");
        assert!(judge(full_line.clone()), "ends at the limit and at EOF");
        assert!(
            judge(format!("{full_line}\r\n")),
            "ends at the limit before CRLF"
        );
        assert!(
            judge(format!("{full_line}\r")),
            "ends at the limit before CR and EOF"
        );
        // A CR inside a line is data: past the limit it cuts the line.
        assert!(
            !judge(format!("{full_line}\rx\n")),
            "a CR in the line past the limit"
        );
        assert!(
            !judge(format!("{full_line}extra\n")),
            "name goes on past the limit"
        );
        // The next line is judged on its own again.
        assert!(judge(format!("{full_line}extra\n{DIAG}\n")));
    }

    #[test]
    fn the_verdict_needs_a_dropped_connection_and_the_marked_line() {
        let marked = marks_with(1);
        let unmarked = marks_with(0);
        let dropped = connection_closed_error("eof");
        assert!(judge_after_failure(&marked, 0, &dropped));
        assert!(!judge_after_failure(&marked, 0, &Plain));
        assert!(!judge_after_failure(&unmarked, 0, &dropped));
        // A line from before the verdict does not count.
        assert!(!judge_after_failure(&marked, 1, &dropped));
    }

    #[test]
    fn a_verdict_counts_only_refusals_after_it_was_taken() {
        let marks = Arc::new(marks_with(3));
        let dropped = connection_closed_error("eof");
        let later = verdict(80, Arc::clone(&marks));
        assert!(!later.remote_listener_absent(&dropped), "old refusals only");
        marks.record_connect_failure();
        assert!(later.remote_listener_absent(&dropped));
        let after = verdict(80, marks);
        assert!(!after.remote_listener_absent(&dropped));
    }

    #[test]
    fn a_verdict_handle_explains_only_an_absent_listener() {
        let dropped = Context("request failed", Box::new(connection_closed_error("eof")));
        let absent = ListenerVerdict::with_observation(4321, true).explain_failure(&dropped);
        assert_eq!(absent, Some(RemoteListenerAbsent { remote_port: 4321 }));
        let text = absent.map(|a| a.to_string()).unwrap_or_default();
        assert!(text.contains("4321") && text.is_ascii(), "{text}");
        assert_eq!(
            ListenerVerdict::with_observation(4321, false).explain_failure(&dropped),
            None
        );
    }

    /// With a poll of 1 s and a grace of 100 ms the verdict waits about the grace, not the poll.
    #[test]
    fn the_verdict_waits_the_grace_even_with_a_longer_poll() {
        let timing = VerdictTiming {
            grace: Duration::from_millis(100),
            poll: Duration::from_secs(1),
        };
        let marks = marks_with(0);
        let dropped = connection_closed_error("eof");
        let started = Instant::now();
        assert!(!super::judge_after_failure(&marks, 0, &dropped, timing));
        let took = started.elapsed();
        assert!(took >= timing.grace, "{took:?}");
        assert!(took < Duration::from_millis(600), "{took:?}");
    }

    /// A line recorded after the deadline does not count, even when it is there at the first
    /// look. The deadline is well in the past when the line comes, so this does not depend on
    /// how fine the clock is.
    #[test]
    fn a_line_after_the_deadline_does_not_count() {
        let poll = Duration::from_millis(20);
        let marks = marks_with(0);
        let deadline = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        marks.record_connect_failure();
        assert!(!refused_until(&marks, 0, deadline, poll));
        // The same line counts for a deadline still ahead, and for none that it was there before.
        assert!(refused_until(
            &marks,
            0,
            Instant::now() + Duration::from_millis(100),
            poll
        ));
        assert!(!refused_until(
            &marks,
            1,
            Instant::now() + Duration::from_millis(30),
            poll
        ));
    }

    /// A line that comes while the verdict waits counts.
    #[test]
    fn a_line_during_the_grace_counts() {
        let marks = Arc::new(marks_with(0));
        let relay = {
            let marks = Arc::clone(&marks);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                marks.record_connect_failure();
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(refused_until(&marks, 0, deadline, Duration::from_millis(5)));
        relay.join().expect("relay thread");
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    fn verdict(marks: &Arc<StderrMarks>) -> ListenerVerdict {
        ListenerVerdict::new(
            4100,
            Arc::clone(marks),
            VerdictTiming::new(&SshSettings::default()),
        )
    }

    #[test]
    fn a_refused_channel_gives_its_code_only_to_the_verdict_of_its_connection() {
        let marks = Arc::new(StderrMarks::per_connection());
        let local = SocketAddr::from(([127, 0, 0, 1], 50_001));
        let other_local = SocketAddr::from(([127, 0, 0, 1], 50_002));
        let mine = verdict(&marks);
        let other = verdict(&marks);
        let other_slot = marks.accept(other_local);
        let my_slot = marks.accept(local);
        let mine = mine.for_local(local);
        let _other = other.for_local(other_local);
        let dropped = connection_closed_error("closed");
        assert_eq!(mine.channel_open_refused(&dropped), None);
        other_slot.refuse(ChannelRefusal::Other(1));
        // Another connection's refusal is not this one's.
        assert_eq!(mine.channel_open_refused(&dropped), None);
        my_slot.refuse(ChannelRefusal::Other(4));
        assert_eq!(
            mine.channel_open_refused(&dropped),
            Some(ChannelOpenRefused { code: 4 })
        );
        // A failure that is not a dropped connection is not explained by the refusal.
        let failure = io::Error::other("another failure");
        assert_eq!(mine.channel_open_refused(&failure), None);
        // A refusal is not an absent listener.
        assert!(!mine.remote_listener_absent(&dropped));
        // A verdict that names no connection gives no code.
        assert_eq!(verdict(&marks).channel_open_refused(&dropped), None);
    }

    /// Two connections from the same address, one after the other: each verdict keeps its
    /// own connection's result, whichever comes first, the accept or the naming.
    #[test]
    fn two_connections_from_one_address_keep_their_own_results() {
        let marks = Arc::new(StderrMarks::per_connection());
        let local = SocketAddr::from(([127, 0, 0, 1], 50_003));
        let dropped = connection_closed_error("closed");
        // The first is accepted before its verdict names it.
        let first = verdict(&marks);
        let first_slot = marks.accept(local);
        let first = first.for_local(local);
        first_slot.refuse(ChannelRefusal::Other(1));
        // The second, from the same address once the first closed, is named first.
        let second = verdict(&marks).for_local(local);
        let second_slot = marks.accept(local);
        second_slot.refuse(ChannelRefusal::Other(4));
        assert_eq!(
            first.channel_open_refused(&dropped),
            Some(ChannelOpenRefused { code: 1 })
        );
        assert_eq!(
            second.channel_open_refused(&dropped),
            Some(ChannelOpenRefused { code: 4 })
        );
        // A third that opens: neither old result is its.
        let third = verdict(&marks).for_local(local);
        let _third_slot = marks.accept(local);
        assert_eq!(third.channel_open_refused(&dropped), None);
        assert!(!third.remote_listener_absent(&dropped));
    }

    /// Reason 2 of another connection on the same embedded tunnel is no absent listener for a
    /// connection whose channel opened.
    #[test]
    fn a_connect_failure_of_another_connection_is_not_this_ones() {
        let marks = Arc::new(StderrMarks::per_connection());
        let opened = SocketAddr::from(([127, 0, 0, 1], 50_004));
        let refused = SocketAddr::from(([127, 0, 0, 1], 50_005));
        let dropped = connection_closed_error("closed");
        let a = verdict(&marks);
        let b = verdict(&marks);
        let _a_slot = marks.accept(opened);
        let b_slot = marks.accept(refused);
        let (a, b) = (a.for_local(opened), b.for_local(refused));
        b_slot.refuse(ChannelRefusal::ConnectFailed);
        marks.record_connect_failure();
        assert!(!a.remote_listener_absent(&dropped));
        assert!(b.remote_listener_absent(&dropped));
        // Reason 2 is the absent listener, not a refused channel.
        assert_eq!(b.channel_open_refused(&dropped), None);
        // A verdict that names no connection still counts the failure since it was taken.
        let unnamed = verdict(&marks);
        assert!(!unnamed.remote_listener_absent(&dropped));
    }

    /// A connection that ended leaves nothing: its slot goes when no verdict took it, a verdict
    /// that names it afterwards knows nothing of it, and a claim whose verdict is gone is
    /// dropped.
    #[test]
    fn an_ended_connection_leaves_no_slot() {
        let marks = Arc::new(StderrMarks::per_connection());
        let dropped = connection_closed_error("closed");
        for port in 50_100..50_150 {
            let local = SocketAddr::from(([127, 0, 0, 1], port));
            let slot = marks.accept(local);
            slot.refuse(ChannelRefusal::Other(1));
            marks.release(local, &slot);
        }
        assert_eq!(marks.kept(), 0);
        // Named after its connection ended: no code.
        let local = SocketAddr::from(([127, 0, 0, 1], 50_200));
        let late = verdict(&marks);
        let slot = marks.accept(local);
        slot.refuse(ChannelRefusal::Other(4));
        marks.release(local, &slot);
        let late = late.for_local(local);
        assert_eq!(late.channel_open_refused(&dropped), None);
        // A slot a verdict took is not dropped by the release.
        let local = SocketAddr::from(([127, 0, 0, 1], 50_201));
        let mine = verdict(&marks);
        let slot = marks.accept(local);
        let mine = mine.for_local(local);
        slot.refuse(ChannelRefusal::Other(3));
        marks.release(local, &slot);
        assert_eq!(
            mine.channel_open_refused(&dropped),
            Some(ChannelOpenRefused { code: 3 })
        );
        // The claim of the late verdict above waits for an accept; once the verdict is gone it
        // is dropped.
        drop(late);
        let other = SocketAddr::from(([127, 0, 0, 1], 50_202));
        let slot = marks.accept(other);
        marks.release(other, &slot);
        assert_eq!(marks.kept(), 0);
    }
}
