//! The connection table's bounds and IDs (docs/design/transport.md "연결 목록"). The entries
//! come from SSH connection attempts whose `ssh` cannot be started: each attempt is one
//! entry that ends in port discovery, on every platform and without a network.
//!
//! Every test here makes SSH entries, so the file needs the system ssh backend.
#![cfg(feature = "system-ssh")]

use std::time::{Duration, Instant};

use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, Cause, ConnectError, ConnectOptions, ConnectionEvent, ConnectionId,
    ConnectionState, ConnectionTable, ConnectionTableConfig, HandshakeError, Holder, PathKind,
    Phase, RedialError, Redialer, RedialerConfig, SshTransport, Transport,
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

/// Redials `lost` with attempts that fail in port discovery.
fn redial_failing(
    transport: &SshTransport,
    config: RedialerConfig,
    lost: ConnectionId,
) -> Result<(), RedialError> {
    let table = transport.connections().expect("a table");
    let redialer = Redialer::new(table, config).expect("valid settings");
    let cancel = AttemptCancel::new();
    redialer.redial(lost, None, &cancel, |options| {
        transport
            .connect_with(&SshTarget::parse("nowhere"), &cancel, options)
            .map(drop)
            .map_err(HandshakeError::Connect)
    })
}

#[test]
fn history_is_capped() {
    let table = ConnectionTable::new(ConnectionTableConfig::new().with_history_limit(2));
    let transport = failing(&table);
    let lost = fail_once(&transport, &ConnectOptions::new());
    // Two attempts: retry_wait, port_discovery, retry_wait, port_discovery, disconnected.
    let config = RedialerConfig::new()
        .with_initial(Duration::from_millis(1))
        .with_jitter(0.0)
        .with_max_attempts(2);
    match redial_failing(&transport, config, lost) {
        Err(RedialError::Exhausted(Some(HandshakeError::Connect(ConnectError::Discovery(_))))) => {}
        other => panic!("expected the attempts to run out: {other:?}"),
    }
    let entry = table.connections().pop().expect("the redial entry");
    assert_eq!(entry.previous, Some(lost));
    assert_eq!(entry.history.len(), 2);
    assert_eq!(entry.history_dropped, 3);
    let kept: Vec<_> = entry
        .history
        .iter()
        .map(|h| (h.state, h.phase, h.cause.clone()))
        .collect();
    assert_eq!(
        kept,
        [
            (
                ConnectionState::Reconnecting,
                Some(Phase::PortDiscovery),
                None
            ),
            (
                ConnectionState::Disconnected,
                None,
                Some(Cause::RedialExhausted)
            ),
        ]
    );
    assert_eq!(entry.last_cause, Some(Cause::RedialExhausted));
}

/// Records when each attempt starts and fails it with `error`.
fn timed_redial(
    config: RedialerConfig,
    error: fn() -> HandshakeError,
) -> (
    Result<(), RedialError>,
    Instant,
    Vec<Instant>,
    ConnectionTable,
) {
    let table = ConnectionTable::default();
    let transport = failing(&table);
    let lost = fail_once(&transport, &ConnectOptions::new());
    let redialer = Redialer::new(&table, config).expect("valid settings");
    let mut starts = Vec::new();
    let started = Instant::now();
    let result = redialer.redial(lost, None, &AttemptCancel::new(), |_| {
        starts.push(Instant::now());
        Err::<(), _>(error())
    });
    (result, started, starts, table)
}

