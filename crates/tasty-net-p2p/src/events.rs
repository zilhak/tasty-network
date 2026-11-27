//! State machine transitions as events (docs/design/p2p.md "상태 기계").
//!
//! Every transition of the endpoint or of a connection attempt gives one event
//! `{from, to, cause, at}`. The queue keeps at most `events.queue_limit` events; when it is
//! full the oldest is dropped and the next read starts with one [`P2pEvent::Lagged`].

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use tasty_net_rendezvous::wire::message::CancelReason;

use crate::close::P2pCloseReason;
use crate::error::{P2pConnectError, StopCause};

/// States of the endpoint (its server registration).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndpointPhase {
    /// P2P is off; there is no socket.
    Off,
    /// Connecting to and registering on the server.
    Connecting,
    /// Registered: connections may be made and received.
    Registered,
    /// Waiting for the next reconnect.
    Backoff,
    /// Stopped without reconnecting.
    Stopped,
}

/// States of a connection attempt and its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttemptPhase {
    /// A: `punch_request` sent, waiting for `punch_go` or `punch_refused`.
    Requesting,
    /// B: `punch_offer` received and answered; waiting for `punch_go` after accepting.
    Offered,
    /// `punch_go` received, waiting for `start_after_ms`.
    Scheduled,
    /// Probes are sent and answered.
    Punching,
    /// The QUIC handshake.
    Handshaking,
    /// A: waiting for the next punch attempt.
    RetryWait,
    /// The QUIC connection is up.
    Connected,
    /// Ended.
    Closed,
}

impl AttemptPhase {
    /// The state name of the design table.
    pub fn name(self) -> &'static str {
        match self {
            AttemptPhase::Requesting => "Requesting",
            AttemptPhase::Offered => "Offered",
            AttemptPhase::Scheduled => "Scheduled",
            AttemptPhase::Punching => "Punching",
            AttemptPhase::Handshaking => "Handshaking",
            AttemptPhase::RetryWait => "RetryWait",
            AttemptPhase::Connected => "Connected",
            AttemptPhase::Closed => "Closed",
        }
    }
}

impl EndpointPhase {
    /// The state name of the design table.
    pub fn name(self) -> &'static str {
        match self {
            EndpointPhase::Off => "Off",
            EndpointPhase::Connecting => "Connecting",
            EndpointPhase::Registered => "Registered",
            EndpointPhase::Backoff => "Backoff",
            EndpointPhase::Stopped => "Stopped",
        }
    }
}

/// A state of either state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// An endpoint state.
    Endpoint(EndpointPhase),
    /// A connection attempt state.
    Attempt(AttemptPhase),
}

/// Which state machine an event is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    /// The endpoint.
    Endpoint,
    /// The connection attempt or connection with this ID ([`crate::P2pConnectionInfo::id`]).
    Connection(u64),
}

/// The values a cause carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CauseDetail {
    /// The connection attempt error.
    Connect(P2pConnectError),
    /// A device-to-device close code.
    Close(P2pCloseReason),
    /// A coordination connection close code.
    ServerCode(u64),
    /// An I/O failure, with its class (`malformed` for a frame or message the device refuses).
    Io {
        /// The class.
        kind: io::ErrorKind,
        /// What failed.
        what: String,
    },
    /// The stop cause.
    Stop(StopCause),
    /// The server's cancel reason.
    Cancel(CancelReason),
    /// An observed remote address change.
    Address {
        /// The address before.
        from: SocketAddr,
        /// The address now.
        to: SocketAddr,
    },
    /// A probe success or an incoming QUIC connection on this address.
    Peer(SocketAddr),
    /// A server value that does not fit a device setting.
    Limits {
        /// The device setting.
        key: &'static str,
        /// The device's value.
        device: String,
        /// The server's value.
        server: String,
    },
}

/// Why a transition happened: the cause name of the design tables and, for an error, its
/// value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cause {
    /// The cause name (`connect_called`, `punch_go`, `idle_timeout` and so on).
    pub name: &'static str,
    /// The value the cause carries.
    pub detail: Option<CauseDetail>,
}

impl Cause {
    /// A cause without a value.
    pub fn named(name: &'static str) -> Self {
        Cause { name, detail: None }
    }

    /// A cause with a value.
    pub fn with(name: &'static str, detail: CauseDetail) -> Self {
        Cause {
            name,
            detail: Some(detail),
        }
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            None => f.write_str(self.name),
            Some(d) => write!(f, "{} ({d:?})", self.name),
        }
    }
}

