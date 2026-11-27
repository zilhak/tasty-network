// This file is derived from OpenSSH, modified: a Rust port of parts of ssh.c, sshconnect.h
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// ssh.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Ssh client program.  This program can be used to log into a remote machine.
//  * The software supports strong authentication, encryption, and forwarding
//  * of X11, TCP/IP, and authentication connections.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  *
//  * Copyright (c) 1999 Niels Provos.  All rights reserved.
//  * Copyright (c) 2000, 2001, 2002, 2003 Markus Friedl.  All rights reserved.
//  *
//  * Modified to work with SSLeay by Niels Provos <provos@citi.umich.edu>
//  * in Canada (German citizen).
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// sshconnect.h:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// struct sshkey;
//
// typedef struct Sensitive Sensitive;
// struct Sensitive {
// 	struct sshkey	**keys;
// 	int		nkeys;
// };
//
// struct ssh_conn_info {
// 	char *conn_hash_hex;
// 	char *shorthost;
// 	char *uidstr;
// 	char *keyalias;
// 	char *thishost;
// 	char *host_arg;
// 	char *portstr;
// 	char *remhost;
// 	char *remuser;
// 	char *homedir;
// 	char *locuser;
// 	char *jmphost;
// };
//
// struct addrinfo;
// struct ssh;
// struct hostkeys;
//
// /* default argument for client percent expansions, minus remote user */
//! The configuration stage of OpenSSH `ssh.c` `main` (`:1039`–`:1612`) and
//! `process_config_files` (`:564`), then this crate's treatment of the applied values
//! (docs/design/ssh-backends.md "지시어 분류").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hostfile::HostKeyQuery;
use crate::keywords::{KEYWORDS, Op, Treatment, treatment};
use crate::misc::{
    Account, ExpandError, Family, TildeError, Token, expand, numeric_host, numeric_text,
    tilde_expand,
};
use crate::options::{
    Canonicalize, Compression, Flag, ForwardEnd, Jump, LogLevel, Options, ProvidedBy, PubkeyAuth,
    SessionType, StrictHostKey, YesNoAsk, clear_or_none,
};
use crate::readconf::{
    DEFAULT_SSH_PORT, Env, Fatal, Flags, LoadedFile, OpenFile, Reader, connection_hash,
    parse_ssh_uri, valid_hostname, valid_ruser,
};
use crate::result::{
    ConfigNote, KeyFile, NoteAction, NoteClass, Origin, Refusal, RefusalKind, ValueSource,
};
use crate::settings;

/// Which configuration files to read (`ssh -F`, docs/design/ssh-backends.md
/// `embedded.config_file`, `embedded.system_config_file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFiles {
    /// `ssh -F <path>`: only that file, read as a user configuration without the permission
    /// check. It must exist.
    Only(PathBuf),
    /// `ssh -F none`: no file.
    None,
    /// The user configuration (with the permission check) and then the system one. A file
    /// that is `None` or missing is skipped.
    Default {
        /// Usually `<ssh folder>/config`.
        user: Option<PathBuf>,
        /// Usually `/etc/ssh/ssh_config`.
        system: Option<PathBuf>,
    },
}

/// The input of [`ConfigSet::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInputs {
    /// The files.
    pub files: ConfigFiles,
}

/// Why [`ConfigSet::load`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The `-F` file cannot be opened (`Can't open user config file`).
    CannotOpen {
        /// The file.
        path: PathBuf,
        /// The error.
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::CannotOpen { path, message } => write!(
                f,
                "Can't open user config file {}: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The top-level configuration files, read once their mode passed (see [`ConfigSet::load`]).
/// `Include` files are read when resolving,
/// because which files an `Include` names depends on the target (its `%` tokens) and on the
/// blocks that apply.
#[derive(Debug, Clone)]
pub struct ConfigSet {
    files: Vec<(LoadedFile, Flags)>,
    /// The `ssh -F` value, if one is given (`none` for [`ConfigFiles::None`]).
    only: Option<PathBuf>,
}

/// A command-line value of [`ResolveContext`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLine {
    /// `-o <keyword>=<value>` (or with a space).
    Option(String),
    /// `-p <port>`.
    Port(String),
    /// `-i <file>`. Like `ssh`, a file that does not exist is skipped.
    Identity(String),
    /// `-l <user>`.
    User(String),
    /// `-N`: no command (`SessionType none`).
    NoCommand,
    /// `-s`: the command is a subsystem.
    Subsystem,
}

/// A command-line value and the label of its source (for example `forced`, `target`,
/// `override`, `setting:ssh.connect_timeout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLineValue {
    /// The value.
    pub value: CommandLine,
    /// The source label.
    pub label: String,
}

/// The local side of a resolution: the command-line values in order and what `ssh` takes
/// from the local account and environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveContext {
    /// Values before the configuration files, in order (the first value wins).
    pub command_line: Vec<CommandLineValue>,
    /// `embedded.local_user`: `%u`, `Match localuser`, the default remote user.
    pub local_user: Option<String>,
    /// `embedded.home`: `~` and `%d`.
    pub home: Option<String>,
    /// `embedded.ssh_dir` when it is not `<home>/.ssh`: `~/.ssh/` paths go there.
    pub ssh_dir: Option<String>,
    /// The local uid: `%i` and the owner check of configuration files.
    pub local_uid: Option<u32>,
    /// The local host name: `%l`, `%L`, `%C`.
    pub local_hostname: Option<String>,
    /// The environment for `${NAME}`.
    pub env: BTreeMap<String, String>,
    /// The remote command (`Match command`, `RemoteCommand` conflicts).
    pub command: Option<String>,
}

