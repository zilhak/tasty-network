//! Two P2P devices that meet through a rendezvous server and echo one stream
//! (docs/design/testing.md "크레이트 단독", `scripts/check.sh p2p-example-pair`).
//!
//! ```text
//! p2p_peer id --key-dir <dir>
//!     make the device key if missing and print the device ID
//! p2p_peer --listen --key-dir <dir> --server <addr> --server-key <id> --peer <id> [--timeout <s>]
//!     take one connection from the paired peer, echo one stream, wait for the peer to close
//! p2p_peer --dial <id> --key-dir <dir> --server <addr> --server-key <id> [--message <text>] [--timeout <s>]
//!     connect to the peer, send the message, check the echo, close
//! ```
//!
//! `--timeout` bounds the whole run (default 60 s); until then the dialer asks again while the
//! server does not know the peer yet. Each side prints one result line on stdout and exits
//! with 0 on success. The line names the path (the candidate kind that worked), the local and
//! remote addresses and the server.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::{
    P2pCloseReason, P2pConfig, P2pConnectError, P2pConnection, P2pConnectionInfo, P2pEndpoint,
    PairingList, RendezvousServer,
};

const USAGE: &str = "usage: p2p_peer id --key-dir <dir>
       p2p_peer --listen --key-dir <dir> --server <addr> --server-key <id> --peer <id> [--timeout <s>]
       p2p_peer --dial <id> --key-dir <dir> --server <addr> --server-key <id> [--message <text>] [--timeout <s>]";

/// The default bound of a run.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// The wait between two connect requests while the peer is not registered yet.
const ASK_AGAIN: Duration = Duration::from_millis(200);
/// The default message.
const DEFAULT_MESSAGE: &str = "hello over p2p";

