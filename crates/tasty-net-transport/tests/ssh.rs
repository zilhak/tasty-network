//! The SSH forward transport with a fake `ssh`: a shell script that prints the connection
//! record for a discovery and, for `-L`, runs this test binary as a forwarder over real
//! loopback ports (the same fake the tasty-net-ssh tests use).
#![cfg(unix)]

mod common;

use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tasty_net_frame::StreamTag;
use tasty_net_identity::{ConnectionRecord, ConnectionToken};
use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SpawnHook, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, Cause, ConnectError, ConnectOptions, ConnectionEvent, ConnectionState,
    ConnectionTable, ConnectionTableConfig, Direction, EdgeTransport, Finish, HandshakeError,
    Holder, PathKind, Phase, RedialError, Redialer, RedialerConfig, RejectReason, Side,
    SshTransport, TooOldEvidence, Transport, TransportKind, Via, dial_handshake, dial_request,
};

use common::{client, echo_server, echo_server_reporting, token};

const FAKE_ARGS_ENV: &str = "TASTY_NET_TRANSPORT_FAKE_SSH_ARGS";
const FORWARDER_TEST: &str = "fake_ssh_forwarder_entry";
/// A file the fake forwarder creates once it started and is about to prepare (empty: none).
const FAKE_READY_ENV: &str = "TASTY_NET_TRANSPORT_FAKE_SSH_PREPARING";
/// A file the fake forwarder waits for before it binds its local port, the tunnel's
/// readiness (empty: binds at once).
const FAKE_GO_ENV: &str = "TASTY_NET_TRANSPORT_FAKE_SSH_GO";

