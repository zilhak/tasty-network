//! The `p2p_udp` column of the connection table (docs/design/transport.md "필드"): the entry of
//! one QUIC connection follows what `tasty-net-p2p` shows of it ([`P2pConnectionInfo`]).

use std::net::SocketAddr;

use tasty_net_p2p::{AttemptPhase, P2pConnectionInfo, Side as P2pSide};

use crate::connection_table::{
    Cause, ConnectionId, ConnectionPeer, ConnectionState, ConnectionTable, EntryRef, NewEntry,
    PathKind, Phase, Segment, Via,
};
use crate::role::Side;
use crate::settings::{Setting, SettingSource, SettingValue};

/// The name of the P2P address segment.
const UDP_SEGMENT: &str = "udp";

/// The text of an address the P2P connection does not know.
const UNKNOWN: &str = "unknown";

/// A new `p2p_udp` entry on `table`, set up from the first state of the attempt.
pub(crate) fn open_entry(
    table: &ConnectionTable,
    local_side: Side,
    previous: Option<ConnectionId>,
    peer: ConnectionPeer,
) -> EntryRef {
    let state = match previous {
        Some(_) => ConnectionState::Reconnecting,
        None => ConnectionState::Connecting,
    };
    let first = match local_side {
        Side::Dialer => AttemptPhase::Requesting,
        Side::Acceptor => AttemptPhase::Offered,
    };
    table.open(NewEntry {
        previous,
        parent: None,
        peer,
        path: PathKind::P2pUdp,
        local_side,
        segments: Vec::new(),
        via: empty_via(),
        state,
        phase: Some(Phase::P2p(first)),
        held: false,
        path_fixed: true,
    })
}

/// Makes the redialer's entry a `p2p_udp` one for the attempt it starts.
pub(crate) fn take_pending(pending: &EntryRef) {
    pending.disown_p2p(|info| {
        info.path = PathKind::P2pUdp;
        info.local_side = Side::Dialer;
        info.segments = Vec::new();
        info.via = empty_via();
        info.p2p_id = None;
        info.attempt = None;
        // The attempt's own transitions come with its history.
        if info.state != ConnectionState::Disconnected {
            info.phase = Some(Phase::P2p(AttemptPhase::Requesting));
        }
    });
}

fn empty_via() -> Via {
    Via::Rendezvous {
        server: None,
        candidate: None,
        nat: None,
    }
}

/// How far an entry has followed the history of its P2P connection.
#[derive(Debug, Default)]
pub(crate) struct Followed {
    /// The P2P connection the entry follows, once one was seen.
    id: Option<u64>,
    /// The history entries already copied (the dropped ones counted).
    seen: u64,
}

impl Followed {
    /// The P2P connection followed, once one was seen.
    pub(crate) fn id(&self) -> Option<u64> {
        self.id
    }
}

/// Copies `info` into `entry`: the address segment, the server and candidate, the RTT, the
/// attempt and the applied values, and every transition after the ones `followed` copied
/// already. A new P2P connection (another ID) starts its history from its first entry. With
/// `owner` (the P2P connection's number in the process, [`EntryRef::own_p2p`]) every change
/// is made only while that connection still owns the entry, checked in the same step.
pub(crate) fn follow(
    entry: &EntryRef,
    info: &P2pConnectionInfo,
    followed: &mut Followed,
    owner: Option<u64>,
) {
    if followed.id != Some(info.id) {
        *followed = Followed {
            id: Some(info.id),
            seen: 0,
        };
    }
    let owned = entry.update_owned(owner, |e| {
        e.p2p_id = Some(info.id);
        e.local_side = match info.side {
            P2pSide::Dialer => Side::Dialer,
            P2pSide::Acceptor => Side::Acceptor,
        };
        e.peer.device_id = Some(info.peer);
        e.segments = vec![udp_segment(info)];
        e.via = Via::Rendezvous {
            server: info.rendezvous,
            candidate: info.remote_candidate.map(|c| c.kind),
            nat: info.nat,
        };
        e.rtt = info.rtt;
        e.attempt = Some(info.attempt.clone());
        // The link's values stay; the P2P connection's are replaced by its current ones.
        e.applied
            .retain(|s| !matches!(s.source, SettingSource::P2p(_)));
        e.applied.extend(info.applied.iter().map(|a| Setting {
            key: a.key,
            value: SettingValue::P2p(a.value),
            source: SettingSource::P2p(a.source),
        }));
    });
    if !owned {
        return;
    }
    let total = info
        .history_dropped
        .saturating_add(u64::try_from(info.history.len()).unwrap_or(u64::MAX));
    let skip = followed.seen.saturating_sub(info.history_dropped);
    let skip = usize::try_from(skip).unwrap_or(usize::MAX);
    for item in info.history.iter().skip(skip) {
        if item.to == AttemptPhase::Closed {
            // The connection's end ends the entry, with the transport's cause.
            continue;
        }
        let phase = match item.to {
            // The QUIC connection is up: the handshake v2 on its first channel is next.
            AttemptPhase::Connected => Phase::Handshake,
            other => Phase::P2p(other),
        };
        entry.step_owned(owner, phase, Cause::P2p(item.cause.clone()));
    }
    followed.seen = total;
}

