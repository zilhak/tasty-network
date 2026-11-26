//! Classification of stream read failures, so both ends name the cause of a disconnect the
//! same way.

use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

/// Why [`read_frame`](crate::read_frame) failed.
///
/// Errno values differ between operating systems, so the classification uses
/// [`io::ErrorKind`]. A transport that is not a socket (for example QUIC) maps its own
/// errors onto the same kinds, and a consumer classifies them without knowing the
/// transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// The read timeout expired. macOS and Linux report it as `WouldBlock` (EAGAIN),
    /// Windows as `TimedOut`. It can expire while a frame is partly received, so it does
    /// not mean that no frame arrived at all.
    HeartbeatTimeout,
    /// The peer closed (EOF) or reset the connection.
    PeerClosed,
    /// The bytes do not form a frame, such as an unknown tag or a length over the limit.
    Protocol,
    /// Any other receive error.
    Other,
}

impl ReadFailure {
    /// Classifies an I/O error by its kind.
    ///
    /// | kind | class |
    /// |---|---|
    /// | `WouldBlock`, `TimedOut` | [`HeartbeatTimeout`](Self::HeartbeatTimeout) |
    /// | `UnexpectedEof`, `ConnectionReset`, `ConnectionAborted` | [`PeerClosed`](Self::PeerClosed) |
    /// | `InvalidData` | [`Protocol`](Self::Protocol) |
    /// | anything else | [`Other`](Self::Other) |
    pub fn of_io(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Self::HeartbeatTimeout,
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted => Self::PeerClosed,
            io::ErrorKind::InvalidData => Self::Protocol,
            _ => Self::Other,
        }
    }

    /// Classifies any error by the first [`io::Error`] in its `source()` chain, starting
    /// with `error` itself. Without an `io::Error` in the chain the class is
    /// [`Other`](Self::Other).
    ///
    /// An `anyhow::Error` can be passed as `err.as_ref()`.
    pub fn of_error(error: &(dyn Error + 'static)) -> Self {
        let mut current = Some(error);
        while let Some(e) = current {
            if let Some(io) = e.downcast_ref::<io::Error>() {
                return Self::of_io(io);
            }
            current = e.source();
        }
        Self::Other
    }

    /// The cause as one English sentence for a log line. `timeout` is the read timeout
    /// that was actually set on this stream; only the timeout cause mentions it.
    ///
    /// The timeout is written exactly, in the largest unit that has no fraction: `30 s`,
    /// `250 ms`, `1500 ms`, `750 us`, `10 ns`.
    pub fn describe(self, timeout: Duration) -> String {
        match self {
            Self::HeartbeatTimeout => format!(
                "heartbeat timeout: no complete frame arrived within the {} read timeout",
                exact_duration(timeout)
            ),
            _ => self.to_string(),
        }
    }
}

/// Writes `d` without losing precision, in the largest unit that divides it.
fn exact_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos.is_multiple_of(1_000_000_000) {
        format!("{} s", nanos / 1_000_000_000)
    } else if nanos.is_multiple_of(1_000_000) {
        format!("{} ms", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        format!("{} us", nanos / 1_000)
    } else {
        format!("{nanos} ns")
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HeartbeatTimeout => "heartbeat timeout: no complete frame arrived in time",
            Self::PeerClosed => "the peer closed the connection",
            Self::Protocol => "protocol error: the bytes do not form a valid frame",
            Self::Other => "other receive error",
        })
    }
}

