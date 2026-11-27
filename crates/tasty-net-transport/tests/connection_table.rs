//! The connection table's bounds and IDs (docs/design/transport.md "연결 목록"). Each test
//! makes entries that end at once, on every platform and without a network, in each way the
//! build has: an accepted loopback channel whose dialer closes during the handshake (an
//! `ssh_inbound` entry, with or without SSH), and, with the system ssh backend, an SSH
//! connection attempt whose `ssh` cannot be started (it ends in port discovery).

use std::net::{Shutdown, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_identity::{ConnectionToken, DeviceKey};
use tasty_net_transport::{
    AcceptConfig, AttachRequest, AttemptCancel, BulkRequest, Cause, ConnectOptions,
    ConnectionEvent, ConnectionId, ConnectionState, ConnectionTable, ConnectionTableConfig,
    DomainAccept, DomainPolicy, DomainReject, HandshakeError, Phase, RawChannel, RedialError,
    Redialer, RedialerConfig, accept_handshake,
};
#[cfg(feature = "system-ssh")]
use {
    tasty_net_ssh::SshTarget,
    tasty_net_transport::{Holder, PathKind, Transport},
};

/// Makes one entry that ends and returns its ID (the argument is the entry it follows).
type EndOne = dyn Fn(Option<ConnectionId>) -> ConnectionId;
/// One redial attempt that reports in the table.
type Attempt = dyn Fn(&ConnectionTable, &ConnectOptions) -> Result<(), HandshakeError>;

/// One way to make entries that end at once, and what such an entry shows.
struct Ending {
    what: &'static str,
    table: ConnectionTable,
    /// Makes one entry that ends and returns its ID. `after` is the entry it follows; only a
    /// way with `chains` records it.
    end_one: Box<EndOne>,
    /// Whether `after` becomes the entry's `previous` (the dialing side's redial chain).
    chains: bool,
    /// The phase the entry is in until it ends.
    phase: Phase,
    /// Whether a cause is the one this way ends an entry with.
    cause: fn(&Cause) -> bool,
    /// One redial attempt of this way.
    attempt: Box<Attempt>,
    /// The phase a redial attempt of this way shows on the redial's entry, if any.
    attempt_phase: Option<Phase>,
    /// Whether an error is the one a redial attempt of this way fails with.
    attempt_error: fn(&HandshakeError) -> bool,
}

/// Every way this build has, each with its own table made with `config`.
fn endings(config: &ConnectionTableConfig) -> Vec<Ending> {
    vec![
        inbound(ConnectionTable::new(config.clone())),
        #[cfg(feature = "system-ssh")]
        ssh::ending(ConnectionTable::new(config.clone())),
    ]
}

fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

fn token() -> ConnectionToken {
    ConnectionToken::from_str(&"a".repeat(64)).expect("token")
}

/// The domain is never asked: every entry ends before it.
struct NoDomain;

impl DomainPolicy for NoDomain {
    fn accept_attach(&self, _request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        panic!("no attach is accepted in these tests")
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        panic!("no bulk channel is accepted in these tests")
    }
}

/// A valid message 1 of an attach: the acceptor answers with its challenge and waits for the
/// dialer's proof.
fn attach_line() -> Vec<u8> {
    let line = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "stream.open",
        "params": {
            "proto": 2, "purpose": "attach", "token": token().expose_hex(),
            "device_id": key(1).id().to_string(), "instance_id": "dialer-1",
            "nonce": "11".repeat(32), "holder": "dialer", "caps": [],
        },
    });
    format!("{line}\n").into_bytes()
}

/// Accepted loopback channels in `table`: the dialer sends message 1 and closes its sending
/// direction, so each `ssh_inbound` entry ends `peer_closed` in the handshake. No SSH runs.
fn inbound(table: ConnectionTable) -> Ending {
    let config = AcceptConfig::new(key(2), "acceptor-1", Some(token()), Arc::new(NoDomain))
        .with_connections(table.clone());
    let reported = table.clone();
    let end_one = move |_after: Option<ConnectionId>| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let dialer = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        dialer
            .shutdown(Shutdown::Write)
            .expect("close the sending direction");
        let (accepted, _) = listener.accept().expect("accept");
        let channel = RawChannel::from_tcp(accepted).expect("channel");
        let before = reported.connections().last().map(|e| e.id);
        let result = accept_handshake(
            channel,
            &attach_line(),
            Vec::new(),
            &config,
            &AttemptCancel::new(),
        );
        assert!(
            matches!(result, Err(HandshakeError::PeerClosed)),
            "{result:?}"
        );
        drop(dialer);
        let last = reported
            .connections()
            .last()
            .map(|e| e.id)
            .expect("an entry");
        assert_ne!(Some(last), before, "the channel made a new entry");
        last
    };
    Ending {
        what: "inbound",
        table,
        end_one: Box::new(end_one),
        chains: false,
        phase: Phase::Handshake,
        cause: |c| matches!(c, Cause::PeerClosed),
        attempt: Box::new(|_, _| Err(HandshakeError::TimedOut)),
        attempt_phase: None,
        attempt_error: |e| matches!(e, HandshakeError::TimedOut),
    }
}

