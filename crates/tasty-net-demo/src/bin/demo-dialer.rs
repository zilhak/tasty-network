//! `demo-dialer`: reaches a `demo-server` over SSH and keeps the attach link up.
//!
//! Each attempt reads the server's connection record over SSH (`cat`, then `type`, of
//! `--record` on the SSH host), opens or reuses the `-L` tunnel to the record's port and runs
//! handshake v2 with the record's token. While a link is up, every stdin line `send <text>`
//! goes out as a Data frame and the echo is printed. When the link is lost (or the first
//! attempt fails in a way connecting again can change) the dialer connects again from
//! discovery through the transport's `Redialer` with its default settings
//! (docs/design/transport.md "보관 상한과 재연결 도우미"): the new connection's entry in the
//! connection table names the lost one. A refusal that connecting again cannot change (a
//! wrong token, for example) ends the dialer with exit code 3; when the redialer gives up
//! (`redial.give_up_after`), or the loss is not one to redial, it ends with exit code 4.
//! `quit` or the end of stdin detaches and ends with exit code 0. The stdin line `table`
//! prints the connection table in the lines of `tasty_net_demo::table`.
//!
//! `--holder acceptor` proposes the role reversal (docs/design/transport.md "역할" (b)): the
//! server holds and this end provides the workspace over the same `-L` tunnel, and this end
//! still connects again after a loss. After the ACK it sends one Control descriptor (a
//! snapshot with its own instance ID), then writes every Data frame back unchanged. A refused
//! proposal (`role_refused`) ends the dialer with exit code 3 like any other refusal.
//! `--instance-id` replaces the instance ID (default `demo-dialer-<pid>`); a test gives the
//! server's to check that a self attach is refused.
//!
//! The link's channels hold the `-L` tunnel, and the tunnel's `ssh` child is ended when the
//! last of them goes. On `quit` the stdin thread holds the writer while it detaches, so the
//! dialer waits for that thread to finish (bounded by the write timeout and the drain cap)
//! before it returns; otherwise the process could end first and leave the `ssh` running. A
//! refusal ends the dialer without waiting for stdin: no link was up, and the main thread
//! drops what is left of the attempt and the transport before it returns.
//!
//! Test hook: `TASTY_NET_DEMO_HOLD_WRITER_MS=<ms>` makes the stdin thread keep the writer that
//! long after its Detach before letting it go, so a test can check that the dialer still
//! waits for it (`tests/ssh_attach.rs`).
//!
//! ```text
//! demo-dialer --destination <user@host> --record <remote path>
//!             [--ssh-port <port>] [--identity <file>] [--option Key=Value]...
//!             [--ssh <program>] [--verify-host-keys] [--read-timeout-ms <ms>]
//!             [--holder dialer|acceptor] [--instance-id <id>]
//! ```
//!
//! Events on stdout (one per line):
//!
//! | Line | When |
//! |------|------|
//! | `dialer pid=<pid>` | at start |
//! | `dialer ssh-spawned pid=<pid> role=<tunnel\|discovery>` | an `ssh` child was created |
//! | `dialer attempt n=<n>` | an attempt starts |
//! | `dialer connected attempt=<n> instance=<id> tunnel_pid=<pid>` | the handshake succeeded |
//! | `dialer roles local=dialer launcher=<side> dialer=<side> provider=<side> holder=<side> redialer=<side>` | right after `connected` (`<side>` is `dialer` or `acceptor`) |
//! | `dialer control <json>` | a Control frame arrived (the server's snapshot) |
//! | `dialer echo <text>` | a Data frame arrived on a link this end holds |
//! | `dialer served <text>` | a Data frame arrived on a link this end provides, and went back |
//! | `dialer send-failed error=<text>` | a `send` line found no link, or writing failed |
//! | `dialer lost failure=<name>` | the link's reader failed |
//! | `dialer retry error=<text>` | an attempt failed in a way connecting again can change |
//! | `dialer rejected reason=<reason or -> error=<text>` | it did not; the dialer ends. The reason is the peer's wire reason, or `self_attach` when the peer turned out to be this instance |
//! | `dialer gave-up error=<text>` | the redialer gave up, or the loss was not one to redial; the dialer ends |
//! | `dialer table ...` · `dialer conn ...` · `dialer link ...` · `dialer table-end ...` | the stdin line `table` (`tasty_net_demo::table`) |
//! | `dialer stdin-thread-late limit_ms=<ms>` | after `quit`, the stdin thread's Detach did not end in time |
//! | `dialer stdin-thread-panicked` | after `quit`, the stdin thread had panicked |
//! | `dialer bye` | the dialer ends after `quit` |
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tasty_net_demo::table::table_lines;
use tasty_net_demo::{Args, Snapshot, emit, emit_lines, error_chain, failure_name, link_timeouts};
use tasty_net_frame::StreamTag;
use tasty_net_identity::DeviceKey;
use tasty_net_ssh::{PortDiscoveryConfig, PortMode, SpawnHook, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, ChannelReader, ChannelWriter, ConnectOptions, ConnectionId, ConnectionTable,
    DialConfig, EndReason, HandshakeError, Holder, LinkCloser, LinkId, RedialError, Redialer,
    RedialerConfig, Side, SshTransport, Transport, dial_handshake,
};

