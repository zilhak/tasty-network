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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tasty_net_frame::StreamTag;
use tasty_net_identity::{ConnectionRecord, ConnectionToken};
use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SpawnHook, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, ConnectError, HandshakeError, Holder, RejectReason, SshTransport,
    TooOldEvidence, Transport, TransportKind, dial_handshake,
};

use common::{client, echo_server, token};

const FAKE_ARGS_ENV: &str = "TASTY_NET_TRANSPORT_FAKE_SSH_ARGS";
const FORWARDER_TEST: &str = "fake_ssh_forwarder_entry";
/// How long the fake forwarder waits before it binds its local port (the tunnel's readiness).
const FAKE_DELAY_ENV: &str = "TASTY_NET_TRANSPORT_FAKE_SSH_BIND_DELAY_MS";

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
}

impl FakeSsh {
    fn new(name: &str) -> Self {
        Self::with_bind_delay(name, Duration::ZERO)
    }

    /// A fake whose tunnels become ready only `delay` after they start.
    fn with_bind_delay(name: &str, delay: Duration) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "tasty-net-transport-fake-ssh-{}-{seq}-{name}",
            std::process::id()
        ));
        let script = base.with_extension("sh");
        let record = base.with_extension("record");
        let tunnels = base.with_extension("tunnels");
        let exe = std::env::current_exe().expect("test binary path");
        write_file(
            &script,
            &format!(
                "#!/bin/sh\ncase \" $* \" in *\" -L \"*)\necho tunnel >> '{tunnels}'\n\
                 {FAKE_ARGS_ENV}=\"$*\"\nexport {FAKE_ARGS_ENV}\n\
                 {FAKE_DELAY_ENV}={delay}\nexport {FAKE_DELAY_ENV}\n\
                 exec '{exe}' {FORWARDER_TEST} --exact --nocapture --test-threads=1 >/dev/null;;\n\
                 esac\ncat '{record}'\n",
                tunnels = tunnels.display(),
                record = record.display(),
                exe = exe.display(),
                delay = delay.as_millis(),
            ),
            "700",
        );
        Self {
            script,
            record,
            tunnels,
            pids: Arc::new(Mutex::new(Vec::new())),
        }
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
        for path in [&self.script, &self.record, &self.tunnels] {
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
    if let Some(ms) = std::env::var(FAKE_DELAY_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
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
    let fake = FakeSsh::with_bind_delay("prepare", Duration::from_millis(1500));
    fake.publish_record(server.addr.port(), token('a'), "instance-1");
    let transport = Arc::new(fake.transport());
    let cancel = AttemptCancel::new();
    let attempt = {
        let transport = Arc::clone(&transport);
        let cancel = cancel.clone();
        std::thread::spawn(move || transport.connect(&target(), &cancel).map(drop))
    };
    // The attempt is still inside the preparation when the cancel comes.
    wait_until("the tunnel to start", || {
        fake.tunnels_started() == 1 || attempt.is_finished()
    });
    assert!(
        !attempt.is_finished(),
        "{:?}",
        attempt.join().expect("attempt")
    );
    cancel.cancel();
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