/// Writes `body` through a child shell, so no descriptor of the file stays open in this
/// multi-threaded process when another test executes it.
fn write_file(path: &Path, body: &str, mode: &str) {
    let status = Command::new("sh")
        .args([
            "-c",
            "printf '%s' \"$1\" > \"$2\" && chmod \"$3\" \"$2\"",
            "sh",
        ])
        .args([body, &path.display().to_string(), mode])
        .status()
        .expect("sh");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

/// One fake ssh: its script, the record it prints, and a log with one line per tunnel.
struct FakeSsh {
    script: PathBuf,
    record: PathBuf,
    tunnels: PathBuf,
    pids: Arc<Mutex<Vec<u32>>>,
    /// The preparing and go files of a gated fake.
    gate: Option<(PathBuf, PathBuf)>,
    /// Held for the life of the fake (a test), declared last so it is released after the
    /// fake's files are gone. A new tunnel's local port is reserved and released before ssh
    /// binds it, so two tests starting tunnels at once can meet on one port: one test's
    /// readiness probe then reaches the other's forwarder, which that test later reaps. The
    /// tests that start tunnels run one at a time.
    _one_at_a_time: MutexGuard<'static, ()>,
}

/// The lock that keeps tunnel-starting tests apart. A test that failed while holding it
/// leaves it poisoned; the next test takes it all the same.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TUNNEL_TESTS: Mutex<()> = Mutex::new(());
    TUNNEL_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeSsh {
    fn new(name: &str) -> Self {
        Self::build(name, false)
    }

    /// A fake whose tunnels say when they are preparing and become ready only after
    /// [`FakeSsh::let_tunnels_bind`].
    fn gated(name: &str) -> Self {
        Self::build(name, true)
    }

    fn build(name: &str, gated: bool) -> Self {
        let one_at_a_time = one_at_a_time();
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "tasty-net-transport-fake-ssh-{}-{seq}-{name}",
            std::process::id()
        ));
        let script = base.with_extension("sh");
        let record = base.with_extension("record");
        let tunnels = base.with_extension("tunnels");
        let gate = gated.then(|| (base.with_extension("preparing"), base.with_extension("go")));
        let (preparing, go) = match &gate {
            Some((preparing, go)) => (preparing.display().to_string(), go.display().to_string()),
            None => (String::new(), String::new()),
        };
        let exe = std::env::current_exe().expect("test binary path");
        write_file(
            &script,
            &format!(
                "#!/bin/sh\ncase \" $* \" in *\" -L \"*)\necho tunnel >> '{tunnels}'\n\
                 {FAKE_ARGS_ENV}=\"$*\"\nexport {FAKE_ARGS_ENV}\n\
                 {FAKE_READY_ENV}='{preparing}'\nexport {FAKE_READY_ENV}\n\
                 {FAKE_GO_ENV}='{go}'\nexport {FAKE_GO_ENV}\n\
                 exec '{exe}' {FORWARDER_TEST} --exact --nocapture --test-threads=1 >/dev/null;;\n\
                 esac\ncat '{record}'\n",
                tunnels = tunnels.display(),
                record = record.display(),
                exe = exe.display(),
            ),
            "700",
        );
        Self {
            script,
            record,
            tunnels,
            pids: Arc::new(Mutex::new(Vec::new())),
            gate,
            _one_at_a_time: one_at_a_time,
        }
    }

    /// Whether a gated tunnel said it is preparing.
    fn a_tunnel_is_preparing(&self) -> bool {
        self.gate
            .as_ref()
            .is_some_and(|(preparing, _)| preparing.exists())
    }

    /// Lets every gated tunnel, now and later, bind its port.
    fn let_tunnels_bind(&self) {
        let (_, go) = self.gate.as_ref().expect("a gated fake");
        write_file(go, "go\n", "600");
    }

    fn publish(&self, text: &str) {
        write_file(&self.record, text, "600");
    }

    fn publish_record(&self, port: u16, token: ConnectionToken, instance_id: &str) {
        let record = ConnectionRecord {
            port,
            token,
            instance_id: instance_id.into(),
        };
        self.publish(&record.to_line());
    }

    /// A transport whose launcher records the PID of every ssh child.
    fn transport(&self) -> SshTransport {
        let pids = Arc::clone(&self.pids);
        let hook = SpawnHook::new(move |mut cmd| {
            let child = cmd.spawn()?;
            pids.lock().expect("pids").push(child.id());
            Ok(child)
        });
        let discovery = PortDiscoveryConfig {
            remote_program: "netprog".into(),
            remote_args: vec!["port".into()],
            data_dir: ".netprog".into(),
            port_file: "record".into(),
            explicit_port_file: None,
        };
        SshTransport::new(
            SshLauncher::new(&self.script).with_spawn_hook(hook),
            discovery,
            PortMode::FileUnix,
        )
    }

    /// How many tunnels the fake started.
    fn tunnels_started(&self) -> usize {
        std::fs::read_to_string(&self.tunnels)
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// The PIDs of the tunnel children (the discovery children print and exit).
    fn tunnel_pids(&self) -> Vec<u32> {
        self.pids
            .lock()
            .expect("pids")
            .iter()
            .copied()
            .filter(|&pid| running(pid))
            .collect()
    }
}

impl Drop for FakeSsh {
    fn drop(&mut self) {
        let gate = self.gate.iter().flat_map(|(a, b)| [a, b]);
        for path in [&self.script, &self.record, &self.tunnels]
            .into_iter()
            .chain(gate)
        {
            if let Err(e) = std::fs::remove_file(path)
                && e.kind() != io::ErrorKind::NotFound
            {
                eprintln!("fake ssh cleanup failed: {e}");
            }
        }
    }
}

/// Whether `pid` still exists (a reaped child does not).
fn running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        || Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
}

