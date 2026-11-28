//! The configuration of one purpose, resolved by the OpenSSH port with the values of the
//! option order (docs/design/ssh-backends.md "옵션 적용 순서", "용도마다 해석하고, 같을 때만
//! 세션을 나눈다").

use std::collections::BTreeMap;
use std::path::PathBuf;

use tasty_net_ssh_config::options::Jump;
use tasty_net_ssh_config::{
    CommandLine, CommandLineValue, ConfigError, ConfigFiles, ConfigInputs, ConfigSet, Origin,
    Refusal, RefusalKind, Resolution, ResolveContext,
};

use crate::SshTarget;
use crate::backend::{EffectiveTarget, SshBackendError};
use crate::inspect::{
    ConfigFileReport, ConfigFileState, ConfigNote, JumpHop, NoteAction, NoteClass, RefusalNote,
    ResolvedTarget, ValueSource,
};
use crate::settings::SshSettings;

/// The OpenSSH default port.
pub(crate) const SSH_PORT: u16 = 22;
/// `RequiredRSASize` when the resolution has none (`SSH_RSA_MINIMUM_MODULUS_SIZE`, `ssh.h`).
pub(crate) const RSA_MINIMUM_BITS: i32 = 1024;

/// What a resolution is for, with the remote command when there is one.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Purpose<'a> {
    /// A `-L` tunnel: `sessiontype none`, no command.
    Tunnel,
    /// Port discovery: `sessiontype default` and the discovery command.
    Discovery(&'a str),
}

/// The resolutions of one purpose: the jump hosts (first hop first) and the destination.
#[derive(Debug)]
pub(crate) struct Chain {
    pub(crate) hops: Vec<Resolution>,
    pub(crate) dest: Resolution,
    /// The notes of every resolution of the chain, without repeats.
    pub(crate) notes: Vec<ConfigNote>,
}

impl Chain {
    /// The remote user of a resolution (checked by [`resolve_chain`]).
    pub(crate) fn user(res: &Resolution) -> &str {
        res.options.user.as_deref().unwrap_or_default()
    }

    pub(crate) fn port(res: &Resolution) -> u16 {
        res.options.port.unwrap_or(SSH_PORT)
    }

