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
use crate::cancel::SshCancel;
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
/// The PEM line of an encrypted PKCS#8 key. russh decodes such a file without a passphrase as
/// a plain one and gives a DER error, not `KeyIsEncrypted` (`keys/format/pkcs8.rs`).
const PKCS8_ENCRYPTED_HEADER: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";

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
    open_within(ctx, chain, None).map_err(|stop| match stop {
        OpenStop::Failed(e) => e,
        // Without a bound the open is neither cancelled nor out of time here.
        OpenStop::Expired | OpenStop::Cancelled => SshBackendError::Session {
            detail: "the session open stopped".into(),
        },
    })
}

/// A bound on opening a new session: its deadline (prompt time not counted), and the cancel
/// looked at every `poll`.
pub(crate) struct OpenBound {
    pub(crate) deadline: Instant,
    pub(crate) cancel: Option<SshCancel>,
    pub(crate) poll: Duration,
}

/// Why [`open_within`] gave no session.
pub(crate) enum OpenStop {
    Failed(SshBackendError),
    Expired,
    Cancelled,
}

/// [`open`] within `bound`: connecting, the host key check and authentication (agent answers
/// included) stop at its deadline or cancel.
pub(crate) fn open_within(
    ctx: Ctx<'_>,
    chain: &Chain,
    bound: Option<OpenBound>,
) -> Result<Arc<Session>, OpenStop> {
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
    let runtime = ctx.shared.runtime(ctx.settings).map_err(OpenStop::Failed)?;
    let Some(rt) = runtime.handle().cloned() else {
        return Err(OpenStop::Failed(SshBackendError::Session {
            detail: "the embedded SSH runtime stopped".into(),
        }));
    };
    let notes = Arc::new(Mutex::new(chain.notes.clone()));
    let paused = Arc::new(Paused::default());
    let mut plans = Vec::new();
    for res in chain.hops.iter().chain(std::iter::once(&chain.dest)) {
        plans.push(plan(ctx, res, &notes, &paused).map_err(|refusal| {
            OpenStop::Failed(refused(
                refusal,
                notes.lock().map(|n| n.clone()).unwrap_or_default(),
            ))
        })?);
    }
    let conn = match bound {
        None => run_blocking(&runtime, connect_chain(plans, rt)).map_err(OpenStop::Failed)?,
        Some(bound) => {
            let paused = Arc::clone(&paused);
            let out = run_blocking(&runtime, async move {
                Ok(bounded(
                    connect_chain(plans, rt),
                    bound.deadline,
                    &paused,
                    bound.cancel.as_ref(),
                    bound.poll,
                )
                .await)
            })
            .map_err(OpenStop::Failed)?;
            match out {
                Bounded::Done(conn) => conn.map_err(OpenStop::Failed)?,
                Bounded::Expired => return Err(OpenStop::Expired),
                Bounded::Cancelled => return Err(OpenStop::Cancelled),
            }
        }
    };
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
/// When the server answers with another list of methods (a partial success that asks for one
/// more method gives one), the order starts over from the first preferred method with the new
/// list, as `authmethod_get` (`sshconnect2.c:2457`) does, so a method that comes earlier in the
/// order can still follow the one the server accepted. A partial success with the same list
/// tries the same method again from its start (`pubkey_reset`, `sshconnect2.c:636`).
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
    let mut order = plan.methods.iter();
    let mut again = None;
    while let Some(method) = again.take().or_else(|| order.next()) {
        if !remaining.contains(method) {
            continue;
        }
        tried.push(name(*method));
        let outcome = match method {
            MethodKind::PublicKey => publickey(handle, plan).await?,
            MethodKind::KeyboardInteractive => keyboard_interactive(handle, plan).await?,
            MethodKind::Password => password(handle, plan).await?,
            _ => Step::FAILED,
        };
        match outcome {
            Step::Done => return Ok(()),
            Step::Failed {
                next: Some(next),
                partial,
            } => {
                if next != remaining {
                    order = plan.methods.iter();
                } else if partial {
                    again = Some(method);
                }
                remaining = next;
            }
            Step::Failed { next: None, .. } => {}
        }
    }
    Err(SshBackendError::AuthFailed {
        user: plan.user.clone(),
        host: plan.host.clone(),
        tried,
        notes: plan
            .notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    })
}

