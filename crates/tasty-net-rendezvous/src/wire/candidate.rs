//! The candidate address rule (docs/design/rendezvous.md "후보 주소 규칙").
//!
//! The server filters what it puts into `punch_go.candidates` with [`select_candidates`], and a
//! device filters the candidates it received with [`refilter_received`]. Both use
//! [`candidate_allowed`], so the two sides judge every address the same way.
//!
//! Rule: host candidates are private IPv4 (RFC 1918) and global unicast IPv6 (`2000::/3`)
//! only; reflexive candidates (`server_reflexive`, `reported_reflexive`) are any address that is
//! not unspecified, multicast, broadcast or link-local; every port is 1–65535. Everything else
//! is dropped and counted. Loopback is allowed for reflexive candidates so that examples and
//! tests can run against a loopback server.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::message::{Candidate, PunchMode};

/// Where a candidate address came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// The remote address of the coordination connection, as the server observed it.
    ServerReflexive,
    /// A reflexive address the device reported from its NAT judgement.
    ReportedReflexive,
    /// A host address the device reported.
    Host,
}

/// The `2000::/3` prefix of global unicast IPv6: the top 3 bits of the first segment.
const GLOBAL_UNICAST_MASK: u16 = 0xe000;
const GLOBAL_UNICAST_PREFIX: u16 = 0x2000;

/// Whether an address of `kind` passes the rule.
pub fn candidate_allowed(kind: CandidateKind, addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    let ip = addr.ip().to_canonical();
    match kind {
        CandidateKind::Host => match ip {
            IpAddr::V4(v4) => v4.is_private(),
            IpAddr::V6(v6) => is_global_unicast_v6(&v6),
        },
        CandidateKind::ServerReflexive | CandidateKind::ReportedReflexive => match ip {
            IpAddr::V4(v4) => {
                !(v4.is_unspecified()
                    || v4.is_multicast()
                    || v4.is_broadcast()
                    || v4.is_link_local())
            }
            IpAddr::V6(v6) => {
                !(v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local())
            }
        },
    }
}

fn is_global_unicast_v6(v6: &Ipv6Addr) -> bool {
    v6.segments()
        .first()
        .is_some_and(|s| s & GLOBAL_UNICAST_MASK == GLOBAL_UNICAST_PREFIX)
}

/// Parse a reported address string. IPv4-mapped IPv6 addresses become IPv4.
pub fn parse_address(text: &str) -> Option<SocketAddr> {
    let addr: SocketAddr = text.parse().ok()?;
    Some(SocketAddr::new(addr.ip().to_canonical(), addr.port()))
}

/// The server's caps on each kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateLimits {
    /// `max_reported_reflexive`.
    pub max_reported_reflexive: usize,
    /// `max_host_candidates`.
    pub max_host_candidates: usize,
}

/// The candidates the server sends and what it left out.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CandidateSelection {
    /// In priority order: server reflexive, reported reflexive, host.
    pub candidates: Vec<Candidate>,
    /// Addresses the rule refused (including reported reflexive of another family).
    pub rejected: usize,
    /// Addresses left out by the per-kind caps. This is normal server behaviour.
    pub truncated: usize,
    /// Addresses left out because the same address came earlier.
    pub duplicates: usize,
}

/// Build `punch_go.candidates` for one device from the peer's addresses.
///
/// Order and caps: `server_reflexive` (1) → `reported_reflexive` (up to
/// `max_reported_reflexive`) → `host` (in the order the device sent, up to
/// `max_host_candidates`). `host_only` keeps host candidates only. A reported reflexive address
/// must have the family of the server reflexive address. The same address and port is kept
/// once, as the earlier kind.
pub fn select_candidates(
    server_reflexive: SocketAddr,
    reported_reflexive: &[SocketAddr],
    host: &[SocketAddr],
    mode: PunchMode,
    limits: CandidateLimits,
) -> CandidateSelection {
    let mut out = CandidateSelection::default();
    let mut seen = HashSet::new();
    let server_reflexive = SocketAddr::new(
        server_reflexive.ip().to_canonical(),
        server_reflexive.port(),
    );
    if mode == PunchMode::All {
        push_kind(
            &mut out,
            &mut seen,
            CandidateKind::ServerReflexive,
            std::slice::from_ref(&server_reflexive),
            usize::MAX,
            |_| true,
        );
        let family_v4 = server_reflexive.is_ipv4();
        push_kind(
            &mut out,
            &mut seen,
            CandidateKind::ReportedReflexive,
            reported_reflexive,
            limits.max_reported_reflexive,
            |a| a.is_ipv4() == family_v4,
        );
    }
    push_kind(
        &mut out,
        &mut seen,
        CandidateKind::Host,
        host,
        limits.max_host_candidates,
        |_| true,
    );
    out
}

fn push_kind(
    out: &mut CandidateSelection,
    seen: &mut HashSet<SocketAddr>,
    kind: CandidateKind,
    addrs: &[SocketAddr],
    cap: usize,
    family_ok: impl Fn(&SocketAddr) -> bool,
) {
    let mut taken = 0;
    for addr in addrs {
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        if !candidate_allowed(kind, addr) || !family_ok(&addr) {
            out.rejected += 1;
        } else if seen.contains(&addr) {
            out.duplicates += 1;
        } else if taken >= cap {
            out.truncated += 1;
        } else {
            seen.insert(addr);
            taken += 1;
            out.candidates.push(Candidate { addr, kind });
        }
    }
}

