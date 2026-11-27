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
//! and exits 1. A datagram longer than the largest answer of this subset is not read as an
//! answer: it prints `stun-probe server=<addr> result=oversize len=<bytes>` and exits 1. A wrong
//! argument exits 2. Packets that are not the answer to this request (another transaction ID,
//! not STUN, a request) are skipped and counted in the timeout line.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tasty_net_rendezvous::wire::stun::{self, StunClass};

const USAGE: &str = "usage: stun_probe <host:port> <timeout ms>";

/// The largest answer to a Binding Request without a punch token: the 20-byte header and one
/// IPv6 XOR-MAPPED-ADDRESS (docs/design/rendezvous.md "포트와 전송", 크기와 증폭).
const MAX_ANSWER: usize = 44;

/// The receive buffer, larger than [`MAX_ANSWER`] so that a longer datagram is seen as longer
/// (a datagram cut to the buffer still reads as at least this many bytes).
const RECV_BUFFER: usize = 1500;

/// How a probe ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The server answered with this mapped address.
    Answer {
        local: SocketAddr,
        mapped: SocketAddr,
    },
    /// No answer before the deadline; `skipped` packets were not the answer.
    Timeout { skipped: u32 },
    /// A datagram longer than [`MAX_ANSWER`] came; it is not read as an answer.
    Oversize { len: usize },
}

fn probe(server: SocketAddr, timeout: Duration) -> Result<Outcome, String> {
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
            return Ok(Outcome::Timeout { skipped });
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
        if n > MAX_ANSWER {
            return Ok(Outcome::Oversize { len: n });
        }
        let answer = buf
            .get(..n)
            .and_then(|packet| stun::parse(packet).ok())
            .filter(|m| m.class == StunClass::BindingSuccess && m.transaction_id == txid);
        match answer.and_then(|m| m.mapped) {
            Some(mapped) => return Ok(Outcome::Answer { local, mapped }),
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
        Ok(Outcome::Answer { local, mapped }) => {
            println!("stun-probe server={server} local={local} mapped={mapped}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Timeout { skipped }) => {
            println!("stun-probe server={server} result=timeout skipped={skipped}");
            ExitCode::FAILURE
        }
        Ok(Outcome::Oversize { len }) => {
            println!("stun-probe server={server} result=oversize len={len}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("stun_probe: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake server that answers the first request with `answer(request txid)`.
    fn fake_server(answer: fn(&stun::TransactionId) -> Vec<u8>) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0_u8; RECV_BUFFER];
            let (n, from) = socket.recv_from(&mut buf).unwrap();
            let request = stun::parse(&buf[..n]).unwrap();
            socket
                .send_to(&answer(&request.transaction_id), from)
                .unwrap();
        });
        addr
    }

    fn soon() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn an_answer_gives_the_mapped_address() {
        let server = fake_server(|txid| {
            stun::binding_success(txid, "127.0.0.1:4000".parse().unwrap(), None)
        });
        let outcome = probe(server, soon()).unwrap();
        let want: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        assert!(
            matches!(outcome, Outcome::Answer { mapped, .. } if mapped == want),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_largest_answer_is_read() {
        let server = fake_server(|txid| {
            let answer = stun::binding_success(txid, "[2001:db8::1]:4000".parse().unwrap(), None);
            assert_eq!(answer.len(), MAX_ANSWER);
            answer
        });
        let outcome = probe(server, soon()).unwrap();
        assert!(matches!(outcome, Outcome::Answer { .. }), "{outcome:?}");
    }

    #[test]
    fn a_datagram_longer_than_the_largest_answer_is_not_an_answer() {
        // A valid answer followed by more bytes: cut to its first bytes it would parse, so the
        // probe must refuse it by its length, not by the codec.
        let server = fake_server(|txid| {
            let mut answer = stun::binding_success(txid, "127.0.0.1:4000".parse().unwrap(), None);
            answer.resize(RECV_BUFFER + 100, 0);
            answer
        });
        let outcome = probe(server, soon()).unwrap();
        assert!(
            matches!(outcome, Outcome::Oversize { len } if len > MAX_ANSWER),
            "{outcome:?}"
        );
    }

    #[test]
    fn one_byte_over_the_largest_answer_is_not_an_answer() {
        let server = fake_server(|txid| {
            let mut answer = stun::binding_success(txid, "127.0.0.1:4000".parse().unwrap(), None);
            answer.resize(MAX_ANSWER + 1, 0);
            answer
        });
        let outcome = probe(server, soon()).unwrap();
        assert_eq!(
            outcome,
            Outcome::Oversize {
                len: MAX_ANSWER + 1
            }
        );
    }

    #[test]
    fn another_transaction_is_skipped_until_the_deadline() {
        let server = fake_server(|txid| {
            let mut other = *txid;
            other[0] ^= 0xff;
            stun::binding_success(&other, "127.0.0.1:4000".parse().unwrap(), None)
        });
        let outcome = probe(server, Duration::from_millis(300)).unwrap();
        assert_eq!(outcome, Outcome::Timeout { skipped: 1 });
    }
}