fn target() -> SshTarget {
    SshTarget::parse("fake-host")
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// In forwarder mode (the env var is set) this acts as `ssh -N -L` and never returns. In a
/// normal test run it returns at once.
#[test]
fn fake_ssh_forwarder_entry() {
    let Ok(args) = std::env::var(FAKE_ARGS_ENV) else {
        return;
    };
    let words: Vec<&str> = args.split_whitespace().collect();
    let spec = words
        .iter()
        .position(|w| *w == "-L")
        .and_then(|i| words.get(i + 1))
        .expect("-L spec");
    let parts: Vec<&str> = spec.split(':').collect();
    let local: u16 = parts[1].parse().expect("local port");
    let remote: u16 = parts[3].parse().expect("remote port");
    // A gated fake says it is preparing, then binds only once the test lets it (or after a
    // long time, so a failed test does not leave it waiting for ever).
    if let Ok(preparing) = std::env::var(FAKE_READY_ENV)
        && !preparing.is_empty()
    {
        std::fs::write(&preparing, b"preparing\n").expect("preparing file");
        let go = std::env::var(FAKE_GO_ENV).expect("go file");
        let until = Instant::now() + Duration::from_secs(30);
        while !Path::new(&go).exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let listener = match TcpListener::bind(("127.0.0.1", local)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind [127.0.0.1]:{local}: {e}");
            std::process::exit(255);
        }
    };
    for (n, conn) in listener.incoming().enumerate() {
        let Ok(conn) = conn else { continue };
        std::thread::spawn(move || forward(n, conn, remote));
    }
}

fn forward(n: usize, client: TcpStream, remote: u16) {
    let Ok(server) = TcpStream::connect(("127.0.0.1", remote)) else {
        eprintln!("channel {n}: open failed: connect failed: Connection refused");
        return;
    };
    let (mut c_read, mut s_write) = (
        client.try_clone().expect("clone"),
        server.try_clone().expect("clone"),
    );
    let up = std::thread::spawn(move || {
        let _copied = io::copy(&mut c_read, &mut s_write);
        let _closed = s_write.shutdown(std::net::Shutdown::Write);
    });
    let (mut s_read, mut c_write) = (server, client);
    let _copied = io::copy(&mut s_read, &mut c_write);
    let _closed = c_write.shutdown(std::net::Shutdown::Write);
    let _joined = up.join();
}

#[test]
fn a_discovered_peer_gets_a_link_and_a_bulk_channel_through_the_tunnel() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("link");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = fake.transport();
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let mut link =
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    assert_eq!(link.peer().transport(), TransportKind::Ssh);
    assert_eq!(link.client_id(), Some(1));
    assert_eq!(server.policy.links.load(Ordering::SeqCst), 1);

    let (mut reader, writer) = link.take_ordered().expect("ordered");
    writer
        .write_frame(StreamTag::Data, b"over ssh")
        .expect("write");
    assert_eq!(reader.read_frame().expect("echo").payload, b"over ssh");

    let mut bulk = link.open_bulk(None).expect("bulk");
    bulk.writer
        .write_frame(StreamTag::Data, b"bulk")
        .expect("write");
    assert_eq!(
        bulk.reader.read_frame().expect("bulk echo").payload,
        b"bulk"
    );
    // The bulk channel went through the same tunnel.
    assert_eq!(fake.tunnels_started(), 1);
    assert_eq!(transport.live_tunnels(), 1);
}

/// Connections to the same record share the published tunnel; a new instance ID gets a new
/// one; the last channel through a tunnel reaps its ssh child.
#[test]
fn a_published_tunnel_is_shared_until_its_last_channel_ends() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("reuse");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = fake.transport();
    let cancel = AttemptCancel::new();
    let dial = || {
        let conn = transport.connect(&target(), &cancel).expect("connect");
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake")
    };
    let first = dial();
    let second = dial();
    assert_eq!(fake.tunnels_started(), 1);
    assert_eq!(transport.live_tunnels(), 1);

    // A restarted peer publishes a new instance ID: the old tunnel is not used for it.
    fake.publish_record(server.addr.port(), token('a'), "instance-2");
    let third = dial();
    assert_eq!(fake.tunnels_started(), 2);
    assert_eq!(transport.live_tunnels(), 2);
    let pids = fake.tunnel_pids();
    assert_eq!(pids.len(), 2, "{pids:?}");

    drop(first);
    assert_eq!(
        transport.live_tunnels(),
        2,
        "the second link still holds the first tunnel"
    );
    drop(second);
    drop(third);
    wait_until("the tunnels to be reaped", || transport.live_tunnels() == 0);
    wait_until("the ssh children to exit", || fake.tunnel_pids().is_empty());
}

#[test]
fn a_port_only_record_is_a_peer_too_old_without_a_tunnel() {
    let fake = FakeSsh::new("legacy");
    fake.publish("45123\n");
    match fake.transport().connect(&target(), &AttemptCancel::new()) {
        Err(ConnectError::PeerTooOld { evidence }) => {
            assert_eq!(evidence, TooOldEvidence::PortOnlyDiscovery);
        }
        other => panic!("expected PeerTooOld: {other:?}"),
    }
    assert_eq!(fake.tunnels_started(), 0);
}

