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

use std::error::Error;
use std::fmt;
use std::io;
#[cfg(feature = "system-ssh")]
use std::io::{Read, Write};
use std::sync::Arc;
#[cfg(feature = "system-ssh")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// time still counts.
#[derive(Clone, Debug)]
pub struct ListenerVerdict {
    remote_port: u16,
    marks: Arc<StderrMarks>,
    /// Failure lines seen before this verdict was taken.
    baseline: u64,
    timing: VerdictTiming,
}

impl ListenerVerdict {
    pub(crate) fn new(remote_port: u16, marks: Arc<StderrMarks>, timing: VerdictTiming) -> Self {
        let baseline = marks.connect_failures();
        Self {
            remote_port,
            marks,
            baseline,
            timing,
        }
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
    pub fn remote_listener_absent(&self, failure: &(dyn Error + 'static)) -> bool {
        judge_after_failure(&self.marks, self.baseline, failure, self.timing)
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
