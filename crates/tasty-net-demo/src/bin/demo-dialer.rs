//! `demo-dialer`: reaches a `demo-server` over SSH and keeps the attach link up.
//!
//! Each attempt reads the server's connection record over SSH (`cat`, then `type`, of
//! `--record` on the SSH host), opens or reuses the `-L` tunnel to the record's port and runs
//! handshake v2 with the record's token. While a link is up, every stdin line `send <text>`
//! goes out as a Data frame and the echo is printed. When the link is lost (or the first
//! attempt fails in a way connecting again can change) the dialer connects again from
//! discovery through the transport's `Redialer` with its default settings
//! (docs/design/transport.md "보관 상한과 재연결 도우미"): the new connection's entry in the
//! connection table names the lost one. Exit code 3 is only for a refusal with a handshake
//! refusal reason: the peer refused with a refusing ACK (a wrong token, a refused role
//! proposal) or the peer turned out to be this instance (`self_attach`). Every other failure
//! that connecting again cannot change (a protocol error, a bad signature, a cancel that did
//! not come from `quit`) is not one to redial and ends the dialer with exit code 4, as when
//! the redialer gives up (`redial.give_up_after`). `quit` or the end of stdin detaches and
//! ends with exit code 0, also when it cancelled an attempt in the SSH connection or in the
//! handshake. The stdin line `table` prints the connection table in the lines of
//! `tasty_net_demo::table`. Built with the `embedded-ssh` feature, the stdin line
//! `drop-session` ends the embedded SSH session the last tunnel opened (the test hook
//! `SessionHook`), as a test ends the tunnel's `ssh` child of the system backend.
//!
//! `--holder acceptor` proposes the role reversal (docs/design/transport.md "역할" (b)): the
//! server holds and this end provides the workspace over the same `-L` tunnel, and this end
//! still connects again after a loss. After the ACK it sends one Control descriptor (a
//! snapshot with its own instance ID), then writes every Data frame back unchanged. A refused
//! proposal (`role_refused`) ends the dialer with exit code 3 like any other refusal.
//! `--instance-id` replaces the instance ID (default `demo-dialer-<pid>`); a test gives the
//! server's to check that a self attach is refused.
//!
//! `--channels` opens, every time the first link comes up (the first connection and each one
//! after a loss), a bulk channel under it and a request channel through the same tunnel, and
//! calls `system.info` once on the request channel (`--also-call <method>` calls that method
//! first, a test's way to send a method name the server does not know). Both stay open until
//! that link ends. The
//! bulk channel is opened only under the first link, never under the second one.
//! `--also-holder dialer|acceptor` opens a second attach link with that holder through the
//! same tunnel every time the first link comes up, so one tunnel carries links of both
//! directions. The second link is not redialed on its own: when it ends, it comes back only
//! with the next connection of the first link. Its events carry the `extra-` prefix. After
//! `quit` or the end of stdin no channel and no second link is opened, and one being opened
//! is cancelled.
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
//!             [--channels] [--also-call <method>] [--also-holder dialer|acceptor]
//! ```
//!
//! Events on stdout (one per line):
//!
//! | Line | When |
//! |------|------|
//! | `dialer pid=<pid>` | at start |
//! | `dialer ssh-spawned pid=<pid> role=<tunnel\|discovery>` | an `ssh` child was created |
//! | `dialer attempt n=<n>` | an attempt starts |
//! | `dialer connected attempt=<n> instance=<id> tunnel_pid=<pid> backend=<system\|embedded> session=<n>` | the handshake succeeded (the server's ID encoded as in `tasty_net_demo::table`; `tunnel_pid` is 0 and `session` the SSH session number for the embedded backend) |
//! | `dialer session-dropped id=<n>` · `dialer drop-session-failed error=<text>` | the stdin line `drop-session` ended the embedded session, or found none |
//! | `dialer roles local=dialer launcher=<side> dialer=<side> provider=<side> holder=<side> redialer=<side>` | right after `connected` (`<side>` is `dialer` or `acceptor`) |
//! | `dialer channels-open instance=<id>` | `--channels`: the bulk and request channels are up and `system.info` answered (the value encoded as in `tasty_net_demo::table`) |
//! | `dialer call method=<method> answer=<result\|error>` | `--channels --also-call`: the request channel answered that method (the value encoded as in `tasty_net_demo::table`) |
//! | `dialer channels-failed error=<text>` | `--channels`: opening them failed; the link stays |
//! | `dialer extra-connected holder=<holder> link_id=<hex>` | `--also-holder`: the second link is up |
//! | `dialer extra-failed error=<text>` | `--also-holder`: the second link did not come up |
//! | `dialer extra-ended end=<name>` | `--also-holder`: the second link's reader ended |
//! | `dialer extra-send-failed error=<text>` | `--also-holder`: writing on the second link failed (its descriptor, or a frame it served back) |
//! | `dialer extra-panicked` | `--also-holder`: the second link's reader thread had panicked |
//! | `dialer control <json>` | a Control frame arrived (the server's snapshot); `dialer extra-control` on the second link |
//! | `dialer echo <text>` | a Data frame arrived on a link this end holds; `dialer extra-echo` on the second link |
//! | `dialer served <text>` | a Data frame arrived on a link this end provides, and went back; `dialer extra-served` on the second link |
//! | `dialer send-failed error=<text>` | a `send` line found no link, or writing failed |
//! | `dialer lost failure=<name>` | the link's reader failed |
//! | `dialer retry error=<text>` | an attempt failed in a way connecting again can change |
//! | `dialer rejected reason=<reason> error=<text>` | the peer refused with a refusing ACK, or it is this instance; the dialer ends with code 3. The reason is the peer's wire reason, or `self_attach` |
//! | `dialer gave-up error=<text>` | the redialer gave up, or the failure or loss was not one to redial; the dialer ends with code 4 |
//! | `dialer table ...` · `dialer conn ...` · `dialer link ...` · `dialer table-end ...` | the stdin line `table` (`tasty_net_demo::table`) |
//! | `dialer stdin-thread-late limit_ms=<ms>` | after `quit`, the stdin thread's Detach did not end in time |
//! | `dialer stdin-thread-panicked` | after `quit`, the stdin thread had panicked |
//! | `dialer bye` | the dialer ends after `quit` |
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tasty_net_demo::table::{encode_value, table_lines};
use tasty_net_demo::{Args, Snapshot, emit, emit_lines, error_chain, failure_name, link_timeouts};
use tasty_net_frame::StreamTag;
use tasty_net_identity::DeviceKey;
use tasty_net_ssh::{
    PortDiscoveryConfig, PortMode, SpawnHook, SshConnector, SshLauncher, SshTarget,
};
#[cfg(feature = "embedded-ssh")]
use tasty_net_ssh::{SessionControl, SessionHook};
use tasty_net_transport::{
    AttemptCancel, BulkChannel, ChannelReader, ChannelWriter, ConnectError, ConnectOptions,
    ConnectionId, ConnectionTable, DialConfig, EndReason, HandshakeError, Holder, Link, LinkCloser,
    LinkId, LinkTimeouts, RedialError, Redialer, RedialerConfig, RequestClient, RequestError, Side,
    SshTransport, Transport, dial_handshake, dial_request,
};