/// SSH connection attempts whose `ssh` program does not exist.
#[cfg(feature = "system-ssh")]
mod ssh {
    use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SshLauncher, SshTarget};
    use tasty_net_transport::{
        AttemptCancel, ConnectError, ConnectOptions, ConnectionId, ConnectionTable, HandshakeError,
        SshTransport, Transport,
    };

    use super::{Cause, Ending, Phase};

    /// A transport whose `ssh` program does not exist, reporting in `table`.
    pub fn failing(table: &ConnectionTable) -> SshTransport {
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
    pub fn fail_once(transport: &SshTransport, options: &ConnectOptions) -> ConnectionId {
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

    pub fn ending(table: ConnectionTable) -> Ending {
        let transport = failing(&table);
        let end_one = move |after: Option<ConnectionId>| {
            let options = match after {
                Some(previous) => ConnectOptions::new().after(previous),
                None => ConnectOptions::new(),
            };
            fail_once(&transport, &options)
        };
        Ending {
            what: "ssh",
            table,
            end_one: Box::new(end_one),
            chains: true,
            phase: Phase::PortDiscovery,
            cause: |c| matches!(c, Cause::Discovery(_)),
            attempt: Box::new(|table, options| {
                failing(table)
                    .connect_with(&SshTarget::parse("nowhere"), &AttemptCancel::new(), options)
                    .map(drop)
                    .map_err(HandshakeError::Connect)
            }),
            attempt_phase: Some(Phase::PortDiscovery),
            attempt_error: |e| matches!(e, HandshakeError::Connect(ConnectError::Discovery(_))),
        }
    }
}

#[test]
fn ids_are_stable_and_never_reused() {
    for way in endings(&ConnectionTableConfig::new().with_keep_closed(1)) {
        let table = &way.table;
        let first = (way.end_one)(None);
        let entry = table.connection(first).expect("kept");
        assert_eq!(entry.id, first, "{}", way.what);
        assert_eq!(entry.state, ConnectionState::Disconnected, "{}", way.what);
        // The ID is the entry's for its whole life: every transition was reported under it.
        let ids: Vec<_> = table
            .events()
            .into_iter()
            .filter_map(|e| match e {
                ConnectionEvent::Transition { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [first, first], "{}", way.what);

        let mut seen = vec![first];
        for _ in 0..4 {
            let next = (way.end_one)(Some(*seen.last().expect("one")));
            assert!(
                next > *seen.last().expect("one"),
                "{}: numbers rise",
                way.what
            );
            assert!(
                !seen.contains(&next),
                "{}: a dropped entry's number is not given again",
                way.what
            );
            seen.push(next);
        }
        // Only the last closed entry is kept.
        let kept = table.connections();
        assert_eq!(kept.len(), 1, "{}", way.what);
        assert_eq!(kept[0].id, *seen.last().expect("one"), "{}", way.what);
        if way.chains {
            // It names the dropped one before it.
            assert_eq!(kept[0].previous, Some(seen[seen.len() - 2]), "{}", way.what);
            assert!(kept[0].previous_dropped, "{}", way.what);
        }
    }
}

#[test]
fn closed_entries_are_capped() {
    for way in endings(&ConnectionTableConfig::new().with_keep_closed(2)) {
        let table = &way.table;
        let ids: Vec<_> = (0..4).map(|_| (way.end_one)(None)).collect();
        let kept = table.connections();
        assert_eq!(
            kept.iter().map(|e| e.id).collect::<Vec<_>>(),
            ids[2..],
            "{}: the two most recent closed entries",
            way.what
        );
        for entry in &kept {
            assert_eq!(entry.state, ConnectionState::Disconnected, "{}", way.what);
            assert!(
                entry.last_cause.as_ref().is_some_and(way.cause),
                "{}: {:?}",
                way.what,
                entry.last_cause
            );
            let steps: Vec<_> = entry.history.iter().map(|h| (h.state, h.phase)).collect();
            assert_eq!(
                steps,
                [
                    (ConnectionState::Connecting, Some(way.phase)),
                    (ConnectionState::Disconnected, None),
                ],
                "{}",
                way.what
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
        assert_eq!(removed, ids[..2], "{}", way.what);
    }
}

#[test]
fn event_queue_reports_lag() {
    for way in endings(&ConnectionTableConfig::new().with_event_queue_limit(3)) {
        let table = &way.table;
        // Three entries, two transitions each: six events into a queue of three.
        let ids: Vec<_> = (0..3).map(|_| (way.end_one)(None)).collect();
        let events = table.events();
        assert_eq!(events.len(), 4, "{}: {events:#?}", way.what);
        assert_eq!(
            events[0],
            ConnectionEvent::Lagged { dropped: 3 },
            "{}",
            way.what
        );
        // The newest three are kept, oldest first.
        let kept: Vec<_> = events[1..]
            .iter()
            .map(|e| match e {
                ConnectionEvent::Transition { id, entry } => (*id, entry.state),
                other => panic!("{}: a transition: {other:?}", way.what),
            })
            .collect();
        assert_eq!(
            kept,
            [
                (ids[1], ConnectionState::Disconnected),
                (ids[2], ConnectionState::Connecting),
                (ids[2], ConnectionState::Disconnected),
            ],
            "{}",
            way.what
        );
        assert!(
            table.events().is_empty(),
            "{}: reading takes the events",
            way.what
        );
        assert_eq!(
            table
                .settings()
                .iter()
                .find(|s| s.key == "connections.event_queue_limit")
                .map(|s| (s.value.to_string(), s.source.as_str())),
            Some(("3".to_string(), "configured")),
            "{}",
            way.what
        );
    }
}

#[test]
fn history_is_capped() {
    for way in endings(&ConnectionTableConfig::new().with_history_limit(2)) {
        let table = &way.table;
        let lost = (way.end_one)(None);
        // Two attempts. With an attempt phase: retry_wait, the attempt's phase, retry_wait,
        // the attempt's phase, disconnected. Without one: retry_wait, retry_wait after the
        // first failure, disconnected.
        let config = RedialerConfig::new()
            .with_initial(Duration::from_millis(1))
            .with_jitter(0.0)
            .with_max_attempts(2);
        let redialer = Redialer::new(table, config).expect("valid settings");
        let cancel = AttemptCancel::new();
        match redialer.redial(lost, None, &cancel, |options| (way.attempt)(table, options)) {
            Err(RedialError::Exhausted(Some(error))) if (way.attempt_error)(&error) => {}
            other => panic!("{}: expected the attempts to run out: {other:?}", way.what),
        }
        let entry = table.connections().pop().expect("the redial entry");
        assert_eq!(entry.previous, Some(lost), "{}", way.what);
        assert_eq!(entry.history.len(), 2, "{}", way.what);
        let (dropped, before_the_end) = match way.attempt_phase {
            Some(phase) => (3, (ConnectionState::Reconnecting, Some(phase), None)),
            None => (
                1,
                (
                    ConnectionState::Reconnecting,
                    Some(Phase::RetryWait),
                    Some(Cause::HandshakeTimedOut),
                ),
            ),
        };
        assert_eq!(entry.history_dropped, dropped, "{}", way.what);
        let kept: Vec<_> = entry
            .history
            .iter()
            .map(|h| (h.state, h.phase, h.cause.clone()))
            .collect();
        assert_eq!(
            kept,
            [
                before_the_end,
                (
                    ConnectionState::Disconnected,
                    None,
                    Some(Cause::RedialExhausted)
                ),
            ],
            "{}",
            way.what
        );
        assert_eq!(
            entry.last_cause,
            Some(Cause::RedialExhausted),
            "{}",
            way.what
        );
    }
}

/// Records when each attempt starts and fails it with `error`. The lost entry is an accepted
/// loopback channel (no SSH): the redialer only needs an entry with a retryable cause.
fn timed_redial(
    config: RedialerConfig,
    error: fn() -> HandshakeError,
) -> (
    Result<(), RedialError>,
    Instant,
    Vec<Instant>,
    ConnectionTable,
) {
    let way = inbound(ConnectionTable::default());
    let lost = (way.end_one)(None);
    let table = way.table;
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
/// holder, and the redial's entry follows them, not the lost entry's path. Only the SSH
/// transport takes a holder from the options.
#[cfg(feature = "system-ssh")]
#[test]
fn a_redial_keeps_the_chosen_role_reversal() {
    let table = ConnectionTable::default();
    let transport = ssh::failing(&table);
    let lost = ssh::fail_once(&transport, &ConnectOptions::new());
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
