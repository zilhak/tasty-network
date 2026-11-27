//! `stun_probe`: send one STUN Binding Request to a rendezvous server port and print the
//! address the answer carries. `rendezvous/check-config.sh` uses it to see that the primary and
//! the alternate port answer, with this crate's STUN codec and no outside STUN client.
//!
//! ```text
//! cargo run -p tasty-net-rendezvous --example stun_probe -- <host:port> <timeout ms>
//! ```
//!
//! On an answer it prints one line and exits 0:
//!
//! ```text
//! stun-probe server=<addr> local=<addr> mapped=<addr>
//! ```
//!
//! With no matching answer before the timeout it prints `stun-probe server=<addr> result=timeout`
//! and exits 1. A wrong argument exits 2. Packets that are not the answer to this request
//! (another transaction ID, not STUN, a request) are skipped and counted in the timeout line.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tasty_net_rendezvous::wire::stun::{self, StunClass};

const USAGE: &str = "usage: stun_probe <host:port> <timeout ms>";

/// The largest datagram read; a STUN answer of this subset is 44 bytes at most.
const RECV_BUFFER: usize = 1500;

fn probe(server: SocketAddr, timeout: Duration) -> Result<ExitCode, String> {
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .map_err(|e| format!("bind address: {e}"))?;
    let socket = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    socket
        .connect(server)
        .map_err(|e| format!("connect {server}: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("local address: {e}"))?;
    let txid = stun::random_transaction_id().map_err(|e| format!("transaction ID: {e}"))?;
    socket
        .send(&stun::binding_request(&txid, None))
        .map_err(|e| format!("send to {server}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut skipped = 0_u32;
    let mut buf = [0_u8; RECV_BUFFER];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            println!("stun-probe server={server} result=timeout skipped={skipped}");
            return Ok(ExitCode::FAILURE);
        }
        socket
            .set_read_timeout(Some(left))
            .map_err(|e| format!("read timeout: {e}"))?;
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(format!("receive from {server}: {e}")),
        };
        let answer = buf
            .get(..n)
            .and_then(|packet| stun::parse(packet).ok())
            .filter(|m| m.class == StunClass::BindingSuccess && m.transaction_id == txid);
        match answer.and_then(|m| m.mapped) {
            Some(mapped) => {
                println!("stun-probe server={server} local={local} mapped={mapped}");
                return Ok(ExitCode::SUCCESS);
            }
            None => skipped = skipped.saturating_add(1),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [target, timeout_ms] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let Ok(timeout_ms) = timeout_ms.parse::<u64>() else {
        eprintln!("stun_probe: timeout must be a number of milliseconds\n{USAGE}");
        return ExitCode::from(2);
    };
    let server = match target.to_socket_addrs().map(|mut a| a.next()) {
        Ok(Some(addr)) => addr,
        Ok(None) => {
            eprintln!("stun_probe: {target} has no address");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("stun_probe: {target}: {e}");
            return ExitCode::from(2);
        }
    };
    match probe(server, Duration::from_millis(timeout_ms)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("stun_probe: {e}");
            ExitCode::FAILURE
        }
    }
}
