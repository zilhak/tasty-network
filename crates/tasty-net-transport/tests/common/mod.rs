//! A loopback TCP acceptor shared by the socket tests: it runs the accepting handshake on
//! every connection and echoes every frame of the links and bulk channels it accepts.

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tasty_net_identity::{ConnectionToken, DeviceKey};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelReader,
    ChannelWriter, ConnectionTable, DialConfig, DomainAccept, DomainPolicy, DomainReject,
    LinkTimeouts, MAX_HANDSHAKE_LINE, RawChannel, accept_handshake, read_first_line,
};

pub fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

pub fn token(c: char) -> ConnectionToken {
    ConnectionToken::from_str(&c.to_string().repeat(64)).expect("token")
}

pub fn timeouts() -> LinkTimeouts {
    LinkTimeouts {
        read_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
        role_proposal_timeout: Duration::from_millis(500),
        drain_cap: Duration::from_millis(500),
    }
}

pub fn client() -> DialConfig {
    DialConfig::new(key(1), "dialer-1").with_timeouts(timeouts())
}

/// Takes every attach link and every bulk channel, and counts the links.
#[derive(Default)]
pub struct EchoPolicy {
    pub links: AtomicUsize,
}

impl DomainPolicy for EchoPolicy {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        let n = self.links.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(DomainAccept {
            client_id: Some(u32::try_from(n).expect("few links")),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Ok(DomainAccept::default())
    }
}

/// What the accept thread started: each served socket (to close it at the end) and its thread.
type Served = Arc<Mutex<Vec<(TcpStream, JoinHandle<()>)>>>;

/// The acceptor's address and policy. Dropping it closes the listener and every served
/// socket and joins the accept thread and every serving thread.
pub struct EchoServer {
    pub addr: SocketAddr,
    pub policy: Arc<EchoPolicy>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    served: Served,
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocked accept; it sees the flag and returns, dropping the listener.
        drop(TcpStream::connect(self.addr));
        if let Some(accept) = self.accept.take() {
            accept.join().expect("accept thread");
        }
        let served = std::mem::take(&mut *self.served.lock().expect("served"));
        for (stream, thread) in served {
            drop(stream.shutdown(Shutdown::Both));
            thread.join().expect("serving thread");
        }
    }
}

/// Starts an acceptor with `token` on a loopback port.
pub fn echo_server(token: ConnectionToken) -> EchoServer {
    echo_server_reporting(token, None)
}

/// [`echo_server`] that reports its channels in `table`.
pub fn echo_server_reporting(token: ConnectionToken, table: Option<ConnectionTable>) -> EchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let policy = Arc::new(EchoPolicy::default());
    let config = AcceptConfig::new(key(2), "acceptor-1", Some(token), policy.clone())
        .with_timeouts(timeouts());
    let config = match table {
        Some(table) => config.with_connections(table),
        None => config,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let served: Served = Arc::default();
    let accept = {
        let stop = Arc::clone(&stop);
        let served = Arc::clone(&served);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let Ok(kept) = stream.try_clone() else {
                    continue;
                };
                let config = config.clone();
                let thread =
                    std::thread::spawn(move || serve_one(RawChannel::from_tcp(stream), &config));
                served.lock().expect("served").push((kept, thread));
            }
        })
    };
    EchoServer {
        addr,
        policy,
        stop,
        accept: Some(accept),
        served,
    }
}

fn serve_one(channel: std::io::Result<RawChannel>, config: &AcceptConfig) {
    let Ok(mut channel) = channel else { return };
    let Ok(line) = read_first_line(
        &mut channel,
        MAX_HANDSHAKE_LINE,
        Instant::now() + Duration::from_secs(10),
    ) else {
        return;
    };
    match accept_handshake(channel, &line, Vec::new(), config, &AttemptCancel::new()) {
        Ok(Accepted::Link(mut link)) => {
            if let Some((reader, writer)) = link.take_ordered() {
                echo(reader, &writer);
            }
        }
        Ok(Accepted::Bulk(bulk)) => echo(bulk.reader, &bulk.writer),
        Ok(Accepted::Request(_)) | Err(_) => {}
    }
}

/// Writes every frame back until the reader ends.
fn echo(mut reader: ChannelReader, writer: &ChannelWriter) {
    while let Ok(frame) = reader.read_frame() {
        if writer.write_frame(frame.tag, &frame.payload).is_err() {
            return;
        }
    }
}
