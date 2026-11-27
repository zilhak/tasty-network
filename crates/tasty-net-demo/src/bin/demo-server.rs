//! `demo-server`: the accepting end of an attach, reduced to an echo.
//!
//! It listens on a loopback port, publishes its connection record (port, token, instance ID)
//! at `--record`, and runs the accepting handshake v2 on every connection.
//!
//! - Forward link (`holder = "dialer"`): this end provides the workspace. It sends one Control
//!   snapshot, then every Data frame comes back unchanged.
//! - Role reversal (`holder = "acceptor"`, docs/design/transport.md "역할" (b)): the dialer
//!   proposes that this end holds. `--proposals accept` accepts every proposal of an
//!   authenticated peer, `refuse` (the default) refuses it with `role_refused`. On an accepted
//!   link the dialer provides the workspace: this end waits for its Control descriptor, sends
//!   every stdin line `send <text>` as a Data frame on the newest such link, and prints what
//!   comes back.
//!
//! A bulk channel under an attach link is accepted and every frame on it comes back unchanged.
//! A request channel answers `system.info` with this end's instance ID and refuses every
//! other method.
//!
//! `--instance-id` replaces the instance ID (default `demo-server-<pid>-<random hex>`, fresh
//! per start); a test gives one with characters that the event values must encode.
//!
//! A Detach frame ends a link. The server ends when its stdin closes or reads `quit`, and then
//! retires its record. The stdin line `table` prints the connection table (one `ssh_inbound`
//! entry per accepted channel) in the lines of `tasty_net_demo::table`.
//!
//! ```text
//! demo-server --record <path> [--proposals accept|refuse] [--read-timeout-ms <ms>]
//!             [--instance-id <id>]
//! ```
//!
//! Events on stdout (one per line):
//!
//! | Line | When |
//! |------|------|
//! | `server ready pid=<pid> port=<port> instance=<id>` | the record is published (the value encoded as in `tasty_net_demo::table`) |
//! | `server proposal peer=<instance> answer=<accept\|refuse>` | a peer proposed that this end holds (the value encoded as in `tasty_net_demo::table`) |
//! | `server attached client=<n> live=<links> peer=<instance>` | the domain check took an attach link (the value encoded as in `tasty_net_demo::table`) |
//! | `server roles client=<n> local=acceptor launcher=<side> dialer=<side> provider=<side> holder=<side> redialer=<side>` | an attach link is up (`<side>` is `dialer` or `acceptor`) |
//! | `server control client=<n> <json>` | the descriptor of a link this end holds arrived |
//! | `server control-invalid client=<n> error=<text>` | the first Control of a link this end would hold was not a descriptor; the link ends |
//! | `server echo client=<n> <text>` | a Data frame came back on a link this end holds |
//! | `server send-failed error=<text>` | a `send` line found no held link, or writing failed |
//! | `server link-ended client=<n> end=<detach\|failure name>` | the link's reader ended |
//! | `server released client=<n> live=<links>` | the link's registration was dropped |
//! | `server bulk-accepted peer=<instance>` | a bulk channel under an attach link is up (the value encoded as in `tasty_net_demo::table`) |
//! | `server bulk-ended end=<failure name or detach>` | the bulk channel's reader ended |
//! | `server request method=<method>` | a request channel answered a request (the value encoded as in `tasty_net_demo::table`) |
//! | `server request-failed error=<text>` | serving a request channel failed; `server request-ended` follows |
//! | `server request-ended` | a request channel ended |
//! | `server table ...` · `server conn ...` · `server link ...` · `server table-end ...` | the stdin line `table` (`tasty_net_demo::table`) |
//! | `server refused reason=<wire reason>` | this end refused a handshake |
//! | `server handshake-failed error=<text>` | a handshake failed without a refusal |
//! | `server bye` | the server ends |
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tasty_net_demo::table::{encode_value, table_lines};
use tasty_net_demo::{Args, Snapshot, emit, emit_lines, error_chain, failure_name, link_timeouts};
use tasty_net_frame::StreamTag;
use tasty_net_identity::{ConnectionRecord, ConnectionToken, DeviceKey, RecordPublication};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkChannel, BulkRequest, ChannelReader,
    ChannelWriter, ConnectionTable, DomainAccept, DomainPolicy, DomainReject, HandshakeError, Link,
    MAX_HANDSHAKE_LINE, PeerInfo, RawChannel, RequestHandler, RequestServer, RoleProposal, Side,
    accept_handshake, read_first_line,
};

