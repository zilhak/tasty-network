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
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long to wait after a failure for the stderr line. ssh writes the refusal line and then
/// closes the local connection, but the caller can see the failure before the relay thread has
/// handled the line.
const VERDICT_GRACE: Duration = Duration::from_millis(500);

/// Whether an ssh stderr line reports a channel that could not open because the connect to the
/// remote port failed.
pub(crate) fn is_channel_connect_failure(line: &str) -> bool {
    line.trim_start().starts_with("channel ") && line.contains("open failed: connect failed")
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
#[derive(Default)]
struct LineScanner {
    line: Vec<u8>,
}

impl LineScanner {
    /// A line longer than this loses its tail. The lines to judge are short.
    const MAX_LINE: usize = 1024;

    fn feed(&mut self, bytes: &[u8], seen: &AtomicBool) {
        for &b in bytes {
            if b == b'\n' {
                self.finish_line(seen);
            } else if self.line.len() < Self::MAX_LINE {
                self.line.push(b);
            }
        }
    }

    fn finish_line(&mut self, seen: &AtomicBool) {
        if is_channel_connect_failure(&String::from_utf8_lossy(&self.line)) {
            seen.store(true, Ordering::Release);
        }
        self.line.clear();
    }
}

/// Passes the tunnel ssh's stderr on unchanged and marks the channel connect failure line. It
/// reads until stderr closes (ssh exits). Reading goes on even if passing on fails; if it
/// stopped, ssh would block on the pipe.
pub(crate) fn relay_stderr(mut from: impl Read, mut to: impl Write, seen: &AtomicBool) {
    let mut buf = [0u8; 4096];
    let mut scanner = LineScanner::default();
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

/// If the failure is a dropped connection, waits briefly for the stderr mark and decides.
fn judge_after_failure(seen: &AtomicBool, failure: &(dyn Error + 'static)) -> bool {
    let dropped = is_dropped_connection(failure);
    if !dropped {
        return false;
    }
    let deadline = Instant::now() + VERDICT_GRACE;
    loop {
        let refused = seen.load(Ordering::Acquire);
        if refused || Instant::now() >= deadline {
            return listener_absent(refused, dropped);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Judges failures of a tunnel after the tunnel moved elsewhere (a connection worker and so on).
/// It reads the tunnel's channel connect failure mark, so take it with
/// `SshTunnel::listener_verdict` before handing the tunnel over.
#[derive(Clone, Debug)]
pub struct ListenerVerdict {
    remote_port: u16,
    channel_connect_failed: Arc<AtomicBool>,
}

impl ListenerVerdict {
    pub(crate) fn new(remote_port: u16, channel_connect_failed: Arc<AtomicBool>) -> Self {
        Self {
            remote_port,
            channel_connect_failed,
        }
    }

    /// A handle with a fixed observation of the ssh stderr. For testing code that uses the
    /// verdict without a tunnel.
    pub fn with_observation(remote_port: u16, channel_connect_failed: bool) -> Self {
        Self::new(
            remote_port,
            Arc::new(AtomicBool::new(channel_connect_failed)),
        )
    }

    /// Whether a request through the tunnel failed with `failure` because nothing listens on the
    /// remote port. The connection must have dropped without an answer and ssh must have
    /// reported a channel connect failure. It waits up to 0.5 s for that line.
    pub fn remote_listener_absent(&self, failure: &(dyn Error + 'static)) -> bool {
        judge_after_failure(&self.channel_connect_failed, failure)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let seen = AtomicBool::new(false);
        let mut out = Vec::new();
        relay_stderr(Chunks(input), &mut out, &seen);
        assert_eq!(out, expected);
        assert!(seen.load(Ordering::Acquire));

        let seen = AtomicBool::new(false);
        let mut out = Vec::new();
        relay_stderr(
            Chunks(vec![b"Warning: added host\n" as &[u8]]),
            &mut out,
            &seen,
        );
        assert!(!seen.load(Ordering::Acquire));
    }

    #[test]
    fn the_verdict_needs_a_dropped_connection_and_the_marked_line() {
        let marked = AtomicBool::new(true);
        let unmarked = AtomicBool::new(false);
        let dropped = connection_closed_error("eof");
        assert!(judge_after_failure(&marked, &dropped));
        assert!(!judge_after_failure(&marked, &Plain));
        assert!(!judge_after_failure(&unmarked, &dropped));
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
}
