//! The connection table's bounds and IDs (docs/design/transport.md "연결 목록"). The entries
//! come from SSH connection attempts whose `ssh` cannot be started: each attempt is one
//! entry that ends in port discovery, on every platform and without a network.

use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, Cause, ConnectError, ConnectOptions, ConnectionEvent, ConnectionId,
    ConnectionState, ConnectionTable, ConnectionTableConfig, Phase, SshTransport, Transport,
};

/// A transport whose `ssh` program does not exist, reporting in `table`.
fn failing(table: &ConnectionTable) -> SshTransport {
    let missing =
        std::env::temp_dir().join(format!("tasty-net-transport-no-ssh-{}", std::process::id()));
    let discovery = PortDiscoveryConfig {
        remote_program: "netprog".into(),
        remote_args: vec!["port".into()],
        data_dir: ".netprog".into(),
        port_file: "record".into(),
        explicit_port_file: None,
    };
    SshTransport::new(SshLauncher::new(missing), discovery, PortMode::FileUnix)
        .with_connections(table.clone())
}

/// One attempt that fails in port discovery; returns its entry's ID.
fn fail_once(transport: &SshTransport, options: &ConnectOptions) -> ConnectionId {
    let table = transport.connections().expect("a table");
    let before = table.connections().last().map(|e| e.id);
    match transport.connect_with(&SshTarget::parse("nowhere"), &AttemptCancel::new(), options) {
        Err(ConnectError::Discovery(_)) => {}
        other => panic!("expected a discovery failure: {other:?}"),
    }
    let last = table.connections().last().map(|e| e.id).expect("an entry");
    assert_ne!(Some(last), before, "the attempt made a new entry");
    last
}

#[test]
fn ids_are_stable_and_never_reused() {
    let table = ConnectionTable::new(ConnectionTableConfig::new().with_keep_closed(1));
    let transport = failing(&table);
    let first = fail_once(&transport, &ConnectOptions::new());
    let entry = table.connection(first).expect("kept");
    assert_eq!(entry.id, first);
    assert_eq!(entry.state, ConnectionState::Disconnected);
    // The ID is the entry's for its whole life: every transition was reported under it.
    let ids: Vec<_> = table
        .events()
        .into_iter()
        .filter_map(|e| match e {
            ConnectionEvent::Transition { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, [first, first]);

    let mut seen = vec![first];
    for _ in 0..4 {
        let next = fail_once(
            &transport,
            &ConnectOptions::new().after(*seen.last().expect("one")),
        );
        assert!(next > *seen.last().expect("one"), "numbers rise");
        assert!(
            !seen.contains(&next),
            "a dropped entry's number is not given again"
        );
        seen.push(next);
    }
    // Only the last closed entry is kept; it names the dropped one before it.
    let kept = table.connections();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, *seen.last().expect("one"));
    assert_eq!(kept[0].previous, Some(seen[seen.len() - 2]));
    assert!(kept[0].previous_dropped);
}

#[test]
fn closed_entries_are_capped() {
    let table = ConnectionTable::new(ConnectionTableConfig::new().with_keep_closed(2));
    let transport = failing(&table);
    let ids: Vec<_> = (0..4)
        .map(|_| fail_once(&transport, &ConnectOptions::new()))
        .collect();
    let kept = table.connections();
    assert_eq!(
        kept.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[2..],
        "the two most recent closed entries"
    );
    for entry in &kept {
        assert_eq!(entry.state, ConnectionState::Disconnected);
        assert!(
            matches!(entry.last_cause, Some(Cause::Discovery(_))),
            "{:?}",
            entry.last_cause
        );
        let steps: Vec<_> = entry.history.iter().map(|h| (h.state, h.phase)).collect();
        assert_eq!(
            steps,
            [
                (ConnectionState::Connecting, Some(Phase::PortDiscovery)),
                (ConnectionState::Disconnected, None),
            ]
        );
    }
    let removed: Vec<_> = table
        .events()
        .into_iter()
        .filter_map(|e| match e {
            ConnectionEvent::Removed { id } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(removed, ids[..2]);
}

#[test]
fn event_queue_reports_lag() {
    let table = ConnectionTable::new(ConnectionTableConfig::new().with_event_queue_limit(3));
    let transport = failing(&table);
    // Three attempts, two transitions each: six events into a queue of three.
    let ids: Vec<_> = (0..3)
        .map(|_| fail_once(&transport, &ConnectOptions::new()))
        .collect();
    let events = table.events();
    assert_eq!(events.len(), 4, "{events:#?}");
    assert_eq!(events[0], ConnectionEvent::Lagged { dropped: 3 });
    // The newest three are kept, oldest first.
    let kept: Vec<_> = events[1..]
        .iter()
        .map(|e| match e {
            ConnectionEvent::Transition { id, entry } => (*id, entry.state),
            other => panic!("a transition: {other:?}"),
        })
        .collect();
    assert_eq!(
        kept,
        [
            (ids[1], ConnectionState::Disconnected),
            (ids[2], ConnectionState::Connecting),
            (ids[2], ConnectionState::Disconnected),
        ]
    );
    assert!(table.events().is_empty(), "reading takes the events");
    assert_eq!(
        table
            .settings()
            .iter()
            .find(|s| s.key == "connections.event_queue_limit")
            .map(|s| (s.value.to_string(), s.source.as_str())),
        Some(("3".to_string(), "configured"))
    );
}