const USAGE: &str = "usage: demo-server --record <path> [--proposals accept|refuse] [--read-timeout-ms <ms>] \
[--instance-id <id>]";

/// The writers of the links this end holds, newest last, for the stdin `send` lines.
type Held = Arc<Mutex<Vec<(u32, ChannelWriter)>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The name of a side in event lines.
fn side_name(side: Side) -> &'static str {
    match side {
        Side::Dialer => "dialer",
        Side::Acceptor => "acceptor",
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("demo-server: {}", error_chain(error.as_ref()));
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// Counts the attach links whose registration is alive. The count changes and its line is
/// printed under one lock, so the printed `live=` values come in the order the count changed.
#[derive(Default)]
struct LivePolicy {
    next_client: AtomicU32,
    live: Arc<Mutex<usize>>,
}

/// The registration of one attach link: the link holds it and drops it when it ends.
struct Registration {
    client: u32,
    live: Arc<Mutex<usize>>,
}

fn lock_live(live: &Mutex<usize>) -> std::sync::MutexGuard<'_, usize> {
    live.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut live = lock_live(&self.live);
        *live -= 1;
        emit(&format!(
            "server released client={} live={}",
            self.client, *live
        ));
    }
}

impl DomainPolicy for LivePolicy {
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        let client = self.next_client.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut live = lock_live(&self.live);
            *live += 1;
            emit(&format!(
                "server attached client={client} live={} peer={}",
                *live,
                encode_value(request.peer().instance_id())
            ));
        }
        Ok(DomainAccept {
            client_id: Some(client),
            guard: Some(Box::new(Registration {
                client,
                live: Arc::clone(&self.live),
            })),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        emit(&format!(
            "server bulk-accepted peer={}",
            encode_value(request.parent().instance_id())
        ));
        Ok(DomainAccept::default())
    }
}

/// The request methods of the demo: `system.info` gives this end's instance ID.
struct Requests {
    instance_id: String,
}

impl RequestHandler for Requests {
    fn handle(
        &self,
        _peer: &PeerInfo,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Value> {
        emit(&format!("server request method={}", encode_value(method)));
        match method {
            "system.info" => Ok(serde_json::json!({ "instance_id": self.instance_id })),
            _ => Err(serde_json::json!({ "code": "demo.unknown_method" })),
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1), &[])?;
    args.check_known(&["record", "proposals", "read-timeout-ms", "instance-id"])?;
    let record_path = PathBuf::from(args.required("record")?);
    let accept_proposals = match args.value("proposals").unwrap_or("refuse") {
        "accept" => true,
        "refuse" => false,
        _ => {
            return Err(Box::new(tasty_net_demo::UsageError(
                "--proposals needs accept or refuse".into(),
            )));
        }
    };
    let timeouts = link_timeouts(args.millis("read-timeout-ms", Duration::from_secs(30))?);

    let key = DeviceKey::generate()?;
    let device = key.id().to_string();
    let token = ConnectionToken::issue()?;
    // A fresh ID per start, so a dialer can tell a restarted server from the old one.
    let instance_id = match args.value("instance-id") {
        Some(id) => id.to_string(),
        None => {
            let nonce = ConnectionToken::issue()?;
            format!(
                "demo-server-{}-{}",
                std::process::id(),
                &nonce.expose_hex()[..8]
            )
        }
    };

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let policy = Arc::new(LivePolicy::default());
    let connections = ConnectionTable::default();
    let config = AcceptConfig::new(key, instance_id.clone(), Some(token.clone()), policy)
        .with_timeouts(timeouts)
        .with_connections(connections.clone())
        .with_role_proposals(move |proposal| answer(proposal, accept_proposals));
    let record = ConnectionRecord {
        port,
        token,
        instance_id: instance_id.clone(),
    };
    let publication = Arc::new(Mutex::new(Some(record.publish(&record_path)?)));
    emit(&format!(
        "server ready pid={} port={port} instance={}",
        std::process::id(),
        encode_value(&instance_id)
    ));

    let held: Held = Arc::default();
    {
        let publication = Arc::clone(&publication);
        let held = Arc::clone(&held);
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || watch_stdin(&publication, &held, &connections, &device))?;
    }

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                emit(&format!("server accept-failed error={error}"));
                continue;
            }
        };
        let config = config.clone();
        let instance_id = instance_id.clone();
        let held = Arc::clone(&held);
        std::thread::Builder::new()
            .name("serve".into())
            .spawn(move || serve(stream, &config, &instance_id, &held))?;
    }
    Ok(())
}