impl Error for ReadFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps another error and exposes it as `source()`, like `anyhow` context does.
    #[derive(Debug)]
    struct Wrapped(Box<dyn Error + Send + Sync>);

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapped")
        }
    }

    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    #[derive(Debug)]
    struct NotIo;

    impl fmt::Display for NotIo {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("expected Control ack frame")
        }
    }

    impl Error for NotIo {}

    #[test]
    fn each_read_error_kind_maps_to_its_cause() {
        let cases = [
            (io::ErrorKind::WouldBlock, ReadFailure::HeartbeatTimeout),
            (io::ErrorKind::TimedOut, ReadFailure::HeartbeatTimeout),
            (io::ErrorKind::UnexpectedEof, ReadFailure::PeerClosed),
            (io::ErrorKind::ConnectionReset, ReadFailure::PeerClosed),
            (io::ErrorKind::ConnectionAborted, ReadFailure::PeerClosed),
            (io::ErrorKind::InvalidData, ReadFailure::Protocol),
            (io::ErrorKind::PermissionDenied, ReadFailure::Other),
            (io::ErrorKind::InvalidInput, ReadFailure::Other),
        ];
        for (kind, expected) in cases {
            let error = io::Error::from(kind);
            assert_eq!(ReadFailure::of_io(&error), expected, "{kind:?}");
            assert_eq!(
                ReadFailure::of_error(&error),
                expected,
                "{kind:?} as dyn Error"
            );
            let wrapped = Wrapped(Box::new(io::Error::from(kind)));
            assert_eq!(
                ReadFailure::of_error(&wrapped),
                expected,
                "{kind:?} wrapped"
            );
            let twice = Wrapped(Box::new(Wrapped(Box::new(io::Error::from(kind)))));
            assert_eq!(
                ReadFailure::of_error(&twice),
                expected,
                "{kind:?} wrapped twice"
            );
        }
    }

    /// A `dyn Error + Send + Sync` (what `anyhow::Error::as_ref` gives) is accepted.
    #[test]
    fn a_send_sync_error_object_is_accepted() {
        let boxed: Box<dyn Error + Send + Sync> =
            Box::new(io::Error::from(io::ErrorKind::TimedOut));
        assert_eq!(
            ReadFailure::of_error(boxed.as_ref()),
            ReadFailure::HeartbeatTimeout
        );
    }

    #[test]
    fn read_failure_is_a_std_error() {
        let err: Box<dyn Error + Send + Sync> = Box::new(ReadFailure::PeerClosed);
        assert_eq!(err.to_string(), "the peer closed the connection");
        assert!(err.source().is_none());
    }

    #[test]
    fn a_non_io_error_is_other() {
        assert_eq!(ReadFailure::of_error(&NotIo), ReadFailure::Other);
        assert_eq!(
            ReadFailure::of_error(&Wrapped(Box::new(NotIo))),
            ReadFailure::Other
        );
    }

    #[test]
    fn the_timeout_cause_states_the_armed_seconds() {
        let text = ReadFailure::HeartbeatTimeout.describe(Duration::from_secs(45));
        assert!(text.contains("within the 45 s read timeout"), "{text}");
        assert!(!text.contains("no frame"), "{text}");
    }

    /// A short per-link timeout (as tests use) must not read as 0 seconds.
    #[test]
    fn a_sub_second_timeout_is_stated_exactly() {
        let cases = [
            (Duration::from_millis(250), "within the 250 ms read timeout"),
            (
                Duration::from_millis(1500),
                "within the 1500 ms read timeout",
            ),
            (Duration::from_micros(750), "within the 750 us read timeout"),
            (Duration::from_nanos(10), "within the 10 ns read timeout"),
            (Duration::ZERO, "within the 0 s read timeout"),
        ];
        for (timeout, expected) in cases {
            let text = ReadFailure::HeartbeatTimeout.describe(timeout);
            assert!(text.contains(expected), "{timeout:?}: {text}");
        }
    }

    #[test]
    fn other_causes_describe_as_their_display() {
        for failure in [
            ReadFailure::PeerClosed,
            ReadFailure::Protocol,
            ReadFailure::Other,
        ] {
            assert_eq!(
                failure.describe(Duration::from_secs(1)),
                failure.to_string()
            );
        }
    }

    #[test]
    fn a_frame_with_an_unknown_tag_is_a_protocol_failure() {
        let bytes = [0xEEu8, 0, 0, 0, 0];
        let error = crate::read_frame(&mut std::io::Cursor::new(bytes)).unwrap_err();
        assert_eq!(ReadFailure::of_io(&error), ReadFailure::Protocol);
    }

    #[test]
    fn a_stream_that_ends_mid_header_is_a_peer_close() {
        let bytes = [0u8, 0];
        let error = crate::read_frame(&mut std::io::Cursor::new(bytes)).unwrap_err();
        assert_eq!(ReadFailure::of_io(&error), ReadFailure::PeerClosed);
    }

    /// A reader whose timeout expires reports `WouldBlock` (Unix) or `TimedOut`
    /// (Windows); both read as a heartbeat timeout whatever OS the test runs on.
    #[test]
    fn a_read_timeout_from_the_reader_is_a_heartbeat_timeout() {
        struct Expiring(io::ErrorKind);
        impl io::Read for Expiring {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(self.0))
            }
        }
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            let error = crate::read_frame(&mut Expiring(kind)).unwrap_err();
            assert_eq!(
                ReadFailure::of_io(&error),
                ReadFailure::HeartbeatTimeout,
                "{kind:?}"
            );
        }
    }
}
