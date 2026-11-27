//! NAT judgement (docs/design/p2p.md "NAT 판별").
//!
//! The device sends STUN Binding Requests from its one socket to the server's primary address
//! and the alternate addresses of `welcome.stun`, every `nat.stun_rto` up to
//! `nat.stun_attempts` times per target, all targets at once, and compares the
//! XOR-MAPPED-ADDRESS of the answers. An answer counts only with the request's transaction ID
//! and from the address the request went to (RFC 8489 §6.3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tasty_net_rendezvous::packet_io::PacketIo;
use tasty_net_rendezvous::wire::message::NatMapping;
use tasty_net_rendezvous::wire::stun::{self, StunClass};
use tokio::sync::mpsc;
use tracing::debug;

use crate::socket::{self as sock, StunRouter, canonical};

/// One STUN target and what it answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StunAnswer {
    /// The server address asked.
    pub server: SocketAddr,
    /// The reflexive address in its answer; `None` without an answer.
    pub reflexive: Option<SocketAddr>,
}

/// The result of one judgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NatJudgement {
    /// The mapping.
    pub mapping: NatMapping,
    /// Every target with its answer, the primary first.
    pub answers: Vec<StunAnswer>,
}

impl NatJudgement {
    /// The targets that did not answer.
    pub fn missing(&self) -> Vec<SocketAddr> {
        self.answers
            .iter()
            .filter(|a| a.reflexive.is_none())
            .map(|a| a.server)
            .collect()
    }
}

/// The mapping from the answers (the table of docs/design/p2p.md "NAT 판별"). `answers[0]` is
/// the primary address; a second server IP is there when an answered or unanswered target has
/// an IP other than the primary's.
pub fn judge(answers: &[StunAnswer]) -> NatMapping {
    let Some(primary) = answers.first() else {
        return NatMapping::Unknown;
    };
    let got: Vec<SocketAddr> = answers.iter().filter_map(|a| a.reflexive).collect();
    if got.len() <= 1 {
        return NatMapping::Unknown;
    }
    if got.windows(2).any(|w| w[0] != w[1]) {
        return NatMapping::Dependent;
    }
    if got.len() < answers.len() {
        return NatMapping::Unknown;
    }
    let second_ip = answers
        .iter()
        .any(|a| a.server.ip().to_canonical() != primary.server.ip().to_canonical());
    if second_ip {
        NatMapping::Independent
    } else {
        NatMapping::IndependentPortOnly
    }
}

/// Ask every target at once and judge.
pub(crate) async fn detect(
    router: &Arc<StunRouter>,
    io: &Arc<dyn PacketIo>,
    targets: &[SocketAddr],
    rto: Duration,
    attempts: u32,
) -> NatJudgement {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut asked = sock::Waiting::new(router.clone(), tx);
    let mut answers: Vec<StunAnswer> = targets
        .iter()
        .map(|t| StunAnswer {
            server: canonical(*t),
            reflexive: None,
        })
        .collect();
    let mut txids = Vec::new();
    for t in targets {
        match stun::random_transaction_id() {
            Ok(txid) => {
                asked.add(txid, canonical(*t));
                txids.push((txid, *t));
            }
            Err(e) => debug!(error = %e, "no random transaction ID"),
        }
    }
    let mut tick = tokio::time::interval(rto);
    let mut sent = 0;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if sent == attempts {
                    break;
                }
                sent += 1;
                for (txid, t) in &txids {
                    let answered = answers.iter().any(|a| a.server == canonical(*t) && a.reflexive.is_some());
                    if !answered
                        && let Err(e) = sock::send(io.as_ref(), &stun::binding_request(txid, None), *t)
                    {
                        debug!(error = %e, target = %t, "STUN request not sent");
                    }
                }
            }
            Some(reply) = rx.recv() => {
                let from = canonical(reply.from);
                if reply.msg.class == StunClass::BindingSuccess
                    && asked.sent().get(&reply.msg.transaction_id) == Some(&from)
                    && let Some(mapped) = reply.msg.mapped
                    && let Some(a) = answers.iter_mut().find(|a| a.server == from)
                {
                    a.reflexive = Some(canonical(mapped));
                }
                if answers.iter().all(|a| a.reflexive.is_some()) {
                    break;
                }
            }
        }
    }
    drop(asked);
    NatJudgement {
        mapping: judge(&answers),
        answers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn ans(server: &str, reflexive: Option<&str>) -> StunAnswer {
        StunAnswer {
            server: a(server),
            reflexive: reflexive.map(a),
        }
    }

    #[test]
    fn nat_mapping_follows_stun_results() {
        let p1 = "198.51.100.1:3478";
        let p2 = "198.51.100.1:3479";
        let q1 = "198.51.100.2:3478";
        let q2 = "198.51.100.2:3479";
        let m = Some("203.0.113.5:40000");
        let other = Some("203.0.113.5:40001");
        let cases: Vec<(Vec<StunAnswer>, NatMapping)> = vec![
            // IP2 advertised, every target answered the same.
            (
                vec![ans(p1, m), ans(p2, m), ans(q1, m), ans(q2, m)],
                NatMapping::Independent,
            ),
            // No IP2: the two ports agree.
            (
                vec![ans(p1, m), ans(p2, m)],
                NatMapping::IndependentPortOnly,
            ),
            // The ports differ.
            (vec![ans(p1, m), ans(p2, other)], NatMapping::Dependent),
            // IP1:P1 = IP1:P2 but IP2:P1 differs.
            (
                vec![ans(p1, m), ans(p2, m), ans(q1, other), ans(q2, other)],
                NatMapping::Dependent,
            ),
            // An advertised target did not answer, the rest agree.
            (
                vec![ans(p1, m), ans(p2, m), ans(q1, None), ans(q2, m)],
                NatMapping::Unknown,
            ),
            // A missing answer and a difference: the difference decides.
            (
                vec![ans(p1, m), ans(p2, other), ans(q1, None)],
                NatMapping::Dependent,
            ),
            // One answer only, or none.
            (vec![ans(p1, m), ans(p2, None)], NatMapping::Unknown),
            (vec![ans(p1, None), ans(p2, None)], NatMapping::Unknown),
            (vec![ans(p1, m)], NatMapping::Unknown),
            (vec![], NatMapping::Unknown),
        ];
        for (answers, want) in cases {
            assert_eq!(judge(&answers), want, "{answers:?}");
        }
        let j = NatJudgement {
            mapping: NatMapping::Unknown,
            answers: vec![ans(p1, m), ans(q1, None)],
        };
        assert_eq!(j.missing(), vec![a(q1)]);
    }

    /// A judgement cut short (its future dropped) leaves no transaction ID waited for.
    #[tokio::test]
    async fn a_dropped_judgement_forgets_its_transaction_ids() {
        let router = Arc::new(StunRouter::default());
        let io: Arc<dyn PacketIo> = Arc::new(crate::probe::tests_support::Sink);
        let targets = [a("198.51.100.1:3478"), a("198.51.100.2:3478")];
        let detect = detect(&router, &io, &targets, Duration::from_millis(10), 100);
        tokio::pin!(detect);
        let cut = tokio::time::timeout(Duration::from_millis(50), &mut detect).await;
        assert!(cut.is_err(), "no answers, so it runs on");
        assert_eq!(router.waiting(), 2);
        drop(detect);
        assert_eq!(router.waiting(), 0);
    }
}