/// Answers a role reversal proposal of an authenticated peer.
fn answer(proposal: RoleProposal, accept: bool) {
    emit(&format!(
        "server proposal peer={} answer={}",
        encode_value(proposal.peer().instance_id()),
        if accept { "accept" } else { "refuse" }
    ));
    if accept {
        proposal.accept();
    } else {
        proposal.refuse();
    }
}

/// Sends `send <text>` lines on the newest held link and prints the connection table on
/// `table`. Ends the process when stdin closes or reads `quit`, after retiring the record.
fn watch_stdin(
    publication: &Mutex<Option<RecordPublication>>,
    held: &Held,
    connections: &ConnectionTable,
    device: &str,
) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end();
        if line == "quit" {
            break;
        }
        if line == "table" {
            emit_lines(&table_lines("server", device, &connections.connections()));
            continue;
        }
        if let Some(text) = line.strip_prefix("send ") {
            let sent = lock(held)
                .last()
                .map(|(_, writer)| writer.write_frame(StreamTag::Data, text.as_bytes()));
            match sent {
                Some(Ok(())) => {}
                Some(Err(error)) => emit(&format!("server send-failed error={error}")),
                None => emit("server send-failed error=no held link"),
            }
        }
    }
    let taken = publication
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(publication) = taken
        && let Err(error) = publication.retire()
    {
        emit(&format!(
            "server retire-failed error={}",
            error_chain(&error)
        ));
    }
    emit("server bye");
    std::process::exit(0);
}