const USAGE: &str = "usage: demo-dialer --destination <user@host> --record <remote path> \
[--ssh-port <port>] [--identity <file>] [--option Key=Value]... [--ssh <program>] \
[--verify-host-keys] [--read-timeout-ms <ms>] [--holder dialer|acceptor] [--instance-id <id>]";

/// Exit code when a refusal ends the dialer without another attempt.
const EXIT_REJECTED: u8 = 3;

/// Exit code when the redialer gave up or the loss was not one to redial.
const EXIT_GAVE_UP: u8 = 4;

/// Added to the write timeout and the drain cap when waiting for the stdin thread to finish
/// its Detach after `quit`: the time to drop the writer and end the tunnel `ssh`.
const DETACH_END_MARGIN: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("demo-dialer: {}", error_chain(error.as_ref()));
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// The test hook of the module text.
const HOLD_WRITER_ENV: &str = "TASTY_NET_DEMO_HOLD_WRITER_MS";

/// What the stdin thread and the connection loop share.
#[derive(Default)]
struct Shared {
    quit: AtomicBool,
    /// How long the stdin thread keeps the writer after its Detach ([`HOLD_WRITER_ENV`]).
    hold_writer: Duration,
    /// The writer and closer of the link that is up.
    link: Mutex<Option<(ChannelWriter, LinkCloser)>>,
    /// The attempt in progress, or the redial waiting for its next one, so `quit` can cancel
    /// it.
    attempt: Mutex<Option<AttemptCancel>>,
    /// The connection table the transport reports in, for the `table` line.
    connections: ConnectionTable,
    /// This device's ID, for the `table` line.
    device: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why no link came up.
enum NoLink {
    /// The first attempt (no lost entry yet) failed.
    Attempt(HandshakeError),
    /// The redial of a lost entry ended.
    Redial(RedialError),
}

/// What reaches the server and what every attempt dials with.
struct Dial {
    transport: SshTransport,
    target: SshTarget,
    config: DialConfig,
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1), &["verify-host-keys"])?;
    args.check_known(&[
        "destination",
        "record",
        "ssh-port",
        "identity",
        "option",
        "ssh",
        "verify-host-keys",
        "read-timeout-ms",
        "holder",
        "instance-id",
    ])?;
    let holder = match args.value("holder") {
        None => Holder::Dialer,
        Some(text) => Holder::parse(text).ok_or_else(|| {
            tasty_net_demo::UsageError("--holder needs dialer or acceptor".into())
        })?,
    };
    let target = SshTarget {
        destination: args.required("destination")?.to_string(),
        ssh_port: args
            .value("ssh-port")
            .map(|p| p.parse::<u16>())
            .transpose()
            .map_err(|_| tasty_net_demo::UsageError("--ssh-port needs a port number".into()))?,
        identity_file: args.value("identity").map(str::to_string),
        extra_options: args
            .values("option")
            .into_iter()
            .map(str::to_string)
            .collect(),
    };
    let discovery = PortDiscoveryConfig {
        remote_program: "demo-server".into(),
        remote_args: Vec::new(),
        data_dir: String::new(),
        port_file: String::new(),
        explicit_port_file: Some(args.required("record")?.to_string()),
    };
    let timeouts = link_timeouts(args.millis("read-timeout-ms", Duration::from_secs(30))?);

    let tunnel_pid = Arc::new(AtomicU32::new(0));
    let launcher = match args.value("ssh") {
        Some(program) => SshLauncher::new(program),
        None => SshLauncher::system(),
    }
    .with_verify(args.flag("verify-host-keys"))
    .with_spawn_hook(spawn_hook(Arc::clone(&tunnel_pid)));
    let connections = ConnectionTable::default();
    let transport = SshTransport::new(launcher, discovery, PortMode::FileUnix)
        .with_connections(connections.clone());
    let redialer = Redialer::new(&connections, RedialerConfig::new())?;
    let instance_id = match args.value("instance-id") {
        Some(id) => id.to_string(),
        None => format!("demo-dialer-{}", std::process::id()),
    };
    let key = DeviceKey::generate()?;
    let device = key.id().to_string();
    let dial = Dial {
        transport,
        target,
        config: DialConfig::new(key, instance_id.clone()).with_timeouts(timeouts),
    };

    emit(&format!("dialer pid={}", std::process::id()));
    let hold_writer = match std::env::var(HOLD_WRITER_ENV) {
        Ok(ms) => Duration::from_millis(ms.parse().map_err(|_| {
            tasty_net_demo::UsageError(format!("{HOLD_WRITER_ENV} needs milliseconds"))
        })?),
        Err(_) => Duration::ZERO,
    };
    let detach_limit = timeouts
        .write_timeout
        .saturating_add(timeouts.drain_cap)
        .saturating_add(hold_writer)
        .saturating_add(DETACH_END_MARGIN);
    let shared = Arc::new(Shared {
        hold_writer,
        connections: connections.clone(),
        device,
        ..Shared::default()
    });
    let stdin_thread = {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || read_stdin(&shared))?
    };

    let options = ConnectOptions::new().with_holder(holder);
    let mut attempt = 0u32;
    // The entry of the loss to redial; none before the first attempt.
    let mut lost: Option<ConnectionId> = None;
    while !shared.quit.load(Ordering::SeqCst) {
        let cancel = AttemptCancel::new();
        *lock(&shared.attempt) = Some(cancel.clone());
        if shared.quit.load(Ordering::SeqCst) {
            cancel.cancel();
        }
        let mut once = |options: &ConnectOptions| {
            attempt += 1;
            emit(&format!("dialer attempt n={attempt}"));
            let result = dial
                .transport
                .connect_with(&dial.target, &cancel, options)
                .map_err(HandshakeError::Connect)
                .and_then(|conn| dial_handshake(conn, &dial.config, holder, None, &cancel));
            if let Err(error) = &result
                && error.is_retryable()
            {
                emit(&format!("dialer retry error={}", error_chain(error)));
            }
            result
        };
        let result = match lost.take() {
            None => once(&options).map_err(NoLink::Attempt),
            Some(entry) => redialer
                .redial(entry, None, &cancel, &mut once)
                .map_err(NoLink::Redial),
        };
        lock(&shared.attempt).take();
        let quitting = shared.quit.load(Ordering::SeqCst);
        let mut link = match result {
            Ok(link) => link,
            Err(
                NoLink::Attempt(HandshakeError::Cancelled)
                | NoLink::Redial(
                    RedialError::Cancelled | RedialError::Failed(HandshakeError::Cancelled),
                ),
            ) if quitting => break,
            Err(NoLink::Attempt(error)) if error.is_retryable() => {
                // The failed attempt's entry is the newest one; the redialer names it.
                lost = Some(
                    connections
                        .connections()
                        .last()
                        .map(|entry| entry.id)
                        .ok_or("a failed attempt left no entry in the connection table")?,
                );
                continue;
            }
            Err(NoLink::Attempt(error) | NoLink::Redial(RedialError::Failed(error))) => {
                let reason = match &error {
                    HandshakeError::Rejected { reason, .. } => reason.as_str().to_string(),
                    HandshakeError::SelfAttach => "self_attach".into(),
                    _ => "-".into(),
                };
                emit(&format!(
                    "dialer rejected reason={reason} error={}",
                    error_chain(&error)
                ));
                // The stdin thread may still wait for a line, so it is not waited for. It
                // holds no part of the link (none was up); what the attempt left goes here,
                // in this thread, with the transport, so the tunnel `ssh` ends before exit.
                drop(error);
                drop(lock(&shared.link).take());
                drop(dial);
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
            Err(NoLink::Redial(error)) => {
                emit(&format!("dialer gave-up error={}", error_chain(&error)));
                drop(error);
                drop(dial);
                return Ok(ExitCode::from(EXIT_GAVE_UP));
            }
        };
        let entry = entry_of(&connections, link.link_id());
        let Some((reader, writer)) = link.take_ordered() else {
            return Err("a new link has no ordered channel".into());
        };
        let Some(roles) = link.peer().roles() else {
            return Err("an attach link has no roles".into());
        };
        emit(&format!(
            "dialer connected attempt={attempt} instance={} tunnel_pid={}",
            link.peer().instance_id(),
            tunnel_pid.load(Ordering::SeqCst)
        ));
        emit(&format!(
            "dialer roles local=dialer launcher={} dialer={} provider={} holder={} redialer={}",
            roles.ssh_launcher.map_or("none", side_name),
            side_name(roles.dialer),
            side_name(roles.provider),
            side_name(roles.holder),
            side_name(roles.redialer)
        ));
        let provides = roles.provider == Side::Dialer;
        // The provider sends the descriptor first (O2), before any stdin line can.
        if provides {
            let descriptor = Snapshot {
                instance_id: instance_id.clone(),
                client_id: link.client_id().unwrap_or(0),
            }
            .to_json();
            if let Err(error) = writer.write_frame(StreamTag::Control, descriptor.as_bytes()) {
                emit(&format!("dialer send-failed error={error}"));
            }
        }
        *lock(&shared.link) = Some((writer, link.closer()));
        if shared.quit.load(Ordering::SeqCst) {
            detach(&shared, Duration::ZERO);
        }
        let end = read_link(reader, provides.then_some(&*shared));
        lock(&shared.link).take();
        drop(link);
        if shared.quit.load(Ordering::SeqCst) {
            break;
        }
        emit(&format!("dialer lost failure={end}"));
        lost = Some(entry.ok_or("the lost link has no entry in the connection table")?);
    }
    // `quit` or the end of stdin: the stdin thread sets `quit` and then detaches, holding the
    // writer, which keeps the tunnel. Its end means the writer is gone.
    wait_for_stdin_thread(&stdin_thread, detach_limit);
    if stdin_thread.is_finished() && stdin_thread.join().is_err() {
        emit("dialer stdin-thread-panicked");
    }
    drop(dial);
    emit("dialer bye");
    Ok(ExitCode::SUCCESS)
}

