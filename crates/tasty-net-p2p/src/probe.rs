//! Punch probes (docs/design/p2p.md "펀칭 절차" 4–7).
//!
//! From the start time both devices send a STUN Binding Request with their own token in
//! `TASTY-PUNCH` to the peer's candidates, one candidate every `punch.probe_interval` in turn.
//! A response counts as success only when it carries the request's transaction ID, came from
//! the candidate the request went to, and carries the peer's token.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tasty_net_rendezvous::packet_io::PacketIo;
use tasty_net_rendezvous::wire::message::{Candidate, PunchGo, PunchToken};
use tasty_net_rendezvous::wire::stun::{self, StunClass, StunMessage, TransactionId};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::debug;

use crate::socket::{self as sock, ProbeSession, RemoteCandidate, StunRouter, canonical};

/// Whether `msg` from `from` answers one of the probes in `sent` (transaction ID → the
/// candidate it went to) with the peer's token.
pub(crate) fn probe_succeeded(
    sent: &HashMap<TransactionId, SocketAddr>,
    msg: &StunMessage,
    from: SocketAddr,
    token_peer: &PunchToken,
) -> bool {
    msg.class == StunClass::BindingSuccess
        && sent.get(&msg.transaction_id) == Some(&canonical(from))
        && msg.punch_token.as_ref() == Some(token_peer)
}

/// The candidates of a `punch_go` refiltered with the shared rule, and how many were dropped.
pub(crate) fn received_candidates(go: &PunchGo) -> (Vec<RemoteCandidate>, usize) {
    let (kept, dropped) = tasty_net_rendezvous::wire::candidate::refilter_received(&go.candidates);
    (
        kept.iter()
            .map(|c: &Candidate| RemoteCandidate {
                addr: canonical(c.addr),
                kind: c.kind.into(),
            })
            .collect(),
        dropped,
    )
}