/// How one method ended: success, or failure with the methods the server allows next and
/// whether the server accepted the method as one of several it asks for (partial success).
enum Step {
    Done,
    Failed {
        next: Option<russh::MethodSet>,
        partial: bool,
    },
}

impl Step {
    /// A failure without an answer of the server.
    const FAILED: Self = Self::Failed {
        next: None,
        partial: false,
    };

    /// Whether the method is over: success, or a partial success, after which the order
    /// goes on with the server's answer.
    fn ends(&self) -> bool {
        !matches!(self, Self::Failed { partial: false, .. })
    }
}

fn step(result: russh::client::AuthResult) -> Step {
    match result {
        russh::client::AuthResult::Success => Step::Done,
        russh::client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => Step::Failed {
            next: Some(remaining_methods),
            partial: partial_success,
        },
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
    // The permissions are judged on the file that is read, through one open handle, as
    // `sshkey_load_private_type` (`authfile.c`) does with `fstat` on its descriptor: a file
    // swapped in between cannot be read unjudged.
    let Ok(mut file) = std::fs::File::open(path) else {
        return LoadedKey::Skipped;
    };
    if !key_permissions_ok(&file) {
        plan.note("identityfile", origin, NoteClass::KeyPermissions);
        return LoadedKey::Skipped;
    }
    let mut text = String::new();
    if std::io::Read::read_to_string(&mut file, &mut text).is_err() {
        return LoadedKey::Skipped;
    }
    if text.trim_start().starts_with(PPK_HEADER) {
        plan.note("identityfile", origin, NoteClass::UnsupportedKeyFormat);
        return LoadedKey::Skipped;
    }
    let mut key = russh::keys::decode_secret_key(&text, None);
    let encrypted = match &key {
        Err(russh::keys::Error::KeyIsEncrypted) => true,
        Err(_) => text.lines().any(|l| l.trim() == PKCS8_ENCRYPTED_HEADER),
        Ok(_) => false,
    };
    if encrypted && let Some(prompt) = &plan.prompt {
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

/// Whether an open private key file may be used, as OpenSSH `sshkey_perm_ok` (`authfile.c`)
/// judges it on desktop Unix: a file the user owns that the group or others can read or write
/// (`mode & 077`) is not used. A file of another owner is not judged, as in OpenSSH. Where the
/// local user ID is not known (macOS here), every file is judged, which refuses more and never
/// less. A file whose metadata cannot be read is not used either, as `fstat` failing stops
/// OpenSSH. Android and iOS app storage has no such bits to judge.
#[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
pub(crate) fn key_permissions_ok(file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    const GROUP_OTHER_BITS: u32 = 0o077;
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let owned = super::resolve::local_uid().is_none_or(|uid| uid == meta.uid());
    !(owned && meta.mode() & GROUP_OTHER_BITS != 0)
}

/// Windows, Android and iOS: no Unix permission bits to judge.
#[cfg(not(all(unix, not(any(target_os = "android", target_os = "ios")))))]
pub(crate) fn key_permissions_ok(_file: &std::fs::File) -> bool {
    true
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

/// One public key attempt of the list `ssh` makes (`pubkey_prepare`, `sshconnect2.c`).
enum Candidate {
    /// A user certificate: a `CertificateFile`, the `<file>-cert` of a key file (`file`), or
    /// one the agent holds. The agent signs it when it holds the key, else the key file does.
    Cert {
        cert: Box<Certificate>,
        file: Option<(PathBuf, Origin)>,
    },
    /// A key the agent holds, signed by the agent: a key file's, or one of its own.
    Agent(russh::keys::PublicKey),
    /// A key file the agent does not hold. It is read (with a passphrase when it asks for one)
    /// when it is tried. `public` is its public key when it can be read without that.
    File {
        path: PathBuf,
        origin: Origin,
        public: Option<russh::keys::PublicKey>,
    },
}

/// The public key of a key file without its passphrase: the file itself when it is a public
/// key (an `IdentityFile` may name the `.pub` file of a key the agent holds), else
/// `<file>.pub`, else the public part of the file, which the OpenSSH format keeps readable when
/// the key is encrypted (`sshkey_load_public`, `authfile.c`).
fn file_public(path: &Path) -> Option<russh::keys::PublicKey> {
    // `sshkey_load_public` (`authfile.c`): the file itself as a public key, then `<file>.pub`,
    // then the public part of the private key file.
    let text = std::fs::read_to_string(path).ok();
    if let Some(public) = text
        .as_deref()
        .and_then(|t| russh::keys::PublicKey::from_openssh(t.trim()).ok())
    {
        return Some(public);
    }
    if let Some(public) = read_public(path) {
        return Some(public);
    }
    let text = text?;
    match PrivateKey::from_openssh(text.trim()) {
        Ok(key) => Some(key.public_key().clone()),
        Err(_) => russh::keys::decode_secret_key(&text, None)
            .ok()
            .map(|key| key.public_key().clone()),
    }
}

/// Whether two public keys are the same key (the comment is not part of it).
fn same_key(a: &russh::keys::PublicKey, b: &russh::keys::PublicKey) -> bool {
    a.key_data() == b.key_data()
}

/// Whether an RSA key is smaller than `RequiredRSASize` (`sshkey_check_rsa_length`).
fn too_small(key: &russh::keys::PublicKey, required: u32) -> bool {
    data_too_small(key.key_data(), required)
}

/// [`too_small`] for the key data of a key or a certificate.
fn data_too_small(data: &russh::keys::ssh_key::public::KeyData, required: u32) -> bool {
    data.rsa().is_some_and(|rsa| rsa.key_size() < required)
}

/// The list of `ssh` (`pubkey_prepare`): the `CertificateFile` certificates, the agent's keys
/// that are key files (in the agent's order), the agent's other keys unless `IdentitiesOnly`,
/// then the key files the agent does not hold, each followed by its `<file>-cert` when no
/// `CertificateFile` is given. Agent keys and certificates (of every source) smaller than
/// `RequiredRSASize` are left out, as `pubkey_prepare` checks each with
/// `sshkey_check_rsa_length`.
fn candidates(plan: &AuthPlan, agent_ids: &[AgentIdentity]) -> Vec<Candidate> {
    use russh::keys::ssh_key::certificate::CertType;
    let user_cert = |cert: &Certificate| {
        cert.cert_type() == CertType::User && !data_too_small(cert.public_key(), plan.required_rsa)
    };
    let mut files = Vec::new();
    for (path, origin) in &plan.identities {
        files.push(Candidate::File {
            path: path.clone(),
            origin: origin.clone(),
            public: file_public(path),
        });
        if plan.try_cert_files
            && let Some(cert) = read_certificate(path).filter(user_cert)
        {
            files.push(Candidate::Cert {
                cert: Box::new(cert),
                file: Some((path.clone(), origin.clone())),
            });
        }
    }
    let mut list: Vec<Candidate> = plan
        .certificates
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| Certificate::from_openssh(text.trim()).ok())
        .filter(user_cert)
        .map(|cert| Candidate::Cert {
            cert: Box::new(cert),
            file: None,
        })
        .collect();
    let mut matched = Vec::new();
    let mut others = Vec::new();
    for id in agent_ids {
        let candidate = match id {
            AgentIdentity::PublicKey { key, .. } => {
                if too_small(key, plan.required_rsa) {
                    continue;
                }
                let held = files.iter().position(
                    |f| matches!(f, Candidate::File { public: Some(p), .. } if same_key(p, key)),
                );
                if let Some(at) = held {
                    files.remove(at);
                    matched.push(Candidate::Agent(key.clone()));
                    continue;
                }
                Candidate::Agent(key.clone())
            }
            AgentIdentity::Certificate { certificate, .. } => {
                if !user_cert(certificate) {
                    continue;
                }
                Candidate::Cert {
                    cert: Box::new(certificate.clone()),
                    file: None,
                }
            }
        };
        if !plan.identities_only {
            others.push(candidate);
        }
    }
    list.extend(matched);
    list.extend(others);
    list.extend(files);
    list
}

/// Public key authentication in the order of `ssh` ([`candidates`]). A key is tried only when
/// `PubkeyAcceptedAlgorithms` has its signature algorithm (a certificate: its certificate
/// type), as `pubkey_prepare` filters the list; a key file whose public key cannot be read
/// without its passphrase is judged once it is read. A key file is read when it is tried, so
/// its passphrase is asked only if no agent key or earlier key got in.
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
    let cert_accepted = |cert: &Certificate| {
        let alg = match cert.algorithm() {
            Algorithm::Rsa { .. } => Algorithm::Rsa { hash: rsa_hash },
            other => other,
        };
        plan.pubkey_accepted.contains(&alg.to_certificate_type())
    };
    let mut agent = agent(plan).await;
    let agent_ids = agent
        .as_ref()
        .map(|(_, ids)| ids.clone())
        .unwrap_or_default();
    let list = candidates(plan, &agent_ids);
    // Key files read in this round, so one passphrase serves the key and its certificate.
    let mut loaded: Vec<(PathBuf, Option<Arc<PrivateKey>>)> = Vec::new();
    let mut last = Step::FAILED;
    for candidate in &list {
        let result = match candidate {
            Candidate::Agent(key) => {
                let Some((client, _)) = agent.as_mut() else {
                    continue;
                };
                if !accepted(&key.algorithm()) {
                    continue;
                }
                match handle
                    .authenticate_publickey_with(plan.user.clone(), key.clone(), rsa_hash, client)
                    .await
                {
                    Ok(res) => res,
                    Err(e) => {
                        tracing::debug!(target: LOG_TARGET, "agent signing failed: {e}");
                        continue;
                    }
                }
            }
            Candidate::Cert { cert, file } => {
                if !cert_accepted(cert) {
                    continue;
                }
                let held = agent_ids.iter().any(|id| {
                    matches!(id, AgentIdentity::PublicKey { key, .. }
                        if key.key_data() == cert.public_key())
                }) || agent_ids.iter().any(|id| {
                    matches!(id, AgentIdentity::Certificate { certificate, .. }
                        if certificate == &**cert)
                });
                let mut signed = None;
                if held && let Some((client, _)) = agent.as_mut() {
                    match handle
                        .authenticate_certificate_with(
                            plan.user.clone(),
                            Certificate::clone(cert),
                            rsa_hash,
                            client,
                        )
                        .await
                    {
                        Ok(res) => signed = Some(res),
                        Err(e) => {
                            tracing::debug!(target: LOG_TARGET, "agent signing failed: {e}");
                            continue;
                        }
                    }
                }
                if let Some(res) = signed {
                    res
                } else {
                    // The key file of the certificate: its own file, or a key file with the
                    // same public key (`sign_and_send_pubkey`).
                    let file = file.clone().or_else(|| {
                        list.iter().find_map(|c| match c {
                            Candidate::File {
                                path,
                                origin,
                                public: Some(p),
                            } if p.key_data() == cert.public_key() => {
                                Some((path.clone(), origin.clone()))
                            }
                            _ => None,
                        })
                    });
                    let Some((path, origin)) = file else {
                        continue;
                    };
                    let Some(key) = read_key(plan, &mut loaded, &path, &origin).await else {
                        continue;
                    };
                    if key.public_key().key_data() != cert.public_key() {
                        continue;
                    }
                    // Signed here with the SHA-2 hash the name was judged with, so the name
                    // sent (`rsa-sha2-*-cert-v01@openssh.com`) is the one `cert_accepted` saw.
                    match handle
                        .authenticate_certificate_with(
                            plan.user.clone(),
                            Certificate::clone(cert),
                            rsa_hash,
                            &mut KeySigner(key),
                        )
                        .await
                    {
                        Ok(res) => res,
                        Err(russh::AgentAuthError::Send(_)) => {
                            return Err(session_failed(russh::Error::SendError));
                        }
                        Err(e) => {
                            tracing::debug!(target: LOG_TARGET, "key file signing failed: {e}");
                            continue;
                        }
                    }
                }
            }
            Candidate::File {
                path,
                origin,
                public,
            } => {
                if public.as_ref().is_some_and(|p| !accepted(&p.algorithm())) {
                    continue;
                }
                let Some(key) = read_key(plan, &mut loaded, path, origin).await else {
                    continue;
                };
                if !accepted(&key.algorithm()) {
                    continue;
                }
                handle
                    .authenticate_publickey(
                        plan.user.clone(),
                        PrivateKeyWithHashAlg::new(key, rsa_hash),
                    )
                    .await
                    .map_err(session_failed)?
            }
        };
        last = step(result);
        if last.ends() {
            return Ok(last);
        }
    }
    Ok(last)
}

/// Signs an authentication request with a key file's private key, with the RSA hash of the
/// request, and appends the signature as the agent client does.
pub(crate) struct KeySigner(pub(crate) Arc<PrivateKey>);

impl russh::Signer for KeySigner {
    type Error = russh::AgentAuthError;

    fn auth_sign(
        &mut self,
        _key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        use russh::keys::signature::Signer as _;
        use russh::keys::ssh_encoding::Encode as _;
        use russh::keys::ssh_key::private::KeypairData;
        let signed = match self.0.key_data() {
            KeypairData::Rsa(pair) => (pair, hash_alg).try_sign(&to_sign),
            other => other.try_sign(&to_sign),
        }
        .map_err(russh::keys::ssh_key::Error::from)
        .and_then(|signature| Ok(signature.encode_vec()?))
        .and_then(|blob| Ok(blob.encode(&mut to_sign)?));
        async move {
            signed.map_err(russh::keys::Error::from)?;
            Ok(to_sign)
        }
    }
}

/// A key file read once per round ([`load_key`]); a key smaller than `RequiredRSASize` is not
/// used (`sshconnect2.c` `load_identity_file`).
async fn read_key(
    plan: &AuthPlan,
    loaded: &mut Vec<(PathBuf, Option<Arc<PrivateKey>>)>,
    path: &Path,
    origin: &Origin,
) -> Option<Arc<PrivateKey>> {
    if let Some((_, key)) = loaded.iter().find(|(p, _)| p == path) {
        return key.clone();
    }
    let key = match load_key(plan, path, origin).await {
        LoadedKey::Key(key) if !too_small(key.public_key(), plan.required_rsa) => Some(key),
        LoadedKey::Key(_) | LoadedKey::Skipped => None,
    };
    loaded.push((path.to_path_buf(), key.clone()));
    key
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
                remaining_methods,
                partial_success,
            } => {
                return Ok(Step::Failed {
                    next: Some(remaining_methods),
                    partial: partial_success,
                });
            }
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
                        return Ok(Step::FAILED);
                    }
                    let Some(prompt) = &plan.prompt else {
                        return Ok(Step::FAILED);
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
                        _ => return Ok(Step::FAILED),
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
        return Ok(Step::FAILED);
    };
    let mut last = Step::FAILED;
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
        if last.ends() {
            return Ok(last);
        }
    }
    Ok(last)
}