#[test]
fn a_record_with_another_token_is_refused_by_the_acceptor() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("token");
    fake.publish_record(server.addr.port(), token('b'), "instance-1");
    let transport = fake.transport();
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    match dial_handshake(conn, &client(), Holder::Dialer, None, &cancel) {
        Err(HandshakeError::Rejected { reason, .. }) => {
            assert_eq!(reason, RejectReason::TokenInvalid);
        }
        other => panic!("expected a token refusal: {other:?}"),
    }
    // A tunnel whose first handshake failed is not shared, and the failed connection was
    // its last holder.
    wait_until("the unshared tunnel to be reaped", || {
        fake.tunnel_pids().is_empty()
    });
    assert_eq!(transport.live_tunnels(), 0);
}

#[test]
fn a_stale_record_is_reported_as_an_absent_remote_listener() {
    let fake = FakeSsh::new("absent");
    let port = closed_port();
    fake.publish_record(port, token('a'), "instance-1");
    let transport = fake.transport();
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let error = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel)
        .expect_err("nothing listens");
    match &error {
        HandshakeError::RemoteListenerAbsent(absent) => assert_eq!(absent.remote_port, port),
        other => panic!("expected an absent listener: {other:?}"),
    }
    assert!(error.is_retryable());
}

/// A cancel while a new tunnel is still being prepared makes `connect` return `Cancelled`
/// once the preparation ends, reaps that tunnel and shares nothing: the next attempt starts
/// its own tunnel.
#[test]
fn a_cancel_during_tunnel_preparation_reaps_it_and_returns_cancelled() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::gated("prepare");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = Arc::new(fake.transport());
    let cancel = AttemptCancel::new();
    let attempt = {
        let transport = Arc::clone(&transport);
        let cancel = cancel.clone();
        std::thread::spawn(move || transport.connect(&target(), &cancel).map(drop))
    };
    // The tunnel says it is preparing, and it cannot finish before the test lets it: the
    // cancel comes inside the preparation whatever the scheduling.
    wait_until("the tunnel to prepare", || {
        fake.a_tunnel_is_preparing() || attempt.is_finished()
    });
    assert!(
        !attempt.is_finished(),
        "{:?}",
        attempt.join().expect("attempt")
    );
    cancel.cancel();
    fake.let_tunnels_bind();
    let result = attempt.join().expect("attempt");
    assert!(matches!(result, Err(ConnectError::Cancelled)), "{result:?}");
    wait_until("the cancelled tunnel to be reaped", || {
        fake.tunnel_pids().is_empty()
    });
    let conn = transport
        .connect(&target(), &AttemptCancel::new())
        .expect("a fresh attempt");
    assert_eq!(
        fake.tunnels_started(),
        2,
        "the cancelled tunnel was not shared"
    );
    drop(conn);
}

#[test]
fn cancelling_the_attempt_reaps_its_unpublished_tunnel() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("cancel");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = fake.transport();
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    assert_eq!(fake.tunnel_pids().len(), 1);
    cancel.cancel();
    wait_until("the cancelled tunnel to be reaped", || {
        fake.tunnel_pids().is_empty()
    });
    assert!(matches!(
        conn.endpoint.connect_more(&AttemptCancel::new()),
        Err(ConnectError::Io(_))
    ));
    assert!(matches!(
        transport.connect(&target(), &cancel),
        Err(ConnectError::Cancelled)
    ));
}

