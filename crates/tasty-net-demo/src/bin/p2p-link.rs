//! `p2p-link`: one attach link over the P2P transport (docs/design/crates.md "tasty-net-demo").
//!
//! Two devices meet through a rendezvous server (`--server <addr>`, pinned by its public key
//! `--server-key <id>`) and punch a QUIC connection; handshake v2 runs on its streams through
//! the transport's P2P adapter (`P2pTransport`, `P2pListener`). Each device keeps its key in
//! `--key-dir` (made when missing) and takes connections only from the devices on its pairing
//! list (`--peer <device ID>`, as often as needed). `--id --key-dir <dir>` prints the device
//! ID of a key directory, so two people can exchange their IDs first.
//!
//! - `--listen`: the accepting end. On every attach link it provides the workspace: it sends
//!   one Control snapshot, then every Data frame comes back unchanged. A bulk channel under an
//!   attach link carries bulk file chunks (`tasty_net_frame::encode_bulk_chunk`); every chunk
//!   comes back unchanged, and at the channel's end the listener prints how many file bytes
//!   and chunks it read and their SHA-256. A request channel is refused (closed at once).
//! - `--dial <device ID>`: the holder. The target is paired too. While the peer is not
//!   registered yet (or the server is not reached yet) it asks again, up to `--patience-ms`
//!   (default 30000). Once the link is up, stdin lines drive it: `send <text>` sends a Data
//!   frame and the echo is printed; `bulk <bytes>` opens a bulk channel under the link, sends
//!   that many file bytes (a fixed pseudo-random sequence) in chunks of [`BULK_CHUNK_BYTES`],
//!   reads the echo back and compares; `detach` ends the link with a Detach and keeps the
//!   process up (so the connection table shows the ended entry). A failed link ends the
//!   dialer: exit code 3 when the peer refused (a pairing the peer or the server declined, a
//!   connection the peer closed with a code, a refusing ACK), 4 for any other failure.
//!
//! Both: the stdin line `table` prints the connection table in the lines of
//! `tasty_net_demo::table`, and `quit` or the end of stdin detaches a link that is up, turns
//! P2P off (closing the server connection) and ends with exit code 0. `--read-timeout-ms` is
//! the link's read timeout (default 30000, the same on both ends), `--instance-id` replaces
//! the instance ID (default `p2p-link-<pid>`).
//!
//! Test hook: `TASTY_NET_DEMO_UNPAIR_AFTER_OFFER=1` makes the listener's pairing list answer
//! "paired" only on the first check of each listed device: the `punch_offer` passes and the
//! QUIC connection that follows is closed with `not_paired` (docs/design/handshake.md "흐름
//! (P2P 경로)"), the path of a pairing withdrawn in between (`tests/p2p_attach.rs`).
//!
//! ```text
//! p2p-link --id --key-dir <dir>
//! p2p-link --listen --key-dir <dir> --server <addr> --server-key <id> [--peer <id>]...
//!          [--read-timeout-ms <ms>] [--instance-id <id>]
//! p2p-link --dial <id> --key-dir <dir> --server <addr> --server-key <id> [--peer <id>]...
//!          [--read-timeout-ms <ms>] [--instance-id <id>] [--patience-ms <ms>]
//! ```
//!
//! Events on stdout (one per line; `<who>` is `listener` or `dialer`; values that may hold
//! any text are encoded as in `tasty_net_demo::table`):
//!
//! | Line | When |
//! |------|------|
//! | `id device=<id>` | `--id` |
//! | `<who> ready pid=<pid> device=<id> instance=<id>` | the P2P endpoint is bound |
//! | `<who> registered` | the endpoint registered on the server (again after a reconnect) |
//! | `<who> attempt-closed id=<n> cause=<name>` | a P2P connection attempt or connection ended (its `tasty-net-p2p` number and cause) |
//! | `listener attached client=<n> peer=<device>` | the domain check took an attach link |
//! | `listener link-ended client=<n> end=<detach\|failure name>` | the link's reader ended |
//! | `listener bulk-accepted peer=<device>` | a bulk channel under an attach link is up |
//! | `listener bulk-ended bytes=<n> chunks=<n> sha256=<hex> end=<eof\|failure name\|bad_chunk>` | a bulk channel ended |
//! | `listener request-refused` | a request channel came; it is closed |
//! | `listener refused reason=<wire reason>` | this end refused a handshake |
//! | `listener handshake-failed error=<text>` | a handshake failed without a refusal |
//! | `dialer retry error=<text>` | the peer or the server is not reached yet; asked again |
//! | `dialer connected link_id=<hex> peer=<device> instance=<id>` | the link is up |
//! | `dialer control <json>` | a Control frame arrived (the listener's snapshot) |
//! | `dialer echo <text>` | a Data frame came back |
//! | `dialer link-ended end=<detach\|failure name>` | the link's reader ended |
//! | `dialer send-failed error=<text>` | a `send` line found no link, or writing failed |
//! | `dialer bulk-done bytes_sent=<n> bytes_received=<n> chunks=<n> chunk_bytes=<n> sha256_sent=<hex> sha256_received=<hex> match=<bool>` | a `bulk` transfer ended and its echo was read |
//! | `dialer bulk-failed error=<text>` | a `bulk` transfer did not finish |
//! | `dialer detached finish=<reader_ended\|drain_cap>` | `detach` (or `quit`) ended the link; whether the link's reader ended within the drain cap after the Detach |
//! | `dialer declined error=<text>` | the peer (or the server) declined the pairing; exit code 3 |
//! | `dialer closed reason=<close code name>` | the peer closed the new connection with a code (`not_paired`); exit code 3 |
//! | `dialer rejected reason=<reason>` | the peer refused with a refusing ACK; exit code 3 |
//! | `dialer failed error=<text>` | any other failure; exit code 4 |
//! | `<who> table ...` · `<who> conn ...` · `<who> link ...` · `<who> table-end ...` | the stdin line `table` (`tasty_net_demo::table`) |
//! | `<who> bye` | the process ends after `quit` |
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tasty_net_demo::table::{encode_value, table_lines};
use tasty_net_demo::{
    Args, Snapshot, UsageError, emit, emit_lines, error_chain, failure_name, link_timeouts,
};
use tasty_net_frame::{ReadFailure, StreamTag, decode_bulk_chunk, encode_bulk_chunk};
use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::{
    AttemptPhase, EndpointPhase, P2pConfig, P2pConnectError, P2pEndpoint, P2pEvent, PairingList,
    PairingStore, Phase, RendezvousServer, Subject,
};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkChannel, BulkRequest, ChannelReader,
    ChannelWriter, ConnectError, ConnectionTable, DialConfig, DomainAccept, DomainPolicy,
    DomainReject, Finish, HandshakeError, Holder, Link, LinkTimeouts, P2pListener, P2pTransport,
    Transport, dial_handshake,
};