/// The `udp` segment: the three local values against the peer address in use.
fn udp_segment(info: &P2pConnectionInfo) -> Segment {
    let local = format!(
        "local_bind={}, local_ip={}, local_reflexive={}",
        text(info.local_bind),
        info.local_ip
            .map_or_else(|| UNKNOWN.to_owned(), |ip| ip.to_string()),
        text(info.local_reflexive),
    );
    Segment {
        name: UDP_SEGMENT,
        local: Some(local),
        remote: info.remote_addr.map(|a| a.to_string()),
    }
}

fn text(addr: Option<SocketAddr>) -> String {
    addr.map_or_else(|| UNKNOWN.to_owned(), |a| a.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use tasty_net_identity::DeviceKey;
    use tasty_net_p2p::{AttemptInfo, HistoryEntry};

    use super::*;

    /// What `tasty-net-p2p` shows of a connection numbered `id` that stood.
    fn connected(id: u64) -> P2pConnectionInfo {
        P2pConnectionInfo {
            id,
            peer: DeviceKey::from_seed([9; 32]).id(),
            side: P2pSide::Dialer,
            state: AttemptPhase::Connected,
            local_bind: None,
            local_ip: None,
            local_reflexive: None,
            remote_addr: None,
            remote_candidate: None,
            rendezvous: None,
            nat: None,
            attempt: AttemptInfo::default(),
            rtt: None,
            last_cause: None,
            history: vec![HistoryEntry {
                from: Some(AttemptPhase::Handshaking),
                to: AttemptPhase::Connected,
                cause: tasty_net_p2p::Cause::named("handshake_done"),
                at: SystemTime::now(),
            }],
            history_dropped: 0,
            applied: Vec::new(),
        }
    }

    /// The thread of an earlier connection wakes late: the redialer's next attempt took the
    /// entry between that thread's look at the entry and its copy. Nothing of the earlier
    /// connection reaches the entry, and its end does not end the entry; the connection that
    /// owns the entry next changes it, also with the same P2P number (another endpoint's).
    #[test]
    fn an_earlier_connection_leaves_the_entry_the_next_attempt_took() {
        let table = ConnectionTable::default();
        let entry = open_entry(&table, Side::Dialer, None, ConnectionPeer::default());
        entry.own_p2p(1, |info| info.p2p_id = Some(5));
        take_pending(&entry);
        let taken = table.connection(entry.id()).expect("the entry");
        follow(&entry, &connected(5), &mut Followed::default(), Some(1));
        entry.end_owned(1, |_, _| Cause::Lost);
        let after = table.connection(entry.id()).expect("the entry");
        assert_eq!(after.p2p_id, None);
        assert_eq!(after.state, taken.state);
        assert_eq!(
            after.history.len(),
            taken.history.len(),
            "{:#?}",
            after.history
        );
        entry.own_p2p(2, |info| info.p2p_id = Some(5));
        follow(&entry, &connected(5), &mut Followed::default(), Some(1));
        let held = table.connection(entry.id()).expect("the entry");
        assert_eq!(held.history.len(), taken.history.len());
        follow(&entry, &connected(5), &mut Followed::default(), Some(2));
        let followed = table.connection(entry.id()).expect("the entry");
        assert_eq!(followed.history.len(), taken.history.len() + 1);
        assert_eq!(followed.phase, Some(Phase::Handshake));
    }
}