/// A device's filter of the candidates it received: drop the addresses the rule refuses and
/// count them. The count of candidates against the device's own cap is not checked here; a
/// `punch_go` over that cap is a server fault (`limits_invalid`).
pub fn refilter_received(candidates: &[Candidate]) -> (Vec<Candidate>, usize) {
    let mut dropped = 0;
    let kept = candidates
        .iter()
        .filter(|c| {
            let ok = candidate_allowed(c.kind, c.addr);
            if !ok {
                dropped += 1;
            }
            ok
        })
        .copied()
        .collect();
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn candidate_rule_matches_device_rule() {
        use CandidateKind::*;
        let cases: &[(CandidateKind, &str, bool)] = &[
            // RFC 1918 ends.
            (Host, "10.0.0.0:1", true),
            (Host, "10.255.255.255:65535", true),
            (Host, "9.255.255.255:1", false),
            (Host, "11.0.0.0:1", false),
            (Host, "172.16.0.0:1", true),
            (Host, "172.31.255.255:1", true),
            (Host, "172.15.255.255:1", false),
            (Host, "172.32.0.0:1", false),
            (Host, "192.168.0.0:1", true),
            (Host, "192.168.255.255:1", true),
            (Host, "192.169.0.0:1", false),
            (Host, "192.167.255.255:1", false),
            // Host: no public, loopback, link-local IPv4.
            (Host, "203.0.113.5:1", false),
            (Host, "127.0.0.1:1", false),
            (Host, "169.254.1.1:1", false),
            // 2000::/3 ends.
            (Host, "[2000::]:1", true),
            (Host, "[3fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:1", true),
            (Host, "[1fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:1", false),
            (Host, "[4000::]:1", false),
            (Host, "[fe80::1]:1", false),
            (Host, "[fd00::1]:1", false),
            (Host, "[::1]:1", false),
            // IPv4-mapped IPv6 is judged as IPv4.
            (Host, "[::ffff:192.168.1.1]:1", true),
            // Port 0.
            (Host, "192.168.1.1:0", false),
            (ServerReflexive, "203.0.113.5:0", false),
            // Reflexive.
            (ServerReflexive, "203.0.113.5:3478", true),
            (ReportedReflexive, "127.0.0.1:3478", true),
            (ReportedReflexive, "[::1]:3478", true),
            (ReportedReflexive, "0.0.0.0:3478", false),
            (ReportedReflexive, "[::]:3478", false),
            (ReportedReflexive, "224.0.0.1:3478", false),
            (ReportedReflexive, "[ff02::1]:3478", false),
            (ReportedReflexive, "255.255.255.255:3478", false),
            (ReportedReflexive, "169.254.0.1:3478", false),
            (ReportedReflexive, "[fe80::1]:3478", false),
            (ReportedReflexive, "[febf::1]:3478", false),
            (ReportedReflexive, "[fec0::1]:3478", true),
            (ServerReflexive, "192.168.1.1:3478", true),
        ];
        for (kind, addr, want) in cases {
            let addr = a(addr);
            assert_eq!(candidate_allowed(*kind, addr), *want, "{kind:?} {addr}");
            // The device-side refilter uses the same function: a candidate survives it
            // exactly when the rule allows it.
            let (kept, dropped) = refilter_received(&[Candidate { addr, kind: *kind }]);
            assert_eq!(kept.len() == 1, *want, "device rule {kind:?} {addr}");
            assert_eq!(dropped == 1, !*want, "device count {kind:?} {addr}");
        }
    }

    #[test]
    fn candidates_follow_kind_priority_and_limit() {
        let limits = CandidateLimits {
            max_reported_reflexive: 3,
            max_host_candidates: 4,
        };
        let server = a("203.0.113.5:40000");
        // Five reported reflexive: one repeats the server reflexive address, one is IPv6
        // (another family), one is link-local; the rest are valid and more than the cap.
        let reported = [
            a("203.0.113.5:40000"),
            a("203.0.113.5:40001"),
            a("[2001:db8::1]:40002"),
            a("203.0.113.5:40003"),
            a("169.254.0.1:40004"),
            a("203.0.113.5:40005"),
            a("203.0.113.5:40006"),
        ];
        // Seven host candidates: one duplicate, one public (refused), the rest valid.
        let host = [
            a("192.168.1.2:5000"),
            a("192.168.1.2:5000"),
            a("10.0.0.2:5000"),
            a("203.0.113.9:5000"),
            a("[2001:db8::2]:5000"),
            a("172.16.0.2:5000"),
            a("10.0.0.3:5000"),
        ];
        let got = select_candidates(server, &reported, &host, PunchMode::All, limits);
        let kinds: Vec<CandidateKind> = got.candidates.iter().map(|c| c.kind).collect();
        use CandidateKind::*;
        assert_eq!(
            kinds,
            vec![
                ServerReflexive,
                ReportedReflexive,
                ReportedReflexive,
                ReportedReflexive,
                Host,
                Host,
                Host,
                Host
            ]
        );
        assert_eq!(got.candidates.len(), 1 + 3 + 4);
        let addrs: Vec<SocketAddr> = got.candidates.iter().map(|c| c.addr).collect();
        assert_eq!(
            addrs,
            vec![
                server,
                a("203.0.113.5:40001"),
                a("203.0.113.5:40003"),
                a("203.0.113.5:40005"),
                a("192.168.1.2:5000"),
                a("10.0.0.2:5000"),
                a("[2001:db8::2]:5000"),
                a("172.16.0.2:5000"),
            ]
        );
        assert_eq!(got.duplicates, 2, "{got:?}");
        assert_eq!(got.rejected, 3, "{got:?}");
        assert_eq!(got.truncated, 2, "{got:?}");

        let host_only = select_candidates(server, &reported, &host, PunchMode::HostOnly, limits);
        assert!(host_only.candidates.iter().all(|c| c.kind == Host));
        assert_eq!(host_only.candidates.len(), 4);
    }
}