#[derive(Default)]
struct Args {
    id: bool,
    listen: bool,
    dial: Option<DeviceId>,
    key_dir: Option<PathBuf>,
    server: Option<SocketAddr>,
    server_key: Option<DeviceId>,
    peer: Option<DeviceId>,
    message: Option<String>,
    timeout: Option<Duration>,
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "id" => a.id = true,
            "--listen" => a.listen = true,
            "--dial" => a.dial = Some(id(&value("--dial")?)?),
            "--key-dir" => a.key_dir = Some(PathBuf::from(value("--key-dir")?)),
            "--server" => {
                let v = value("--server")?;
                a.server = Some(v.parse().map_err(|e| format!("--server {v}: {e}"))?);
            }
            "--server-key" => a.server_key = Some(id(&value("--server-key")?)?),
            "--peer" => a.peer = Some(id(&value("--peer")?)?),
            "--message" => a.message = Some(value("--message")?),
            "--timeout" => {
                let v = value("--timeout")?;
                let s: u64 = v.parse().map_err(|e| format!("--timeout {v}: {e}"))?;
                a.timeout = Some(Duration::from_secs(s));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn id(s: &str) -> Result<DeviceId, String> {
    s.parse().map_err(|e| format!("device ID {s}: {e}"))
}

fn need<T>(v: Option<T>, name: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("{name} is missing"))
}

fn endpoint(a: &Args, peer: DeviceId) -> Result<P2pEndpoint, String> {
    let dir = need(a.key_dir.clone(), "--key-dir")?;
    let key = DeviceKey::load_or_create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(key, pairing);
    config.enabled = true;
    config.key_dir = Some(dir);
    config.server = Some(RendezvousServer {
        addr: need(a.server, "--server")?,
        public_key: need(a.server_key, "--server-key")?,
    });
    P2pEndpoint::bind(config).map_err(|e| format!("bind: {e}"))
}

fn listen(a: &Args, end: Instant) -> Result<String, String> {
    let peer = need(a.peer, "--peer")?;
    let ep = endpoint(a, peer)?;
    eprintln!("p2p_peer: listening as {}", ep.id());
    let left = end.saturating_duration_since(Instant::now());
    let conn = ep
        .accept_timeout(Some(left))
        .map_err(|e| format!("accept: {e}"))?;
    let left = end.saturating_duration_since(Instant::now());
    // Read the information while the connection is up; the dialer closes it after the echo.
    let info = conn.info();
    let mut s = conn
        .accept_bi(Some(left))
        .map_err(|e| format!("accept stream: {e}"))?;
    let mut got = Vec::new();
    s.read_to_end(&mut got).map_err(|e| format!("read: {e}"))?;
    s.write_all(&got).map_err(|e| format!("write: {e}"))?;
    s.finish().map_err(|e| format!("finish: {e}"))?;
    let left = end.saturating_duration_since(Instant::now());
    conn.wait_closed(left)
        .map_err(|e| format!("waiting for the peer to close: {e}"))?;
    let line = format!(
        "echoed {} bytes from {} via {}",
        got.len(),
        conn.remote_device_id(),
        describe(&conn, info)
    );
    ep.close();
    Ok(line)
}

fn dial(a: &Args, peer: DeviceId, end: Instant) -> Result<String, String> {
    let ep = endpoint(a, peer)?;
    eprintln!("p2p_peer: dialing {peer} as {}", ep.id());
    let conn = loop {
        match ep.connect(peer) {
            Ok(c) => break c,
            Err(e)
                if Instant::now() + ASK_AGAIN < end
                    && matches!(
                        e.connect_error(),
                        Some(
                            P2pConnectError::PeerNotFound
                                | P2pConnectError::RendezvousUnavailable { .. }
                        )
                    ) =>
            {
                std::thread::sleep(ASK_AGAIN);
            }
            Err(e) => return Err(format!("connect: {e}")),
        }
    };
    let message = a
        .message
        .clone()
        .unwrap_or_else(|| DEFAULT_MESSAGE.to_owned());
    let mut s = conn.open_bi().map_err(|e| format!("open stream: {e}"))?;
    s.set_read_timeout(Some(end.saturating_duration_since(Instant::now())));
    s.write_all(message.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    s.finish().map_err(|e| format!("finish: {e}"))?;
    let mut back = Vec::new();
    s.read_to_end(&mut back).map_err(|e| format!("read: {e}"))?;
    if back != message.as_bytes() {
        return Err(format!(
            "echo differs: sent {} bytes, got {}",
            message.len(),
            back.len()
        ));
    }
    let line = format!("echo ok with {peer} via {}", describe(&conn, conn.info()));
    conn.close(P2pCloseReason::Closed);
    ep.close();
    Ok(line)
}

/// The connection information worth a line: the path (the candidate kind that worked), the
/// address pair and the server that introduced the two devices.
fn describe(conn: &P2pConnection, info: Option<P2pConnectionInfo>) -> String {
    let Some(i) = info else {
        return format!("remote={}", conn.remote_address());
    };
    let path = i.remote_candidate.map_or("unknown", |c| c.kind.name());
    let local = match (i.local_ip, i.local_bind) {
        (Some(ip), _) => ip.to_string(),
        (None, Some(bind)) => bind.to_string(),
        (None, None) => "unknown".to_owned(),
    };
    let show = |a: Option<SocketAddr>| a.map_or_else(|| "unknown".to_owned(), |a| a.to_string());
    format!(
        "path={path} local={local} local_reflexive={} remote={} server={}",
        show(i.local_reflexive),
        show(i.remote_addr),
        show(i.rendezvous.map(|r| r.addr)),
    )
}

fn run() -> Result<String, String> {
    let a = parse(std::env::args().skip(1))?;
    if a.id {
        let dir = need(a.key_dir, "--key-dir")?;
        let key = DeviceKey::load_or_create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        return Ok(key.id().to_string());
    }
    let end = Instant::now() + a.timeout.unwrap_or(DEFAULT_TIMEOUT);
    match (a.listen, a.dial) {
        (true, None) => listen(&a, end),
        (false, Some(peer)) => dial(&a, peer, end),
        _ => Err("give one of id, --listen or --dial".to_owned()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("p2p_peer: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