const USAGE: &str = "usage: demo-dialer --destination <user@host> --record <remote path> \
[--ssh-port <port>] [--identity <file>] [--option Key=Value]... [--ssh <program>] \
[--verify-host-keys] [--read-timeout-ms <ms>] [--holder dialer|acceptor] [--instance-id <id>] \
[--channels] [--also-call <method>] [--also-holder dialer|acceptor]";

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

/// The writer and closer of a link that is up.
type LinkSlot = Mutex<Option<(ChannelWriter, LinkCloser)>>;

/// What the stdin thread and the connection loop share.
#[derive(Default)]
struct Shared {
    quit: AtomicBool,
    /// How long the stdin thread keeps the writer after its Detach ([`HOLD_WRITER_ENV`]).
    hold_writer: Duration,
    /// The link that is up.
    link: LinkSlot,
    /// The second link of `--also-holder`.
    extra: LinkSlot,
    /// The attempt in progress, or the redial waiting for its next one, so `quit` can cancel
    /// it.
    attempt: Mutex<Option<AttemptCancel>>,
    /// The connection table the transport reports in, for the `table` line.
    connections: ConnectionTable,
    /// This device's ID, for the `table` line.
    device: String,
    /// The embedded SSH session the last tunnel opened, for `drop-session`.
    #[cfg(feature = "embedded-ssh")]
    session: Arc<Mutex<Option<SessionControl>>>,
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
    timeouts: LinkTimeouts,
    instance_id: String,
    /// `--also-call`: a method the request channel calls before `system.info`.
    also_call: Option<String>,
}