/// Send probes until `until`. With `stop_on_success` the first succeeding candidate is
/// returned; otherwise probes go on to `until` and `None` is returned.
pub(crate) async fn run(
    router: &Arc<StunRouter>,
    io: &Arc<dyn PacketIo>,
    session: &Arc<ProbeSession>,
    interval: Duration,
    until: Instant,
    stop_on_success: bool,
) -> Option<RemoteCandidate> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sent = sock::Waiting::new(router.clone(), tx);
    let mut tick = tokio::time::interval(interval);
    let mut next = usize::MIN;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => break None,
            _ = tick.tick() => {
                let candidates = session.candidates();
                if candidates.is_empty() {
                    continue;
                }
                let target = candidates[next % candidates.len()];
                next = next.wrapping_add(1);
                let Ok(txid) = stun::random_transaction_id() else {
                    continue;
                };
                sent.add(txid, canonical(target.addr));
                let probe = stun::binding_request(&txid, Some(&session.token_local));
                if let Err(e) = sock::send(io.as_ref(), &probe, target.addr) {
                    debug!(error = %e, to = %target.addr, "probe not sent");
                }
            }
            Some(reply) = rx.recv() => {
                if stop_on_success
                    && probe_succeeded(sent.sent(), &reply.msg, reply.from, &session.token_peer)
                    && let Some(c) = session.candidate(reply.from)
                {
                    break Some(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;
    use tasty_net_rendezvous::wire::candidate::CandidateKind;
    use tasty_net_rendezvous::wire::message::{NatMapping, PunchMode, Role};

    use crate::socket::RemoteKind;

    #[test]
    fn probe_response_needs_txid_address_and_token() {
        let to: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let elsewhere: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        let txid = [4; 12];
        let peer = PunchToken([8; 16]);
        let sent: HashMap<_, _> = [(txid, to)].into();
        let answer = |txid: &TransactionId, token: Option<&PunchToken>| {
            stun::parse(&stun::binding_success(txid, to, token)).unwrap()
        };
        assert!(probe_succeeded(
            &sent,
            &answer(&txid, Some(&peer)),
            to,
            &peer
        ));
        // A mapped IPv6 form of the same address is the same address.
        let mapped = sock::wire_addr("[::]:0".parse().unwrap(), to);
        assert!(probe_succeeded(
            &sent,
            &answer(&txid, Some(&peer)),
            mapped,
            &peer
        ));
        // Another transaction ID.
        assert!(!probe_succeeded(
            &sent,
            &answer(&[5; 12], Some(&peer)),
            to,
            &peer
        ));
        // From another address than the request went to.
        assert!(!probe_succeeded(
            &sent,
            &answer(&txid, Some(&peer)),
            elsewhere,
            &peer
        ));
        // Without the token, or with another.
        assert!(!probe_succeeded(&sent, &answer(&txid, None), to, &peer));
        assert!(!probe_succeeded(
            &sent,
            &answer(&txid, Some(&PunchToken([9; 16]))),
            to,
            &peer
        ));
        // A request is not an answer.
        let request = stun::parse(&stun::binding_request(&txid, Some(&peer))).unwrap();
        assert!(!probe_succeeded(&sent, &request, to, &peer));
    }

    #[test]
    fn received_candidates_are_refiltered() {
        let c = |addr: &str, kind| Candidate {
            addr: addr.parse().unwrap(),
            kind,
        };
        let go = PunchGo {
            punch_id: 1,
            request_id: None,
            peer: DeviceKey::from_seed([1; 32]).id(),
            role: Role::Controlled,
            mode: PunchMode::All,
            candidates: vec![
                c("203.0.113.5:40000", CandidateKind::ServerReflexive),
                c("0.0.0.0:40000", CandidateKind::ServerReflexive),
                c("203.0.113.5:0", CandidateKind::ReportedReflexive),
                c("169.254.1.1:40000", CandidateKind::ReportedReflexive),
                c("192.168.1.20:5000", CandidateKind::Host),
                c("8.8.8.8:5000", CandidateKind::Host),
                c("[fe80::1]:5000", CandidateKind::Host),
                c("[2001:db8::7]:5000", CandidateKind::Host),
            ],
            peer_nat: NatMapping::Unknown,
            token_local: PunchToken([1; 16]),
            token_peer: PunchToken([2; 16]),
            start_after_ms: 0,
            window_ms: 0,
            handshake_ms: 0,
        };
        let (kept, dropped) = received_candidates(&go);
        assert_eq!(dropped, 5);
        let want = [
            ("203.0.113.5:40000", RemoteKind::ServerReflexive),
            ("192.168.1.20:5000", RemoteKind::Host),
            ("[2001:db8::7]:5000", RemoteKind::Host),
        ];
        assert_eq!(kept.len(), want.len());
        for (got, (addr, kind)) in kept.iter().zip(want) {
            assert_eq!(got.addr, addr.parse::<SocketAddr>().unwrap());
            assert_eq!(got.kind, kind);
        }
    }

    /// A probe run cut short (its future dropped by a cancel) leaves no transaction ID waited
    /// for.
    #[tokio::test]
    async fn a_dropped_probe_run_forgets_its_transaction_ids() {
        let router = Arc::new(StunRouter::default());
        let io: Arc<dyn PacketIo> = Arc::new(tests_support::Sink);
        let peer = DeviceKey::from_seed([3; 32]).id();
        let session = Arc::new(ProbeSession::new(
            peer,
            PunchToken([1; 16]),
            PunchToken([2; 16]),
            PunchMode::All,
            Duration::from_secs(1),
            vec![RemoteCandidate {
                addr: "203.0.113.9:40000".parse().unwrap(),
                kind: RemoteKind::ServerReflexive,
            }],
            8,
        ));
        let until = Instant::now() + Duration::from_secs(60);
        let mut run = Box::pin(run(
            &router,
            &io,
            &session,
            Duration::from_millis(5),
            until,
            true,
        ));
        let cut = tokio::time::timeout(Duration::from_millis(50), &mut run).await;
        assert!(cut.is_err(), "no answers, so it runs on");
        assert!(router.waiting() > 0);
        drop(run);
        assert_eq!(router.waiting(), 0);
    }
}

/// Packet I/O for the unit tests of the STUN side: every send is taken, nothing comes in.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::io;
    use std::net::SocketAddr;
    use std::task::{Context, Poll};

    use tasty_net_rendezvous::packet_io::PacketIo;

    #[derive(Debug)]
    pub(crate) struct Sink;

    impl PacketIo for Sink {
        fn poll_recv_from(
            &self,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<io::Result<(usize, SocketAddr)>> {
            Poll::Pending
        }
        fn poll_send_ready(&self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn try_send_to(&self, _: &[u8], _: SocketAddr) -> io::Result<()> {
            Ok(())
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from(([10, 0, 0, 2], 5000)))
        }
    }
}