/// Runs the accepting handshake on one connection and serves the link it gives.
fn serve(stream: TcpStream, config: &AcceptConfig, instance_id: &str, held: &Held) {
    let mut channel = match RawChannel::from_tcp(stream) {
        Ok(channel) => channel,
        Err(error) => {
            emit(&format!("server handshake-failed error={error}"));
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let line = match read_first_line(&mut channel, MAX_HANDSHAKE_LINE, deadline) {
        Ok(line) => line,
        Err(error) => {
            emit(&format!("server handshake-failed error={error}"));
            return;
        }
    };
    match accept_handshake(channel, &line, Vec::new(), config, &AttemptCancel::new()) {
        Ok(Accepted::Link(link)) => serve_link(*link, instance_id, held),
        Ok(Accepted::Bulk(bulk)) => serve_bulk(bulk),
        Ok(Accepted::Request(server)) => serve_requests(server, instance_id),
        Err(HandshakeError::Refused(reason)) => {
            emit(&format!("server refused reason={}", reason.as_str()));
        }
        Err(error) => {
            emit(&format!(
                "server handshake-failed error={}",
                error_chain(&error)
            ));
        }
    }
}

fn serve_link(mut link: Link, instance_id: &str, held: &Held) {
    let client = link.client_id().unwrap_or(0);
    let Some(roles) = link.peer().roles() else {
        return;
    };
    emit(&format!(
        "server roles client={client} local=acceptor launcher={} dialer={} provider={} \
         holder={} redialer={}",
        roles.ssh_launcher.map_or("none", side_name),
        side_name(roles.dialer),
        side_name(roles.provider),
        side_name(roles.holder),
        side_name(roles.redialer)
    ));
    let Some((reader, mut writer)) = link.take_ordered() else {
        return;
    };
    let end = if roles.provider == Side::Acceptor {
        let snapshot = Snapshot {
            instance_id: instance_id.to_string(),
            client_id: client,
        }
        .to_json();
        match writer.write_frame(StreamTag::Control, snapshot.as_bytes()) {
            Ok(()) => echo(reader, &writer),
            Err(error) => format!("write_failed:{}", error.kind()),
        }
    } else {
        let (end, back) = hold(reader, writer, client, held);
        writer = back;
        end
    };
    emit(&format!("server link-ended client={client} end={end}"));
    writer.finish();
    // Dropping the link drops its registration (`server released`).
    drop(link);
}

/// Serves a link this end holds: waits for the provider's descriptor (the first Control),
/// lends the writer to the stdin `send` lines, and prints every Data frame that comes back
/// until the reader ends. Returns how it ended and the writer.
fn hold(
    mut reader: ChannelReader,
    writer: ChannelWriter,
    client: u32,
    held: &Held,
) -> (String, ChannelWriter) {
    match reader.read_frame() {
        Ok(frame) if frame.tag == StreamTag::Control => {
            // Only a descriptor makes this end the holder; anything else ends the link.
            if let Err(error) = Snapshot::parse(&frame.payload) {
                emit(&format!(
                    "server control-invalid client={client} error={error}"
                ));
                return ("protocol".into(), writer);
            }
            // Held before the line goes out: a `send` that follows the line finds the link.
            lock(held).push((client, writer));
            emit(&format!(
                "server control client={client} {}",
                String::from_utf8_lossy(&frame.payload)
            ));
        }
        Ok(_) => return ("protocol".into(), writer),
        Err(error) => return (failure_name(error.failure()).into(), writer),
    }
    let end = loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Data => emit(&format!(
                    "server echo client={client} {}",
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
    let mut held = lock(held);
    let at = held
        .iter()
        .position(|(c, _)| *c == client)
        .expect("a held link keeps its writer until it ends");
    (end, held.remove(at).1)
}

/// Writes every Data frame back until the reader ends. Returns how it ended.
fn echo(mut reader: ChannelReader, writer: &ChannelWriter) -> String {
    loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Data => {
                    if let Err(error) = writer.write_frame(StreamTag::Data, &frame.payload) {
                        return format!("write_failed:{}", error.kind());
                    }
                }
                StreamTag::Detach => {
                    reader.finish();
                    return "detach".into();
                }
                _ => {}
            },
            Err(error) => return failure_name(error.failure()).into(),
        }
    }
}

/// Writes every frame of a bulk channel back until its reader ends.
fn serve_bulk(mut bulk: BulkChannel) {
    let end = loop {
        match bulk.reader.read_frame() {
            Ok(frame) if frame.tag == StreamTag::Detach => {
                bulk.reader.finish();
                break "detach".to_string();
            }
            Ok(frame) => {
                if let Err(error) = bulk.writer.write_frame(frame.tag, &frame.payload) {
                    break format!("write_failed:{}", error.kind());
                }
            }
            Err(error) => break failure_name(error.failure()).to_string(),
        }
    };
    emit(&format!("server bulk-ended end={end}"));
}

/// Answers the requests of a request channel until it ends.
fn serve_requests(server: RequestServer, instance_id: &str) {
    let handler = Requests {
        instance_id: instance_id.to_string(),
    };
    if let Err(error) = server.serve(&handler) {
        emit(&format!(
            "server request-failed error={}",
            error_chain(&error)
        ));
    }
    emit("server request-ended");
}