const USAGE: &str = "usage: p2p-link --id --key-dir <dir>
       p2p-link --listen --key-dir <dir> --server <addr> --server-key <id> [--peer <id>]... \
[--read-timeout-ms <ms>] [--instance-id <id>]
       p2p-link --dial <id> --key-dir <dir> --server <addr> --server-key <id> [--peer <id>]... \
[--read-timeout-ms <ms>] [--instance-id <id>] [--patience-ms <ms>]";

/// The file bytes of one bulk chunk the dialer sends.
const BULK_CHUNK_BYTES: usize = 64 * 1024;
/// The transfer number of the dialer's bulk chunks (one transfer per bulk channel).
const TRANSFER_ID: u64 = 1;
/// The default of `--patience-ms`.
const DEFAULT_PATIENCE: Duration = Duration::from_secs(30);
/// The wait between two connects while the peer is not reached yet.
const ASK_AGAIN: Duration = Duration::from_millis(200);
/// The default of `--read-timeout-ms`.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one wait for the next P2P event lasts before the event thread looks again.
const EVENT_WAIT: Duration = Duration::from_secs(1);
/// The test hook of the module text.
const UNPAIR_AFTER_OFFER_ENV: &str = "TASTY_NET_DEMO_UNPAIR_AFTER_OFFER";