/// The entry of the connection table that carries the attach link `link`.
fn entry_of(table: &ConnectionTable, link: LinkId) -> Option<ConnectionId> {
    table
        .connections()
        .into_iter()
        .find(|entry| entry.links.iter().any(|l| l.link_id == link))
        .map(|entry| entry.id)
}

/// Waits up to `limit` for the stdin thread to end. Says so on stdout when it does not.
fn wait_for_stdin_thread(thread: &std::thread::JoinHandle<()>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            emit(&format!(
                "dialer stdin-thread-late limit_ms={}",
                limit.as_millis()
            ));
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Creates every `ssh` child of the dialer and reports it. A tunnel child (`-L`) becomes the
/// current tunnel PID.
fn spawn_hook(tunnel_pid: Arc<AtomicU32>) -> SpawnHook {
    SpawnHook::new(move |mut cmd: Command| {
        let tunnel = cmd.get_args().any(|a| a == "-L");
        let child = cmd.spawn()?;
        if tunnel {
            tunnel_pid.store(child.id(), Ordering::SeqCst);
        }
        emit(&format!(
            "dialer ssh-spawned pid={} role={}",
            child.id(),
            if tunnel { "tunnel" } else { "discovery" }
        ));
        Ok(child)
    })
}

/// The name of a side in event lines.
fn side_name(side: Side) -> &'static str {
    tasty_net_demo::table::side_name(side)
}