/// The result of [`ConfigSet::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The options as OpenSSH resolves them (up to the refusal if there is one).
    pub options: Options,
    /// The final host name (`hostname` of `ssh -G`).
    pub host: String,
    /// Where each written value came from, in the order applied.
    pub sources: Vec<ValueSource>,
    /// Notes about keywords (no values).
    pub notes: Vec<ConfigNote>,
    /// Why the resolution is refused, if it is.
    pub refusal: Option<Refusal>,
    /// The host key check this crate does: the first applied `StrictHostKeyChecking` value
    /// that does not turn checking off, else `ask` (docs/design/ssh-backends.md
    /// "옵션 적용 순서").
    pub host_key_checking: StrictHostKey,
    /// Where [`Resolution::host_key_checking`] comes from: the line of that value, or
    /// [`Origin::Default`] for the default `ask`. [`Resolution::source_of`] gives the first
    /// written `StrictHostKeyChecking`, which can be a value this crate does not apply.
    pub host_key_checking_origin: Origin,
    /// The `IdentityFile` entries in the order `ssh` offers them, as `ssh` expands them
    /// before authenticating (`load_public_identity_files`, `ssh.c:2354`-`:2497`). Entries
    /// written `none` are left out (`ssh.c:2403`-`:2409`); [`Options::identity_files`] keeps
    /// every entry as `ssh -G` shows it. When [`Resolution::certificate_files`] is empty,
    /// `ssh` also tries `<path>-cert` after each entry and offers it next when that file holds
    /// a certificate (`ssh.c:2425`-`:2451`). Empty when the resolution is refused before the
    /// end.
    pub identity_files: Vec<KeyFile>,
    /// The `CertificateFile` entries in order, expanded the same way (`ssh.c:2454`-`:2482`).
    pub certificate_files: Vec<KeyFile>,
    /// The known_hosts files `ssh` reads, in its order: `UserKnownHostsFile` (as
    /// [`Options::user_hostfiles`]), then `GlobalKnownHostsFile` with `~` expanded
    /// (`ssh.c:1791`-`:1793`). A file under a home that is not known is left out (a missing
    /// file). Give it to [`crate::KnownHosts::load`].
    pub known_hosts_files: Vec<PathBuf>,
}

impl Resolution {
    /// The `ssh -G` text of the resolution.
    pub fn dump_client_config(&self) -> Result<String, String> {
        self.options.dump_client_config(&self.host)
    }

    /// The name `ssh` looks the host key up by (`get_hostfile_hostname_ipaddr`,
    /// `sshconnect.c:648`-`:700`): `HostKeyAlias`, or the host with the port when it is not 0
    /// or 22.
    pub fn host_key_query(&self) -> HostKeyQuery {
        HostKeyQuery::for_host(
            &self.host,
            self.options.port.unwrap_or(DEFAULT_SSH_PORT),
            self.options.host_key_alias.as_deref(),
        )
    }

    /// Where the value of a keyword (as `ssh -G` names it) came from: its first source, or
    /// the OpenSSH default.
    pub fn source_of(&self, keyword: &str) -> Origin {
        self.sources
            .iter()
            .find(|s| s.keyword == keyword)
            .map_or(Origin::Default, |s| s.origin.clone())
    }
}

/// Open a top-level file, judge it from the same handle and read it only if it passes
/// (`read_config_file_depth`, `readconf.c:2644`-`:2664`). The owner needs the local uid,
/// which `resolve` gets, so here only the mode is judged; `resolve` judges both again and
/// gives the refusal. A file whose mode is refused is not read.
fn load_one(path: &Path, check_perm: bool) -> Result<LoadedFile, std::io::Error> {
    let opened = OpenFile::open(path)?;
    let owner_mode = opened.owner_mode;
    let refused = check_perm
        && owner_mode.is_some_and(|(uid, mode)| !crate::readconf::perm_ok(uid, mode, None));
    let text = if refused {
        drop(opened);
        String::new()
    } else {
        opened.read()
    };
    Ok(LoadedFile {
        path: path.to_path_buf(),
        text: Some(text),
        owner_mode,
    })
}

fn missing(path: &Path) -> LoadedFile {
    LoadedFile {
        path: path.to_path_buf(),
        text: None,
        owner_mode: None,
    }
}