/// An event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum P2pEvent {
    /// A transition.
    Transition {
        /// Which state machine.
        subject: Subject,
        /// The state before; `None` when the machine starts.
        from: Option<Phase>,
        /// The state after.
        to: Phase,
        /// Why.
        cause: Cause,
        /// When.
        at: SystemTime,
    },
    /// Events were dropped because nobody read them.
    Lagged {
        /// How many.
        dropped: u64,
    },
}

/// The event queue.
#[derive(Debug)]
pub(crate) struct Events {
    state: Mutex<Queue>,
    ready: Condvar,
    limit: usize,
}

#[derive(Debug, Default)]
struct Queue {
    events: VecDeque<P2pEvent>,
    dropped: u64,
    /// The endpoint ended: no event comes any more.
    ended: bool,
}

/// [`Events::next`] found the queue empty after the endpoint ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ended;

impl Events {
    pub(crate) fn new(limit: usize) -> Self {
        Events {
            state: Mutex::default(),
            ready: Condvar::new(),
            limit,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, event: P2pEvent) {
        let mut q = self.lock();
        while q.events.len() >= self.limit {
            q.events.pop_front();
            q.dropped += 1;
        }
        q.events.push_back(event);
        self.ready.notify_all();
    }

    /// The endpoint ended after its last event: wake every waiter, and from now on an empty
    /// queue answers at once.
    pub(crate) fn end(&self) {
        self.lock().ended = true;
        self.ready.notify_all();
    }

    fn take_lagged(q: &mut Queue) -> Option<P2pEvent> {
        (q.dropped > u64::MIN).then(|| {
            let dropped = std::mem::take(&mut q.dropped);
            P2pEvent::Lagged { dropped }
        })
    }

    /// Every queued event, oldest first.
    pub(crate) fn drain(&self) -> Vec<P2pEvent> {
        let mut q = self.lock();
        let mut out: Vec<P2pEvent> = Self::take_lagged(&mut q).into_iter().collect();
        out.extend(q.events.drain(..));
        out
    }

    /// The oldest event, waiting up to `limit` for one. The queued events come first; once
    /// the queue is empty after [`Events::end`], [`Ended`] at once.
    pub(crate) fn next(&self, limit: Duration) -> Result<Option<P2pEvent>, Ended> {
        let until = Instant::now().checked_add(limit);
        let mut q = self.lock();
        loop {
            if let Some(lagged) = Self::take_lagged(&mut q) {
                return Ok(Some(lagged));
            }
            if let Some(e) = q.events.pop_front() {
                return Ok(Some(e));
            }
            if q.ended {
                return Err(Ended);
            }
            let Some(left) = until.and_then(|u| u.checked_duration_since(Instant::now())) else {
                return Ok(None);
            };
            q = self
                .ready
                .wait_timeout(q, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ev(n: u64) -> P2pEvent {
        P2pEvent::Transition {
            subject: Subject::Connection(n),
            from: None,
            to: Phase::Attempt(AttemptPhase::Requesting),
            cause: Cause::named("connect_called"),
            at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn full_queue_drops_the_oldest_and_reports_lag() {
        let q = Events::new(2);
        for n in 0..5 {
            q.push(ev(n));
        }
        assert_eq!(
            q.drain(),
            vec![P2pEvent::Lagged { dropped: 3 }, ev(3), ev(4)]
        );
        assert_eq!(q.drain(), vec![]);
        assert_eq!(q.next(Duration::from_millis(10)), Ok(None));
    }

    /// After the end the queued events still come first, then every caller, the waiting ones
    /// and the later ones, gets `Ended` at once.
    #[test]
    fn the_end_wakes_every_waiter_after_the_queued_events() {
        let q = Arc::new(Events::new(8));
        let long = Duration::from_secs(60);
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let q = Arc::clone(&q);
                std::thread::spawn(move || {
                    let mut got = Vec::new();
                    loop {
                        match q.next(long) {
                            Ok(Some(e)) => got.push(e),
                            Ok(None) => return (got, false),
                            Err(Ended) => return (got, true),
                        }
                    }
                })
            })
            .collect();
        let started = Instant::now();
        q.push(ev(1));
        q.end();
        let mut events = Vec::new();
        for w in waiters {
            let (got, ended) = w.join().expect("waiter");
            assert!(ended);
            events.extend(got);
        }
        assert_eq!(events, vec![ev(1)]);
        assert!(started.elapsed() < long);
        q.push(ev(2));
        assert_eq!(q.next(long), Ok(Some(ev(2))));
        assert_eq!(q.next(long), Err(Ended));
        assert!(started.elapsed() < long);
    }
}
