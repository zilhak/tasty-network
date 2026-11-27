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

use crate::keywords::{KEYWORDS, Op, Treatment, treatment};
use crate::misc::{Account, ExpandError, TildeError, Token, expand, tilde_expand};
use crate::options::{
    Canonicalize, Compression, Flag, ForwardEnd, Jump, LogLevel, Options, ProvidedBy, PubkeyAuth,
    SessionType, StrictHostKey, YesNoAsk, clear_or_none,
};
use crate::readconf::{
    DEFAULT_SSH_PORT, Env, Fatal, Flags, LoadedFile, Reader, connection_hash, owner_mode,
    parse_ssh_uri, valid_hostname, valid_ruser,
};
use crate::result::{ConfigNote, NoteAction, NoteClass, Origin, Refusal, RefusalKind, ValueSource};
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

/// The top-level configuration files, read. `Include` files are read when resolving,
/// because which files an `Include` names depends on the target (its `%` tokens) and on the
/// blocks that apply.
#[derive(Debug, Clone)]
pub struct ConfigSet {
    files: Vec<(LoadedFile, Flags)>,
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
}

impl Resolution {
    /// The `ssh -G` text of the resolution.
    pub fn dump_client_config(&self) -> Result<String, String> {
        self.options.dump_client_config(&self.host)
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

fn load_one(path: &Path) -> Result<LoadedFile, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let meta = std::fs::metadata(path)?;
            Ok(LoadedFile {
                path: path.to_path_buf(),
                text: Some(String::from_utf8_lossy(&bytes).into_owned()),
                owner_mode: owner_mode(&meta),
            })
        }
        Err(e) => Err(e),
    }
}

fn missing(path: &Path) -> LoadedFile {
    LoadedFile {
        path: path.to_path_buf(),
        text: None,
        owner_mode: None,
    }
}

impl ConfigSet {
    /// Read the top-level files.
    pub fn load(inputs: ConfigInputs) -> Result<ConfigSet, ConfigError> {
        let mut files = Vec::new();
        let mut only = None;
        match inputs.files {
            ConfigFiles::Only(path) => {
                let f = load_one(&path).map_err(|e| ConfigError::CannotOpen {
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
            ConfigFiles::None => {}
            ConfigFiles::Default { user, system } => {
                if let Some(p) = user {
                    // `read_config_file` returns 0 for a file it cannot open.
                    let f = load_one(&p).unwrap_or_else(|_| missing(&p));
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
                    let f = load_one(&p).unwrap_or_else(|_| missing(&p));
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
                    let p = tilde_path(&r.env.account, path, &origin)?;
                    if Path::new(&p).exists() {
                        r.o.add_identity_file(p.clone(), ProvidedBy::IdentityOption)
                            .map_err(|e| {
                                config_error(
                                    Some(&origin),
                                    format!("Too many identity files specified (max {})", e.max),
                                )
                            })?;
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
        let was_addr = is_addr_fast(&host);
        if !was_addr {
            host = host.to_ascii_lowercase();
        }
        let canonicalize = r.o.canonicalize_hostname.unwrap_or(Canonicalize::No);
        if canonicalize != Canonicalize::No || was_addr {
            if let Some(normal) = normalize_address(&host) {
                if !normal.eq_ignore_ascii_case(&host) {
                    host = normal;
                }
            } else if !was_addr && canonicalize != Canonicalize::No {
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
        let path = |s: &str, what: &str| -> Result<String, Fatal> {
            let t = tilde_path(&account, s, &Origin::Default)?;
            percent_dollar(&t, what)
        };
        if let Some(c) = r.o.remote_command.clone() {
            r.o.remote_command = Some(percent(&c, "remotecommand")?);
        }
        if let Some(c) = r.o.control_path.clone() {
            r.o.control_path = Some(path(&c, "controlpath")?);
        }
        if let Some(c) = r.o.identity_agent.clone() {
            r.o.identity_agent = Some(path(&c, "identityagent")?);
        }
        let revoked = std::mem::take(&mut r.o.revoked_host_keys);
        for k in revoked {
            let v = path(&k, "revokedhostkeys")?;
            r.o.revoked_host_keys.push(v);
        }
        if let Some(c) = r.o.forward_agent_sock_path.clone() {
            r.o.forward_agent_sock_path = Some(path(&c, "forwardagent")?);
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
            let v = path(&f, "userknownhostsfile")?;
            r.o.user_hostfiles.push(v);
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

/// `tilde_expand_filename`: a home that is not known leaves the path as written; another
/// user's home cannot be looked up.
fn tilde_path(account: &Account, s: &str, origin: &Origin) -> Result<String, Fatal> {
    match tilde_expand(s, account) {
        Ok(p) => Ok(p),
        Err(TildeError::NoHome) => Ok(s.to_string()),
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

/// The numeric form `resolve_addr` gives an address (`getnameinfo(NI_NUMERICHOST)`), or
/// `None` if the name is not an address.
fn normalize_address(name: &str) -> Option<String> {
    name.parse::<std::net::IpAddr>().ok().map(|a| a.to_string())
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

    #[test]
    fn addresses() {
        assert!(is_addr_fast("192.0.2.1"));
        assert!(is_addr_fast("::1"));
        assert!(!is_addr_fast("host1"));
        assert_eq!(
            normalize_address("2001:DB8::1").as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(normalize_address("host"), None);
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
}