/// The peer (or the server) refused: no point in connecting again.
const EXIT_REFUSED: u8 = 3;
/// Any other failure of the dialer.
const EXIT_FAILED: u8 = 4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("p2p-link: {}", error_chain(error.as_ref()));
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

type Failure = Box<dyn std::error::Error>;

fn usage(text: impl Into<String>) -> Failure {
    Box::new(UsageError(text.into()))
}

fn device_id(option: &str, text: &str) -> Result<DeviceId, Failure> {
    text.parse()
        .map_err(|e| usage(format!("--{option} {text:?} is no device ID: {e}")))
}

fn run() -> Result<ExitCode, Failure> {
    let args = Args::parse(std::env::args_os().skip(1), &["id", "listen"])?;
    args.check_known(&[
        "id",
        "listen",
        "dial",
        "key-dir",
        "server",
        "server-key",
        "peer",
        "read-timeout-ms",
        "instance-id",
        "patience-ms",
    ])?;
    let key_dir = PathBuf::from(args.required("key-dir")?);
    let key = DeviceKey::load_or_create(&key_dir)?;
    if args.flag("id") {
        emit(&format!("id device={}", key.id()));
        return Ok(ExitCode::SUCCESS);
    }
    let dial = args
        .value("dial")
        .map(|text| device_id("dial", text))
        .transpose()?;
    let who = match (args.flag("listen"), dial) {
        (true, None) => "listener",
        (false, Some(_)) => "dialer",
        _ => return Err(usage("give one of --id, --listen or --dial <device ID>")),
    };
    let server_addr: SocketAddr = {
        let text = args.required("server")?;
        text.parse()
            .map_err(|e| usage(format!("--server {text:?} is no socket address: {e}")))?
    };
    let server = RendezvousServer {
        addr: server_addr,
        public_key: device_id("server-key", args.required("server-key")?)?,
    };
    let mut peers = args
        .values("peer")
        .into_iter()
        .map(|text| device_id("peer", text))
        .collect::<Result<Vec<_>, _>>()?;
    peers.extend(dial);
    let timeouts = link_timeouts(args.millis("read-timeout-ms", DEFAULT_READ_TIMEOUT)?);
    let instance_id = args.value("instance-id").map_or_else(
        || format!("p2p-link-{}", std::process::id()),
        str::to_string,
    );

    let pairing = Arc::new(Pairing::new(
        &peers,
        std::env::var_os(UNPAIR_AFTER_OFFER_ENV).is_some_and(|v| v == "1"),
    ));
    let mut config = P2pConfig::new(key.clone(), pairing);
    config.enabled = true;
    config.key_dir = Some(key_dir);
    config.server = Some(server);
    let endpoint = P2pEndpoint::bind(config)?;
    let device = endpoint.id().to_string();
    emit(&format!(
        "{who} ready pid={} device={device} instance={}",
        std::process::id(),
        encode_value(&instance_id)
    ));
    let events = watch_events(who, endpoint.clone())?;
    let table = ConnectionTable::default();
    let end = Ends {
        who,
        device,
        endpoint,
        table,
        events,
    };
    match dial {
        None => listen(end, key, instance_id, timeouts),
        Some(target) => {
            let patience = args.millis("patience-ms", DEFAULT_PATIENCE)?;
            Ok(dial_link(end, key, instance_id, timeouts, target, patience))
        }
    }
}

/// What both ends share.
struct Ends {
    who: &'static str,
    device: String,
    endpoint: P2pEndpoint,
    table: ConnectionTable,
    events: JoinHandle<()>,
}

impl Ends {
    fn print_table(&self) {
        emit_lines(&table_lines(
            self.who,
            &self.device,
            &self.table.connections(),
        ));
    }

    /// Turns P2P off and waits for the event thread, which ends with the endpoint.
    fn close(self) {
        self.endpoint.close();
        if self.events.join().is_err() {
            emit(&format!("{} event-thread-panicked", self.who));
        }
    }
}