/// The side that starts the tunnel makes one entry per tunnel: the attach links, the bulk
/// channels on it and a second connection that shares it are all in that one entry. A
/// reconnection through a new tunnel is a new entry naming the old one. The far end makes an
/// `ssh_inbound` entry per channel, with the same link ID and edge value.
#[test]
fn a_tunnel_is_one_entry_with_its_links_and_channels() {
    let far = ConnectionTable::default();
    let server = echo_server_reporting(token('a'), Some(far.clone()));
    let fake = FakeSsh::new("entries");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let cancel = AttemptCancel::new();
    let options = ConnectOptions::new().with_name("box");
    let conn = transport
        .connect_with(&target(), &cancel, &options)
        .expect("connect");
    let link = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let bulk = link.open_bulk(None).expect("bulk");
    // A second connection to the same record shares the tunnel and its entry.
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let second = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    assert_eq!(fake.tunnels_started(), 1);

    let entries = table.connections();
    assert_eq!(entries.len(), 1, "{entries:#?}");
    let entry = &entries[0];
    assert_eq!(entry.path, PathKind::SshForward);
    assert_eq!(entry.direction, Direction::Forward);
    assert_eq!(entry.local_side, Side::Dialer);
    assert_eq!(entry.state, ConnectionState::Connected);
    assert_eq!(entry.phase, None);
    assert_eq!(entry.peer.name.as_deref(), Some("box"));
    assert_eq!(entry.peer.instance_id.as_deref(), Some("acceptor-1"));
    assert_eq!(entry.rtt, None);
    assert_eq!(
        entry.via,
        Via::Ssh {
            target: "fake-host".into(),
            remote_port: Some(server.addr.port()),
        }
    );
    let names: Vec<_> = entry.segments.iter().map(|s| s.name).collect();
    assert_eq!(names, ["local_forward", "ssh", "remote_listener"]);
    assert!(
        entry.segments[0]
            .local
            .as_deref()
            .is_some_and(|l| l.starts_with("127.0.0.1:")),
        "{:?}",
        entry.segments[0]
    );
    assert_eq!(entry.segments[1].remote.as_deref(), Some("fake-host"));
    assert_eq!(
        entry.segments[2].remote,
        Some(format!("127.0.0.1:{}", server.addr.port()))
    );
    let link_ids: Vec<_> = entry.links.iter().map(|l| l.link_id).collect();
    assert_eq!(link_ids, [link.link_id(), second.link_id()]);
    assert!(
        entry
            .links
            .iter()
            .all(|l| l.transport == EdgeTransport::SshForward && l.local_side == Side::Dialer)
    );
    assert_eq!(entry.bulk_channels, 1);
    let steps: Vec<_> = entry.history.iter().map(|h| (h.state, h.phase)).collect();
    assert_eq!(
        steps,
        [
            (ConnectionState::Connecting, Some(Phase::PortDiscovery)),
            (ConnectionState::Connecting, Some(Phase::Tunnel)),
            (ConnectionState::Connecting, Some(Phase::Handshake)),
            (ConnectionState::Connected, None),
        ]
    );

    // The far end saw three loopback channels: two links and the bulk channel.
    wait_until("the far end's entries", || far.connections().len() == 3);
    let far_entries = far.connections();
    let far_links: Vec<_> = far_entries.iter().flat_map(|e| e.links.clone()).collect();
    assert_eq!(far_links.len(), 2);
    for (near, far) in entry.links.iter().zip(&far_links) {
        assert_eq!(near.link_id, far.link_id);
        assert_eq!(
            near.transport, far.transport,
            "both ends give one edge value"
        );
        assert_eq!(far.local_side, Side::Acceptor);
    }
    assert!(far_entries.iter().all(|e| e.path == PathKind::SshInbound));

    // The tunnel ends with its last channel; a reconnection is a new entry naming it.
    let first = entry.id;
    drop((bulk, link, second));
    wait_until("the entry to end", || {
        table
            .connection(first)
            .is_some_and(|e| e.state == ConnectionState::Disconnected)
    });
    let ended = table.connection(first).expect("kept");
    assert!(ended.last_cause.is_some(), "{ended:#?}");
    let conn = transport
        .connect_with(&target(), &cancel, &ConnectOptions::new().after(first))
        .expect("reconnect");
    let again = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let entries = table.connections();
    assert_eq!(entries.len(), 2);
    let next = &entries[1];
    assert!(next.id > first);
    assert_eq!(next.previous, Some(first));
    assert!(!next.previous_dropped);
    assert_eq!(next.state, ConnectionState::Connected);
    assert_eq!(next.history[0].state, ConnectionState::Reconnecting);
    assert_eq!(next.links[0].link_id, again.link_id());
    assert_ne!(again.link_id(), link_ids[0], "a new link, a new ID");
}

