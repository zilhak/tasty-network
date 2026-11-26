//! `demo-server`: the workspace-providing side of an attach, reduced to an echo.
//!
//! It listens on a loopback port, publishes its connection record (port, token, instance ID)
//! at `--record`, and runs the accepting handshake v2 on every connection. An attach link gets
//! one Control snapshot, then every Data frame comes back unchanged. A Detach frame ends the
//! link. The server ends when its stdin closes or reads `quit`, and then retires its record.
//!
//! ```text
//! demo-server --record <path> [--read-timeout-ms <ms>]
//! ```
//!
//! Events on stdout (one per line):
//!
//! | Line | When |
//! |------|------|
//! | `server ready pid=<pid> port=<port> instance=<id>` | the record is published |
//! | `server attached client=<n> live=<links> peer=<instance>` | the domain check took an attach link |
//! | `server link-ended client=<n> end=<detach\|failure name>` | the link's reader ended |
//! | `server released client=<n> live=<links>` | the link's registration was dropped |
//! | `server refused reason=<wire reason>` | this end refused a handshake |
//! | `server handshake-failed error=<text>` | a handshake failed without a refusal |
//! | `server bye` | the server ends |
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tasty_net_demo::{Args, emit, error_chain, failure_name, link_timeouts};
use tasty_net_frame::StreamTag;
use tasty_net_identity::{ConnectionRecord, ConnectionToken, DeviceKey, RecordPublication};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelReader,
    ChannelWriter, DomainAccept, DomainPolicy, DomainReject, HandshakeError, Link,
    MAX_HANDSHAKE_LINE, RawChannel, accept_handshake, read_first_line,
};

const USAGE: &str = "usage: demo-server --record <path> [--read-timeout-ms <ms>]";

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

/// Counts the attach links whose registration is alive.
#[derive(Default)]
struct LivePolicy {
    next_client: AtomicU32,
    live: Arc<AtomicUsize>,
}

/// The registration of one attach link: the link holds it and drops it when it ends.
struct Registration {
    client: u32,
    live: Arc<AtomicUsize>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let live = self.live.fetch_sub(1, Ordering::SeqCst) - 1;
        emit(&format!(
            "server released client={} live={live}",
            self.client
        ));
    }
}

impl DomainPolicy for LivePolicy {
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        let client = self.next_client.fetch_add(1, Ordering::SeqCst) + 1;
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        emit(&format!(
            "server attached client={client} live={live} peer={}",
            request.peer().instance_id()
        ));
        Ok(DomainAccept {
            client_id: Some(client),
            guard: Some(Box::new(Registration {
                client,
                live: Arc::clone(&self.live),
            })),
            ..DomainAccept::default()
        })
    }

    fn accept_bulk(&self, _request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        Err(DomainReject::new("demo.no_bulk"))
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1), &[])?;
    args.check_known(&["record", "read-timeout-ms"])?;
    let record_path = PathBuf::from(args.required("record")?);
    let timeouts = link_timeouts(args.millis("read-timeout-ms", Duration::from_secs(30))?);

    let key = DeviceKey::generate()?;
    let token = ConnectionToken::issue()?;
    // A fresh ID per start, so a dialer can tell a restarted server from the old one.
    let nonce = ConnectionToken::issue()?;
    let instance_id = format!(
        "demo-server-{}-{}",
        std::process::id(),
        &nonce.expose_hex()[..8]
    );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let policy = Arc::new(LivePolicy::default());
    let config = AcceptConfig::new(key, instance_id.clone(), Some(token.clone()), policy)
        .with_timeouts(timeouts);
    let record = ConnectionRecord {
        port,
        token,
        instance_id: instance_id.clone(),
    };
    let publication = Arc::new(Mutex::new(Some(record.publish(&record_path)?)));
    emit(&format!(
        "server ready pid={} port={port} instance={instance_id}",
        std::process::id()
    ));

    {
        let publication = Arc::clone(&publication);
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || watch_stdin(&publication))?;
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
        std::thread::Builder::new()
            .name("serve".into())
            .spawn(move || serve(stream, &config, &instance_id))?;
    }
    Ok(())
}

/// Ends the process when stdin closes or reads `quit`, after retiring the record.
fn watch_stdin(publication: &Mutex<Option<RecordPublication>>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) if line.trim() == "quit" => break,
            Ok(_) => {}
            Err(_) => break,
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
fn serve(stream: TcpStream, config: &AcceptConfig, instance_id: &str) {
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
        Ok(Accepted::Link(link)) => serve_link(*link, instance_id),
        Ok(Accepted::Bulk(_) | Accepted::Request(_)) => {
            emit("server handshake-failed error=only attach links are served");
        }
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

fn serve_link(mut link: Link, instance_id: &str) {
    let client = link.client_id().unwrap_or(0);
    let Some((reader, mut writer)) = link.take_ordered() else {
        return;
    };
    let snapshot =
        format!("{{\"snapshot\":{{\"instance_id\":\"{instance_id}\",\"client_id\":{client}}}}}");
    let end = match writer.write_frame(StreamTag::Control, snapshot.as_bytes()) {
        Ok(()) => echo(reader, &writer),
        Err(error) => format!("write_failed:{}", error.kind()),
    };
    emit(&format!("server link-ended client={client} end={end}"));
    writer.finish();
    // Dropping the link drops its registration (`server released`).
    drop(link);
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