impl ConfigSet {
    /// Read the top-level files: each is opened, its mode judged from the same handle, and
    /// read to the end only if it passes.
    pub fn load(inputs: ConfigInputs) -> Result<ConfigSet, ConfigError> {
        let mut files = Vec::new();
        let mut only = None;
        match inputs.files {
            ConfigFiles::Only(path) => {
                let f = load_one(&path, false).map_err(|e| ConfigError::CannotOpen {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
                files.push((
                    f,
                    Flags {
                        user_conf: true,
                        ..Flags::default()
                    },
                ));
                only = Some(path);
            }
            // `ssh.c:1064` keeps `config = "none"`; the `ProxyCommand` that `ProxyJump`
            // makes passes it on (`:1320`-`:1321`).
            ConfigFiles::None => only = Some(PathBuf::from("none")),
            ConfigFiles::Default { user, system } => {
                if let Some(p) = user {
                    // `read_config_file` returns 0 for a file it cannot open.
                    let f = load_one(&p, true).unwrap_or_else(|_| missing(&p));
                    files.push((
                        f,
                        Flags {
                            user_conf: true,
                            check_perm: true,
                            ..Flags::default()
                        },
                    ));
                }
                if let Some(p) = system {
                    let f = load_one(&p, false).unwrap_or_else(|_| missing(&p));
                    files.push((f, Flags::default()));
                }
            }
        }
        Ok(ConfigSet { files, only })
    }

    /// The top-level files and whether each exists.
    pub fn files(&self) -> impl Iterator<Item = (&Path, bool)> {
        self.files
            .iter()
            .map(|(f, _)| (f.path.as_path(), f.text.is_some()))
    }

    /// Resolve the options for a destination (`[user@]host` or `ssh://...`).
    pub fn resolve(&self, target: &str, ctx: &ResolveContext) -> Resolution {
        let vars = |n: &str| ctx.env.get(n).cloned();
        let loaded: Vec<LoadedFile> = self.files.iter().map(|(f, _)| f.clone()).collect();
        let env = Env {
            account: Account {
                user: ctx.local_user.clone(),
                home: ctx.home.clone(),
                ssh_dir: ctx.ssh_dir.clone(),
            },
            uid: ctx.local_uid,
            local_hostname: ctx.local_hostname.clone(),
            vars: &vars,
            host_arg: String::new(),
            command: ctx.command.clone(),
            version: settings::OPENSSH_VERSION,
            max_depth: settings::INCLUDE_MAX_DEPTH,
            loaded: &loaded,
        };
        let mut r = Reader::new(env, String::new());
        let mut st = State::default();
        let host = match self.stages(&mut r, &mut st, target, ctx) {
            Ok(host) => host,
            Err(Fatal(refusal)) => {
                r.refusal = Some(refusal);
                st.host.clone()
            }
        };
        let (host_key_checking, host_key_checking_origin) = r
            .strict_values
            .iter()
            .find(|(v, _)| *v != StrictHostKey::Off)
            .cloned()
            .unwrap_or((StrictHostKey::Ask, Origin::Default));
        Resolution {
            options: r.o,
            host,
            sources: r.sources,
            notes: r.notes,
            refusal: r.refusal,
            host_key_checking,
            host_key_checking_origin,
            identity_files: std::mem::take(&mut st.identity_files),
            certificate_files: std::mem::take(&mut st.certificate_files),
            known_hosts_files: std::mem::take(&mut st.known_hosts_files),
        }
    }

    /// `process_config_files`.
    fn process_files(&self, r: &mut Reader<'_>, final_pass: bool) -> Result<(), Fatal> {
        for (f, flags) in &self.files {
            let mut active = true;
            let flags = Flags {
                final_pass,
                ..*flags
            };
            r.read_file(&f.path, flags, &mut active, Default::default())?;
        }
        Ok(())
    }

    fn stages(
        &self,
        r: &mut Reader<'_>,
        st: &mut State,
        target: &str,
        ctx: &ResolveContext,
    ) -> Result<String, Fatal> {
        let mut user_on_cmdline = false;
        for v in &ctx.command_line {
            let origin = Origin::CommandLine {
                label: v.label.clone(),
            };
            match &v.value {
                CommandLine::Option(line) => r.command_line(line, &v.label)?,
                CommandLine::Port(p) => {
                    if r.o.port.is_none() {
                        let Some(port) = crate::misc::a2port(p).filter(|p| *p > 0) else {
                            return Err(config_error(Some(&origin), format!("Bad port '{p}'")));
                        };
                        r.o.port = Some(port);
                        push_source(r, Op::Port, p, &origin);
                    }
                }
                CommandLine::User(u) => {
                    if r.o.user.is_none() {
                        r.o.user = Some(u.clone());
                        user_on_cmdline = true;
                        push_source(r, Op::User, u, &origin);
                    }
                }
                CommandLine::Identity(path) => {
                    // A path under an unknown home is a missing file, which `ssh` skips.
                    if let Some(p) = tilde_path(&r.env.account, path, &origin)?
                        && Path::new(&p).exists()
                    {
                        let before = r.o.identity_files.len();
                        r.o.add_identity_file(p.clone(), ProvidedBy::IdentityOption)
                            .map_err(|e| {
                                config_error(
                                    Some(&origin),
                                    format!("Too many identity files specified (max {})", e.max),
                                )
                            })?;
                        if r.o.identity_files.len() > before {
                            r.identity_origins.push(origin.clone());
                        }
                        push_source(r, Op::IdentityFile, &p, &origin);
                    }
                }
                CommandLine::NoCommand => {
                    if r.o.session_type.is_some_and(|t| t != SessionType::None) {
                        return Err(config_error(
                            Some(&origin),
                            "Cannot specify -N with -s/SessionType".to_string(),
                        ));
                    }
                    r.o.session_type = Some(SessionType::None);
                    push_source(r, Op::SessionType, "none", &origin);
                    // `ssh.c:1034`: `-N` also sets `RequestTTY no`, over an earlier `-o`.
                    r.o.request_tty = Some(crate::options::RequestTty::No);
                    let name = crate::keywords::keyword_name(Op::RequestTty);
                    r.sources.retain(|v| v.keyword != name);
                    push_source(r, Op::RequestTty, "no", &origin);
                }
                CommandLine::Subsystem => {
                    if r.o
                        .session_type
                        .is_some_and(|t| t != SessionType::Subsystem)
                    {
                        return Err(config_error(
                            Some(&origin),
                            "Cannot specify -s with -N/SessionType".to_string(),
                        ));
                    }
                    r.o.session_type = Some(SessionType::Subsystem);
                    push_source(r, Op::SessionType, "subsystem", &origin);
                }
            }
        }

        // The destination: an ssh:// URI or [user@]host.
        let target_origin = Origin::CommandLine {
            label: "destination".to_string(),
        };
        let mut host = match parse_ssh_uri(target) {
            Err(()) => {
                return Err(config_error(
                    Some(&target_origin),
                    format!("invalid destination \"{target}\""),
                ));
            }
            Ok(Some(u)) => {
                // `ssh.c:1084`-`:1089`: with no user so far the URI's user (maybe none) is
                // taken and counts as given on the command line, so a `User` from the files
                // is not expanded.
                if r.o.user.is_none() {
                    if let Some(user) = u.user {
                        push_source(r, Op::User, &user, &target_origin);
                        r.o.user = Some(user);
                    }
                    user_on_cmdline = true;
                }
                if r.o.port.is_none()
                    && let Some(port) = u.port
                {
                    r.o.port = Some(port);
                    push_source(r, Op::Port, &port.to_string(), &target_origin);
                }
                u.host
            }
            Ok(None) => match target.rfind('@') {
                Some(0) => {
                    return Err(config_error(
                        Some(&target_origin),
                        format!("invalid destination \"{target}\""),
                    ));
                }
                Some(at) => {
                    if r.o.user.is_none() {
                        let user = target.get(..at).unwrap_or("").to_string();
                        push_source(r, Op::User, &user, &target_origin);
                        r.o.user = Some(user);
                        user_on_cmdline = true;
                    }
                    target.get(at + 1..).unwrap_or("").to_string()
                }
                None => target.to_string(),
            },
        };
        if host.is_empty() {
            return Err(config_error(
                Some(&target_origin),
                "no host name".to_string(),
            ));
        }
        if r.o.user.as_deref().is_some_and(|u| !valid_ruser(u)) {
            return Err(config_error(
                Some(&target_origin),
                "remote username contains invalid characters".to_string(),
            ));
        }
        if !valid_hostname(&host) {
            return Err(config_error(
                Some(&target_origin),
                "hostname contains invalid characters".to_string(),
            ));
        }
        r.o.host_arg = host.clone();
        r.env.host_arg = host.clone();
        r.host = host.clone();
        st.host = host.clone();

        self.process_files(r, false)?;
        r.o.fill_for_canonicalization();

        if let Some(hostname) = r.o.hostname.clone() {
            host = r.expand_hostname(&hostname, &host).map_err(|e| {
                config_error(None, format!("HostName: percent_expand failed: {e:?}"))
            })?;
            r.o.hostname = Some(host.clone());
        }
        // `is_addr` (`ssh.c:293`-`:315`) and `resolve_addr` (`:324`-`:367`) ask `getaddrinfo` for
        // the configured address family.
        let family = address_family(&r.o);
        // The numeric forms are those of the C library of this OS; where this crate has no
        // rule for it and the libraries differ, the address `ssh` would use is not known.
        let numeric = match numeric_host(&host, family) {
            Ok(a) => a,
            Err(message) => {
                r.refuse(Refusal {
                    kind: RefusalKind::Undeterminable,
                    keyword: Some("hostname".to_string()),
                    origin: first_origin(r, "hostname"),
                    message,
                });
                None
            }
        };
        let was_addr = is_addr_fast(&host) || numeric.is_some();
        if !was_addr {
            host = host.to_ascii_lowercase();
        }
        let canonicalize = r.o.canonicalize_hostname.unwrap_or(Canonicalize::No);
        if canonicalize != Canonicalize::No || was_addr {
            // A name that is not an address stays one in lower case (`numeric_host` reads
            // hexadecimal digits in both cases), so the first reading holds.
            if let Some(normal) = numeric.map(numeric_text) {
                if !normal.eq_ignore_ascii_case(&host) {
                    host = normal;
                }
            } else if !is_addr_fast(&host) && canonicalize != Canonicalize::No {
                let direct = clear_or_none(r.o.proxy_command.as_deref()) && jump_is_none(&r.o);
                if direct || canonicalize == Canonicalize::Always {
                    r.refuse(Refusal {
                        kind: RefusalKind::SecurityNotImplemented,
                        keyword: Some("canonicalizehostname".to_string()),
                        origin: first_origin(r, "canonicalizehostname"),
                        message: "CanonicalizeHostname needs name service lookups".to_string(),
                    });
                }
            }
        }
        st.host = host.clone();
        if canonicalize != Canonicalize::No && !r.want_final {
            r.want_final = true;
        }
        if r.want_final {
            r.o.hostname = Some(host.clone());
            r.host = host.clone();
            self.process_files(r, true)?;
        }

        r.o.fill_defaults().map_err(|e| {
            config_error(
                None,
                format!("{}: bad algorithm list", crate::keywords::keyword_name(e.0)),
            )
        })?;
        // The entries `fill_default_options` added are the default key files.
        let written = r.o.identity_files.len();
        r.identity_origins.resize(written, Origin::Default);
        // `ssh -G` stops on this in `dump_client_config` and `ssh` when it connects
        // (`sshconnect2.c:245`); the resolution stops here.
        if r.o.assembled_hostkeyalgorithms().is_none() {
            return Err(Fatal(Refusal {
                kind: RefusalKind::ConfigError,
                keyword: Some("hostkeyalgorithms".to_string()),
                origin: first_origin(r, "hostkeyalgorithms"),
                message: "expand HostKeyAlgorithms: the list cannot be assembled".to_string(),
            }));
        }
        let user_was_default = r.o.user.is_none();
        if user_was_default {
            r.o.user = r.env.account.user.clone();
        }

        // Judge before ProxyJump builds its own ProxyCommand.
        judge(r);

        if let Some(Jump::Hosts {
            user: jump_user,
            host: jump_host,
            port: jump_port,
            ..
        }) = r.o.jump.clone()
        {
            let port = r.o.port.filter(|p| *p > 0).unwrap_or(DEFAULT_SSH_PORT);
            let jumpport = jump_port.filter(|p| *p > 0).unwrap_or(DEFAULT_SSH_PORT);
            let jumpuser = jump_user.clone().or_else(|| r.o.user.clone());
            if jump_host == host && port == jumpport && r.o.user == jumpuser {
                return Err(config_error(None, format!("jumphost loop via {jump_host}")));
            }
            if r.o
                .proxy_command
                .as_deref()
                .is_some_and(|c| !c.eq_ignore_ascii_case("none"))
            {
                return Err(config_error(
                    None,
                    "inconsistent options: ProxyCommand+ProxyJump".to_string(),
                ));
            }
            r.o.proxy_use_fdpass = Some(Flag::No);
            r.o.proxy_command = Some(jump_command(&r.o, self.only.as_deref()));
        }
        if r.o.port.is_none_or(|p| p == 0) {
            r.o.port = Some(DEFAULT_SSH_PORT);
        }
        if let Some(a) = &mut r.o.host_key_alias {
            *a = a.to_ascii_lowercase();
        }
        if r.o.proxy_command.as_deref() == Some("-") && r.o.proxy_use_fdpass == Some(Flag::Yes) {
            return Err(config_error(
                None,
                "ProxyCommand=- and ProxyUseFDPass are incompatible".to_string(),
            ));
        }
        let command = ctx.command.clone().filter(|c| !c.is_empty());
        if r.o.update_hostkeys == Some(YesNoAsk::Ask) {
            let persist = r.o.control_persist.is_some_and(|(on, _)| on);
            if (persist && r.o.control_path.is_some())
                || command.is_some()
                || r.o.remote_command.is_some()
                || r.o.request_tty == Some(crate::options::RequestTty::No)
                || r.o.log_level.is_some_and(|l| l < LogLevel::Info)
            {
                r.o.update_hostkeys = Some(YesNoAsk::No);
            }
        }
        if r.o.connection_attempts.is_some_and(|n| n <= 0) {
            return Err(config_error(
                None,
                "Invalid number of ConnectionAttempts".to_string(),
            ));
        }
        if command.is_some() && r.o.remote_command.is_some() {
            return Err(config_error(
                None,
                "Cannot execute command-line and remote command.".to_string(),
            ));
        }
        if r.o.fork_after_authentication == Some(Flag::Yes)
            && command.is_none()
            && r.o.remote_command.is_none()
            && r.o.session_type != Some(SessionType::None)
        {
            return Err(config_error(
                None,
                "Cannot fork into background without a command to execute.".to_string(),
            ));
        }

        // `cinfo` and the expansions of `ssh.c:1405`–`:1611`.
        let thishost = r.env.local_hostname.clone();
        let port = r.o.port.unwrap_or(DEFAULT_SSH_PORT).to_string();
        let jmphost = match &r.o.jump {
            Some(Jump::Hosts { host, .. }) => host.clone(),
            _ => String::new(),
        };
        let mut tokens: Vec<Token> = vec![
            (
                'L',
                thishost
                    .as_deref()
                    .map(|t| t.split('.').next().unwrap_or("").to_string()),
            ),
            ('i', r.env.uid.map(|u| u.to_string())),
            (
                'k',
                Some(
                    r.o.host_key_alias
                        .clone()
                        .unwrap_or_else(|| r.o.host_arg.clone()),
                ),
            ),
            ('l', thishost.clone()),
            ('n', Some(r.o.host_arg.clone())),
            ('p', Some(port.clone())),
            ('d', r.env.account.home.clone()),
            ('h', Some(host.clone())),
            ('u', r.env.account.user.clone()),
            ('j', Some(jmphost.clone())),
        ];
        let vars = r.env.vars;
        if !user_on_cmdline
            && !user_was_default
            && let Some(user) = r.o.user.clone()
        {
            let expanded =
                expand(&user, true, Some(&tokens), vars).map_err(|e| expand_refusal("user", e))?;
            let changed = expanded != user;
            r.o.user = Some(expanded);
            if changed && !r.o.user.as_deref().is_some_and(valid_ruser) {
                return Err(config_error(
                    None,
                    "remote username contains invalid characters".to_string(),
                ));
            }
        } else if user_on_cmdline && !r.o.user.as_deref().is_some_and(valid_ruser) {
            return Err(config_error(
                None,
                "remote username contains invalid characters".to_string(),
            ));
        }
        let remuser = r.o.user.clone();
        let conn_hash = match (&thishost, &remuser) {
            (Some(t), Some(u)) => Some(connection_hash([t, &host, &port, u, &jmphost])),
            _ => None,
        };
        tokens.push(('C', conn_hash));
        tokens.push(('r', remuser));

        let percent = |s: &str, what: &str| {
            expand(s, false, Some(&tokens), vars).map_err(|e| expand_refusal(what, e))
        };
        let percent_dollar = |s: &str, what: &str| {
            expand(s, true, Some(&tokens), vars).map_err(|e| expand_refusal(what, e))
        };
        let account = r.env.account.clone();
        // `None`: the file is under a home that is not known.
        let file = |s: &str, what: &str| -> Result<Option<String>, Fatal> {
            match tilde_path(&account, s, &Origin::Default)? {
                Some(t) => percent_dollar(&t, what).map(Some),
                None => Ok(None),
            }
        };
        // A file `ssh` must read: without the home it cannot be known.
        let needed = |s: &str, what: &str| -> Result<String, Fatal> {
            file(s, what)?.ok_or_else(|| {
                Fatal(Refusal {
                    kind: RefusalKind::Undeterminable,
                    keyword: Some(what.to_string()),
                    origin: None,
                    message: format!("{what}: the path needs the home folder, which is not known"),
                })
            })
        };
        if let Some(c) = r.o.remote_command.clone() {
            r.o.remote_command = Some(percent(&c, "remotecommand")?);
        }
        // A socket under an unknown home is a missing socket: no control socket, no agent.
        if let Some(c) = r.o.control_path.clone() {
            r.o.control_path = file(&c, "controlpath")?;
        }
        if let Some(c) = r.o.identity_agent.clone() {
            r.o.identity_agent =
                Some(file(&c, "identityagent")?.unwrap_or_else(|| "none".to_string()));
        }
        let revoked = std::mem::take(&mut r.o.revoked_host_keys);
        for k in revoked {
            // `ssh` stops when it cannot read a revoked keys file (`sshconnect.c:1544`-`:1563`),
            // so a file under an unknown home is not skipped.
            let v = needed(&k, "revokedhostkeys")?;
            r.o.revoked_host_keys.push(v);
        }
        if let Some(c) = r.o.forward_agent_sock_path.clone() {
            r.o.forward_agent_sock_path = file(&c, "forwardagent")?;
        }
        if let Some(c) = r.o.version_addendum.clone() {
            r.o.version_addendum = Some(percent_dollar(&c, "versionaddendum")?);
        }
        for (list, name) in [
            (&mut r.o.system_hostfiles, "GlobalKnownHostsFiles"),
            (&mut r.o.user_hostfiles, "UserKnownHostsFiles"),
        ] {
            if list.first().is_some_and(|f| f.eq_ignore_ascii_case("none")) {
                if list.len() > 1 {
                    return Err(config_error(
                        None,
                        format!("Invalid {name}: \"none\" appears with other entries"),
                    ));
                }
                list.clear();
            }
        }
        let user_files = std::mem::take(&mut r.o.user_hostfiles);
        for f in user_files {
            // A missing file: no key is known from it.
            if let Some(v) = file(&f, "userknownhostsfile")? {
                r.o.user_hostfiles.push(v);
            }
        }
        for f in &r.o.user_hostfiles {
            st.known_hosts_files.push(PathBuf::from(f));
        }
        for f in &r.o.system_hostfiles {
            if let Some(p) = tilde_path(&account, f, &Origin::Default)? {
                st.known_hosts_files.push(PathBuf::from(p));
            }
        }
        // `load_public_identity_files`: `~`, then the `%` tokens and `${NAME}`.
        let key_file = |(written, by): &(String, ProvidedBy),
                        origin: Option<&Origin>,
                        what: &str|
         -> Result<KeyFile, Fatal> {
            let origin = origin.cloned().unwrap_or(Origin::Default);
            let with_origin = |Fatal(mut f): Fatal| {
                f.origin.get_or_insert_with(|| origin.clone());
                Fatal(f)
            };
            let path = match tilde_path(&account, written, &origin)? {
                Some(t) => Some(percent_dollar(&t, what).map_err(with_origin)?),
                None => None,
            };
            Ok(KeyFile {
                written: written.clone(),
                path,
                provided_by: *by,
                origin,
            })
        };
        for (i, f) in r.o.identity_files.iter().enumerate() {
            if !f.0.eq_ignore_ascii_case("none") {
                let k = key_file(f, r.identity_origins.get(i), "identityfile")?;
                st.identity_files.push(k);
            }
        }
        for (i, f) in r.o.certificate_files.iter().enumerate() {
            let k = key_file(f, r.certificate_origins.get(i), "certificatefile")?;
            st.certificate_files.push(k);
        }
        let setenv = std::mem::take(&mut r.o.setenv);
        for e in setenv {
            let (name, value) = e.split_once('=').unwrap_or((&e, ""));
            let v = percent_dollar(value, "setenv")?;
            r.o.setenv.push(format!("{name}={v}"));
        }
        for list in [&mut r.o.local_forwards, &mut r.o.remote_forwards] {
            for f in list.iter_mut() {
                for end in [&mut f.listen, &mut f.connect] {
                    if let ForwardEnd::Path(p) = end {
                        *p = percent(p, "forward path")?;
                    }
                }
            }
        }
        Ok(host)
    }
}

/// What the stages keep for a refusal.
#[derive(Default)]
struct State {
    host: String,
    identity_files: Vec<KeyFile>,
    certificate_files: Vec<KeyFile>,
    known_hosts_files: Vec<PathBuf>,
}

fn config_error(origin: Option<&Origin>, message: String) -> Fatal {
    Fatal(Refusal {
        kind: RefusalKind::ConfigError,
        keyword: None,
        origin: origin.cloned(),
        message,
    })
}

fn expand_refusal(what: &str, e: ExpandError) -> Fatal {
    let (kind, message) = match e {
        ExpandError::Invalid(m) => (RefusalKind::ConfigError, format!("{what}: {m}")),
        ExpandError::MissingVariable(n) => (
            RefusalKind::ConfigError,
            format!("{what}: invalid environment variable expansion (${{{n}}} has no value)"),
        ),
        ExpandError::Unavailable(k) => (
            RefusalKind::Undeterminable,
            format!("{what}: the value of %{k} is not known"),
        ),
    };
    Fatal(Refusal {
        kind,
        keyword: Some(what.to_string()),
        origin: None,
        message,
    })
}

/// `tilde_expand_filename`. `None` if the path needs the home and the home is not known: the
/// file is missing (docs/design/ssh-backends.md "내장 백엔드가 읽는 파일"), never a `~`
/// folder of the working directory. Another user's home cannot be looked up.
fn tilde_path(account: &Account, s: &str, origin: &Origin) -> Result<Option<String>, Fatal> {
    match tilde_expand(s, account) {
        Ok(p) => Ok(Some(p)),
        Err(TildeError::NoHome) => Ok(None),
        Err(TildeError::NoSuchUser(u)) => Err(Fatal(Refusal {
            kind: RefusalKind::Undeterminable,
            keyword: None,
            origin: Some(origin.clone()),
            message: format!("the home of user {u} is not known"),
        })),
    }
}

fn push_source(r: &mut Reader<'_>, op: Op, value: &str, origin: &Origin) {
    r.sources.push(ValueSource {
        keyword: crate::keywords::keyword_name(op),
        value: value.to_string(),
        origin: origin.clone(),
    });
}

fn jump_is_none(o: &Options) -> bool {
    match &o.jump {
        None | Some(Jump::None) => true,
        Some(Jump::Hosts { host, .. }) => clear_or_none(Some(host)),
    }
}

/// `is_addr_fast`.
fn is_addr_fast(name: &str) -> bool {
    name.contains('%')
        || name.contains(':')
        || name.bytes().all(|c| c.is_ascii_digit() || c == b'.')
}

/// The `ai_family` hint of `is_addr` and `resolve_addr`.
fn address_family(o: &Options) -> Family {
    match o.address_family {
        Some(crate::options::AddressFamily::Inet) => Family::Inet,
        Some(crate::options::AddressFamily::Inet6) => Family::Inet6,
        _ => Family::Any,
    }
}

/// The `ProxyCommand` that `ProxyJump` makes (`ssh.c:1306`–`:1324`).
fn jump_command(o: &Options, config: Option<&Path>) -> String {
    let Some(Jump::Hosts {
        user,
        host,
        port,
        extra,
    }) = &o.jump
    else {
        return String::new();
    };
    let mut s = String::from("ssh");
    if let Some(u) = user {
        s.push_str(&format!(" -l {u}"));
    }
    if let Some(p) = port.filter(|p| *p > 0) {
        s.push_str(&format!(" -p {p}"));
    }
    if let Some(e) = extra {
        s.push_str(&format!(" -J {e}"));
    }
    if let Some(c) = config {
        s.push_str(&format!(" -F {}", c.display()));
    }
    s.push_str(&format!(" -W '[%h]:%p' {host}"));
    s
}

fn first_origin(r: &Reader<'_>, keyword: &str) -> Option<Origin> {
    r.sources
        .iter()
        .find(|s| s.keyword == keyword)
        .map(|s| s.origin.clone())
}

/// This crate's treatment of the applied values (docs/design/ssh-backends.md "지시어 분류",
/// "아는 지시어의 판정 때"): after the first values won, only for written values.
fn judge(r: &mut Reader<'_>) {
    let written: Vec<(&'static str, Origin)> = {
        let mut seen: Vec<(&'static str, Origin)> = Vec::new();
        for s in &r.sources {
            if !seen.iter().any(|(k, _)| *k == s.keyword) {
                seen.push((s.keyword, s.origin.clone()));
            }
        }
        seen
    };
    let op_of = |k: &str| KEYWORDS.iter().find(|(n, _)| *n == k).map(|(_, op)| *op);
    for (keyword, origin) in written {
        let Some(op) = op_of(keyword) else { continue };
        let o = &r.o;
        let refuse = match op {
            Op::KnownHostsCommand => o.known_hosts_command.is_some(),
            Op::BindInterface => o.bind_interface.is_some(),
            // `ProxyJump` sets `ProxyCommand none` (`parse_jump`), so a jump is not refused here.
            Op::ProxyCommand => !clear_or_none(o.proxy_command.as_deref()),
            Op::VerifyHostKeyDns => o.verify_host_key_dns.is_some_and(|v| v != YesNoAsk::No),
            Op::CanonicalizeHostname => o
                .canonicalize_hostname
                .is_some_and(|v| v != Canonicalize::No),
            Op::PubkeyAuthentication => o.pubkey_authentication == Some(PubkeyAuth::HostBound),
            _ => false,
        };
        if refuse {
            r.notes.push(ConfigNote {
                keyword: keyword.to_string(),
                origin: origin.clone(),
                action: NoteAction::Refused,
                class: NoteClass::SecurityNotImplemented,
            });
            r.refuse(Refusal {
                kind: RefusalKind::SecurityNotImplemented,
                keyword: Some(keyword.to_string()),
                origin: Some(origin),
                message: format!("{keyword} is not implemented by the embedded backend"),
            });
            continue;
        }
        let shown = match op {
            Op::StrictHostKeyChecking => o.strict_host_key_checking == Some(StrictHostKey::Off),
            Op::NoHostAuthenticationForLocalhost => {
                o.no_host_authentication_for_localhost == Some(Flag::Yes)
            }
            Op::PubkeyAuthentication => o.pubkey_authentication == Some(PubkeyAuth::All),
            Op::GssAuthentication => o.gss_authentication == Some(Flag::Yes),
            Op::GssDelegateCreds => o.gss_deleg_creds == Some(Flag::Yes),
            Op::HostbasedAuthentication => o.hostbased_authentication == Some(Flag::Yes),
            Op::Pkcs11Provider => o.pkcs11_provider.is_some(),
            Op::SecurityKeyProvider => o.sk_provider.is_some(),
            Op::Compression => o.compression == Some(Compression::Delayed),
            _ => treatment(op) == Treatment::ReadNotSupported,
        };
        if shown {
            let class = match op {
                Op::StrictHostKeyChecking | Op::NoHostAuthenticationForLocalhost => {
                    NoteClass::HostKeyCheckKept
                }
                Op::PubkeyAuthentication => NoteClass::PubkeyUnbound,
                _ => NoteClass::NotSupported,
            };
            r.notes.push(ConfigNote {
                keyword: keyword.to_string(),
                origin,
                action: NoteAction::Shown,
                class,
            });
        }
    }
    let effective = r.o.session_type;
    let lines: Vec<Origin> = r
        .session_type_values
        .iter()
        .filter(|(v, _)| Some(*v) != effective)
        .map(|(_, o)| o.clone())
        .collect();
    for origin in lines {
        r.notes.push(ConfigNote {
            keyword: "sessiontype".to_string(),
            origin,
            action: NoteAction::Shown,
            class: NoteClass::SessionTypeNotApplied,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::misc::{Libc, Reading, numeric_host_of, settled};

    #[test]
    fn addresses() {
        assert!(is_addr_fast("192.0.2.1"));
        assert!(is_addr_fast("::1"));
        assert!(!is_addr_fast("host1"));
    }

    /// The numeric forms of each C library (docs/design/ssh-backends.md "숫자 주소"): the
    /// name, the family asked for, then the text `ssh` keeps for glibc, bionic and Darwin and
    /// for an OS without a rule (Windows and the rest). `-` is not an address (the name is kept
    /// as written), `refused` is a refusal.
    const NUMERIC_FORMS: &[(&str, Family, [&str; 4])] = &[
        // IPv4: the parts and their bases.
        ("192.0.2.1", Family::Any, ["192.0.2.1"; 4]),
        (
            "127.1",
            Family::Any,
            ["127.0.0.1", "-", "127.0.0.1", "refused"],
        ),
        (
            "10.1.1",
            Family::Any,
            ["10.1.0.1", "-", "10.1.0.1", "refused"],
        ),
        (
            "1.2.65535",
            Family::Any,
            ["1.2.255.255", "-", "1.2.255.255", "refused"],
        ),
        (
            "4294967295",
            Family::Any,
            ["255.255.255.255", "-", "255.255.255.255", "refused"],
        ),
        (
            "010.0.0.1",
            Family::Any,
            ["8.0.0.1", "10.0.0.1", "10.0.0.1", "refused"],
        ),
        (
            "08.0.0.1",
            Family::Any,
            ["-", "8.0.0.1", "8.0.0.1", "refused"],
        ),
        (
            "0300.0.0.1",
            Family::Any,
            ["192.0.0.1", "-", "192.0.0.1", "refused"],
        ),
        ("00.0.0.01", Family::Any, ["0.0.0.1"; 4]),
        (
            "0x7F000001",
            Family::Any,
            ["127.0.0.1", "-", "127.0.0.1", "refused"],
        ),
        (
            "0x7f.1",
            Family::Any,
            ["127.0.0.1", "-", "127.0.0.1", "refused"],
        ),
        // Apple only: a `0x` with no digit before a `.`, a one-part value past 32 bits.
        ("0x.1.2.3", Family::Any, ["-", "-", "0.1.2.3", "refused"]),
        ("4294967296", Family::Any, ["-", "-", "0.0.0.0", "refused"]),
        ("1.2.3.256", Family::Any, ["-"; 4]),
        ("1.2.3.4.5", Family::Any, ["-"; 4]),
        ("0x", Family::Any, ["-"; 4]),
        ("1..2", Family::Any, ["-"; 4]),
        ("1.2.3.", Family::Any, ["-"; 4]),
        ("host", Family::Any, ["-"; 4]),
        // The family asked for.
        ("192.0.2.1", Family::Inet, ["192.0.2.1"; 4]),
        ("127.1", Family::Inet6, ["-"; 4]),
        ("0x7F000001", Family::Inet6, ["-"; 4]),
        ("192.0.2.1", Family::Inet6, ["-"; 4]),
        ("2001:db8::1", Family::Inet, ["-"; 4]),
        // IPv6 and its text.
        ("2001:DB8::1", Family::Any, ["2001:db8::1"; 4]),
        ("2001:DB8::1", Family::Inet6, ["2001:db8::1"; 4]),
        ("::1", Family::Any, ["::1"; 4]),
        ("::", Family::Any, ["::"; 4]),
        ("1:2:3:4:5:6:7::", Family::Any, ["1:2:3:4:5:6:7:0"; 4]),
        ("1::2:3:4:5:6:7:8", Family::Any, ["-"; 4]),
        ("1:2:3:4:5:6:7:8:9", Family::Any, ["-"; 4]),
        (":1::", Family::Any, ["-"; 4]),
        ("1::2::3", Family::Any, ["-"; 4]),
        ("1:2:", Family::Any, ["-"; 4]),
        ("::FFFF:C000:201", Family::Any, ["::ffff:192.0.2.1"; 4]),
        ("::192.0.2.1", Family::Any, ["::192.0.2.1"; 4]),
        ("::192.0.2.1", Family::Inet, ["-"; 4]),
        ("::ffff:0:192.0.2.1", Family::Any, ["::ffff:0:c000:201"; 4]),
        (
            "1:2:3:4:5:6:1.2.3.4",
            Family::Any,
            ["1:2:3:4:5:6:102:304"; 4],
        ),
        ("1:2:3:4:5:6:7:1.2.3.4", Family::Any, ["-"; 4]),
        ("1.2.3.4::", Family::Any, ["-"; 4]),
        ("fe80::1%lo", Family::Any, ["-"; 4]),
        // Darwin takes a nonzero second word of a link-local (or node- or link-local
        // multicast) address as the interface: the text depends on the interfaces.
        ("fe80::1", Family::Any, ["fe80::1"; 4]),
        (
            "fe80:1::1",
            Family::Any,
            ["fe80:1::1", "fe80:1::1", "refused", "refused"],
        ),
        (
            "FE80:1::1",
            Family::Inet6,
            ["fe80:1::1", "fe80:1::1", "refused", "refused"],
        ),
        (
            "febf:1::1",
            Family::Any,
            ["febf:1::1", "febf:1::1", "refused", "refused"],
        ),
        ("fec0:1::1", Family::Any, ["fec0:1::1"; 4]),
        (
            "ff02:1::1",
            Family::Any,
            ["ff02:1::1", "ff02:1::1", "refused", "refused"],
        ),
        (
            "ff11:1::1",
            Family::Any,
            ["ff11:1::1", "ff11:1::1", "refused", "refused"],
        ),
        ("ff05:1::1", Family::Any, ["ff05:1::1"; 4]),
        // A unicast-prefix-based multicast address (flags 0x30) is not link-local for
        // Darwin; any other flags are, and node-local has no flags condition.
        ("ff32:40:fe80::1234", Family::Any, ["ff32:40:fe80::1234"; 4]),
        (
            "ff72:1::1",
            Family::Any,
            ["ff72:1::1", "ff72:1::1", "refused", "refused"],
        ),
        (
            "ff31:1::1",
            Family::Any,
            ["ff31:1::1", "ff31:1::1", "refused", "refused"],
        ),
        ("fe80:1::1", Family::Inet, ["-"; 4]),
        // An IPv4-mapped address asked for IPv4: only glibc takes the IPv4 address.
        (
            "::ffff:192.0.2.1",
            Family::Inet,
            ["192.0.2.1", "-", "-", "refused"],
        ),
        (
            "::FFFF:C000:201",
            Family::Inet,
            ["192.0.2.1", "-", "-", "refused"],
        ),
        // The dotted tail and the group digits of `inet_pton6`.
        (
            "::ffff:010.0.0.1",
            Family::Any,
            ["-", "::ffff:10.0.0.1", "::ffff:10.0.0.1", "refused"],
        ),
        ("00001::", Family::Any, ["-", "-", "1::", "refused"]),
        // The first part of the tail is first read as group digits (at most four in glibc and
        // bionic).
        (
            "::ffff:00001.0.0.1",
            Family::Any,
            ["-", "-", "::ffff:1.0.0.1", "refused"],
        ),
        (
            "::ffff:0001.0.0.1",
            Family::Any,
            ["-", "::ffff:1.0.0.1", "::ffff:1.0.0.1", "refused"],
        ),
        (
            "::ffff:0000000000255.0.0.1",
            Family::Any,
            ["-", "-", "::ffff:255.0.0.1", "refused"],
        ),
        ("0001::", Family::Any, ["1::"; 4]),
    ];

    fn text(a: Option<std::net::IpAddr>) -> String {
        a.map_or_else(|| "-".to_string(), numeric_text)
    }

    fn reading_text(r: Reading) -> String {
        match r {
            Reading::Address(a) => numeric_text(a),
            Reading::Name => "-".to_string(),
            Reading::Scoped(_) => "refused".to_string(),
        }
    }

    #[test]
    fn numeric_forms_of_each_c_library() {
        for (name, family, want) in NUMERIC_FORMS {
            for (libc, want) in Libc::ALL.iter().zip(want) {
                let got = reading_text(numeric_host_of(*libc, name, *family));
                assert_eq!(&got, want, "{} {name} {family:?}", libc.name());
            }
            // Without a rule: the reading every library gives, else a refusal.
            let all: Vec<(Libc, Reading)> = Libc::ALL
                .iter()
                .map(|l| (*l, numeric_host_of(*l, name, *family)))
                .collect();
            let other = settled(&all).map_or_else(|| "refused".to_string(), text);
            assert_eq!(other, want[3], "no rule: {name} {family:?}");
        }
    }

    #[test]
    fn numeric_forms_of_this_os() {
        let column = if cfg!(all(target_os = "linux", target_env = "gnu")) {
            0
        } else if cfg!(target_os = "android") {
            1
        } else if cfg!(target_vendor = "apple") {
            2
        } else {
            3
        };
        for (name, family, want) in NUMERIC_FORMS {
            let got = match numeric_host(name, *family) {
                Ok(a) => text(a),
                Err(m) => {
                    assert!(m.contains(name), "{m}");
                    "refused".to_string()
                }
            };
            assert_eq!(got, want[column], "{name} {family:?}");
        }
    }

    #[test]
    fn jump_proxy_command() {
        let o = Options {
            jump: Some(Jump::Hosts {
                user: Some("u".to_string()),
                host: "hop".to_string(),
                port: Some(2222),
                extra: Some("first".to_string()),
            }),
            ..Options::default()
        };
        assert_eq!(
            jump_command(&o, Some(Path::new("/c"))),
            "ssh -l u -p 2222 -J first -F /c -W '[%h]:%p' hop"
        );
    }

    #[test]
    fn an_include_of_path_max_bytes_is_skipped_before_the_home() {
        // `glob.c:200` judges the written length before the home of `~user` is looked up.
        let dir = std::env::temp_dir().join(format!(
            "tasty-net-ssh-config-long-include-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("a folder");
        let top = dir.join("config");
        let ctx = ResolveContext {
            local_user: Some("tester".to_string()),
            home: Some("/home/tester".to_string()),
            ..ResolveContext::default()
        };
        let head = "~nosuchaccount/";
        let run = |len: usize| {
            let arg = format!("{head}{}", "a".repeat(len - head.len()));
            std::fs::write(&top, format!("Include {arg}\nPort 2222\n")).expect("write");
            ConfigSet::load(ConfigInputs {
                files: ConfigFiles::Only(top.clone()),
            })
            .expect("load")
            .resolve("web", &ctx)
        };
        // `PATH_MAX` bytes: no match, the line goes on.
        let long = run(crate::glob::PATH_MAX);
        assert!(long.refusal.is_none(), "{:?}", long.refusal);
        assert_eq!(long.options.port, Some(2222));
        // One byte shorter: the home of the unknown user is needed.
        let short = run(crate::glob::PATH_MAX - 1);
        let f = short.refusal.expect("a refusal");
        assert_eq!(f.kind, RefusalKind::Undeterminable);
        assert_eq!(f.keyword.as_deref(), Some("include"));
        std::fs::remove_dir_all(&dir).expect("remove the folder");
    }

    #[test]
    fn an_include_cut_at_a_nul_is_judged_by_its_head() {
        // Only the part before the NUL is the line, so a short `~user` Include with a long
        // tail after a NUL needs the home of the user like the same line without the tail.
        let dir = std::env::temp_dir().join(format!(
            "tasty-net-ssh-config-nul-include-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("a folder");
        let top = dir.join("config");
        let ctx = ResolveContext {
            local_user: Some("tester".to_string()),
            home: Some("/home/tester".to_string()),
            ..ResolveContext::default()
        };
        let tail = "a".repeat(crate::glob::PATH_MAX);
        std::fs::write(
            &top,
            format!("Include ~nosuchaccount/extra.conf\0{tail}\nPort 2222\n"),
        )
        .expect("write");
        let r = ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Only(top.clone()),
        })
        .expect("load")
        .resolve("web", &ctx);
        let f = r.refusal.expect("a refusal");
        assert_eq!(f.kind, RefusalKind::Undeterminable);
        assert_eq!(f.keyword.as_deref(), Some("include"));
        assert_eq!(f.origin, Some(Origin::File { path: top, line: 1 }));
        std::fs::remove_dir_all(&dir).expect("remove the folder");
    }
}