/// A refused handshake ends both entries with its reason: the near one as the peer's
/// refusal, the far one as this end's. The near entry's last transition and its event carry
/// the same cause, also for a request channel and when the table keeps no closed entry.
#[test]
fn a_refused_handshake_ends_both_entries_with_the_reason() {
    let far = ConnectionTable::default();
    let server = echo_server_reporting(token('a'), Some(far.clone()));
    let fake = FakeSsh::new("refused-entry");
    fake.publish_record(server.addr.port(), token('b'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    assert!(dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).is_err());
    let rejected = Some(Cause::Rejected(RejectReason::TokenInvalid));
    // The tunnel's entry ended when the failed attempt let go of it.
    let entry = table.connections().pop().expect("the entry");
    assert_eq!(entry.state, ConnectionState::Disconnected);
    assert_eq!(entry.last_cause, rejected);
    let last = entry.history.last().expect("history");
    assert_eq!(
        (last.state, last.cause.clone()),
        (ConnectionState::Disconnected, rejected.clone())
    );
    assert_eq!(ended_causes(&table), std::slice::from_ref(&rejected));
    wait_until("the far entry to end", || {
        far.connections()
            .first()
            .is_some_and(|e| e.state == ConnectionState::Disconnected)
    });
    assert_eq!(
        far.connections()[0].last_cause,
        Some(Cause::Refused(RejectReason::TokenInvalid))
    );

    // A request channel, with no closed entry kept: only the event tells the end.
    let table = ConnectionTable::new(ConnectionTableConfig::new().with_keep_closed(0));
    let transport = fake.transport().with_connections(table.clone());
    let conn = transport.connect(&target(), &cancel).expect("connect");
    assert!(dial_request(conn, &client(), &cancel).is_err());
    assert!(table.connections().is_empty(), "no closed entry kept");
    assert_eq!(ended_causes(&table), [rejected]);
}

/// The causes of the `disconnected` transitions among the table's events.
fn ended_causes(table: &ConnectionTable) -> Vec<Option<Cause>> {
    table
        .events()
        .into_iter()
        .filter_map(|e| match e {
            ConnectionEvent::Transition { entry, .. }
                if entry.state == ConnectionState::Disconnected =>
            {
                Some(entry.cause)
            }
            _ => None,
        })
        .collect()
}

/// The entry of a redialed SSH connection shows the settings applied to it: the link
/// timeouts and the redial values, each `configured` where the consumer gave it and
/// `default` where not.
#[test]
fn ssh_connection_reports_applied_sources() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("applied");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    // The lost connection: an attempt whose ssh cannot start.
    let broken = SshTransport::new(
        SshLauncher::new(fake.script.with_extension("missing")),
        PortDiscoveryConfig {
            remote_program: "netprog".into(),
            remote_args: vec!["port".into()],
            data_dir: ".netprog".into(),
            port_file: "record".into(),
            explicit_port_file: None,
        },
        PortMode::FileUnix,
    )
    .with_connections(table.clone());
    let cancel = AttemptCancel::new();
    assert!(broken.connect(&target(), &cancel).is_err());
    let lost = table.connections()[0].id;

    let redialer = Redialer::new(
        &table,
        RedialerConfig::new().with_initial(Duration::from_millis(10)),
    )
    .expect("valid");
    // common::timeouts() keeps the default handshake timeout and changes the others.
    let link = redialer
        .redial(lost, None, &cancel, |options| {
            let conn = transport
                .connect_with(&target(), &cancel, options)
                .map_err(HandshakeError::Connect)?;
            dial_handshake(conn, &client(), Holder::Dialer, None, &cancel)
        })
        .expect("redialed");
    let entry = table.connections().pop().expect("the redialed entry");
    assert_eq!(entry.previous, Some(lost));
    assert_eq!(entry.state, ConnectionState::Connected);
    assert_eq!(entry.links[0].link_id, link.link_id());
    assert_eq!(entry.redial.as_ref().map(|r| r.attempt), Some(1));
    let source = |key: &str| {
        entry
            .applied
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.source.as_str())
            .unwrap_or_else(|| panic!("{key} is not applied: {:#?}", entry.applied))
    };
    assert_eq!(source("link.read_timeout"), "configured");
    assert_eq!(source("link.role_proposal_timeout"), "configured");
    assert_eq!(source("link.drain_cap"), "configured");
    assert_eq!(source("link.handshake_timeout"), "default");
    assert_eq!(source("redial.initial"), "configured");
    for key in [
        "redial.multiplier",
        "redial.max_delay",
        "redial.jitter",
        "redial.give_up_after",
        "redial.max_attempts",
    ] {
        assert_eq!(source(key), "default", "{key}");
    }
}