#[test]
fn redialer_follows_settings() {
    let ms = Duration::from_millis;
    // The waits: min(max_delay, initial × multiplier^(n−1)) × (1 + u × jitter), cut to
    // [0, max_delay].
    let config = RedialerConfig::new()
        .with_initial(ms(40))
        .with_multiplier(2.0)
        .with_max_delay(ms(100))
        .with_jitter(0.25)
        .with_max_attempts(4);
    assert_eq!(config.wait_before(1, 0.0), ms(40));
    assert_eq!(config.wait_before(2, -1.0), ms(60));
    assert_eq!(config.wait_before(3, 0.0), ms(100), "the base is capped");
    assert_eq!(
        config.wait_before(3, 1.0),
        ms(100),
        "and so is the jittered wait"
    );
    assert_eq!(config.wait_before(4, -1.0), ms(75));

    // Four attempts, each after at least the shortest wait the settings allow, then the
    // attempts run out. Real waits only bound from below; the give-up time is the redial
    // unit tests' (on a fake clock).
    let (result, started, starts, table) =
        timed_redial(config.clone(), || HandshakeError::TimedOut);
    assert!(
        matches!(
            result,
            Err(RedialError::Exhausted(Some(HandshakeError::TimedOut)))
        ),
        "{result:?}"
    );
    assert_eq!(starts.len(), 4);
    let mut previous = started;
    for (n, at) in (1u64..).zip(&starts) {
        let gap = at.duration_since(previous);
        let low = config.wait_before(n, -1.0);
        assert!(gap >= low, "wait {n}: {gap:?} shorter than {low:?}");
        previous = *at;
    }
    let entry = table.connections().pop().expect("the redial entry");
    assert_eq!(entry.last_cause, Some(Cause::RedialExhausted));
    assert_eq!(entry.redial.as_ref().map(|r| r.attempt), Some(4));

    // An error connecting again does not change stops at once.
    let config = RedialerConfig::new().with_initial(ms(1));
    let (result, _, starts, table) = timed_redial(config, || HandshakeError::SelfAttach);
    assert!(
        matches!(result, Err(RedialError::Failed(HandshakeError::SelfAttach))),
        "{result:?}"
    );
    assert_eq!(starts.len(), 1);
    let entry = table.connections().pop().expect("the redial entry");
    assert_eq!(entry.state, ConnectionState::Disconnected);
    assert!(
        matches!(entry.last_cause, Some(Cause::HandshakeFailed(_))),
        "{:?}",
        entry.last_cause
    );

    // give_up_after = 0: no redialing, no entry.
    let config = RedialerConfig::new().with_give_up_after(Duration::ZERO);
    let (result, _, starts, table) = timed_redial(config, || HandshakeError::TimedOut);
    assert!(
        matches!(result, Err(RedialError::NotRedialed)),
        "{result:?}"
    );
    assert!(starts.is_empty());
    assert_eq!(table.connections().len(), 1, "only the lost entry");

    // Settings that cannot be used are refused.
    let table = ConnectionTable::default();
    assert!(Redialer::new(&table, RedialerConfig::new().with_multiplier(0.5)).is_err());
    assert!(Redialer::new(&table, RedialerConfig::new().with_jitter(1.5)).is_err());
}

/// A redial keeps the path the consumer's profile chose: the attempt's options name the
/// holder, and the redial's entry follows them, not the lost entry's path.
#[test]
fn a_redial_keeps_the_chosen_role_reversal() {
    let table = ConnectionTable::default();
    let transport = failing(&table);
    let lost = fail_once(&transport, &ConnectOptions::new());
    assert_eq!(
        table.connection(lost).map(|e| e.path),
        Some(PathKind::SshForward)
    );
    let redialer = Redialer::new(
        &table,
        RedialerConfig::new()
            .with_initial(Duration::from_millis(1))
            .with_max_attempts(1),
    )
    .expect("valid");
    let cancel = AttemptCancel::new();
    let result = redialer.redial(lost, None, &cancel, |options| {
        let options = options.clone().with_holder(Holder::Acceptor);
        transport
            .connect_with(&SshTarget::parse("nowhere"), &cancel, &options)
            .map(drop)
            .map_err(HandshakeError::Connect)
    });
    assert!(
        matches!(result, Err(RedialError::Exhausted(_))),
        "{result:?}"
    );
    let entry = table.connections().pop().expect("the redial's entry");
    assert_eq!(entry.previous, Some(lost));
    assert_eq!(entry.path, PathKind::SshRoleReversed);
}