fn parse_holder(args: &Args, name: &str) -> Result<Option<Holder>, tasty_net_demo::UsageError> {
    args.value(name)
        .map(|text| {
            Holder::parse(text).ok_or_else(|| {
                tasty_net_demo::UsageError(format!("--{name} needs dialer or acceptor"))
            })
        })
        .transpose()
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse(
        std::env::args_os().skip(1),
        &["verify-host-keys", "channels"],
    )?;
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
        "channels",
        "also-call",
        "also-holder",
    ])?;
    let holder = parse_holder(&args, "holder")?.unwrap_or(Holder::Dialer);
    let also = parse_holder(&args, "also-holder")?;
    let open_channels = args.flag("channels");
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
    let connector = SshConnector::new(launcher.settings().clone()).with_launcher(launcher);
    #[cfg(feature = "embedded-ssh")]
    let session: Arc<Mutex<Option<SessionControl>>> = Arc::default();
    #[cfg(feature = "embedded-ssh")]
    let connector = {
        let slot = Arc::clone(&session);
        connector.with_session_hook(SessionHook::new(move |control| {
            *lock(&slot) = Some(control);
        }))
    };
    let connections = ConnectionTable::default();
    let transport = SshTransport::from_connector(connector, discovery, PortMode::FileUnix)
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
        timeouts,
        instance_id,
        also_call: args.value("also-call").map(str::to_string),
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
        #[cfg(feature = "embedded-ssh")]
        session,
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
        let no_link = match result {
            Ok(link) => Ok(link),
            Err(no_link) => Err((next_after(&no_link, quitting), no_link)),
        };
        let mut link = match no_link {
            Ok(link) => link,
            Err((Next::Quit, _)) => break,
            Err((Next::Redial, _)) => {
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
            Err((
                Next::Rejected,
                NoLink::Attempt(error) | NoLink::Redial(RedialError::Failed(error)),
            )) => {
                let reason = refusal(&error).unwrap_or("-");
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
            Err((_, no_link)) => {
                let error = match &no_link {
                    NoLink::Attempt(error) => error_chain(error),
                    NoLink::Redial(error) => error_chain(error),
                };
                emit(&format!("dialer gave-up error={error}"));
                drop(no_link);
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
        let provides = roles.provider == Side::Dialer;
        // The provider sends the descriptor first (O2), before any stdin line can.
        if provides {
            send_descriptor(&writer, &dial.instance_id, &link, "");
        }
        // The link is in place before `connected` is printed, so a stdin line sent on that
        // line finds it.
        *lock(&shared.link) = Some((writer, link.closer()));
        let ssh = entry
            .and_then(|id| connections.connection(id))
            .and_then(|e| e.ssh);
        emit(&format!(
            "dialer connected attempt={attempt} instance={} tunnel_pid={} backend={} session={}",
            encode_value(link.peer().instance_id()),
            tunnel_pid.load(Ordering::SeqCst),
            ssh.as_ref()
                .and_then(|s| s.backend.backend)
                .map_or("-", |b| b.name()),
            ssh.and_then(|s| s.session_id)
                .map_or_else(|| "-".to_string(), |id| id.to_string()),
        ));
        emit(&format!(
            "dialer roles local=dialer launcher={} dialer={} provider={} holder={} redialer={}",
            roles.ssh_launcher.map_or("none", side_name),
            side_name(roles.dialer),
            side_name(roles.provider),
            side_name(roles.holder),
            side_name(roles.redialer)
        ));
        if shared.quit.load(Ordering::SeqCst) {
            detach(&shared.link, Duration::ZERO);
        }
        // After quit no more connections start, and `quit` cancels one in progress.
        let channels = open_channels
            .then(|| aux_cancel(&shared).and_then(|cancel| channels(&link, &dial, &cancel)))
            .flatten();
        let extra = also.and_then(|holder| {
            aux_cancel(&shared).and_then(|cancel| open_extra(&shared, &dial, holder, &cancel))
        });
        lock(&shared.attempt).take();
        let end = read_link(reader, provides.then_some(&shared.link), "");
        lock(&shared.link).take();
        // Everything on the tunnel goes with the link, so a lost tunnel ends with it.
        end_extra(&shared, extra);
        drop(channels);
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

/// What the loop does when an attempt or a redial gave no link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    /// `quit` cancelled it, in the SSH connection or in the handshake: end with code 0.
    Quit,
    /// The first attempt failed in a way connecting again can change: redial its entry.
    Redial,
    /// A refusal with a handshake refusal reason ([`refusal`]): end with code 3.
    Rejected,
    /// The redialer gave up, or the failure or loss is not one to redial: end with code 4.
    GaveUp,
}

/// Whether `error` is a cancelled attempt, in the SSH connection or in the handshake.
fn cancelled(error: &HandshakeError) -> bool {
    matches!(
        error,
        HandshakeError::Cancelled | HandshakeError::Connect(ConnectError::Cancelled)
    )
}

/// The refusal reason of `error` when it is a refusal: the peer's refusing ACK, or the peer
/// being this instance. Other failures carry no refusal reason.
fn refusal(error: &HandshakeError) -> Option<&str> {
    match error {
        HandshakeError::Rejected { reason, .. } => Some(reason.as_str()),
        HandshakeError::SelfAttach => Some("self_attach"),
        _ => None,
    }
}

/// The [`Next`] step after `no_link`. `quitting` is whether `quit` came (it cancels the
/// attempt), so a cancelled attempt is the end that `quit` asked for. A cancel without `quit`
/// has no cause in this binary (only `quit` cancels); should one come, it is not one to
/// redial and ends with code 4.
fn next_after(no_link: &NoLink, quitting: bool) -> Next {
    match no_link {
        NoLink::Attempt(error) | NoLink::Redial(RedialError::Failed(error))
            if quitting && cancelled(error) =>
        {
            Next::Quit
        }
        NoLink::Redial(RedialError::Cancelled) if quitting => Next::Quit,
        NoLink::Attempt(error) if error.is_retryable() => Next::Redial,
        NoLink::Attempt(error) | NoLink::Redial(RedialError::Failed(error))
            if refusal(error).is_some() =>
        {
            Next::Rejected
        }
        NoLink::Attempt(_) | NoLink::Redial(_) => Next::GaveUp,
    }
}

/// The entry of the connection table that carries the attach link `link`.
fn entry_of(table: &ConnectionTable, link: LinkId) -> Option<ConnectionId> {
    table
        .connections()
        .into_iter()
        .find(|entry| entry.links.iter().any(|l| l.link_id == link))
        .map(|entry| entry.id)
}

/// Sends the descriptor of a link this end provides: a snapshot with this instance's ID.
fn send_descriptor(writer: &ChannelWriter, instance_id: &str, link: &Link, prefix: &str) {
    let descriptor = Snapshot {
        instance_id: instance_id.to_string(),
        client_id: link.client_id().unwrap_or(0),
    }
    .to_json();
    if let Err(error) = writer.write_frame(StreamTag::Control, descriptor.as_bytes()) {
        emit(&format!("dialer {prefix}send-failed error={error}"));
    }
}

/// The bulk and request channels of `--channels`, held until the link ends.
struct Channels {
    _bulk: BulkChannel,
    _requests: RequestClient,
}

/// Opens a bulk channel under `link` and a request channel through the same tunnel, and calls
/// `system.info` once. Says why on stdout when one of them fails.
fn channels(link: &Link, dial: &Dial, cancel: &AttemptCancel) -> Option<Channels> {
    let failed = |error: &dyn std::error::Error| {
        emit(&format!(
            "dialer channels-failed error={}",
            error_chain(error)
        ));
    };
    let bulk = link.open_bulk(None).map_err(|e| failed(&e)).ok()?;
    let requests = dial
        .transport
        .connect(&dial.target, cancel)
        .map_err(HandshakeError::Connect)
        .and_then(|conn| dial_request(conn, &dial.config, cancel))
        .map_err(|e| failed(&e))
        .ok()?;
    if let Some(method) = &dial.also_call {
        let answer = match requests.call(
            method,
            serde_json::json!({}),
            dial.timeouts.handshake_timeout,
        ) {
            Ok(_) => "result",
            Err(RequestError::Remote(_)) => "error",
            Err(error) => {
                failed(&error);
                return None;
            }
        };
        emit(&format!(
            "dialer call method={} answer={answer}",
            encode_value(method)
        ));
    }
    let answer = requests
        .call(
            "system.info",
            serde_json::json!({}),
            dial.timeouts.handshake_timeout,
        )
        .map_err(|e| failed(&e))
        .ok()?;
    emit(&format!(
        "dialer channels-open instance={}",
        answer
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "-".to_string(), encode_value)
    ));
    Some(Channels {
        _bulk: bulk,
        _requests: requests,
    })
}

/// Opens the second link of `--also-holder` through the same tunnel and reads it on its own
/// thread until it ends. Says why on stdout when it does not come up.
fn open_extra(
    shared: &Arc<Shared>,
    dial: &Dial,
    holder: Holder,
    cancel: &AttemptCancel,
) -> Option<JoinHandle<()>> {
    let result = dial
        .transport
        .connect(&dial.target, cancel)
        .map_err(HandshakeError::Connect)
        .and_then(|conn| dial_handshake(conn, &dial.config, holder, None, cancel));
    let mut link = match result {
        Ok(link) => link,
        Err(error) => {
            emit(&format!(
                "dialer extra-failed error={}",
                error_chain(&error)
            ));
            return None;
        }
    };
    let (reader, writer) = link.take_ordered()?;
    let provides = link.peer().roles()?.provider == Side::Dialer;
    if provides {
        send_descriptor(&writer, &dial.instance_id, &link, "extra-");
    }
    emit(&format!(
        "dialer extra-connected holder={} link_id={}",
        holder.as_str(),
        link.link_id()
    ));
    *lock(&shared.extra) = Some((writer, link.closer()));
    if shared.quit.load(Ordering::SeqCst) {
        detach(&shared.extra, Duration::ZERO);
    }
    let shared = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("extra".into())
        .spawn(move || {
            let end = read_link(reader, provides.then_some(&shared.extra), "extra-");
            lock(&shared.extra).take();
            drop(link);
            emit(&format!("dialer extra-ended end={end}"));
        });
    match spawned {
        Ok(thread) => Some(thread),
        Err(error) => {
            emit(&format!("dialer extra-failed error={error}"));
            None
        }
    }
}

/// The cancel of an auxiliary connection (the request channel, the second link), registered
/// where `quit` cancels it; none once `quit` came, so no connection starts after it.
fn aux_cancel(shared: &Shared) -> Option<AttemptCancel> {
    let cancel = AttemptCancel::new();
    *lock(&shared.attempt) = Some(cancel.clone());
    // `quit` is set before the registered cancel is cancelled ([`request_quit`]), so either
    // this sees it or `quit` cancels the one just registered.
    if shared.quit.load(Ordering::SeqCst) {
        lock(&shared.attempt).take();
        return None;
    }
    Some(cancel)
}

/// Marks that `quit` came and cancels the connection in progress (an attempt, the
/// redialer's wait or an auxiliary connection).
fn request_quit(shared: &Shared) {
    shared.quit.store(true, Ordering::SeqCst);
    if let Some(cancel) = lock(&shared.attempt).as_ref() {
        cancel.cancel();
    }
}

/// Ends the second link when it is still up and waits for its thread.
fn end_extra(shared: &Shared, extra: Option<JoinHandle<()>>) {
    if let Some((_, closer)) = lock(&shared.extra).take() {
        closer.force_close(EndReason::Forced);
    }
    if let Some(thread) = extra
        && thread.join().is_err()
    {
        emit("dialer extra-panicked");
    }
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
/// (this end provides the workspace), every Data frame goes back through the writer in that
/// slot. `prefix` goes before the event names (`extra-` for the second link).
fn read_link(mut reader: ChannelReader, provider: Option<&LinkSlot>, prefix: &str) -> &'static str {
    loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Data => {
                    let text = String::from_utf8_lossy(&frame.payload);
                    match provider {
                        None => emit(&format!("dialer {prefix}echo {text}")),
                        Some(slot) => {
                            let sent = lock(slot).as_ref().map(|(writer, _)| {
                                writer.write_frame(StreamTag::Data, &frame.payload)
                            });
                            match sent {
                                Some(Ok(())) => emit(&format!("dialer {prefix}served {text}")),
                                Some(Err(error)) => {
                                    emit(&format!("dialer {prefix}send-failed error={error}"));
                                }
                                // Detaching: the writer is gone.
                                None => {}
                            }
                        }
                    }
                }
                StreamTag::Control => {
                    emit(&format!(
                        "dialer {prefix}control {}",
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
        if line == "drop-session" {
            drop_session(shared);
            continue;
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
    request_quit(shared);
    detach(&shared.link, shared.hold_writer);
    detach(&shared.extra, Duration::ZERO);
}

/// Ends the embedded SSH session the last tunnel opened (`drop-session`).
fn drop_session(shared: &Shared) {
    #[cfg(feature = "embedded-ssh")]
    if let Some(control) = lock(&shared.session).take() {
        control.disconnect();
        emit(&format!("dialer session-dropped id={}", control.id()));
        return;
    }
    let _ = shared;
    emit("dialer drop-session-failed error=no embedded session");
}

/// Sends Detach on the link in `slot` and ends its writer; the server answers by ending its
/// side, which ends the reader. When the drain cap runs out the link is closed both ways.
/// The writer goes `hold` after that (the test hook).
fn detach(slot: &LinkSlot, hold: Duration) {
    let taken = lock(slot).take();
    if let Some((mut writer, closer)) = taken {
        if writer.write_frame(StreamTag::Detach, &[]).is_err() {
            closer.force_close(EndReason::Forced);
            return;
        }
        writer.finish();
        std::thread::sleep(hold);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_transport::RejectReason;

    fn rejected(reason: RejectReason) -> HandshakeError {
        HandshakeError::Rejected {
            reason,
            error: String::new(),
            ext: None,
        }
    }

    /// A cancel in the SSH connection ends like a cancel in the handshake, on the first
    /// attempt and in a redial. Only a refusal with a refusal reason is code 3; any other
    /// failure that is not retried, and the redialer's end, is code 4.
    #[test]
    fn a_cancel_after_quit_is_a_quit_wherever_it_came() {
        let connect = || HandshakeError::Connect(ConnectError::Cancelled);
        let cases = [
            (NoLink::Attempt(HandshakeError::Cancelled), Next::Quit),
            (NoLink::Attempt(connect()), Next::Quit),
            (NoLink::Redial(RedialError::Cancelled), Next::Quit),
            (NoLink::Redial(RedialError::Failed(connect())), Next::Quit),
            (
                NoLink::Redial(RedialError::Failed(HandshakeError::Cancelled)),
                Next::Quit,
            ),
            (
                NoLink::Attempt(rejected(RejectReason::TokenInvalid)),
                Next::Rejected,
            ),
            (
                NoLink::Redial(RedialError::Failed(rejected(RejectReason::RoleRefused))),
                Next::Rejected,
            ),
            (NoLink::Attempt(HandshakeError::SelfAttach), Next::Rejected),
            (NoLink::Attempt(HandshakeError::TimedOut), Next::Redial),
            (NoLink::Redial(RedialError::Exhausted(None)), Next::GaveUp),
            (NoLink::Redial(RedialError::NotRedialed), Next::GaveUp),
            (NoLink::Attempt(protocol()), Next::GaveUp),
            (
                NoLink::Redial(RedialError::Failed(protocol())),
                Next::GaveUp,
            ),
            (
                NoLink::Attempt(HandshakeError::SignatureInvalid),
                Next::GaveUp,
            ),
        ];
        for (no_link, want) in &cases {
            assert_eq!(next_after(no_link, true), *want, "{}", describe(no_link));
        }
        // Without quit a cancel is not one to redial and has no refusal reason: code 4.
        let without_quit = [
            NoLink::Attempt(connect()),
            NoLink::Attempt(HandshakeError::Cancelled),
            NoLink::Redial(RedialError::Cancelled),
            NoLink::Redial(RedialError::Failed(connect())),
        ];
        for no_link in &without_quit {
            assert_eq!(
                next_after(no_link, false),
                Next::GaveUp,
                "{}",
                describe(no_link)
            );
        }
        // Refusals keep code 3 without quit too.
        assert_eq!(
            next_after(&NoLink::Attempt(HandshakeError::SelfAttach), false),
            Next::Rejected
        );
    }

    /// An auxiliary connection gets a cancel that `quit` reaches, and none starts after it.
    #[test]
    fn quit_cancels_an_auxiliary_connection_and_stops_new_ones() {
        let shared = Shared::default();
        let cancel = aux_cancel(&shared).expect("before quit");
        assert!(!cancel.is_cancelled());
        request_quit(&shared);
        assert!(cancel.is_cancelled(), "quit cancels the one in progress");
        assert!(aux_cancel(&shared).is_none(), "none starts after quit");
    }

    fn protocol() -> HandshakeError {
        HandshakeError::Protocol("an unexpected first reply")
    }

    fn describe(no_link: &NoLink) -> String {
        match no_link {
            NoLink::Attempt(e) => format!("attempt: {e:?}"),
            NoLink::Redial(e) => format!("redial: {e:?}"),
        }
    }
}