/// The pairing list of `--peer` (and the dial target), with the test hook of the module text.
#[derive(Debug)]
struct Pairing {
    list: PairingList,
    /// The hook is on: a listed device is paired at its first check only.
    once: bool,
    checked: Mutex<HashSet<DeviceId>>,
}

impl Pairing {
    fn new(peers: &[DeviceId], once: bool) -> Self {
        let list = PairingList::new();
        for peer in peers {
            list.add(*peer);
        }
        Self {
            list,
            once,
            checked: Mutex::default(),
        }
    }
}

impl PairingStore for Pairing {
    fn is_paired(&self, device: &DeviceId) -> bool {
        if !self.list.is_paired(device) {
            return false;
        }
        // With the hook, only the first check of a device finds it paired.
        !self.once || lock(&self.checked).insert(*device)
    }
}

/// Prints the registrations and the ends of the P2P attempts of `endpoint` until it is off.
fn watch_events(who: &'static str, endpoint: P2pEndpoint) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("p2p-events".into())
        .spawn(move || {
            loop {
                match endpoint.next_event(EVENT_WAIT) {
                    Ok(Some(P2pEvent::Transition {
                        subject, to, cause, ..
                    })) => match (subject, to) {
                        (Subject::Endpoint, Phase::Endpoint(EndpointPhase::Registered)) => {
                            emit(&format!("{who} registered"));
                        }
                        (Subject::Connection(id), Phase::Attempt(AttemptPhase::Closed)) => {
                            emit(&format!(
                                "{who} attempt-closed id={id} cause={}",
                                encode_value(cause.name)
                            ));
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
}

/// Reads stdin lines into a channel, so the main thread can wait on them; the channel ends
/// with stdin.
fn stdin_lines() -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        // Without the thread the channel is closed at once: the process ends as on `quit`.
        emit(&format!("stdin-thread-failed error={error}"));
    }
    rx
}

// ----- listener -----

/// Numbers the attach links and prints the domain check's decisions.
#[derive(Default)]
struct Policy {
    next_client: AtomicU32,
}

impl DomainPolicy for Policy {
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        let client = self.next_client.fetch_add(1, Ordering::SeqCst) + 1;
        emit(&format!(
            "listener attached client={client} peer={}",
            request.peer().device_id()
        ));
        Ok(DomainAccept {
            client_id: Some(client),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        emit(&format!(
            "listener bulk-accepted peer={}",
            request.parent().device_id()
        ));
        Ok(DomainAccept::default())
    }
}

fn listen(
    end: Ends,
    key: DeviceKey,
    instance_id: String,
    timeouts: LinkTimeouts,
) -> Result<ExitCode, Failure> {
    let accept = AcceptConfig::new(key, instance_id.clone(), None, Arc::new(Policy::default()))
        .with_timeouts(timeouts)
        .with_connections(end.table.clone());
    let listener = P2pListener::new(end.endpoint.clone(), accept)?;
    let accepting = std::thread::Builder::new()
        .name("accept".into())
        .spawn(move || {
            // Fails once the endpoint is off.
            while let Ok(inbound) = listener.accept() {
                let instance_id = instance_id.clone();
                let spawned = std::thread::Builder::new()
                    .name("serve".into())
                    .spawn(move || serve(inbound.handshake(&AttemptCancel::new()), &instance_id));
                if let Err(error) = spawned {
                    emit(&format!("listener handshake-failed error={error}"));
                }
            }
        })?;
    for line in stdin_lines() {
        match line.as_str() {
            "quit" => break,
            "table" => end.print_table(),
            _ => {}
        }
    }
    end.close();
    if accepting.join().is_err() {
        emit("listener accept-thread-panicked");
    }
    emit("listener bye");
    Ok(ExitCode::SUCCESS)
}

fn serve(accepted: Result<Accepted, HandshakeError>, instance_id: &str) {
    match accepted {
        Ok(Accepted::Link(link)) => serve_link(*link, instance_id),
        Ok(Accepted::Bulk(bulk)) => serve_bulk(bulk),
        Ok(Accepted::Request(server)) => {
            emit("listener request-refused");
            drop(server);
        }
        Err(HandshakeError::Refused(reason)) => {
            emit(&format!("listener refused reason={}", reason.as_str()));
        }
        Err(error) => emit(&format!(
            "listener handshake-failed error={}",
            encode_value(&error_chain(&error))
        )),
    }
}

/// Provides the workspace of a link: one Control snapshot, then every Data frame back.
fn serve_link(mut link: Link, instance_id: &str) {
    let client = link.client_id().unwrap_or(0);
    let Some((mut reader, mut writer)) = link.take_ordered() else {
        return;
    };
    let snapshot = Snapshot {
        instance_id: instance_id.to_string(),
        client_id: client,
    }
    .to_json();
    let mut detached = false;
    let end = match writer.write_frame(StreamTag::Control, snapshot.as_bytes()) {
        Err(error) => format!("write_failed:{}", error.kind()),
        Ok(()) => loop {
            match reader.read_frame() {
                Ok(frame) => match frame.tag {
                    StreamTag::Data if !detached => {
                        if let Err(error) = writer.write_frame(StreamTag::Data, &frame.payload) {
                            break format!("write_failed:{}", error.kind());
                        }
                    }
                    // The accepting end finishes its side only once it read the dialer's EOF
                    // (docs/design/transport.md "P2P의 닫기 범위"), so after the Detach it reads
                    // on to the EOF that follows it.
                    StreamTag::Detach => detached = true,
                    _ => {}
                },
                Err(error) if detached && error.failure() == ReadFailure::PeerClosed => {
                    break "detach".to_string();
                }
                Err(error) => break failure_name(error.failure()).to_string(),
            }
        },
    };
    emit(&format!("listener link-ended client={client} end={end}"));
    writer.finish();
    drop(link);
}

/// Reads the bulk file chunks of one bulk channel and writes each back; at the channel's end
/// prints what was read.
fn serve_bulk(mut bulk: BulkChannel) {
    let mut digest = Sha256::new();
    let (mut bytes, mut chunks) = (0usize, 0u32);
    let end = loop {
        match bulk.reader.read_frame() {
            // The transfer ends with the dialer's EOF, which this end reads before it finishes
            // its side (docs/design/transport.md "P2P의 닫기 범위").
            Ok(frame) if frame.tag == StreamTag::Data => {
                match decode_bulk_chunk(&frame.payload) {
                    Some((TRANSFER_ID, seq, file)) if seq == chunks => {
                        digest.update(file);
                        bytes += file.len();
                        chunks += 1;
                    }
                    _ => break "bad_chunk".to_string(),
                }
                if let Err(error) = bulk.writer.write_frame(frame.tag, &frame.payload) {
                    break format!("write_failed:{}", error.kind());
                }
            }
            Ok(_) => {}
            Err(error) if error.failure() == ReadFailure::PeerClosed => break "eof".to_string(),
            Err(error) => break failure_name(error.failure()).to_string(),
        }
    };
    bulk.writer.finish();
    emit(&format!(
        "listener bulk-ended bytes={bytes} chunks={chunks} sha256={} end={end}",
        hex(&digest.finalize())
    ));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ----- dialer -----

/// The ordered channel of the link that is up, and the thread reading it.
struct Up {
    link: Link,
    writer: ChannelWriter,
    reader: JoinHandle<()>,
}

impl Up {
    /// Ends the link with a Detach, waits for its reader and drops it.
    fn detach(mut self) {
        if let Err(error) = self.writer.write_frame(StreamTag::Detach, &[]) {
            emit(&format!(
                "dialer send-failed error={}",
                encode_value(&error.to_string())
            ));
        }
        let finish = match self.writer.finish() {
            Finish::ReaderEnded => "reader_ended",
            Finish::DrainCapReached { .. } => "drain_cap",
        };
        if self.reader.join().is_err() {
            emit("dialer reader-panicked");
        }
        drop(self.writer);
        drop(self.link);
        emit(&format!("dialer detached finish={finish}"));
    }
}

fn dial_link(
    end: Ends,
    key: DeviceKey,
    instance_id: String,
    timeouts: LinkTimeouts,
    target: DeviceId,
    patience: Duration,
) -> ExitCode {
    let transport = P2pTransport::new(end.endpoint.clone()).with_connections(end.table.clone());
    let dial = DialConfig::new(key, instance_id).with_timeouts(timeouts);
    let linked = connect(&transport, target, patience)
        .map_err(HandshakeError::Connect)
        .and_then(|conn| dial_handshake(conn, &dial, Holder::Dialer, None, &AttemptCancel::new()));
    let mut link = match linked {
        Ok(link) => link,
        Err(error) => {
            let code = report_failure(&error);
            drop(transport);
            end.close();
            return ExitCode::from(code);
        }
    };
    emit(&format!(
        "dialer connected link_id={} peer={} instance={}",
        link.link_id(),
        link.peer().device_id(),
        encode_value(link.peer().instance_id())
    ));
    let mut up = link.take_ordered().and_then(|(reader, writer)| {
        let reading = std::thread::Builder::new()
            .name("reader".into())
            .spawn(move || read_link(reader));
        match reading {
            Ok(reader) => Some(Up {
                link,
                writer,
                reader,
            }),
            Err(error) => {
                emit(&format!("dialer failed error={error}"));
                None
            }
        }
    });
    for line in stdin_lines() {
        if line == "quit" {
            break;
        }
        if line == "table" {
            end.print_table();
        } else if line == "detach" {
            if let Some(up) = up.take() {
                up.detach();
            }
        } else if let Some(text) = line.strip_prefix("send ") {
            let sent = up
                .as_ref()
                .map(|up| up.writer.write_frame(StreamTag::Data, text.as_bytes()));
            match sent {
                Some(Ok(())) => {}
                Some(Err(error)) => emit(&format!(
                    "dialer send-failed error={}",
                    encode_value(&error.to_string())
                )),
                None => emit("dialer send-failed error=no%20link"),
            }
        } else if let Some(size) = line.strip_prefix("bulk ") {
            match (size.parse::<usize>(), &up) {
                (Ok(size), Some(up)) => match bulk_transfer(&up.link, size) {
                    Ok(line) => emit(&line),
                    Err(error) => emit(&format!(
                        "dialer bulk-failed error={}",
                        encode_value(&error)
                    )),
                },
                (Err(error), _) => emit(&format!(
                    "dialer bulk-failed error={}",
                    encode_value(&format!("{size:?}: {error}"))
                )),
                (_, None) => emit("dialer bulk-failed error=no%20link"),
            }
        }
    }
    if let Some(up) = up.take() {
        up.detach();
    }
    drop(transport);
    end.close();
    emit("dialer bye");
    ExitCode::SUCCESS
}

/// A channel to `target`, asking again while the peer or the server is not reached yet.
fn connect(
    transport: &P2pTransport,
    target: DeviceId,
    patience: Duration,
) -> Result<tasty_net_transport::Connection, ConnectError> {
    let started = Instant::now();
    loop {
        match transport.connect(&target, &AttemptCancel::new()) {
            Err(ConnectError::P2p(error))
                if started.elapsed() + ASK_AGAIN < patience
                    && matches!(
                        error.connect_error(),
                        Some(
                            P2pConnectError::PeerNotFound
                                | P2pConnectError::RendezvousUnavailable { .. }
                        )
                    ) =>
            {
                emit(&format!(
                    "dialer retry error={}",
                    encode_value(&error.to_string())
                ));
                std::thread::sleep(ASK_AGAIN);
            }
            other => return other,
        }
    }
}

/// Prints why the link did not come up; the exit code.
fn report_failure(error: &HandshakeError) -> u8 {
    match error {
        HandshakeError::Connect(ConnectError::P2p(p2p))
            if matches!(p2p.connect_error(), Some(P2pConnectError::Declined)) =>
        {
            emit(&format!(
                "dialer declined error={}",
                encode_value(&p2p.to_string())
            ));
            EXIT_REFUSED
        }
        HandshakeError::Closed(reason) => {
            emit(&format!("dialer closed reason={}", reason.name()));
            EXIT_REFUSED
        }
        HandshakeError::Rejected { reason, .. } => {
            emit(&format!("dialer rejected reason={}", reason.as_str()));
            EXIT_REFUSED
        }
        other => {
            emit(&format!(
                "dialer failed error={}",
                encode_value(&error_chain(other))
            ));
            EXIT_FAILED
        }
    }
}

/// Prints every frame of the link until its reader ends.
fn read_link(mut reader: ChannelReader) {
    let end = loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Control => emit(&format!(
                    "dialer control {}",
                    String::from_utf8_lossy(&frame.payload)
                )),
                StreamTag::Data => emit(&format!(
                    "dialer echo {}",
                    String::from_utf8_lossy(&frame.payload)
                )),
                StreamTag::Detach => {
                    reader.finish();
                    break "detach".to_string();
                }
                _ => {}
            },
            Err(error) => break failure_name(error.failure()).to_string(),
        }
    };
    emit(&format!("dialer link-ended end={end}"));
}

/// The file bytes of a `bulk` transfer: a fixed pseudo-random sequence (xorshift), so no
/// chunk equals another.
struct Pattern(u64);

impl Pattern {
    fn new() -> Self {
        Self(0x9E37_79B9_7F4A_7C15)
    }

    fn fill(&mut self, out: &mut [u8]) {
        for byte in out {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            *byte = self.0.to_le_bytes()[0];
        }
    }
}

/// Sends `size` file bytes on a new bulk channel under `link` and reads the chunks back.
/// Returns the `dialer bulk-done` line.
fn bulk_transfer(link: &Link, size: usize) -> Result<String, String> {
    let bulk = link.open_bulk(None).map_err(|e| error_chain(&e))?;
    let BulkChannel {
        mut reader,
        mut writer,
        ..
    } = bulk;
    let sender = std::thread::Builder::new()
        .name("bulk-send".into())
        .spawn(move || -> Result<(u32, [u8; 32]), String> {
            let mut pattern = Pattern::new();
            let mut digest = Sha256::new();
            let mut left = size;
            let mut seq = 0u32;
            let mut chunk = vec![0u8; BULK_CHUNK_BYTES];
            while left > 0 {
                let len = left.min(BULK_CHUNK_BYTES);
                pattern.fill(&mut chunk[..len]);
                digest.update(&chunk[..len]);
                writer
                    .write_frame(
                        StreamTag::Data,
                        &encode_bulk_chunk(TRANSFER_ID, seq, &chunk[..len]),
                    )
                    .map_err(|e| format!("writing chunk {seq}: {e}"))?;
                left -= len;
                seq += 1;
            }
            writer
                .half_close()
                .map_err(|e| format!("half close: {e}"))?;
            Ok((seq, digest.finalize().into()))
        })
        .map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let (mut received, mut chunks) = (0usize, 0u32);
    let read = loop {
        match reader.read_frame() {
            Ok(frame) if frame.tag == StreamTag::Data => match decode_bulk_chunk(&frame.payload) {
                Some((TRANSFER_ID, seq, file)) if seq == chunks => {
                    digest.update(file);
                    received += file.len();
                    chunks += 1;
                }
                _ => break Err(format!("chunk {chunks} came back changed")),
            },
            Ok(_) => {}
            Err(error) if error.failure() == ReadFailure::PeerClosed => break Ok(()),
            Err(error) => break Err(format!("reading: {}", failure_name(error.failure()))),
        }
    };
    reader.finish();
    let sent = sender
        .join()
        .map_err(|_| "the bulk sender panicked".to_string())?;
    read?;
    let (sent_chunks, sent_digest) = sent?;
    let received_digest: [u8; 32] = digest.finalize().into();
    if sent_chunks != chunks {
        return Err(format!("{sent_chunks} chunks sent, {chunks} came back"));
    }
    Ok(format!(
        "dialer bulk-done bytes_sent={size} bytes_received={received} chunks={chunks} \
         chunk_bytes={BULK_CHUNK_BYTES} sha256_sent={} sha256_received={} match={}",
        hex(&sent_digest),
        hex(&received_digest),
        size == received && sent_digest == received_digest
    ))
}