/// Prints every frame of the link until it ends. Returns the end's name. With `provider`
/// (this end provides the workspace), every Data frame goes back through the link's writer.
fn read_link(mut reader: ChannelReader, provider: Option<&Shared>) -> &'static str {
    loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Data => {
                    let text = String::from_utf8_lossy(&frame.payload);
                    match provider {
                        None => emit(&format!("dialer echo {text}")),
                        Some(shared) => {
                            let sent = lock(&shared.link).as_ref().map(|(writer, _)| {
                                writer.write_frame(StreamTag::Data, &frame.payload)
                            });
                            match sent {
                                Some(Ok(())) => emit(&format!("dialer served {text}")),
                                Some(Err(error)) => {
                                    emit(&format!("dialer send-failed error={error}"));
                                }
                                // Detaching: the writer is gone.
                                None => {}
                            }
                        }
                    }
                }
                StreamTag::Control => {
                    emit(&format!(
                        "dialer control {}",
                        String::from_utf8_lossy(&frame.payload)
                    ));
                }
                StreamTag::Detach => {
                    reader.finish();
                    return "detach";
                }
                _ => {}
            },
            Err(error) => return failure_name(error.failure()),
        }
    }
}

/// Reads stdin commands: `send <text>`, `table` and `quit`. The end of stdin is `quit`.
fn read_stdin(shared: &Shared) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end();
        if line == "quit" {
            break;
        }
        if line == "table" {
            emit_lines(&table_lines(
                "dialer",
                &shared.device,
                &shared.connections.connections(),
            ));
            continue;
        }
        match line.strip_prefix("send ") {
            Some(text) => {
                let sent = lock(&shared.link)
                    .as_ref()
                    .map(|(writer, _)| writer.write_frame(StreamTag::Data, text.as_bytes()));
                match sent {
                    Some(Ok(())) => {}
                    Some(Err(error)) => emit(&format!("dialer send-failed error={error}")),
                    None => emit("dialer send-failed error=not connected"),
                }
            }
            None => emit(&format!("dialer unknown-command {line}")),
        }
    }
    shared.quit.store(true, Ordering::SeqCst);
    // Cancels the attempt in progress, or wakes the redialer's wait.
    if let Some(cancel) = lock(&shared.attempt).as_ref() {
        cancel.cancel();
    }
    detach(shared, shared.hold_writer);
}

/// Sends Detach on the link that is up and ends its writer; the server answers by ending its
/// side, which ends the reader. When the drain cap runs out the link is closed both ways.
/// The writer goes `hold` after that (the test hook).
fn detach(shared: &Shared, hold: Duration) {
    let taken = lock(&shared.link).take();
    if let Some((mut writer, closer)) = taken {
        if writer.write_frame(StreamTag::Detach, &[]).is_err() {
            closer.force_close(EndReason::Forced);
            return;
        }
        writer.finish();
        std::thread::sleep(hold);
    }
}
