//! Embedded SSH sessions: the TCP connection (or the jump host's `direct-tcpip` channel), the
//! handshake with the host key check, authentication, and the pool of open sessions
//! (docs/design/ssh-backends.md "내장 백엔드: 세션 하나에 채널 여럿", "지시어 분류").

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client::{Config, GexParams, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, Certificate, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{Disconnect, Limits, MethodKind};
use tasty_net_ssh_config::options::{AddressFamily, Flag, KeepAlives, PubkeyAuth};
use tasty_net_ssh_config::{Origin, Resolution};

use crate::backend::SshBackendError;
use crate::inspect::{ConfigNote, NoteClass};
use crate::prompt::{KeyboardInteractiveRequest, SshPrompt};
use crate::settings::SshSettings;

use super::algorithms::{Proposal, proposal};
use super::hostkey::{Client, HostCheck};
use super::resolve::{Chain, RSA_MINIMUM_BITS, backend_note, refused};
use super::{
    Bounded, Ctx, LOG_TARGET, Paused, Runtime, SessionControl, ask, bounded, run_blocking,
};

/// How long `ssh` waits between two `ConnectionAttempts` (`sleep(1)`, `sshconnect.c:503`).
const ATTEMPT_PAUSE: Duration = Duration::from_secs(1);
/// `ConnectionAttempts` when the resolution has none (`readconf.c` `fill_default_options`).
const CONNECTION_ATTEMPTS: i32 = 1;
/// `NumberOfPasswordPrompts` when the resolution has none (`readconf.c:2924`).
const PASSWORD_PROMPTS: i32 = 3;
/// `PreferredAuthentications` when the resolution has none (`sshconnect2.c` `authlist`).
const DEFAULT_METHODS: &str = "gssapi-with-mic,hostbased,publickey,keyboard-interactive,password";
/// The originator of the `direct-tcpip` channels of jump hosts (`ssh -W` sends these).
pub(crate) const ORIGINATOR: &str = "127.0.0.1";
/// The start of a PuTTY key file, which OpenSSH does not read.
const PPK_HEADER: &str = "PuTTY-User-Key-File-";

/// One SSH connection: a russh handle and the jump host connection it runs over.
pub(crate) struct Conn {
    pub(crate) handle: Arc<Handle<Client>>,
    rt: tokio::runtime::Handle,
    _jump: Option<Arc<Conn>>,
}

impl Conn {
    /// Sends `SSH_MSG_DISCONNECT` without waiting.
    pub(crate) fn disconnect(&self) {
        let handle = Arc::clone(&self.handle);
        drop(self.rt.spawn(async move {
            // A session that already ended has nobody to tell.
            handle
                .disconnect(Disconnect::ByApplication, "", "")
                .await
                .unwrap_or_default();
        }));
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// An open embedded session: one number of the connector, its connection and the notes of
/// its connection (key files, algorithms, the agent, `@cert-authority` lines).
pub(crate) struct Session {
    pub(crate) id: u64,
    pub(crate) conn: Arc<Conn>,
    pub(crate) notes: Vec<ConfigNote>,
    pub(crate) runtime: Arc<Runtime>,
}

/// The pool key of a chain: the resolutions as `ssh -G` prints them, without the values that
/// are per channel (docs/design/ssh-backends.md "용도마다 해석하고, 같을 때만 세션을
/// 나눈다"), with the key files and known_hosts files.
fn session_key(chain: &Chain) -> String {
    const PER_CHANNEL: [&str; 5] = [
        "sessiontype",
        "sendenv",
        "setenv",
        "remotecommand",
        "requesttty",
    ];
    let mut key = String::new();
    for res in chain.hops.iter().chain(std::iter::once(&chain.dest)) {
        let dump = res.dump_client_config().unwrap_or_default();
        for line in dump.lines() {
            let keyword = line.split(' ').next().unwrap_or_default();
            if !PER_CHANNEL.contains(&keyword) {
                key.push_str(line);
                key.push('\n');
            }
        }
        for k in &res.identity_files {
            key.push_str(k.path.as_deref().unwrap_or_default());
            key.push('\n');
        }
        for f in &res.known_hosts_files {
            key.push_str(&f.to_string_lossy());
            key.push('\n');
        }
        key.push('\0');
    }
    key
}

/// The session for `chain`: an open one with the same key, or a new one.
pub(crate) fn open(ctx: Ctx<'_>, chain: &Chain) -> Result<Arc<Session>, SshBackendError> {
    let key = session_key(chain);
    if let Some(found) = ctx
        .shared
        .pool()
        .get(&key)
        .and_then(std::sync::Weak::upgrade)
        .filter(|s| !s.conn.is_closed())
    {
        return Ok(found);
    }
    let runtime = ctx.shared.runtime(ctx.settings)?;
    let Some(rt) = runtime.handle().cloned() else {
        return Err(SshBackendError::Session {
            detail: "the embedded SSH runtime stopped".into(),
        });
    };
    let notes = Arc::new(Mutex::new(chain.notes.clone()));
    let paused = Arc::new(Paused::default());
    let mut plans = Vec::new();
    for res in chain.hops.iter().chain(std::iter::once(&chain.dest)) {
        plans.push(plan(ctx, res, &notes, &paused).map_err(|refusal| {
            refused(refusal, notes.lock().map(|n| n.clone()).unwrap_or_default())
        })?);
    }
    let conn = run_blocking(&runtime, connect_chain(plans, rt))?;
    let id = ctx.next_session.fetch_add(1, Ordering::Relaxed);
    let notes = notes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let session = Arc::new(Session {
        id,
        conn,
        notes,
        runtime,
    });
    {
        let mut pool = ctx.shared.pool();
        pool.retain(|_, s| s.strong_count() > 0);
        pool.insert(key, Arc::downgrade(&session));
    }
    if let Some(hook) = ctx.hook {
        hook.call(SessionControl {
            id,
            session: Arc::downgrade(&session.conn),
        });
    }
    Ok(session)
}

/// Where the agent is (`IdentityAgent`, else `SSH_AUTH_SOCK`).
enum AgentSource {
    None,
    Env,
    Path(String),
}

/// What authenticating one host needs.
struct AuthPlan {
    user: String,
    host: String,
    methods: Vec<MethodKind>,
    identities: Vec<(PathBuf, Origin)>,
    certificates: Vec<PathBuf>,
    try_cert_files: bool,
    identities_only: bool,
    agent: AgentSource,
    kbd_devices: Option<String>,
    prompts: u32,
    pubkey_accepted: Vec<String>,
    required_rsa: u32,
    prompt: Option<Arc<dyn SshPrompt>>,
    prompt_timeout: Duration,
    paused: Arc<Paused>,
    notes: Arc<Mutex<Vec<ConfigNote>>>,
}

impl AuthPlan {
    fn note(&self, keyword: &str, origin: &Origin, class: NoteClass) {
        let note = backend_note(keyword, origin, class);
        let mut notes = self
            .notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
}

/// How to connect to one host of a chain.
struct HostPlan {
    host: String,
    port: u16,
    config: Arc<Config>,
    check: Arc<HostCheck>,
    connect_timeout: Duration,
    attempts: u32,
    family: Option<AddressFamily>,
    bind: Option<String>,
    tcp_keepalive: bool,
    channel_open_timeout: Duration,
    paused: Arc<Paused>,
    auth: AuthPlan,
}

/// The russh configuration of a resolution: the settings, and the directives where they win.
fn config(
    settings: &SshSettings,
    res: &Resolution,
    proposal: Proposal,
    notes: &Mutex<Vec<ConfigNote>>,
) -> Result<Config, String> {
    let o = &res.options;
    let mut rekey_bytes = settings.rekey_bytes();
    if let Some(limit) = o.rekey_limit.and_then(|l| usize::try_from(l).ok())
        && limit > usize::MIN
    {
        // `Limits::new` asserts the bound of the setting's range.
        let max = SshSettings::default().rekey_bytes();
        if limit > max {
            let note = backend_note(
                "rekeylimit",
                &res.source_of("rekeylimit"),
                NoteClass::Clamped,
            );
            let mut notes = notes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        rekey_bytes = limit.min(max);
    }
    let rekey_time = o
        .rekey_interval
        .and_then(|s| u64::try_from(s).ok())
        .filter(|s| *s > u64::MIN)
        .map_or(settings.rekey_time(), Duration::from_secs);
    let (min, preferred, max) = settings.gex_bits();
    let keepalive = o
        .server_alive_interval
        .and_then(|s| u64::try_from(s).ok())
        .filter(|s| *s > u64::MIN)
        .map(Duration::from_secs);
    Ok(Config {
        limits: Limits::new(rekey_bytes, rekey_bytes, rekey_time),
        window_size: settings.window_size(),
        maximum_packet_size: settings.max_packet_size(),
        channel_buffer_size: settings.channel_buffer(),
        preferred: proposal.preferred,
        inactivity_timeout: None,
        keepalive_interval: keepalive,
        keepalive_max: o
            .server_alive_count_max
            .and_then(|c| usize::try_from(c).ok())
            .unwrap_or(usize::try_from(settings.server_alive_count_max()).unwrap_or(usize::MAX)),
        gex: GexParams::for_client_config(min, preferred, max).map_err(|e| e.to_string())?,
        nodelay: true,
        ..Config::default()
    })
}

fn plan(
    ctx: Ctx<'_>,
    res: &Resolution,
    notes: &Arc<Mutex<Vec<ConfigNote>>>,
    paused: &Arc<Paused>,
) -> Result<HostPlan, crate::inspect::RefusalNote> {
    let o = &res.options;
    let settings = ctx.settings;
    let check = HostCheck::prepare(
        res,
        ctx.prompt.cloned(),
        settings.prompt_timeout(),
        Arc::clone(paused),
        Arc::clone(notes),
    )?;
    let proposal = {
        let mut n = notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        proposal(res, &check.known_types(), &mut n)?
    };
    let pubkey_accepted = proposal.pubkey_accepted.clone();
    let config =
        config(settings, res, proposal, notes).map_err(|message| crate::inspect::RefusalNote {
            kind: "config_error",
            keyword: None,
            origin: None,
            message,
        })?;
    let batch = o.batch_mode.is_some_and(Flag::is_yes);
    let methods = o
        .preferred_authentications
        .as_deref()
        .unwrap_or(DEFAULT_METHODS)
        .split(',')
        .filter_map(|m| m.parse::<MethodKind>().ok())
        .filter(|m| match m {
            MethodKind::PublicKey => !matches!(o.pubkey_authentication, Some(PubkeyAuth::No)),
            MethodKind::Password => !matches!(o.password_authentication, Some(Flag::No)),
            MethodKind::KeyboardInteractive => {
                !matches!(o.kbd_interactive_authentication, Some(Flag::No))
            }
            // GSSAPI and hostbased are shown by the resolution and not tried.
            _ => false,
        })
        .collect();
    let agent = match o.identity_agent.as_deref() {
        Some(a) if a.eq_ignore_ascii_case("none") => AgentSource::None,
        Some("SSH_AUTH_SOCK") | None => AgentSource::Env,
        Some(path) => AgentSource::Path(path.to_string()),
    };
    let to_u32 =
        |v: Option<i32>, default: i32| u32::try_from(v.unwrap_or(default)).unwrap_or_default();
    Ok(HostPlan {
        host: res.host.clone(),
        port: Chain::port(res),
        config: Arc::new(config),
        check: Arc::new(check),
        connect_timeout: o
            .connection_timeout
            .and_then(|s| u64::try_from(s).ok())
            .map_or(settings.connect_timeout(), Duration::from_secs),
        attempts: to_u32(o.connection_attempts, CONNECTION_ATTEMPTS).max(1),
        family: o.address_family,
        bind: o.bind_address.clone(),
        tcp_keepalive: !matches!(o.tcp_keep_alive, Some(KeepAlives::Off)),
        channel_open_timeout: settings.channel_open_timeout(),
        paused: Arc::clone(paused),
        auth: AuthPlan {
            user: Chain::user(res).to_string(),
            host: res.host.clone(),
            methods,
            identities: res
                .identity_files
                .iter()
                .filter_map(|k| Some((PathBuf::from(k.path.as_ref()?), k.origin.clone())))
                .collect(),
            certificates: res
                .certificate_files
                .iter()
                .filter_map(|k| k.path.as_ref().map(PathBuf::from))
                .collect(),
            try_cert_files: res.certificate_files.is_empty(),
            identities_only: o.identities_only.is_some_and(Flag::is_yes),
            agent,
            kbd_devices: o.kbd_interactive_devices.clone(),
            prompts: to_u32(o.number_of_password_prompts, PASSWORD_PROMPTS),
            pubkey_accepted,
            required_rsa: to_u32(o.required_rsa_size, RSA_MINIMUM_BITS),
            prompt: if batch { None } else { ctx.prompt.cloned() },
            prompt_timeout: settings.prompt_timeout(),
            paused: Arc::clone(paused),
            notes: Arc::clone(notes),
        },
    })
}

/// Connects the hosts of a chain in order, each over the one before it.
async fn connect_chain(
    plans: Vec<HostPlan>,
    rt: tokio::runtime::Handle,
) -> Result<Arc<Conn>, SshBackendError> {
    let mut parent: Option<Arc<Conn>> = None;
    for plan in plans {
        let mut handle = match &parent {
            None => connect_direct(&plan).await?,
            Some(jump) => {
                let started = Instant::now();
                let opened = tokio::time::timeout(
                    plan.channel_open_timeout,
                    jump.handle.channel_open_direct_tcpip(
                        plan.host.clone(),
                        u32::from(plan.port),
                        ORIGINATOR,
                        u32::from(u16::MIN),
                    ),
                )
                .await;
                let channel = match opened {
                    Ok(Ok(channel)) => channel,
                    Ok(Err(e)) => {
                        return Err(SshBackendError::Connect {
                            host: plan.host.clone(),
                            port: plan.port,
                            detail: format!("the jump host could not open the channel: {e}"),
                        });
                    }
                    Err(_) => {
                        return Err(SshBackendError::TimedOut {
                            host: plan.host.clone(),
                            waited: started.elapsed(),
                        });
                    }
                };
                handshake(&plan, channel.into_stream(), None).await?
            }
        };
        authenticate(&mut handle, &plan.auth).await?;
        parent = Some(Arc::new(Conn {
            handle: Arc::new(handle),
            rt: rt.clone(),
            _jump: parent.take(),
        }));
    }
    parent.ok_or_else(|| SshBackendError::Session {
        detail: "no host to connect".into(),
    })
}

/// The TCP connection to the first host: its addresses in order, `ConnectionAttempts` rounds.
async fn connect_direct(plan: &HostPlan) -> Result<Handle<Client>, SshBackendError> {
    let fail = |detail: String| SshBackendError::Connect {
        host: plan.host.clone(),
        port: plan.port,
        detail,
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((plan.host.as_str(), plan.port))
        .await
        .map_err(|e| fail(format!("name lookup failed: {e}")))?
        .filter(|a| match plan.family {
            Some(AddressFamily::Inet) => a.is_ipv4(),
            Some(AddressFamily::Inet6) => a.is_ipv6(),
            _ => true,
        })
        .collect();
    if addrs.is_empty() {
        return Err(fail("no address of the allowed family".into()));
    }
    let bind = match &plan.bind {
        Some(b) => Some(
            tokio::net::lookup_host((b.as_str(), u16::MIN))
                .await
                .map_err(|e| fail(format!("BindAddress {b}: {e}")))?
                .collect::<Vec<_>>(),
        ),
        None => None,
    };
    let mut last = String::new();
    for round in 0..plan.attempts {
        if round > u32::MIN {
            tokio::time::sleep(ATTEMPT_PAUSE).await;
        }
        for addr in &addrs {
            let socket = match socket_for(addr, bind.as_deref(), plan.tcp_keepalive) {
                Ok(s) => s,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            match tokio::time::timeout(plan.connect_timeout, socket.connect(*addr)).await {
                Ok(Ok(stream)) => {
                    // russh sets `TCP_NODELAY` only for its own connect.
                    stream.set_nodelay(true).unwrap_or_default();
                    return handshake(plan, stream, Some(addr.ip().to_string())).await;
                }
                Ok(Err(e)) => last = format!("{addr}: {e}"),
                Err(_) => last = format!("{addr}: timed out after {:?}", plan.connect_timeout),
            }
        }
    }
    Err(fail(last))
}

fn socket_for(
    addr: &SocketAddr,
    bind: Option<&[SocketAddr]>,
    keepalive: bool,
) -> std::io::Result<tokio::net::TcpSocket> {
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    socket.set_keepalive(keepalive)?;
    if let Some(bind) = bind {
        let Some(local) = bind.iter().find(|b| b.is_ipv4() == addr.is_ipv4()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "BindAddress has no address of the destination's family",
            ));
        };
        socket.bind(*local)?;
    }
    Ok(socket)
}

/// The SSH handshake over `stream`, with the host key check, within `ConnectTimeout` (prompt
/// time does not count).
async fn handshake<S>(
    plan: &HostPlan,
    stream: S,
    address: Option<String>,
) -> Result<Handle<Client>, SshBackendError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let refusal = Arc::new(Mutex::new(None));
    let client = Client {
        check: Arc::clone(&plan.check),
        address,
        refusal: Arc::clone(&refusal),
    };
    let started = Instant::now();
    let done = bounded(
        russh::client::connect_stream(Arc::clone(&plan.config), stream, client),
        started + plan.connect_timeout,
        &plan.paused,
        None,
        plan.connect_timeout,
    )
    .await;
    let refused = refusal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match (done, refused) {
        (_, Some(refusal)) => Err(SshBackendError::HostKey(Box::new(refusal))),
        (Bounded::Done(Ok(handle)), None) => Ok(handle),
        (Bounded::Done(Err(e)), None) => Err(SshBackendError::Connect {
            host: plan.host.clone(),
            port: plan.port,
            detail: format!("the SSH handshake failed: {e}"),
        }),
        (Bounded::Expired | Bounded::Cancelled, None) => Err(SshBackendError::TimedOut {
            host: plan.host.clone(),
            waited: started.elapsed(),
        }),
    }
}

fn name(method: MethodKind) -> String {
    String::from(&method)
}

/// `none`, then the methods of `PreferredAuthentications` the server allows, as `ssh` does.
async fn authenticate(handle: &mut Handle<Client>, plan: &AuthPlan) -> Result<(), SshBackendError> {
    let session_failed = |e: russh::Error| SshBackendError::Session {
        detail: format!("authentication: {e}"),
    };
    let mut tried = vec![name(MethodKind::None)];
    let mut remaining = match handle
        .authenticate_none(plan.user.clone())
        .await
        .map_err(session_failed)?
    {
        russh::client::AuthResult::Success => return Ok(()),
        russh::client::AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };
    for method in &plan.methods {
        if !remaining.contains(method) {
            continue;
        }
        tried.push(name(*method));
        let outcome = match method {
            MethodKind::PublicKey => publickey(handle, plan).await?,
            MethodKind::KeyboardInteractive => keyboard_interactive(handle, plan).await?,
            MethodKind::Password => password(handle, plan).await?,
            _ => Step::Failed(None),
        };
        match outcome {
            Step::Done => return Ok(()),
            Step::Failed(Some(next)) => remaining = next,
            Step::Failed(None) => {}
        }
    }
    Err(SshBackendError::AuthFailed {
        user: plan.user.clone(),
        host: plan.host.clone(),
        tried,
    })
}

/// How one method ended: success, or failure with the methods the server allows next.
enum Step {
    Done,
    Failed(Option<russh::MethodSet>),
}

fn step(result: russh::client::AuthResult) -> Step {
    match result {
        russh::client::AuthResult::Success => Step::Done,
        russh::client::AuthResult::Failure {
            remaining_methods, ..
        } => Step::Failed(Some(remaining_methods)),
    }
}

/// The signature algorithm name of a user key with `hash` (`rsa-sha2-512` for RSA, ...).
fn signature_name(alg: &Algorithm, hash: Option<HashAlg>) -> String {
    match alg {
        Algorithm::Rsa { .. } => Algorithm::Rsa { hash }.as_str().to_string(),
        other => other.as_str().to_string(),
    }
}

/// A key file as `ssh` reads it: PuTTY files and security keys are skipped with a note.
enum LoadedKey {
    Key(Arc<PrivateKey>),
    Skipped,
}

async fn load_key(plan: &AuthPlan, path: &Path, origin: &Origin) -> LoadedKey {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LoadedKey::Skipped;
    };
    if text.trim_start().starts_with(PPK_HEADER) {
        plan.note("identityfile", origin, NoteClass::UnsupportedKeyFormat);
        return LoadedKey::Skipped;
    }
    let mut key = russh::keys::decode_secret_key(&text, None);
    if matches!(key, Err(russh::keys::Error::KeyIsEncrypted))
        && let Some(prompt) = &plan.prompt
    {
        for _ in 0..plan.prompts {
            let file = path.to_path_buf();
            let Some(Some(passphrase)) = ask(
                Arc::clone(prompt),
                plan.prompt_timeout,
                &plan.paused,
                move |p| p.passphrase(&file),
            )
            .await
            else {
                break;
            };
            if passphrase.is_empty() {
                break;
            }
            key = russh::keys::decode_secret_key(&text, Some(&passphrase));
            if key.is_ok() {
                break;
            }
        }
    }
    match key {
        Ok(key)
            if matches!(
                key.algorithm(),
                Algorithm::SkEd25519 | Algorithm::SkEcdsaSha2NistP256
            ) =>
        {
            plan.note("identityfile", origin, NoteClass::UnsupportedKeyFormat);
            LoadedKey::Skipped
        }
        Ok(key) => LoadedKey::Key(Arc::new(key)),
        Err(e) => {
            tracing::debug!(target: LOG_TARGET, "key file {} not used: {e}", path.display());
            LoadedKey::Skipped
        }
    }
}

/// The agent's identities, or `None` (noted) when there is no agent to ask.
async fn agent(
    plan: &AuthPlan,
) -> Option<(
    AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>,
    Vec<AgentIdentity>,
)> {
    let connected = match &plan.agent {
        AgentSource::None => return None,
        AgentSource::Env => match std::env::var_os("SSH_AUTH_SOCK") {
            Some(path) => connect_agent(Path::new(&path)).await,
            None => default_agent().await,
        },
        AgentSource::Path(path) => connect_agent(Path::new(path)).await,
    };
    let Some(mut client) = connected else {
        plan.note(
            "identityagent",
            &Origin::Default,
            NoteClass::AgentUnavailable,
        );
        return None;
    };
    match client.request_identities().await {
        Ok(ids) => Some((client, ids)),
        Err(_) => {
            plan.note(
                "identityagent",
                &Origin::Default,
                NoteClass::AgentUnavailable,
            );
            None
        }
    }
}

#[cfg(unix)]
async fn connect_agent(
    path: &Path,
) -> Option<AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>> {
    AgentClient::connect_uds(path)
        .await
        .ok()
        .map(AgentClient::dynamic)
}

#[cfg(windows)]
async fn connect_agent(
    path: &Path,
) -> Option<AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>> {
    AgentClient::connect_named_pipe(path)
        .await
        .ok()
        .map(AgentClient::dynamic)
}

/// Without `SSH_AUTH_SOCK`: Windows has the OpenSSH agent pipe and Pageant; elsewhere there is
/// no agent.
#[cfg(windows)]
async fn default_agent()
-> Option<AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>> {
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    if let Some(client) = connect_agent(Path::new(OPENSSH_AGENT_PIPE)).await {
        return Some(client);
    }
    AgentClient::connect_pageant()
        .await
        .ok()
        .map(AgentClient::dynamic)
}

#[cfg(not(windows))]
async fn default_agent()
-> Option<AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>> {
    None
}

/// Public key authentication in the order of `ssh` (`pubkey_prepare`): the key files in order
/// (signed by the agent when it holds the same key), then the agent's other keys unless
/// `IdentitiesOnly`.
async fn publickey(handle: &mut Handle<Client>, plan: &AuthPlan) -> Result<Step, SshBackendError> {
    let session_failed = |e: russh::Error| SshBackendError::Session {
        detail: format!("public key authentication: {e}"),
    };
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(session_failed)?
        .flatten();
    let accepted = |alg: &Algorithm| {
        plan.pubkey_accepted
            .contains(&signature_name(alg, rsa_hash))
    };
    let mut last = Step::Failed(None);
    let mut agent = agent(plan).await;
    let mut used_agent: Vec<Vec<u8>> = Vec::new();
    for (path, origin) in &plan.identities {
        let key = match load_key(plan, path, origin).await {
            LoadedKey::Key(key) => key,
            LoadedKey::Skipped => {
                // The agent can hold the key of a file it cannot read (a `.pub` next to it).
                let Some(public) = read_public(path) else {
                    continue;
                };
                if let Some((client, ids)) = agent.as_mut()
                    && let Some(id) = ids.iter().find(|i| *i.public_key() == public)
                {
                    let id_key = id.public_key().into_owned();
                    used_agent.push(id_key.to_bytes().unwrap_or_default());
                    if !accepted(&id_key.algorithm()) {
                        continue;
                    }
                    let r = handle
                        .authenticate_publickey_with(plan.user.clone(), id_key, rsa_hash, client)
                        .await;
                    match r {
                        Ok(res) => last = step(res),
                        Err(e) => {
                            tracing::debug!(target: LOG_TARGET, "agent signing failed: {e}");
                            continue;
                        }
                    }
                    if matches!(last, Step::Done) {
                        return Ok(last);
                    }
                }
                continue;
            }
        };
        let public = key.public_key().clone();
        if let Some(rsa) = public.key_data().rsa()
            && rsa.key_size() < plan.required_rsa
        {
            continue;
        }
        if !accepted(&key.algorithm()) {
            continue;
        }
        if plan.try_cert_files
            && let Some(cert) = read_certificate(path)
        {
            last = step(
                handle
                    .authenticate_openssh_cert(plan.user.clone(), Arc::clone(&key), cert)
                    .await
                    .map_err(session_failed)?,
            );
            if matches!(last, Step::Done) {
                return Ok(last);
            }
        }
        last = step(
            handle
                .authenticate_publickey(
                    plan.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::clone(&key), rsa_hash),
                )
                .await
                .map_err(session_failed)?,
        );
        if matches!(last, Step::Done) {
            return Ok(last);
        }
    }
    for cert_path in &plan.certificates {
        let Ok(text) = std::fs::read_to_string(cert_path) else {
            continue;
        };
        let Ok(cert) = Certificate::from_openssh(text.trim()) else {
            continue;
        };
        // The private key of the certificate is one of the key files.
        for (path, origin) in &plan.identities {
            let LoadedKey::Key(key) = load_key(plan, path, origin).await else {
                continue;
            };
            if key.public_key().key_data() != cert.public_key() {
                continue;
            }
            last = step(
                handle
                    .authenticate_openssh_cert(plan.user.clone(), key, cert.clone())
                    .await
                    .map_err(session_failed)?,
            );
            if matches!(last, Step::Done) {
                return Ok(last);
            }
            break;
        }
    }
    if !plan.identities_only
        && let Some((client, ids)) = agent.as_mut()
    {
        for id in ids.clone() {
            let key = id.public_key().into_owned();
            if used_agent.contains(&key.to_bytes().unwrap_or_default())
                || !accepted(&key.algorithm())
            {
                continue;
            }
            match handle
                .authenticate_publickey_with(plan.user.clone(), key, rsa_hash, client)
                .await
            {
                Ok(res) => last = step(res),
                Err(e) => {
                    tracing::debug!(target: LOG_TARGET, "agent signing failed: {e}");
                    continue;
                }
            }
            if matches!(last, Step::Done) {
                return Ok(last);
            }
        }
    }
    Ok(last)
}

/// The public key next to a key file (`<file>.pub`).
fn read_public(path: &Path) -> Option<russh::keys::PublicKey> {
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    let text = std::fs::read_to_string(PathBuf::from(pub_path)).ok()?;
    russh::keys::PublicKey::from_openssh(text.trim()).ok()
}

/// The certificate `ssh` offers after a key file when no `CertificateFile` is given
/// (`<file>-cert`, then `<file>-cert.pub`, `ssh.c:2425`-`:2451`).
fn read_certificate(path: &Path) -> Option<Certificate> {
    let mut cert = path.as_os_str().to_owned();
    cert.push("-cert");
    let mut with_pub = cert.clone();
    with_pub.push(".pub");
    [PathBuf::from(cert), PathBuf::from(with_pub)]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| Certificate::from_openssh(text.trim()).ok())
}

async fn keyboard_interactive(
    handle: &mut Handle<Client>,
    plan: &AuthPlan,
) -> Result<Step, SshBackendError> {
    let session_failed = |e: russh::Error| SshBackendError::Session {
        detail: format!("keyboard-interactive authentication: {e}"),
    };
    let mut response = handle
        .authenticate_keyboard_interactive_start(plan.user.clone(), plan.kbd_devices.clone())
        .await
        .map_err(session_failed)?;
    let mut rounds = u32::MIN;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(Step::Done),
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => return Ok(Step::Failed(Some(remaining_methods))),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                let answers = if prompts.is_empty() {
                    Vec::new()
                } else {
                    rounds += 1;
                    if rounds > plan.prompts {
                        return Ok(Step::Failed(None));
                    }
                    let Some(prompt) = &plan.prompt else {
                        return Ok(Step::Failed(None));
                    };
                    let request = KeyboardInteractiveRequest {
                        name,
                        instructions,
                        prompts: prompts.into_iter().map(|p| (p.prompt, p.echo)).collect(),
                    };
                    match ask(
                        Arc::clone(prompt),
                        plan.prompt_timeout,
                        &plan.paused,
                        move |p| p.keyboard_interactive(&request),
                    )
                    .await
                    {
                        Some(Some(answers)) => answers,
                        _ => return Ok(Step::Failed(None)),
                    }
                };
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(session_failed)?;
            }
        }
    }
}

async fn password(handle: &mut Handle<Client>, plan: &AuthPlan) -> Result<Step, SshBackendError> {
    let Some(prompt) = &plan.prompt else {
        return Ok(Step::Failed(None));
    };
    let mut last = Step::Failed(None);
    for _ in 0..plan.prompts {
        let (user, host) = (plan.user.clone(), plan.host.clone());
        let Some(Some(password)) = ask(
            Arc::clone(prompt),
            plan.prompt_timeout,
            &plan.paused,
            move |p| p.password(&user, &host),
        )
        .await
        else {
            return Ok(last);
        };
        last = step(
            handle
                .authenticate_password(plan.user.clone(), password)
                .await
                .map_err(|e| SshBackendError::Session {
                    detail: format!("password authentication: {e}"),
                })?,
        );
        if matches!(last, Step::Done) {
            return Ok(last);
        }
    }
    Ok(last)
}
