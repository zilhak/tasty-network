//! The accepting side of the P2P adapter: the connections the endpoint accepts and the streams
//! their dialers open (docs/design/transport.md "받는 쪽 진입").

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tasty_net_identity::DeviceId;
use tasty_net_p2p::{P2pEndpoint, P2pError};

use crate::cancel::AttemptCancel;
use crate::channel::RawChannel;
use crate::connection_table::ConnectionPeer;
use crate::handshake::{AcceptConfig, Accepted, accept_p2p_handshake};
use crate::lock::lock;
use crate::reject::HandshakeError;
use crate::role::Side;
use crate::transport::AcceptError;

use super::conn::{ConnSide, ConnState, watch};
use super::poll_interval;
use super::table::{Followed, open_entry};

/// The streams the connections' threads took, waiting for [`P2pListener::accept`].
pub(crate) struct Queue {
    items: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Default)]
struct QueueState {
    waiting: VecDeque<(Arc<ConnState>, RawChannel)>,
    /// The endpoint stopped taking connections, or the listener is dropped.
    ended: bool,
}

impl Queue {
    pub(crate) fn push(&self, conn: Arc<ConnState>, channel: RawChannel) {
        let mut items = lock(&self.items);
        if items.ended {
            return;
        }
        items.waiting.push_back((conn, channel));
        drop(items);
        self.changed.notify_all();
    }

    fn end(&self) {
        lock(&self.items).ended = true;
        self.changed.notify_all();
    }
}

/// The accepting side of P2P channels on one endpoint.
///
/// It takes every connection the endpoint accepts, and every stream the connection's dialer
/// opens on it, as a channel. [`P2pListener::accept`] hands the next one out;
/// [`P2pInbound::handshake`] runs the accepting handshake on it with the listener's
/// [`AcceptConfig`]. Dropping the listener stops taking new connections; channels already
/// handed out keep working, and the connections close by the rules of
/// docs/design/transport.md "P2P의 닫기 범위".
pub struct P2pListener {
    queue: Arc<Queue>,
    config: AcceptConfig,
    stop: Arc<AtomicBool>,
    accepting: Option<JoinHandle<()>>,
}

impl fmt::Debug for P2pListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pListener")
            .field("waiting", &lock(&self.queue.items).waiting.len())
            .finish_non_exhaustive()
    }
}

/// One channel the dialer of an accepted P2P connection opened.
pub struct P2pInbound {
    channel: RawChannel,
    conn: Arc<ConnState>,
    config: AcceptConfig,
}

impl fmt::Debug for P2pInbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pInbound")
            .field("remote_id", &self.remote_id())
            .field("connection", &self.conn.conn.id())
            .finish_non_exhaustive()
    }
}

impl P2pInbound {
    /// The device ID the QUIC connection proved.
    pub fn remote_id(&self) -> DeviceId {
        self.conn.conn.remote_device_id()
    }

    /// Runs the accepting handshake on the channel with the listener's configuration
    /// ([`accept_p2p_handshake`]). The channel is registered with `cancel` until it ends.
    pub fn handshake(self, cancel: &AttemptCancel) -> Result<Accepted, HandshakeError> {
        accept_p2p_handshake(
            self.channel,
            self.conn.conn.remote_device_id(),
            self.conn.conn.id(),
            &self.config,
            cancel,
        )
    }
}

impl P2pListener {
    /// Starts taking the connections `endpoint` accepts. `config` is the accepting end of
    /// every handshake on them; its drain cap is also how long an idle accepted connection
    /// waits for its dialer's close. With a table
    /// ([`AcceptConfig::with_connections`]) every accepted QUIC connection is one `p2p_udp`
    /// entry in it, and its channels are on that entry.
    pub fn new(endpoint: P2pEndpoint, config: AcceptConfig) -> io::Result<Self> {
        let queue = Arc::new(Queue {
            items: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let accepting = {
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let config = config.clone();
            std::thread::Builder::new()
                .name("p2p-listener".into())
                .spawn(move || accept_connections(&endpoint, &config, &queue, &stop))?
        };
        Ok(Self {
            queue,
            config,
            stop,
            accepting: Some(accepting),
        })
    }

    /// Waits for the next channel. Fails once the endpoint is closed or off.
    pub fn accept(&self) -> Result<P2pInbound, AcceptError> {
        self.accept_timeout(None)
    }

    /// [`P2pListener::accept`] waiting at most `limit`; past it the error's kind is
    /// `TimedOut`.
    pub fn accept_timeout(&self, limit: Option<Duration>) -> Result<P2pInbound, AcceptError> {
        let started = Instant::now();
        let mut items = lock(&self.queue.items);
        loop {
            if let Some((conn, channel)) = items.waiting.pop_front() {
                return Ok(P2pInbound {
                    channel,
                    conn,
                    config: self.config.clone(),
                });
            }
            if items.ended {
                return Err(AcceptError::new(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the P2P endpoint takes no more connections",
                )));
            }
            items = match limit {
                None => self
                    .queue
                    .changed
                    .wait(items)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Some(limit) => {
                    let Some(left) = limit
                        .checked_sub(started.elapsed())
                        .filter(|d| !d.is_zero())
                    else {
                        return Err(AcceptError::new(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "no P2P channel came in time",
                        )));
                    };
                    self.queue
                        .changed
                        .wait_timeout(items, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
            };
        }
    }
}

impl Drop for P2pListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.queue.end();
        if let Some(accepting) = self.accepting.take()
            && accepting.join().is_err()
        {
            tracing::debug!("the P2P listener thread panicked");
        }
    }
}

/// Takes the endpoint's connections until the endpoint ends or the listener is dropped. Each
/// connection gets the thread that takes its streams.
fn accept_connections(
    endpoint: &P2pEndpoint,
    config: &AcceptConfig,
    queue: &Arc<Queue>,
    stop: &AtomicBool,
) {
    let poll = poll_interval(endpoint);
    let drain_cap = config.timeouts().drain_cap;
    while !stop.load(Ordering::SeqCst) {
        match endpoint.accept_timeout(Some(poll)) {
            Ok(conn) => {
                let entry = config.connections().map(|table| {
                    let peer = ConnectionPeer {
                        device_id: Some(conn.remote_device_id()),
                        ..ConnectionPeer::default()
                    };
                    open_entry(table, Side::Acceptor, None, peer)
                });
                let state = ConnState::new(
                    conn,
                    ConnSide::Acceptor,
                    drain_cap,
                    entry,
                    Followed::default(),
                );
                watch(state, Some(Arc::downgrade(queue)), poll);
            }
            Err(P2pError::TimedOut) => {}
            Err(error) => {
                tracing::debug!(%error, "the P2P endpoint stopped taking connections");
                break;
            }
        }
    }
    queue.end();
}
