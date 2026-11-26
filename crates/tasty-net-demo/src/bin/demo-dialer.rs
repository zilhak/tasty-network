//! `demo-dialer`: reaches a `demo-server` over SSH and keeps the attach link up.
//!
//! Each attempt reads the server's connection record over SSH (`cat`, then `type`, of
//! `--record` on the SSH host), opens or reuses the `-L` tunnel to the record's port and runs
//! handshake v2 with the record's token. While a link is up, every stdin line `send <text>`
//! goes out as a Data frame and the echo is printed. When the link is lost the dialer waits
//! with backoff and connects again from discovery. A refusal that connecting again cannot
//! change (a wrong token, for example) ends the dialer with exit code 3. `quit` or the end of
//! stdin detaches and ends with exit code 0.
//!
//! ```text
//! demo-dialer --destination <user@host> --record <remote path>
//!             [--ssh-port <port>] [--identity <file>] [--option Key=Value]...
//!             [--ssh <program>] [--verify-host-keys] [--read-timeout-ms <ms>]
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
//! | `dialer control <json>` | a Control frame arrived (the server's snapshot) |
//! | `dialer echo <text>` | a Data frame arrived |
//! | `dialer lost failure=<name>` | the link's reader failed |
//! | `dialer retry error=<text>` | an attempt failed in a way connecting again can change |
//! | `dialer rejected reason=<wire reason or -> error=<text>` | it did not; the dialer ends |
//! | `dialer bye` | the dialer ends after `quit` |
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tasty_net_demo::{Args, emit, error_chain, failure_name, link_timeouts};
use tasty_net_frame::StreamTag;
use tasty_net_identity::DeviceKey;
use tasty_net_ssh::{Backoff, PortDiscoveryConfig, PortMode, SpawnHook, SshLauncher, SshTarget};
use tasty_net_transport::{
    AttemptCancel, ChannelReader, ChannelWriter, DialConfig, EndReason, HandshakeError, Holder,
    LinkCloser, SshTransport, Transport, dial_handshake,
};

const USAGE: &str = "usage: demo-dialer --destination <user@host> --record <remote path> \
[--ssh-port <port>] [--identity <file>] [--option Key=Value]... [--ssh <program>] \
[--verify-host-keys] [--read-timeout-ms <ms>]";

/// Exit code when a refusal ends the dialer without another attempt.
const EXIT_REJECTED: u8 = 3;

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

/// What the stdin thread and the connection loop share.
#[derive(Default)]
struct Shared {
    quit: AtomicBool,
    /// The writer and closer of the link that is up.
    link: Mutex<Option<(ChannelWriter, LinkCloser)>>,
    /// The attempt in progress, so `quit` can cancel it.
    attempt: Mutex<Option<AttemptCancel>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
    ])?;
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
    let transport = SshTransport::new(launcher, discovery, PortMode::FileUnix);
    let config = DialConfig::new(
        DeviceKey::generate()?,
        format!("demo-dialer-{}", std::process::id()),
    )
    .with_timeouts(timeouts);

    emit(&format!("dialer pid={}", std::process::id()));
    let shared = Arc::new(Shared::default());
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || read_stdin(&shared, &wake_tx))?;
    }

    let mut backoff = Backoff::new();
    let mut attempt = 0u32;
    while !shared.quit.load(Ordering::SeqCst) {
        attempt += 1;
        emit(&format!("dialer attempt n={attempt}"));
        let cancel = AttemptCancel::new();
        *lock(&shared.attempt) = Some(cancel.clone());
        if shared.quit.load(Ordering::SeqCst) {
            cancel.cancel();
        }
        let result = transport
            .connect(&target, &cancel)
            .map_err(HandshakeError::Connect)
            .and_then(|conn| dial_handshake(conn, &config, Holder::Dialer, None, &cancel));
        lock(&shared.attempt).take();
        match result {
            Ok(mut link) => {
                backoff.reset();
                let Some((reader, writer)) = link.take_ordered() else {
                    return Err("a new link has no ordered channel".into());
                };
                *lock(&shared.link) = Some((writer, link.closer()));
                emit(&format!(
                    "dialer connected attempt={attempt} instance={} tunnel_pid={}",
                    link.peer().instance_id(),
                    tunnel_pid.load(Ordering::SeqCst)
                ));
                if shared.quit.load(Ordering::SeqCst) {
                    detach(&shared);
                }
                let end = read_link(reader);
                lock(&shared.link).take();
                drop(link);
                if shared.quit.load(Ordering::SeqCst) {
                    break;
                }
                emit(&format!("dialer lost failure={end}"));
            }
            Err(HandshakeError::Cancelled) if shared.quit.load(Ordering::SeqCst) => break,
            Err(error) if !error.is_retryable() => {
                let reason = match &error {
                    HandshakeError::Rejected { reason, .. } => reason.as_str().to_string(),
                    _ => "-".into(),
                };
                emit(&format!(
                    "dialer rejected reason={reason} error={}",
                    error_chain(&error)
                ));
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
            Err(error) => emit(&format!("dialer retry error={}", error_chain(&error))),
        }
        // Wait out the backoff; `quit` wakes the wait.
        let _woken = wake_rx.recv_timeout(backoff.current()).is_ok();
        backoff.advance();
    }
    emit("dialer bye");
    Ok(ExitCode::SUCCESS)
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

/// Prints every frame of the link until it ends. Returns the end's name.
fn read_link(mut reader: ChannelReader) -> &'static str {
    loop {
        match reader.read_frame() {
            Ok(frame) => match frame.tag {
                StreamTag::Data => {
                    emit(&format!(
                        "dialer echo {}",
                        String::from_utf8_lossy(&frame.payload)
                    ));
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

/// Reads stdin commands: `send <text>` and `quit`. The end of stdin is `quit`.
fn read_stdin(shared: &Shared, wake: &mpsc::Sender<()>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end();
        if line == "quit" {
            break;
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
    if let Some(cancel) = lock(&shared.attempt).as_ref() {
        cancel.cancel();
    }
    detach(shared);
    // The loop may be waiting out a backoff. A failed send means it already ended.
    let _sent = wake.send(()).is_ok();
}

/// Sends Detach on the link that is up and ends its writer; the server answers by ending its
/// side, which ends the reader. When the drain cap runs out the link is closed both ways.
fn detach(shared: &Shared) {
    let taken = lock(&shared.link).take();
    if let Some((mut writer, closer)) = taken {
        if writer.write_frame(StreamTag::Detach, &[]).is_err() {
            closer.force_close(EndReason::Forced);
            return;
        }
        writer.finish();
    }
}