/// A transport whose ssh program does not exist, reporting in `table`: its attempts fail in
/// port discovery and give a lost entry to redial.
fn broken_transport(fake: &FakeSsh, table: &ConnectionTable) -> SshTransport {
    SshTransport::new(
        SshLauncher::new(fake.script.with_extension("missing")),
        PortDiscoveryConfig {
            remote_program: "netprog".into(),
            remote_args: vec!["port".into()],
            data_dir: ".netprog".into(),
            port_file: "record".into(),
            explicit_port_file: None,
        },
        PortMode::FileUnix,
    )
    .with_connections(table.clone())
}

/// A redial that joins a live shared tunnel keeps its own entry `reconnecting` until a
/// handshake on that tunnel succeeds: a failed join is retried, and when the attempts run out
/// the entry ends with `redial_exhausted`; a later attempt through a new tunnel brings the
/// same entry to `connected`.
#[test]
fn a_redial_that_joins_a_shared_tunnel_settles_after_its_handshake() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("redial-join");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let first = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let tunnel_entry = table.connections()[0].id;
    let broken = broken_transport(&fake, &table);
    assert!(broken.connect(&target(), &cancel).is_err());
    let lost = table.connections()[1].id;
    let redialer = Redialer::new(
        &table,
        RedialerConfig::new()
            .with_initial(Duration::from_millis(1))
            .with_jitter(0.0)
            .with_max_attempts(2),
    )
    .expect("valid");
    let pending_of = |table: &ConnectionTable| {
        table
            .connections()
            .into_iter()
            .filter(|e| e.previous == Some(lost))
            .collect::<Vec<_>>()
    };

    // Both attempts join the shared tunnel and fail before a handshake.
    let result = redialer.redial(lost, None, &cancel, |options| {
        let conn = transport
            .connect_with(&target(), &cancel, options)
            .map_err(HandshakeError::Connect)?;
        drop(conn);
        Err::<(), _>(HandshakeError::TimedOut)
    });
    assert!(
        matches!(result, Err(RedialError::Exhausted(_))),
        "{result:?}"
    );
    let pending = pending_of(&table);
    assert_eq!(pending.len(), 1, "{pending:#?}");
    assert_eq!(pending[0].state, ConnectionState::Disconnected);
    assert_eq!(pending[0].last_cause, Some(Cause::RedialExhausted));
    let shared = table.connection(tunnel_entry).expect("the tunnel's entry");
    assert_eq!(shared.state, ConnectionState::Connected);
    assert_eq!(shared.links.len(), 1);

    // The first attempt joins and fails; the shared tunnel then goes, and the second attempt
    // starts a new tunnel whose link comes up on the redial's entry.
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let conn = transport.connect(&target(), &cancel).expect("connect");
    drop(first);
    let mut kept =
        Some(dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake"));
    let old_tunnel = table.connections()[0].id;
    let broken = broken_transport(&fake, &table);
    assert!(broken.connect(&target(), &cancel).is_err());
    let lost = table.connections()[1].id;
    let redialer = Redialer::new(
        &table,
        RedialerConfig::new()
            .with_initial(Duration::from_millis(1))
            .with_jitter(0.0)
            .with_max_attempts(3),
    )
    .expect("valid");
    let mut attempts = 0;
    let link = redialer
        .redial(lost, None, &cancel, |options| {
            attempts += 1;
            let conn = transport
                .connect_with(&target(), &cancel, options)
                .map_err(HandshakeError::Connect)?;
            if attempts == 1 {
                drop(conn);
                drop(kept.take());
                wait_until("the shared tunnel to go", || transport.live_tunnels() == 0);
                return Err(HandshakeError::TimedOut);
            }
            dial_handshake(conn, &client(), Holder::Dialer, None, &cancel)
        })
        .expect("redialed");
    assert_eq!(attempts, 2);
    let pending = table
        .connections()
        .into_iter()
        .find(|e| e.previous == Some(lost))
        .expect("the redial's entry");
    assert_eq!(pending.state, ConnectionState::Connected, "{pending:#?}");
    assert_eq!(pending.links.len(), 1);
    assert_eq!(pending.links[0].link_id, link.link_id());
    let old = table.connection(old_tunnel).expect("kept");
    assert_eq!(old.state, ConnectionState::Disconnected);
}