    /// The effective target: host, port, user and the hops as `user@host:port`.
    pub(crate) fn effective(&self) -> EffectiveTarget {
        EffectiveTarget {
            hostname: self.dest.host.clone(),
            port: Self::port(&self.dest),
            user: Self::user(&self.dest).to_string(),
            jump: self
                .hops
                .iter()
                .map(|h| format!("{}@{}:{}", Self::user(h), h.host, Self::port(h)))
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    pub(crate) fn resolved(&self) -> ResolvedTarget {
        let mut resolved = resolved_target(&self.dest);
        resolved.proxy_jump = self
            .hops
            .iter()
            .map(|h| JumpHop {
                hostname: h.host.clone(),
                port: Self::port(h),
                user: Self::user(h).to_string(),
            })
            .collect();
        resolved
    }
}

/// The files `settings` name (`embedded.config_file`, `embedded.ssh_dir`,
/// `embedded.system_config_file`).
fn config_files(settings: &SshSettings) -> ConfigFiles {
    match settings.embedded_config_file() {
        Some(Some(path)) => ConfigFiles::Only(path),
        Some(None) => ConfigFiles::None,
        None => ConfigFiles::Default {
            user: settings.embedded_ssh_dir().map(|d| d.join("config")),
            system: settings.embedded_system_config_file(),
        },
    }
}

/// Reads the top-level configuration files.
pub(crate) fn load(settings: &SshSettings) -> Result<ConfigSet, SshBackendError> {
    ConfigSet::load(ConfigInputs {
        files: config_files(settings),
    })
    .map_err(|e| {
        let ConfigError::CannotOpen { path, .. } = &e;
        SshBackendError::ConfigRefused {
            refusal: Box::new(RefusalNote {
                kind: "config_error",
                keyword: None,
                origin: Some(format!("file:{}", path.display())),
                message: e.to_string(),
            }),
            notes: Vec::new(),
        }
    })
}

fn option(value: String, label: &str) -> CommandLineValue {
    CommandLineValue {
        value: CommandLine::Option(value),
        label: label.to_string(),
    }
}

/// The command-line values of the option order, places 1 to 8. The connection sharing and
/// tunnel options of place 4 are left out: they are about `ssh` processes, which this backend
/// does not have, and would only add notes.
fn command_line(
    settings: &SshSettings,
    target: &SshTarget,
    purpose: Purpose<'_>,
    verify: bool,
) -> Vec<CommandLineValue> {
    let mut cl = vec![
        option("BatchMode=no".into(), "forced"),
        option(
            format!(
                "ServerAliveInterval={}",
                settings.server_alive_interval().as_secs()
            ),
            "setting:ssh.server_alive_interval",
        ),
        option(
            format!("ServerAliveCountMax={}", settings.server_alive_count_max()),
            "setting:ssh.server_alive_count_max",
        ),
    ];
    if verify {
        cl.push(option("StrictHostKeyChecking=accept-new".into(), "forced"));
    }
    cl.push(match purpose {
        Purpose::Tunnel => CommandLineValue {
            value: CommandLine::NoCommand,
            label: "forced".into(),
        },
        Purpose::Discovery(_) => option("SessionType=default".into(), "forced"),
    });
    if let Some(port) = target.ssh_port {
        cl.push(CommandLineValue {
            value: CommandLine::Port(port.to_string()),
            label: "target".into(),
        });
    }
    if let Some(identity) = &target.identity_file {
        cl.push(CommandLineValue {
            value: CommandLine::Identity(identity.clone()),
            label: "target".into(),
        });
    }
    for opt in &target.extra_options {
        cl.push(option(opt.clone(), "target"));
    }
    for opt in settings.overrides() {
        cl.push(option(opt.clone(), "override"));
    }
    cl.push(option(
        format!("ConnectTimeout={}", settings.connect_timeout().as_secs()),
        "setting:ssh.connect_timeout",
    ));
    cl
}

/// The local values of a resolution: the account settings, the environment, the uid and the
/// host name where the OS shows them without a system call.
fn local_context(settings: &SshSettings) -> ResolveContext {
    let home = settings.embedded_home();
    let ssh_dir = settings.embedded_ssh_dir();
    let default_dir = home.as_ref().map(|h| h.join(".ssh"));
    ResolveContext {
        command_line: Vec::new(),
        local_user: settings.embedded_local_user(),
        home: home.map(|h| h.to_string_lossy().into_owned()),
        ssh_dir: ssh_dir
            .filter(|d| Some(d) != default_dir.as_ref())
            .map(|d| d.to_string_lossy().into_owned()),
        local_uid: local_uid(),
        local_hostname: local_hostname(),
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        command: None,
    }
}

/// The uid of this process, from the owner of `/proc/self` (Linux, Android). `None` elsewhere:
/// the port then checks only the mode of configuration files and `%i` is not known.
fn local_uid() -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").ok().map(|m| m.uid())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// The local host name (`%l`, `%L`, `%C`): `/proc/sys/kernel/hostname` on Linux and Android,
/// `COMPUTERNAME` on Windows. `None` elsewhere.
fn local_hostname() -> Option<String> {
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else {
        std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
    };
    name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

/// Resolves `target` for `purpose`, and its jump hosts the way `ssh` resolves them: each hop
/// with the same files, its own `Host`/`Match`, and only the user and port of the `ProxyJump`
/// text (`ssh -l <user> -p <port> -J <rest> -W ... <hop>`, `ssh.c:1278`-`:1324`).
pub(crate) fn resolve_chain(
    set: &ConfigSet,
    settings: &SshSettings,
    target: &SshTarget,
    purpose: Purpose<'_>,
    verify: bool,
) -> Result<Chain, SshBackendError> {
    let mut ctx = local_context(settings);
    ctx.command_line = command_line(settings, target, purpose, verify);
    ctx.command = match purpose {
        Purpose::Tunnel => None,
        Purpose::Discovery(command) => Some(command.to_string()),
    };
    let dest = set.resolve(&target.destination, &ctx);
    let mut notes = Vec::new();
    absorb(&mut notes, &dest)?;
    let mut hops: Vec<Resolution> = Vec::new();
    let mut next = jump_of(&dest);
    while let Some((destination, cl)) = next {
        let mut hop_ctx = local_context(settings);
        hop_ctx.command_line = cl;
        let hop = set.resolve(&destination, &hop_ctx);
        absorb(&mut notes, &hop)?;
        let seen = std::iter::once(&dest).chain(hops.iter()).any(|r| {
            r.host == hop.host
                && Chain::port(r) == Chain::port(&hop)
                && Chain::user(r) == Chain::user(&hop)
        });
        if seen {
            return Err(refused(
                RefusalNote {
                    kind: "config_error",
                    keyword: Some("proxyjump".into()),
                    origin: Some(dest.source_of("proxyjump").to_string()),
                    message: "jumphost loop via ".to_string() + &hop.host,
                },
                notes,
            ));
        }
        next = jump_of(&hop);
        hops.push(hop);
    }
    // The first hop is the last one found (`ProxyJump` of the destination is the nearest).
    hops.reverse();
    Ok(Chain { hops, dest, notes })
}

/// The next hop of a resolution: its destination and command line.
fn jump_of(res: &Resolution) -> Option<(String, Vec<CommandLineValue>)> {
    let Some(Jump::Hosts {
        user,
        host,
        port,
        extra,
    }) = &res.options.jump
    else {
        return None;
    };
    let mut cl = Vec::new();
    if let Some(user) = user {
        cl.push(CommandLineValue {
            value: CommandLine::User(user.clone()),
            label: "jump".into(),
        });
    }
    if let Some(port) = port {
        cl.push(CommandLineValue {
            value: CommandLine::Port(port.to_string()),
            label: "jump".into(),
        });
    }
    if let Some(extra) = extra {
        cl.push(option(format!("ProxyJump={extra}"), "jump"));
    }
    // `-W` makes the hop's session type `none`.
    cl.push(CommandLineValue {
        value: CommandLine::NoCommand,
        label: "jump".into(),
    });
    Some((host.clone(), cl))
}

/// Adds the notes of `res` to `notes` and stops at its refusal or a missing user.
fn absorb(notes: &mut Vec<ConfigNote>, res: &Resolution) -> Result<(), SshBackendError> {
    for n in &res.notes {
        let note = config_note(n);
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    // The channel timers of `ChannelTimeout` are not in this stage: shown, not silently
    // dropped (docs/design/ssh-backends.md "후속 작업과 미정").
    if !res.options.channel_timeouts.is_empty() {
        let note = backend_note(
            "channeltimeout",
            &res.source_of("channeltimeout"),
            NoteClass::NotSupported,
        );
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    if let Some(refusal) = &res.refusal {
        return Err(refused(refusal_note(refusal), notes.clone()));
    }
    if res.options.user.as_deref().is_none_or(str::is_empty) {
        return Err(SshBackendError::UserRequired {
            host: res.host.clone(),
        });
    }
    Ok(())
}

pub(crate) fn refused(refusal: RefusalNote, notes: Vec<ConfigNote>) -> SshBackendError {
    SshBackendError::ConfigRefused {
        refusal: Box::new(refusal),
        notes,
    }
}

fn origin_place(origin: &Origin) -> (Option<PathBuf>, Option<usize>) {
    match origin {
        Origin::File { path, line } => (Some(path.clone()), Some(*line)),
        _ => (None, None),
    }
}

/// The note of the port as this crate reports it.
pub(crate) fn config_note(n: &tasty_net_ssh_config::ConfigNote) -> ConfigNote {
    use tasty_net_ssh_config::NoteAction as A;
    use tasty_net_ssh_config::NoteClass as C;
    let (file, line) = origin_place(&n.origin);
    ConfigNote {
        keyword: n.keyword.clone(),
        origin: n.origin.to_string(),
        file,
        line,
        action: match n.action {
            A::Shown => NoteAction::Shown,
            A::Refused => NoteAction::Refused,
            A::Ignored => NoteAction::Ignored,
        },
        class: match n.class {
            C::UnknownKeyword => NoteClass::UnknownKeyword,
            C::IgnoredUnknownKeyword => NoteClass::IgnoredUnknownKeyword,
            C::DroppedByOpenSsh => NoteClass::DroppedByOpenSsh,
            C::NotSupported => NoteClass::NotSupported,
            C::SecurityNotImplemented => NoteClass::SecurityNotImplemented,
            C::HostKeyCheckKept => NoteClass::HostKeyCheckKept,
            C::PubkeyUnbound => NoteClass::PubkeyUnbound,
            C::SessionTypeNotApplied => NoteClass::SessionTypeNotApplied,
        },
    }
}

/// A note this backend adds while it connects (a key file, an algorithm, the agent).
pub(crate) fn backend_note(keyword: &str, origin: &Origin, class: NoteClass) -> ConfigNote {
    let (file, line) = origin_place(origin);
    ConfigNote {
        keyword: keyword.to_string(),
        origin: origin.to_string(),
        file,
        line,
        action: NoteAction::Shown,
        class,
    }
}

pub(crate) fn refusal_note(r: &Refusal) -> RefusalNote {
    RefusalNote {
        kind: match r.kind {
            RefusalKind::ConfigError => "config_error",
            RefusalKind::UnknownKeyword => "unknown_keyword",
            RefusalKind::SecurityNotImplemented => "security_not_implemented",
            RefusalKind::RefuseConnection => "refuse_connection",
            RefusalKind::Undeterminable => "undeterminable",
        },
        keyword: r.keyword.clone(),
        origin: r.origin.as_ref().map(Origin::to_string),
        message: r.message.clone(),
    }
}

/// The resolution of one host as the connection info shows it (up to the refusal, when it
/// was refused).
/// The keywords whose values the connection info and the settings query show: the ones that
/// choose where the connection goes (docs/design/ssh-backends.md "연결 정보"). Other values
/// (`SetEnv`, `SendEnv`, `RemoteCommand`, `LocalCommand`, ...) can hold secrets and are left
/// out, as the value of a `ConfigNote` is.
const TARGET_KEYWORDS: &[&str] = &[
    "hostname",
    "user",
    "port",
    "hostkeyalias",
    "proxyjump",
    "identityfile",
    "certificatefile",
];

pub(crate) fn resolved_target(res: &Resolution) -> ResolvedTarget {
    ResolvedTarget {
        hostname: res.host.clone(),
        user: res.options.user.clone().filter(|u| !u.is_empty()),
        port: Chain::port(res),
        identity_files: res
            .identity_files
            .iter()
            .filter_map(|k| k.path.as_ref().map(PathBuf::from))
            .collect(),
        proxy_jump: Vec::new(),
        sources: res
            .sources
            .iter()
            .filter(|s| TARGET_KEYWORDS.contains(&s.keyword))
            .map(|s| ValueSource {
                keyword: s.keyword.to_string(),
                value: s.value.clone(),
                source: s.origin.to_string(),
            })
            .collect(),
    }
}

/// How each top-level file was read: a file whose unknown keyword refused the resolution is
/// `refused` at that line.
pub(crate) fn file_reports(set: &ConfigSet, refusal: Option<&Refusal>) -> Vec<ConfigFileReport> {
    set.files()
        .map(|(path, exists)| {
            let state = match refusal {
                Some(Refusal {
                    kind: RefusalKind::UnknownKeyword,
                    keyword,
                    origin: Some(Origin::File { path: p, line }),
                    ..
                }) if p == path => ConfigFileState::Refused {
                    line: Some(*line),
                    keyword: keyword.clone(),
                },
                _ if exists => ConfigFileState::Loaded,
                _ => ConfigFileState::Missing,
            };
            ConfigFileReport {
                path: path.to_path_buf(),
                state,
            }
        })
        .collect()
}

/// The inspection of a target: the destination's resolution for `purpose` (without its jump
/// hosts' sessions), its notes and refusal.
pub(crate) fn inspect(
    set: &ConfigSet,
    settings: &SshSettings,
    target: &SshTarget,
    purpose: Purpose<'_>,
    verify: bool,
) -> (Resolution, Result<Chain, SshBackendError>) {
    let mut ctx = local_context(settings);
    ctx.command_line = command_line(settings, target, purpose, verify);
    ctx.command = match purpose {
        Purpose::Tunnel => None,
        Purpose::Discovery(command) => Some(command.to_string()),
    };
    let dest = set.resolve(&target.destination, &ctx);
    let chain = resolve_chain(set, settings, target, purpose, verify);
    (dest, chain)
}