#[test]
fn a_link_lost_under_a_live_tunnel_ends_as_lost_and_is_redialed() {
    let table = ConnectionTable::default();
    let fake = FakeSsh::new("lost-link");
    let cancel = AttemptCancel::new();
    let ended = |id| {
        wait_until("the entry ends", || {
            table
                .connection(id)
                .is_some_and(|e| e.state == ConnectionState::Disconnected)
        });
        table.connection(id).and_then(|e| e.last_cause)
    };

    // Contrast: a link this side drops without a failed read was closed here.
    let server = echo_server(token('a'));
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = fake.transport().with_connections(table.clone());
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let link = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let closed = table.connections()[0].id;
    drop(link);
    assert_eq!(ended(closed), Some(Cause::ClosedHere));

    // The far end closes the attach socket while the ssh child keeps running: the read
    // fails, and the consumer then drops the link as usual.
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let mut link =
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let lost = table.connections().last().expect("the second entry").id;
    let (mut reader, writer) = link.take_ordered().expect("the ordered channel");
    drop(server);
    assert!(reader.read_frame().is_err(), "the far end closed");
    assert!(!fake.tunnel_pids().is_empty(), "the ssh child still runs");
    drop((reader, writer, link));
    assert_eq!(ended(lost), Some(Cause::Lost));

    // A lost link is one to redial.
    let redialer = Redialer::new(
        &table,
        RedialerConfig::new()
            .with_initial(Duration::from_millis(1))
            .with_max_attempts(1),
    )
    .expect("valid");
    let mut attempted = false;
    let result = redialer.redial(lost, None, &cancel, |_| {
        attempted = true;
        Err::<(), _>(HandshakeError::TimedOut)
    });
    assert!(attempted, "{result:?}");
}

#[test]
fn a_parent_end_uncounts_a_bulk_channel_still_held() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("held-bulk");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let link = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    // A second link keeps the tunnel and its entry.
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let second = dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let bulk = link.open_bulk(None).expect("bulk");
    let id = table.connections()[0].id;
    assert_eq!(table.connection(id).map(|e| e.bulk_channels), Some(1));

    drop(link);
    wait_until("the bulk channel is no longer counted", || {
        table.connection(id).is_some_and(|e| e.bulk_channels == 0)
    });
    let entry = table.connection(id).expect("the entry");
    assert_eq!(
        entry.state,
        ConnectionState::Connected,
        "the second link keeps it"
    );
    assert_eq!(entry.links.len(), 1);
    drop((bulk, second));
}

#[test]
fn a_drain_cap_that_runs_out_ends_the_link_row_of_a_link_still_held() {
    let server = echo_server(token('a'));
    let fake = FakeSsh::new("drain-cap");
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let table = ConnectionTable::default();
    let transport = fake.transport().with_connections(table.clone());
    let cancel = AttemptCancel::new();
    let conn = transport.connect(&target(), &cancel).expect("connect");
    let mut link =
        dial_handshake(conn, &client(), Holder::Dialer, None, &cancel).expect("handshake");
    let id = table.connections()[0].id;
    assert_eq!(table.connection(id).map(|e| e.links.len()), Some(1));
    // The reader is kept and never read, so it does not end within the drain cap.
    let (reader, mut writer) = link.take_ordered().expect("the ordered channel");
    assert_eq!(writer.finish(), Finish::DrainCapReached { settled: true });
    let entry = table.connection(id).expect("the entry");
    assert!(entry.links.is_empty(), "{:?}", entry.links);
    assert_eq!(entry.last_cause, Some(Cause::Lost));
    drop((reader, writer, link));
}
