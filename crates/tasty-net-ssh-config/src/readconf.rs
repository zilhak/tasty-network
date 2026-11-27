// This file is derived from OpenSSH, modified: a Rust port of parts of readconf.c, readconf.h
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// readconf.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Functions for reading the configuration files.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//
// readconf.h:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Functions for reading the configuration file.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//! Reading configuration files: `process_config_line_depth` (`readconf.c:1143`),
//! `match_cfg_line` (`:700`), `expand_match_exec_or_include_path` (`:654`),
//! `read_config_file_depth` (`:2631`), `parse_forward` (`:3299`), `parse_jump` (`:3474`),
//! `parse_ssh_uri`, `ssh_valid_hostname` and `ssh_valid_ruser`.
//!
//! Errors keep the OpenSSH split: a "bad option" is counted and the file stops the
//! resolution after its last line (`terminating, N bad configuration options`); a fatal
//! error stops it at once. Both refuse the resolution, as `ssh` exits with 255 for both.

use std::path::{Path, PathBuf};

use sha1::Digest as _;

use crate::algorithms::AlgList;
use crate::keywords::{Lookup, Op, keyword_name, parse_token};
use crate::matching::{ListMatch, match_hostname, match_pattern, match_pattern_list};
use crate::misc::{
    self, Account, ExpandError, Token, Uri, WHITESPACE, a2port, a2tun, argv_split, atoi_err,
    convtime, expand, hpdelim, parse_ipqos, parse_pattern_interval, parse_user_host_port,
    setenv_in_list, strdelim, tilde_expand, valid_domain, valid_env_name,
};
use crate::options::{
    AddKeys, AddressFamily, Canonicalize, Compression, ControlMaster, Digest, EscapeChar, Flag,
    Forward, ForwardEnd, Jump, KEYSTROKE_DEFAULT_INTERVAL_MS, KeepAlives, LogFacility, LogLevel,
    Multistate, Options, PubkeyAuth, RequestTty, SessionType, StrictHostKey, Tunnel,
    WarnWeakCrypto, YesNoAsk, clear_or_none,
};
use crate::result::{ConfigNote, NoteAction, NoteClass, Origin, Refusal, RefusalKind, ValueSource};
use crate::scaled::scan_scaled;

/// `SSH_DEFAULT_PORT` (`ssh.h`); `default_ssh_port` gives it when the services database has
/// no `ssh` entry, and the usual entry has the same port.
pub(crate) const DEFAULT_SSH_PORT: u16 = 22;
/// `SSH_MAX_IDENTITY_FILES`, `SSH_MAX_CERTIFICATE_FILES` (`ssh.h`).
const MAX_IDENTITY_FILES: usize = 100;
/// `SSH_MAX_HOSTS_FILES` (`readconf.h`).
const MAX_HOSTS_FILES: usize = 32;
/// The smallest nonzero `RekeyLimit` (`readconf.c:1386`).
const REKEY_LIMIT_MIN: i64 = 16;
/// The largest `ObscureKeystrokeTiming interval:` (`readconf.c:2509`).
const KEYSTROKE_INTERVAL_MAX: i32 = 1000;
/// The largest `StreamLocalBindMask` (`0777`).
const BIND_MASK_MAX: u32 = 0o777;
/// Octal digits.
const OCTAL_RADIX: u32 = 8;
/// `fwdarg[256]`: the forwarding text is cut to one byte less.
const FWDARG_MAX: usize = 255;
/// `NI_MAXHOST`: a forwarding host must be shorter.
const NI_MAXHOST: usize = 1025;
/// `PATH_MAX_SUN` (the size of `sun_path`): a forwarding socket path must be shorter.
/// 104 on the BSD family (macOS, iOS, FreeBSD), 108 elsewhere.
const PATH_MAX_SUN: usize = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)) {
    104
} else {
    108
};
/// `INET6_ADDRSTRLEN + 3`: the longest `Match localnetwork` entry.
const CIDR_ENTRY_MAX: usize = 49;
/// Mask lengths of IPv4 and IPv6.
const IPV4_BITS: u32 = 32;
const IPV6_BITS: u32 = 128;
/// `ESCAPECHAR ^X`: the second byte is in `64..128` and the value is its low five bits.
const CTRL_FIRST: u8 = 64;
const CTRL_END: u8 = 128;
const CTRL_MASK: u8 = 31;
/// The mode bits `CHECKPERM` refuses (group and other write).
#[cfg(unix)]
const GROUP_OTHER_WRITE: u32 = 0o022;
/// `/etc/ssh`, where a relative `Include` of the system configuration points.
const SSHDIR: &str = "/etc/ssh";

/// `SSHCONF_*` flags.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Flags {
    pub(crate) check_perm: bool,
    pub(crate) user_conf: bool,
    pub(crate) final_pass: bool,
    pub(crate) never_match: bool,
}

/// A fatal error: the resolution stops.
#[derive(Debug, Clone)]
pub(crate) struct Fatal(pub(crate) Refusal);

fn fatal(kind: RefusalKind, keyword: Option<&str>, origin: Option<&Origin>, msg: String) -> Fatal {
    Fatal(Refusal {
        kind,
        keyword: keyword.map(str::to_string),
        origin: origin.cloned(),
        message: msg,
    })
}

/// The error of one line.
enum LineErr {
    /// Counted as a bad option of the file.
    Bad(String),
    Fatal(Fatal),
}

impl From<Fatal> for LineErr {
    fn from(f: Fatal) -> Self {
        LineErr::Fatal(f)
    }
}

type LineResult = Result<(), LineErr>;

fn bad<T>(msg: impl Into<String>) -> Result<T, LineErr> {
    Err(LineErr::Bad(msg.into()))
}

/// A top-level file the caller read before resolving.
#[derive(Debug, Clone)]
pub(crate) struct LoadedFile {
    pub(crate) path: PathBuf,
    /// `None`: it does not exist.
    pub(crate) text: Option<String>,
    /// Owner and mode, for `CHECKPERM` (Unix).
    pub(crate) owner_mode: Option<(u32, u32)>,
}

/// What a resolution knows about the local side.
pub(crate) struct Env<'a> {
    pub(crate) account: Account,
    pub(crate) uid: Option<u32>,
    pub(crate) local_hostname: Option<String>,
    pub(crate) vars: &'a dyn Fn(&str) -> Option<String>,
    /// `original_host`: the host as given.
    pub(crate) host_arg: String,
    /// The remote command (`Match command`, `Match sessiontype`).
    pub(crate) command: Option<String>,
    pub(crate) version: &'static str,
    pub(crate) max_depth: usize,
    pub(crate) loaded: &'a [LoadedFile],
}

/// Whether a file could be opened.
enum Opened {
    Yes,
    /// `ENOENT`.
    Missing,
    Error(String),
}

/// The arguments of a line after `argv_split` (`argv_next`, `argv_consume`).
struct Args {
    v: Vec<String>,
    i: usize,
}

impl Args {
    fn next(&mut self) -> Option<String> {
        let a = self.v.get(self.i).cloned();
        if a.is_some() {
            self.i += 1;
        }
        a
    }
    fn remaining(&self) -> usize {
        self.v.len().saturating_sub(self.i)
    }
    fn consume(&mut self) {
        self.i = self.v.len();
    }
    fn peek(&self) -> Option<&str> {
        self.v.get(self.i).map(String::as_str)
    }
}

/// The match result of one `Match` line.
enum MatchOutcome {
    False,
    True,
    /// It depends on a criterion this crate cannot evaluate.
    Undeterminable(&'static str),
}

/// The state of one resolution.
pub(crate) struct Reader<'a> {
    pub(crate) o: Options,
    pub(crate) env: Env<'a>,
    /// The host `Host` lines match (`host` of `process_config_files`).
    pub(crate) host: String,
    pub(crate) want_final: bool,
    pub(crate) sources: Vec<ValueSource>,
    pub(crate) notes: Vec<ConfigNote>,
    /// The first refusal found while reading that does not stop reading.
    pub(crate) refusal: Option<Refusal>,
    /// Every applied `StrictHostKeyChecking` value in order (the first is OpenSSH's value).
    pub(crate) strict_values: Vec<(StrictHostKey, Origin)>,
    /// Every applied `NoHostAuthenticationForLocalhost` value in order.
    pub(crate) localhost_values: Vec<(Flag, Origin)>,
    /// Every `SessionType` value read in an applied block, in order.
    pub(crate) session_type_values: Vec<(SessionType, Origin)>,
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// `strspn(s, WHITESPACE "=")`.
fn skip_ws_eq(s: &str) -> &str {
    s.trim_start_matches(|c| WHITESPACE.contains(c) || c == '=')
}

/// `ssh_valid_hostname`.
pub(crate) fn valid_hostname(s: &str) -> bool {
    !s.starts_with('-')
        && s.bytes().all(|c| {
            !b"'`\"$\\;&<>|(){},".contains(&c) && !misc::is_c_space(c) && !c.is_ascii_control()
        })
}

/// `ssh_valid_ruser`.
pub(crate) fn valid_ruser(s: &str) -> bool {
    let b = s.as_bytes();
    if b.first() == Some(&b'-') {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| {
        !(c.is_ascii_control()
            || b"'`\";&<>|(){}$\\".contains(&c)
            || (misc::is_c_space(c) && b.get(i + 1) == Some(&b'-')))
    })
}

/// `parse_ssh_uri`: `Ok(None)` is "not an ssh URI".
pub(crate) fn parse_ssh_uri(s: &str) -> Result<Option<misc::UserHostPort>, ()> {
    match misc::parse_uri("ssh", s) {
        Uri::Other => Ok(None),
        Uri::Invalid | Uri::Parsed { path: Some(_), .. } => Err(()),
        Uri::Parsed {
            user, host, port, ..
        } => Ok(Some(misc::UserHostPort { user, host, port })),
    }
}

/// `parse_fwd_field`: one `:`-separated field of a forwarding, `[...]` literal. Returns the
/// field, whether it is a path, and the rest of the string.
fn parse_fwd_field(s: &str) -> Option<(String, bool, &str)> {
    if s.is_empty() {
        return None;
    }
    if let Some(inner) = s.strip_prefix('[') {
        let end = inner.find(']').unwrap_or(inner.len());
        let field = inner.get(..end).unwrap_or("");
        let after = inner.get(end..).unwrap_or("");
        let rest = match after.as_bytes() {
            [b']'] => "",
            [b']', b':', ..] => after.get(2..).unwrap_or(""),
            _ => return None,
        };
        return Some((field.to_string(), field.contains('/'), rest));
    }
    let mut field = String::new();
    let mut ispath = false;
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            // The escaped character is kept and has no special meaning.
            '\\' => field.push(it.next()?.1),
            '/' => {
                ispath = true;
                field.push(c);
            }
            ':' => return Some((field, ispath, s.get(i + 1..).unwrap_or(""))),
            _ => field.push(c),
        }
    }
    Some((field, ispath, ""))
}

/// `a2port` with the C `-1` for an invalid port.
fn a2port_c(s: &str) -> i32 {
    a2port(s).map_or(-1, i32::from)
}

/// `parse_forward`.
fn parse_forward(
    spec: &str,
    dynamic: bool,
    remote: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Forward> {
    let p = expand(spec, true, None, env).ok()?;
    let mut cp: Option<&str> =
        Some(p.trim_start_matches(|c: char| c.is_ascii() && misc::is_c_space(c as u8)));
    let mut fields: Vec<(String, bool)> = Vec::new();
    while fields.len() < 4 {
        let Some(s) = cp else { break };
        if s.is_empty() {
            cp = None;
            break;
        }
        let Some((f, ispath, rest)) = parse_fwd_field(s) else {
            break;
        };
        fields.push((f, ispath));
        cp = Some(rest);
    }
    if cp.is_some_and(|c| !c.is_empty()) {
        return None;
    }
    let socks = || ForwardEnd::Port {
        host: Some("socks".to_string()),
        port: Default::default(),
    };
    let path = |s: &str| ForwardEnd::Path(s.to_string());
    let port = |host: Option<&str>, p: &str| ForwardEnd::Port {
        host: host.map(str::to_string),
        port: a2port_c(p),
    };
    let fwd = match fields.as_slice() {
        [(a, ap)] => Forward {
            listen: if *ap { path(a) } else { port(None, a) },
            connect: socks(),
        },
        [(a, ap), (b, bp)] => {
            if *ap && *bp {
                Forward {
                    listen: path(a),
                    connect: path(b),
                }
            } else if *bp {
                Forward {
                    listen: port(None, a),
                    connect: path(b),
                }
            } else {
                Forward {
                    listen: port(Some(a), b),
                    connect: socks(),
                }
            }
        }
        [(a, ap), (b, _), (c, cp)] => {
            if *ap {
                Forward {
                    listen: path(a),
                    connect: port(Some(b), c),
                }
            } else if *cp {
                Forward {
                    listen: port(Some(a), b),
                    connect: path(c),
                }
            } else {
                Forward {
                    listen: port(None, a),
                    connect: port(Some(b), c),
                }
            }
        }
        [(a, _), (b, _), (c, _), (d, _)] => Forward {
            listen: port(Some(a), b),
            connect: port(Some(c), d),
        },
        _ => return None,
    };
    let n = fields.len();
    let is_path = |e: &ForwardEnd| matches!(e, ForwardEnd::Path(_));
    if dynamic {
        if !(n == 1 || n == 2) {
            return None;
        }
    } else {
        if !(n == 3 || n == 4 || is_path(&fwd.connect) || is_path(&fwd.listen)) {
            return None;
        }
        if let ForwardEnd::Port { port, .. } = &fwd.connect
            && *port <= 0
        {
            return None;
        }
    }
    match &fwd.listen {
        ForwardEnd::Port { port, .. } if *port < 0 || (!remote && *port == 0) => return None,
        _ => {}
    }
    for e in [&fwd.listen, &fwd.connect] {
        match e {
            ForwardEnd::Port { host: Some(h), .. } if h.len() >= NI_MAXHOST => return None,
            ForwardEnd::Path(p) if p.len() >= PATH_MAX_SUN => return None,
            _ => {}
        }
    }
    Some(fwd)
}

/// `addr_match_cidr_list(NULL, list)`: whether a `Match localnetwork` list is valid.
fn valid_cidr_list(list: &str) -> bool {
    list.split(',').all(|cp| {
        if cp.is_empty() || cp.len() > CIDR_ENTRY_MAX {
            return false;
        }
        if !cp
            .bytes()
            .all(|c| c.is_ascii_hexdigit() || c == b'.' || c == b':' || c == b'/')
        {
            return false;
        }
        let (addr, mask) = match cp.split_once('/') {
            Some((a, m)) => (a, Some(m)),
            None => (cp, None),
        };
        let Ok(ip) = addr.parse::<std::net::IpAddr>() else {
            return false;
        };
        let bits = if ip.is_ipv4() { IPV4_BITS } else { IPV6_BITS };
        let len = match mask {
            None => bits,
            Some(m) => match misc::strtonum(m, 0, i64::from(i32::MAX))
                .ok()
                .and_then(|v| u32::try_from(v).ok())
            {
                Some(v) => v,
                None => return false,
            },
        };
        if len > bits {
            return false;
        }
        // The host part under the mask must be zero.
        let value: u128 = match ip {
            std::net::IpAddr::V4(a) => u128::from(u32::from(a)),
            std::net::IpAddr::V6(a) => u128::from(a),
        };
        // A zero length leaves every bit to the host part (a shift by 128 would overflow).
        let host_mask = 1u128.checked_shl(bits - len).map_or(u128::MAX, |b| b - 1);
        value & host_mask == 0
    })
}

/// `strtol(s, &end, 8)` with the C rules: leading space and sign, then octal digits up to the
/// first other character. `None` if no digit was read.
fn strtol_octal(s: &str) -> Option<i64> {
    let t = s.trim_start_matches(|c: char| c.is_ascii() && misc::is_c_space(c as u8));
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..).unwrap_or("")),
        Some(b'+') => (false, t.get(1..).unwrap_or("")),
        _ => (false, t),
    };
    let n = digits
        .bytes()
        .take_while(|c| (b'0'..=b'7').contains(c))
        .count();
    if n == 0 {
        return None;
    }
    let v = i64::from_str_radix(digits.get(..n).unwrap_or(""), OCTAL_RADIX).unwrap_or(i64::MAX);
    Some(if neg { -v } else { v })
}

/// Cut a string to at most `max` bytes at a character boundary (`snprintf` into a buffer).
fn cut(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// `ssh_connection_hash`: the SHA-1 of the five strings in hex.
pub(crate) fn connection_hash(parts: [&str; 5]) -> String {
    let mut h = sha1::Sha1::new();
    for p in parts {
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl<'a> Reader<'a> {
    pub(crate) fn new(env: Env<'a>, host: String) -> Self {
        Reader {
            o: Options::default(),
            env,
            host,
            want_final: false,
            sources: Vec::new(),
            notes: Vec::new(),
            refusal: None,
            strict_values: Vec::new(),
            localhost_values: Vec::new(),
            session_type_values: Vec::new(),
        }
    }

    fn record(&mut self, op: Op, value: &str, origin: &Origin) {
        self.sources.push(ValueSource {
            keyword: keyword_name(op),
            value: value.to_string(),
            origin: origin.clone(),
        });
    }

    fn note(&mut self, keyword: &str, origin: &Origin, action: NoteAction, class: NoteClass) {
        let n = ConfigNote {
            keyword: keyword.to_string(),
            origin: origin.clone(),
            action,
            class,
        };
        if !self.notes.contains(&n) {
            self.notes.push(n);
        }
    }

    pub(crate) fn refuse(&mut self, r: Refusal) {
        if self.refusal.is_none() {
            self.refusal = Some(r);
        }
    }

    /// The host of `match_cfg_line` and `expand_match_exec_or_include_path`.
    fn match_host(&self, final_pass: bool, origin: &Origin) -> Result<String, Fatal> {
        if final_pass && let Some(h) = &self.o.hostname {
            return Ok(h.clone());
        }
        match &self.o.hostname {
            Some(h) => self.expand_hostname(h, &self.host).map_err(|e| {
                fatal(
                    RefusalKind::ConfigError,
                    Some("hostname"),
                    Some(origin),
                    format!("percent_expand: {e:?}"),
                )
            }),
            None => Ok(self.host.clone()),
        }
    }

    /// `percent_expand(hostname, "h", host)`.
    pub(crate) fn expand_hostname(
        &self,
        hostname: &str,
        host: &str,
    ) -> Result<String, ExpandError> {
        let tokens: Vec<Token> = vec![('h', Some(host.to_string()))];
        expand(hostname, false, Some(&tokens), self.env.vars)
    }

    /// `expand_match_exec_or_include_path`.
    fn expand_exec_or_include(
        &self,
        path: &str,
        final_pass: bool,
        include: bool,
        origin: &Origin,
    ) -> Result<Result<String, ExpandError>, Fatal> {
        let port = self
            .o
            .port
            .filter(|p| *p > 0)
            .unwrap_or(DEFAULT_SSH_PORT)
            .to_string();
        let ruser = self
            .o
            .user
            .clone()
            .or_else(|| self.env.account.user.clone());
        let host = self.match_host(final_pass, origin)?;
        let thishost = self.env.local_hostname.clone();
        let jmphost = match &self.o.jump {
            Some(Jump::Hosts { host, .. }) if !clear_or_none(Some(host)) => host.clone(),
            _ => String::new(),
        };
        let shorthost = thishost
            .as_deref()
            .map(|t| t.split('.').next().unwrap_or("").to_string());
        let conn_hash = match (&thishost, &ruser) {
            (Some(t), Some(u)) => Some(connection_hash([t, &host, &port, u, &jmphost])),
            _ => None,
        };
        let keyalias = self
            .o
            .host_key_alias
            .clone()
            .unwrap_or_else(|| host.clone());
        let tokens: Vec<Token> = vec![
            ('C', conn_hash),
            ('L', shorthost),
            ('d', self.env.account.home.clone()),
            ('h', Some(host)),
            ('k', Some(keyalias)),
            ('l', thishost),
            ('n', Some(self.env.host_arg.clone())),
            ('p', Some(port)),
            ('r', ruser),
            ('u', self.env.account.user.clone()),
            ('i', self.env.uid.map(|u| u.to_string())),
            ('j', Some(jmphost)),
        ];
        match expand(path, include, Some(&tokens), self.env.vars) {
            Err(ExpandError::Invalid(m)) => Err(fatal(
                RefusalKind::ConfigError,
                None,
                Some(origin),
                format!("percent_expand: {m}"),
            )),
            Err(ExpandError::Unavailable(k)) => Err(fatal(
                RefusalKind::Undeterminable,
                None,
                Some(origin),
                format!("the value of %{k} is not known"),
            )),
            r => Ok(r),
        }
    }

    /// `match_cfg_line`.
    fn match_cfg_line(
        &mut self,
        args: &mut Args,
        final_pass: bool,
        origin: &Origin,
    ) -> Result<MatchOutcome, LineErr> {
        let ruser = self
            .o
            .user
            .clone()
            .or_else(|| self.env.account.user.clone());
        let host = self.match_host(final_pass, origin)?;
        let mut result = true;
        let mut unknown: Option<&'static str> = None;
        let mut attributes = 0usize;
        while let Some(oattrib) = args.next() {
            if oattrib.starts_with('#') {
                args.consume();
                break;
            }
            let negate = oattrib.starts_with('!');
            let mut attrib = oattrib.get(usize::from(negate)..).unwrap_or("").to_string();
            if attrib.eq_ignore_ascii_case("all") {
                let next = args.next();
                if attributes > 1
                    || next
                        .as_deref()
                        .is_some_and(|a| !a.is_empty() && !a.starts_with('#'))
                {
                    return bad(format!(
                        "'{oattrib}' cannot be combined with other Match attributes"
                    ));
                }
                if next.as_deref().is_some_and(|a| a.starts_with('#')) {
                    args.consume();
                }
                if result {
                    result = !negate;
                    if negate {
                        unknown = None;
                    }
                }
                return Ok(outcome(result, unknown));
            }
            attributes += 1;
            if attrib.eq_ignore_ascii_case("canonical") || attrib.eq_ignore_ascii_case("final") {
                if attrib.eq_ignore_ascii_case("final") && !final_pass {
                    self.want_final |= !negate;
                }
                if final_pass == negate {
                    result = false;
                }
                continue;
            }
            let eq_form = [
                "host=",
                "originalhost=",
                "user=",
                "localuser=",
                "localnetwork=",
                "version=",
                "tagged=",
                "command=",
                "exec=",
            ]
            .iter()
            .any(|p| starts_with_ci(&attrib, p));
            let arg = if eq_form {
                let (a, v) = attrib.split_once('=').unwrap_or((&attrib, ""));
                let v = v.to_string();
                attrib = a.to_string();
                Some(v)
            } else {
                match args.next() {
                    Some(a) => Some(a),
                    None => return bad(format!("missing argument for Match '{oattrib}'")),
                }
            };
            let empty_ok =
                attrib.eq_ignore_ascii_case("tagged") || attrib.eq_ignore_ascii_case("command");
            let arg = arg.filter(|a| !a.is_empty() || empty_ok);
            let Some(arg) = arg.filter(|a| !a.starts_with('#')) else {
                return bad(format!("Missing Match criteria for {attrib}"));
            };
            let is = |n: &str| attrib.eq_ignore_ascii_case(n);
            let r: Option<bool> = if is("host") {
                Some(match_hostname(&host, &arg) == ListMatch::Positive)
            } else if is("originalhost") {
                Some(match_hostname(&self.env.host_arg, &arg) == ListMatch::Positive)
            } else if is("user") {
                match &ruser {
                    Some(u) => Some(match_pattern_list(u, &arg, false) == ListMatch::Positive),
                    None => {
                        unknown.get_or_insert("user");
                        None
                    }
                }
            } else if is("localuser") {
                match &self.env.account.user {
                    Some(u) => Some(match_pattern_list(u, &arg, false) == ListMatch::Positive),
                    None => {
                        unknown.get_or_insert("localuser");
                        None
                    }
                }
            } else if is("localnetwork") {
                if !valid_cidr_list(&arg) {
                    return bad(format!("Invalid network list \"{arg}\""));
                }
                unknown.get_or_insert("localnetwork");
                None
            } else if is("version") {
                Some(match_pattern_list(self.env.version, &arg, false) == ListMatch::Positive)
            } else if is("tagged") || is("command") {
                let criteria = if is("tagged") {
                    self.o.tag.clone().unwrap_or_default()
                } else {
                    self.env.command.clone().unwrap_or_default()
                };
                Some(if criteria.is_empty() {
                    arg.is_empty()
                } else {
                    match_pattern_list(&criteria, &arg, false) == ListMatch::Positive
                })
            } else if is("sessiontype") {
                let criteria = match self.o.session_type {
                    Some(SessionType::Subsystem) => "subsystem",
                    Some(SessionType::None) => "none",
                    _ if self.env.command.as_deref().is_some_and(|c| !c.is_empty()) => "exec",
                    _ => "shell",
                };
                Some(match_pattern_list(criteria, &arg, false) == ListMatch::Positive)
            } else if is("exec") {
                let expanded = self.expand_exec_or_include(&arg, final_pass, false, origin)?;
                if expanded.is_err() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some("match"),
                        Some(origin),
                        format!("failed to expand match exec '{arg}'"),
                    )));
                }
                if result {
                    unknown.get_or_insert("exec");
                }
                None
            } else {
                return bad(format!("Unsupported Match attribute {attrib}"));
            };
            if let Some(r) = r
                && r == negate
            {
                result = false;
            }
        }
        if attributes == 0 {
            return bad("One or more attributes required for Match");
        }
        Ok(outcome(result, unknown))
    }

    /// `read_config_file_depth`. `active` is the caller's `*activep`.
    pub(crate) fn read_file(
        &mut self,
        path: &Path,
        flags: Flags,
        active: &mut bool,
        depth: usize,
    ) -> Result<bool, Fatal> {
        match self.read_file_inner(path, flags, active, depth)? {
            Opened::Yes => Ok(true),
            Opened::Missing | Opened::Error(_) => Ok(false),
        }
    }

    fn read_file_inner(
        &mut self,
        path: &Path,
        flags: Flags,
        active: &mut bool,
        depth: usize,
    ) -> Result<Opened, Fatal> {
        if depth > self.env.max_depth {
            return Err(fatal(
                RefusalKind::ConfigError,
                Some("include"),
                None,
                "Too many recursive configuration includes".to_string(),
            ));
        }
        let preloaded = if depth == 0 {
            self.env.loaded.iter().find(|f| f.path == path).cloned()
        } else {
            None
        };
        let (text, owner_mode) = match preloaded {
            Some(f) => match f.text {
                Some(t) => (t, f.owner_mode),
                None => return Ok(Opened::Missing),
            },
            None => match read_text(path) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Opened::Missing),
                Err(e) => return Ok(Opened::Error(e.to_string())),
            },
        };
        if flags.check_perm
            && let Some((uid, mode)) = owner_mode
            && !perm_ok(uid, mode, self.env.uid)
        {
            return Err(fatal(
                RefusalKind::ConfigError,
                None,
                Some(&Origin::File {
                    path: path.to_path_buf(),
                    line: Default::default(),
                }),
                format!("Bad owner or permissions on {}", path.display()),
            ));
        }
        let mut bad_options = 0usize;
        let mut first_bad: Option<(Origin, String, RefusalKind)> = None;
        for (i, line) in text.split_inclusive('\n').enumerate() {
            let origin = Origin::File {
                path: path.to_path_buf(),
                line: i + 1,
            };
            let unknown_before = self
                .notes
                .iter()
                .filter(|n| n.class == NoteClass::UnknownKeyword)
                .count();
            match self.process_line(line, &origin, Some(active), flags, depth) {
                Ok(()) => {}
                Err(LineErr::Bad(msg)) => {
                    bad_options += 1;
                    let unknown_now = self
                        .notes
                        .iter()
                        .filter(|n| n.class == NoteClass::UnknownKeyword)
                        .count();
                    let kind = if unknown_now > unknown_before {
                        RefusalKind::UnknownKeyword
                    } else {
                        RefusalKind::ConfigError
                    };
                    first_bad.get_or_insert((origin, msg, kind));
                }
                Err(LineErr::Fatal(f)) => return Err(f),
            }
        }
        if bad_options > 0 {
            let (origin, msg, kind) = first_bad.unwrap_or((
                Origin::File {
                    path: path.to_path_buf(),
                    line: Default::default(),
                },
                String::new(),
                RefusalKind::ConfigError,
            ));
            return Err(fatal(
                kind,
                None,
                Some(&origin),
                format!(
                    "{}: terminating, {bad_options} bad configuration options ({msg})",
                    path.display()
                ),
            ));
        }
        Ok(Opened::Yes)
    }

    /// A command-line option (`process_config_line` with `activep == NULL`).
    pub(crate) fn command_line(&mut self, line: &str, label: &str) -> Result<(), Fatal> {
        let origin = Origin::CommandLine {
            label: label.to_string(),
        };
        match self.process_line(line, &origin, None, Flags::default(), Default::default()) {
            Ok(()) => Ok(()),
            Err(LineErr::Fatal(f)) => Err(f),
            Err(LineErr::Bad(msg)) => Err(fatal(
                if self
                    .notes
                    .iter()
                    .any(|n| n.class == NoteClass::UnknownKeyword && n.origin == origin)
                {
                    RefusalKind::UnknownKeyword
                } else {
                    RefusalKind::ConfigError
                },
                None,
                Some(&origin),
                msg,
            )),
        }
    }

    /// `process_config_line_depth`. `active` is `None` for a command-line option.
    fn process_line(
        &mut self,
        raw: &str,
        origin: &Origin,
        active: Option<&mut bool>,
        flags: Flags,
        depth: usize,
    ) -> LineResult {
        let cmdline = active.is_none();
        let mut cmdline_active = true;
        let active: &mut bool = match active {
            Some(a) => a,
            None => &mut cmdline_active,
        };
        if raw.is_empty() {
            return Ok(());
        }
        // Strip trailing whitespace; the first byte is never stripped.
        let mut len = raw.len();
        while len > 1
            && raw
                .as_bytes()
                .get(len - 1)
                .is_some_and(|c| b" \t\r\n\x0c".contains(c))
        {
            len -= 1;
        }
        let mut rest = Some(raw.get(..len).unwrap_or(raw).to_string());
        let Some(mut keyword) = strdelim(&mut rest) else {
            return Ok(());
        };
        if keyword.is_empty() {
            match strdelim(&mut rest) {
                Some(k) => keyword = k,
                None => return Ok(()),
            }
        }
        if keyword.is_empty() || keyword.starts_with('\n') || keyword.starts_with('#') {
            return Ok(());
        }
        let keyword = keyword.to_ascii_lowercase();
        let s: String = rest
            .map(|r| r.trim_start_matches(|c| WHITESPACE.contains(c)).to_string())
            .unwrap_or_default();
        if s.is_empty() {
            return bad(format!("no argument after keyword \"{keyword}\""));
        }
        let lookup = parse_token(&keyword, self.o.ignored_unknown.as_deref());
        let Ok(argv) = argv_split(&s, true) else {
            if lookup == Lookup::Unknown {
                self.note(
                    &keyword,
                    origin,
                    NoteAction::Refused,
                    NoteClass::UnknownKeyword,
                );
            }
            return bad("invalid quotes");
        };
        let mut av = Args { v: argv, i: 0 };
        let op = match lookup {
            Lookup::Unknown => {
                self.note(
                    &keyword,
                    origin,
                    NoteAction::Refused,
                    NoteClass::UnknownKeyword,
                );
                return bad(format!("Bad configuration option: {keyword}"));
            }
            Lookup::IgnoredUnknown => {
                self.note(
                    &keyword,
                    origin,
                    NoteAction::Ignored,
                    NoteClass::IgnoredUnknownKeyword,
                );
                return Ok(());
            }
            Lookup::Known(op) => op,
        };
        let a = *active;
        macro_rules! first {
            ($field:ident, $v:expr, $text:expr) => {
                if a && self.o.$field.is_none() {
                    self.o.$field = Some($v);
                    self.record(op, &$text, origin);
                }
            };
        }
        macro_rules! required {
            ($msg:expr) => {
                match av.next().filter(|x| !x.is_empty()) {
                    Some(x) => x,
                    None => return bad($msg),
                }
            };
        }
        macro_rules! multistate {
            ($field:ident, $ty:ty) => {{
                let arg = av.next();
                let Some(v) = arg
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .and_then(<$ty as Multistate>::parse)
                else {
                    return bad(format!(
                        "unsupported option \"{}\".",
                        arg.unwrap_or_default()
                    ));
                };
                first!($field, v, arg.unwrap_or_default());
            }};
        }
        macro_rules! time {
            ($field:ident) => {{
                let arg = required!("missing time value.");
                if arg != "none" {
                    let Some(v) = convtime(&arg) else {
                        return bad("invalid time value.");
                    };
                    first!($field, v, arg);
                }
            }};
        }
        macro_rules! int {
            ($field:ident) => {{
                let arg = av.next();
                let v = match atoi_err(arg.as_deref()) {
                    Ok(v) => v,
                    Err(e) => return bad(format!("integer value {e}.")),
                };
                first!($field, v, arg.unwrap_or_default());
            }};
        }
        macro_rules! string {
            ($field:ident) => {{
                let arg = required!("Missing argument.");
                first!($field, arg.clone(), arg);
            }};
        }
        macro_rules! command {
            ($field:ident) => {{
                let v = skip_ws_eq(&s).to_string();
                first!($field, v.clone(), v);
                av.consume();
            }};
        }
        macro_rules! algos {
            ($field:ident, $list:expr, $msg:literal) => {{
                let arg = required!("Missing argument.");
                let body = arg
                    .strip_prefix('+')
                    .or_else(|| arg.strip_prefix('^'))
                    .unwrap_or(&arg);
                if !arg.starts_with('-') && !$list.valid(body) {
                    return bad(format!($msg, arg));
                }
                first!($field, arg.clone(), arg);
            }};
        }
        macro_rules! str_list {
            ($field:ident, $check_empty:expr, $msg_fatal:literal) => {{
                let found = self.o.$field.is_empty();
                let mut strs: Vec<String> = Vec::new();
                while let Some(arg) = av.next() {
                    if $check_empty && arg.is_empty() {
                        return bad(format!("keyword {keyword} empty argument"));
                    }
                    if arg.eq_ignore_ascii_case("none") && (!strs.is_empty() || av.remaining() > 0)
                    {
                        return bad(format!(
                            "keyword {keyword} \"none\" argument must appear alone."
                        ));
                    }
                    strs.push(arg);
                }
                if strs.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!($msg_fatal, keyword),
                    )));
                }
                if found && a {
                    self.record(op, &strs.join(" "), origin);
                    self.o.$field = strs;
                }
            }};
        }

        match op {
            Op::Ignore => {
                self.note(
                    &keyword,
                    origin,
                    NoteAction::Ignored,
                    NoteClass::DroppedByOpenSsh,
                );
                av.consume();
            }
            Op::Deprecated | Op::Unsupported => {
                self.note(
                    &keyword,
                    origin,
                    NoteAction::Shown,
                    NoteClass::DroppedByOpenSsh,
                );
                av.consume();
            }
            Op::ConnectTimeout => time!(connection_timeout),
            Op::ForwardAgent => {
                let arg = required!("missing argument.");
                if let Some(v) = Flag::parse(&arg) {
                    first!(forward_agent, v, arg);
                } else {
                    first!(forward_agent, Flag::Yes, arg);
                    self.agent_path(&arg, op, origin, a, true)?;
                }
            }
            Op::ForwardX11 => multistate!(forward_x11, Flag),
            Op::ForwardX11Trusted => multistate!(forward_x11_trusted, Flag),
            Op::ForwardX11Timeout => time!(forward_x11_timeout),
            Op::GatewayPorts => multistate!(gateway_ports, Flag),
            Op::ExitOnForwardFailure => multistate!(exit_on_forward_failure, Flag),
            Op::PasswordAuthentication => multistate!(password_authentication, Flag),
            Op::KbdInteractiveAuthentication => multistate!(kbd_interactive_authentication, Flag),
            Op::KbdInteractiveDevices => string!(kbd_interactive_devices),
            Op::PubkeyAuthentication => multistate!(pubkey_authentication, PubkeyAuth),
            Op::HostbasedAuthentication => multistate!(hostbased_authentication, Flag),
            Op::GssAuthentication => multistate!(gss_authentication, Flag),
            Op::GssDelegateCreds => multistate!(gss_deleg_creds, Flag),
            Op::BatchMode => multistate!(batch_mode, Flag),
            Op::CheckHostIP => multistate!(check_host_ip, Flag),
            Op::VerifyHostKeyDns => multistate!(verify_host_key_dns, YesNoAsk),
            Op::StrictHostKeyChecking => {
                let arg = av.next();
                let Some(v) = arg
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .and_then(StrictHostKey::parse)
                else {
                    return bad(format!(
                        "unsupported option \"{}\".",
                        arg.unwrap_or_default()
                    ));
                };
                if a {
                    self.strict_values.push((v, origin.clone()));
                }
                first!(strict_host_key_checking, v, arg.unwrap_or_default());
            }
            Op::Compression => multistate!(compression, Compression),
            Op::TcpKeepAlive => multistate!(tcp_keep_alive, KeepAlives),
            Op::NoHostAuthenticationForLocalhost => {
                let arg = av.next();
                let Some(v) = arg
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .and_then(Flag::parse)
                else {
                    return bad(format!(
                        "unsupported option \"{}\".",
                        arg.unwrap_or_default()
                    ));
                };
                if a {
                    self.localhost_values.push((v, origin.clone()));
                }
                first!(
                    no_host_authentication_for_localhost,
                    v,
                    arg.unwrap_or_default()
                );
            }
            Op::NumberOfPasswordPrompts => int!(number_of_password_prompts),
            Op::RekeyLimit => {
                let arg = required!("Missing argument.");
                let v = if arg == "default" {
                    Default::default()
                } else {
                    let Some(v) = scan_scaled(&arg) else {
                        return bad(format!("Bad number '{arg}'"));
                    };
                    if v != 0 && v < REKEY_LIMIT_MIN {
                        return bad("RekeyLimit too small");
                    }
                    v
                };
                first!(rekey_limit, v, arg);
                if av.remaining() > 0 {
                    if av.peek() == Some("none") {
                        av.next();
                    } else {
                        time!(rekey_interval);
                    }
                }
            }
            Op::IdentityFile | Op::CertificateFile => {
                let arg = required!("Missing argument.");
                if a {
                    let ident = op == Op::IdentityFile;
                    let count = if ident {
                        self.o.identity_files.len()
                    } else {
                        self.o.certificate_files.len()
                    };
                    if count >= MAX_IDENTITY_FILES {
                        return bad("Too many identity files specified");
                    }
                    if ident {
                        self.o.add_identity_file(arg.clone(), flags.user_conf);
                    } else {
                        self.o.add_certificate_file(arg.clone(), flags.user_conf);
                    }
                    self.record(op, &arg, origin);
                }
            }
            Op::XAuthLocation => string!(xauth_location),
            Op::User => string!(user),
            Op::GlobalKnownHostsFile | Op::UserKnownHostsFile => {
                let empty_at_start = if op == Op::GlobalKnownHostsFile {
                    self.o.system_hostfiles.is_empty()
                } else {
                    self.o.user_hostfiles.is_empty()
                };
                let mut i = 0usize;
                let mut added = Vec::new();
                while let Some(arg) = av.next() {
                    if arg.is_empty() {
                        return bad(format!("keyword {keyword} empty argument"));
                    }
                    if arg.eq_ignore_ascii_case("none") && (i > 0 || av.remaining() > 0) {
                        return bad(format!(
                            "keyword {keyword} \"none\" argument must appear alone."
                        ));
                    }
                    i += 1;
                    if a && empty_at_start {
                        let list = if op == Op::GlobalKnownHostsFile {
                            &mut self.o.system_hostfiles
                        } else {
                            &mut self.o.user_hostfiles
                        };
                        if list.len() >= MAX_HOSTS_FILES {
                            return bad(format!("too many {keyword} entries."));
                        }
                        list.push(arg.clone());
                        added.push(arg);
                    }
                }
                if !added.is_empty() {
                    self.record(op, &added.join(" "), origin);
                }
            }
            Op::Hostname => string!(hostname),
            Op::Tag => string!(tag),
            Op::HostKeyAlias => string!(host_key_alias),
            Op::PreferredAuthentications => string!(preferred_authentications),
            Op::BindAddress => string!(bind_address),
            Op::BindInterface => string!(bind_interface),
            Op::Pkcs11Provider => string!(pkcs11_provider),
            Op::SecurityKeyProvider => string!(sk_provider),
            Op::KnownHostsCommand => command!(known_hosts_command),
            Op::ProxyCommand => command!(proxy_command),
            Op::ProxyJump => {
                let v = skip_ws_eq(&s).to_string();
                if self.parse_jump(&v, cmdline, a).is_err() {
                    return bad(format!("Invalid ProxyJump \"{v}\""));
                }
                if a {
                    self.record(op, &v, origin);
                }
                av.consume();
            }
            Op::Port => {
                let arg = required!("Missing argument.");
                let Some(p) = a2port(&arg).filter(|p| *p > 0) else {
                    return bad(format!("Bad port '{arg}'."));
                };
                first!(port, p, arg);
            }
            Op::ConnectionAttempts => int!(connection_attempts),
            Op::Ciphers => algos!(ciphers, AlgList::Ciphers, "Bad SSH2 cipher spec '{}'."),
            Op::Macs => algos!(macs, AlgList::Macs, "Bad SSH2 MAC spec '{}'."),
            Op::KexAlgorithms => {
                algos!(kex_algorithms, AlgList::Kex, "Bad SSH2 KexAlgorithms '{}'.")
            }
            Op::HostKeyAlgorithms => {
                algos!(hostkeyalgorithms, AlgList::HostKey, "Bad key types '{}'.")
            }
            Op::CaSignatureAlgorithms => {
                algos!(
                    ca_sign_algorithms,
                    AlgList::CaSignature,
                    "Bad key types '{}'."
                )
            }
            Op::HostbasedAcceptedAlgorithms => algos!(
                hostbased_accepted_algos,
                AlgList::HostbasedAccepted,
                "Bad key types '{}'."
            ),
            Op::PubkeyAcceptedAlgorithms => algos!(
                pubkey_accepted_algos,
                AlgList::PubkeyAccepted,
                "Bad key types '{}'."
            ),
            Op::LogLevel => {
                let arg = av.next();
                let Some(v) = arg.as_deref().and_then(LogLevel::parse) else {
                    return bad(format!(
                        "unsupported log level '{}'",
                        arg.unwrap_or_else(|| "<NONE>".to_string())
                    ));
                };
                first!(log_level, v, arg.unwrap_or_default());
            }
            Op::LogFacility => {
                let arg = av.next();
                let Some(v) = arg.as_deref().and_then(LogFacility::parse) else {
                    return bad(format!(
                        "unsupported log facility '{}'",
                        arg.unwrap_or_else(|| "<NONE>".to_string())
                    ));
                };
                // OpenSSH sets this one whether or not the block applies.
                if self.o.log_facility.is_none() {
                    self.o.log_facility = Some(v);
                    if a {
                        self.record(op, &arg.unwrap_or_default(), origin);
                    }
                }
            }
            Op::LogVerbose => {
                let mut i = 0usize;
                while let Some(arg) = av.next() {
                    if arg.is_empty() {
                        return bad(format!("keyword {keyword} empty argument"));
                    }
                    if arg.eq_ignore_ascii_case("none") && (i > 0 || av.remaining() > 0) {
                        return bad(format!(
                            "keyword {keyword} \"none\" argument must appear alone."
                        ));
                    }
                    i += 1;
                    // OpenSSH adds only while the list is empty, so only the first value.
                    if a && self.o.log_verbose.is_empty() {
                        self.record(op, &arg, origin);
                        self.o.log_verbose.push(arg);
                    }
                }
            }
            Op::LocalForward | Op::RemoteForward | Op::DynamicForward => {
                let arg = required!("Missing argument.");
                let remote = op == Op::RemoteForward;
                let mut dynamic = op == Op::DynamicForward;
                let mut spec = String::new();
                if !dynamic {
                    match av.next().filter(|x| !x.is_empty()) {
                        Some(arg2) => spec = cut(format!("{arg}:{arg2}"), FWDARG_MAX),
                        None if remote => dynamic = true,
                        None => return bad("Missing target argument."),
                    }
                }
                if dynamic {
                    spec = cut(arg.clone(), FWDARG_MAX);
                }
                let Some(fwd) = parse_forward(&spec, dynamic, remote, self.env.vars) else {
                    return bad("Bad forwarding specification.");
                };
                if a {
                    let list = if remote {
                        &mut self.o.remote_forwards
                    } else {
                        &mut self.o.local_forwards
                    };
                    if !list.contains(&fwd) {
                        list.push(fwd);
                    }
                    self.record(op, &spec, origin);
                }
            }
            Op::PermitRemoteOpen => {
                let found = self.o.permitted_remote_opens.is_empty();
                let mut strs: Vec<String> = Vec::new();
                while let Some(arg) = av.next() {
                    if arg.eq_ignore_ascii_case("none") || arg.eq_ignore_ascii_case("any") {
                        if !strs.is_empty() || av.remaining() > 0 {
                            return bad(format!(
                                "keyword {keyword} \"{arg}\" argument must appear alone."
                            ));
                        }
                    } else {
                        let ok_host = hpdelim(&arg);
                        let Some((_, port)) = ok_host else {
                            return Err(LineErr::Fatal(fatal(
                                RefusalKind::ConfigError,
                                Some(&keyword),
                                Some(origin),
                                format!("missing host in {}", keyword_name(op)),
                            )));
                        };
                        if port.is_none_or(|p| p != "*" && a2port(p).is_none_or(|p| p == 0)) {
                            return Err(LineErr::Fatal(fatal(
                                RefusalKind::ConfigError,
                                Some(&keyword),
                                Some(origin),
                                format!("bad port number in {}", keyword_name(op)),
                            )));
                        }
                    }
                    strs.push(arg);
                }
                if strs.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("missing {} specification", keyword_name(op)),
                    )));
                }
                if found && a {
                    self.record(op, &strs.join(" "), origin);
                    self.o.permitted_remote_opens = strs;
                }
            }
            Op::ClearAllForwardings => multistate!(clear_forwardings, Flag),
            Op::Host => {
                if cmdline {
                    return bad("Host directive not supported as a command-line option");
                }
                *active = false;
                while let Some(arg) = av.next() {
                    if arg.is_empty() {
                        return bad(format!("keyword {keyword} empty argument"));
                    }
                    if flags.never_match {
                        av.consume();
                        break;
                    }
                    let negated = arg.starts_with('!');
                    let pat = arg.get(usize::from(negated)..).unwrap_or("");
                    if match_pattern(&self.host, pat) {
                        if negated {
                            *active = false;
                            av.consume();
                            break;
                        }
                        *active = true;
                    }
                }
            }
            Op::Match => {
                if cmdline {
                    return bad("Match directive not supported as a command-line option");
                }
                let outcome = match self.match_cfg_line(&mut av, flags.final_pass, origin) {
                    Ok(o) => o,
                    Err(LineErr::Bad(m)) => return bad(format!("{m}; Bad Match condition")),
                    Err(e) => return Err(e),
                };
                *active = match outcome {
                    _ if flags.never_match => false,
                    MatchOutcome::True => true,
                    MatchOutcome::False => false,
                    MatchOutcome::Undeterminable(criterion) => {
                        self.refuse(Refusal {
                            kind: RefusalKind::Undeterminable,
                            keyword: Some(format!("match {criterion}")),
                            origin: Some(origin.clone()),
                            message: format!(
                                "Match {criterion} decides whether the block applies and \
                                 cannot be evaluated"
                            ),
                        });
                        self.note(
                            "match",
                            origin,
                            NoteAction::Refused,
                            NoteClass::SecurityNotImplemented,
                        );
                        false
                    }
                };
            }
            Op::EscapeChar => {
                let arg = required!("Missing argument.");
                let b = arg.as_bytes();
                let v = if arg == "none" {
                    EscapeChar::None
                } else if let [c] = b {
                    EscapeChar::Byte(*c)
                } else if let [b'^', c] = b
                    && (CTRL_FIRST..CTRL_END).contains(c)
                {
                    EscapeChar::Byte(c & CTRL_MASK)
                } else {
                    return bad("Bad escape character.");
                };
                first!(escape_char, v, arg);
            }
            Op::AddressFamily => multistate!(address_family, AddressFamily),
            Op::EnableSshKeysign => multistate!(enable_ssh_keysign, Flag),
            Op::IdentitiesOnly => multistate!(identities_only, Flag),
            Op::ServerAliveInterval => time!(server_alive_interval),
            Op::ServerAliveCountMax => int!(server_alive_count_max),
            Op::SendEnv => {
                let mut found = false;
                while let Some(arg) = av.next() {
                    if arg.is_empty() || arg.contains('=') {
                        return bad("Invalid environment name.");
                    }
                    found = true;
                    if !a {
                        continue;
                    }
                    self.record(op, &arg, origin);
                    if let Some(pattern) = arg.strip_prefix('-') {
                        self.o.send_env.retain(|e| !match_pattern(e, pattern));
                        continue;
                    }
                    self.o.send_env.push(arg);
                }
                if !found {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("no {keyword} specified"),
                    )));
                }
            }
            Op::SetEnv => {
                let found = self.o.setenv.is_empty();
                let mut strs: Vec<String> = Vec::new();
                while let Some(arg) = av.next() {
                    if !arg.contains('=') {
                        return bad("Invalid SetEnv.");
                    }
                    if setenv_in_list(&arg, &strs) {
                        continue;
                    }
                    strs.push(arg);
                }
                if strs.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("no {keyword} specified"),
                    )));
                }
                if found && a {
                    for e in &strs {
                        self.record(op, e, origin);
                    }
                    self.o.setenv = strs;
                }
            }
            Op::ControlPath => string!(control_path),
            Op::ControlMaster => multistate!(control_master, ControlMaster),
            Op::ControlPersist => {
                let arg = required!("Missing ControlPersist argument.");
                let v = match arg.as_str() {
                    "no" | "false" => (false, Default::default()),
                    "yes" | "true" => (true, Default::default()),
                    _ => match convtime(&arg) {
                        Some(t) => (true, t),
                        None => return bad("Bad ControlPersist argument."),
                    },
                };
                first!(control_persist, v, arg);
            }
            Op::HashKnownHosts => multistate!(hash_known_hosts, Flag),
            Op::Tunnel => multistate!(tun_open, Tunnel),
            Op::TunnelDevice => {
                let arg = required!("Missing argument.");
                let Some(v) = a2tun(&arg) else {
                    return bad("Bad tun device.");
                };
                first!(tun_device, v, arg);
            }
            Op::LocalCommand => command!(local_command),
            Op::PermitLocalCommand => multistate!(permit_local_command, Flag),
            Op::RemoteCommand => command!(remote_command),
            Op::VisualHostKey => multistate!(visual_host_key, Flag),
            Op::Include => {
                if cmdline {
                    return bad("Include directive not supported as a command-line option");
                }
                self.include(&mut av, &keyword, origin, active, flags, depth)?;
            }
            Op::IpQos => {
                let arg = av.next();
                let Some(mut v1) = parse_ipqos(arg.as_deref()) else {
                    return bad(format!("Bad IPQoS value: {}", arg.unwrap_or_default()));
                };
                if v1 == misc::IPQOS_DEPRECATED {
                    v1 = misc::IPQOS_NONE;
                }
                let arg2 = av.next();
                let mut v2 = match &arg2 {
                    None => v1,
                    Some(x) => match parse_ipqos(Some(x)) {
                        Some(v) => v,
                        None => return bad(format!("Bad IPQoS value: {x}")),
                    },
                };
                if v2 == misc::IPQOS_DEPRECATED {
                    v2 = misc::IPQOS_NONE;
                }
                let text = match &arg2 {
                    Some(x) => format!("{} {x}", arg.unwrap_or_default()),
                    None => arg.unwrap_or_default(),
                };
                first!(ip_qos, (v1, v2), text);
            }
            Op::RequestTty => multistate!(request_tty, RequestTty),
            Op::SessionType => {
                let arg = av.next();
                let Some(v) = arg
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .and_then(SessionType::parse)
                else {
                    return bad(format!(
                        "unsupported option \"{}\".",
                        arg.unwrap_or_default()
                    ));
                };
                if a {
                    self.session_type_values.push((v, origin.clone()));
                }
                first!(session_type, v, arg.unwrap_or_default());
            }
            Op::StdinNull => multistate!(stdin_null, Flag),
            Op::ForkAfterAuthentication => multistate!(fork_after_authentication, Flag),
            Op::IgnoreUnknown => string!(ignored_unknown),
            Op::ProxyUseFdpass => multistate!(proxy_use_fdpass, Flag),
            Op::CanonicalDomains => {
                let found = self.o.canonical_domains.is_empty();
                let mut strs: Vec<String> = Vec::new();
                while let Some(arg) = av.next() {
                    if arg.eq_ignore_ascii_case("none") && (!strs.is_empty() || av.remaining() > 0)
                    {
                        return bad(format!(
                            "keyword {keyword} \"none\" argument must appear alone."
                        ));
                    }
                    // `valid_domain(arg, 1, ...)` lower-cases the value in place and drops one
                    // trailing `.`.
                    match valid_domain(&arg, true) {
                        Ok(domain) => strs.push(domain),
                        Err(e) => return bad(e),
                    }
                }
                if strs.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("no {keyword} specified"),
                    )));
                }
                if found && a {
                    self.record(op, &strs.join(" "), origin);
                    self.o.canonical_domains = strs;
                }
            }
            Op::CanonicalizePermittedCnames => {
                let found = self.o.permitted_cnames.is_empty();
                let mut cnames: Vec<(String, String)> = Vec::new();
                while let Some(arg) = av.next() {
                    if arg.eq_ignore_ascii_case("none") {
                        if !cnames.is_empty() || av.remaining() > 0 {
                            return bad(format!(
                                "keyword {keyword} \"none\" argument must appear alone."
                            ));
                        }
                        cnames.push((arg, String::new()));
                    } else if arg == "*" {
                        cnames.push((arg.clone(), arg));
                    } else {
                        let lower = arg.to_ascii_lowercase();
                        match lower.split_once(':') {
                            Some((src, dst)) if !dst.is_empty() => {
                                cnames.push((src.to_string(), dst.to_string()));
                            }
                            _ => return bad(format!("Invalid permitted CNAME \"{lower}\"")),
                        }
                    }
                }
                if cnames.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("no {keyword} specified"),
                    )));
                }
                if found && a {
                    let text: Vec<String> =
                        cnames.iter().map(|(x, y)| format!("{x}:{y}")).collect();
                    self.record(op, &text.join(" "), origin);
                    self.o.permitted_cnames = cnames;
                }
            }
            Op::CanonicalizeHostname => multistate!(canonicalize_hostname, Canonicalize),
            Op::CanonicalizeMaxDots => int!(canonicalize_max_dots),
            Op::CanonicalizeFallbackLocal => multistate!(canonicalize_fallback_local, Flag),
            Op::StreamLocalBindMask => {
                let arg = required!("Missing StreamLocalBindMask argument.");
                let v = strtol_octal(&arg)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| *v <= BIND_MASK_MAX);
                let Some(v) = v else {
                    return bad("Bad mask.");
                };
                first!(streamlocal_bind_mask, v, arg);
            }
            Op::StreamLocalBindUnlink => multistate!(streamlocal_bind_unlink, Flag),
            Op::RevokedHostKeys => str_list!(revoked_host_keys, true, "no {} specified"),
            Op::FingerprintHash => {
                let arg = required!("Missing argument.");
                let Some(v) = Digest::parse(&arg) else {
                    return bad(format!("Invalid hash algorithm \"{arg}\"."));
                };
                first!(fingerprint_hash, v, arg);
            }
            Op::UpdateHostkeys => multistate!(update_hostkeys, YesNoAsk),
            Op::AddKeysToAgent => {
                let arg = av.next();
                let arg2 = av.next();
                let mode = arg
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .and_then(AddKeys::parse);
                let v = match (mode, &arg2) {
                    (Some(AddKeys::Confirm), Some(t)) => match convtime(t) {
                        Some(t) => (AddKeys::Confirm, t),
                        None => return bad("invalid time value."),
                    },
                    (None, None) => match arg.as_deref().and_then(convtime) {
                        Some(t) => (AddKeys::Yes, t),
                        None => return bad("unsupported option"),
                    },
                    (None, Some(_)) | (Some(_), Some(_)) => return bad("unsupported option"),
                    (Some(m), None) => (m, Default::default()),
                };
                let text = match &arg2 {
                    Some(t) => format!("{} {t}", arg.unwrap_or_default()),
                    None => arg.unwrap_or_default(),
                };
                first!(add_keys_to_agent, v, text);
            }
            Op::IdentityAgent => {
                let arg = required!("Missing argument.");
                self.agent_path(&arg, op, origin, a, false)?;
            }
            Op::EnableEscapeCommandline => multistate!(enable_escape_commandline, Flag),
            Op::RequiredRsaSize => int!(required_rsa_size),
            Op::WarnWeakCrypto => multistate!(warn_weak_crypto, WarnWeakCrypto),
            Op::ObscureKeystrokeTiming => {
                let mut value: Option<i32> = None;
                let mut text = String::new();
                while let Some(arg) = av.next() {
                    if value.is_some() {
                        return bad("invalid arguments");
                    }
                    value = Some(match arg.as_str() {
                        "yes" | "true" => KEYSTROKE_DEFAULT_INTERVAL_MS,
                        "no" | "false" => Default::default(),
                        _ => match arg.strip_prefix("interval:") {
                            Some(n) => {
                                let v = match atoi_err(Some(n)) {
                                    Ok(v) => v,
                                    Err(e) => return bad(format!("integer value {e}.")),
                                };
                                if v <= 0 || v > KEYSTROKE_INTERVAL_MAX {
                                    return bad("value out of range.");
                                }
                                v
                            }
                            None => return bad(format!("unsupported argument \"{arg}\"")),
                        },
                    });
                    text = arg;
                }
                let Some(v) = value else {
                    return bad("missing argument");
                };
                first!(obscure_keystroke_timing_interval, v, text);
            }
            Op::ChannelTimeout => {
                let found = self.o.channel_timeouts.is_empty();
                let mut strs: Vec<String> = Vec::new();
                while let Some(arg) = av.next() {
                    if arg.eq_ignore_ascii_case("none") {
                        if !strs.is_empty() || av.remaining() > 0 {
                            return bad(format!(
                                "keyword {keyword} \"none\" argument must appear alone."
                            ));
                        }
                    } else if !parse_pattern_interval(&arg) {
                        return Err(LineErr::Fatal(fatal(
                            RefusalKind::ConfigError,
                            Some(&keyword),
                            Some(origin),
                            format!("invalid channel timeout {arg}"),
                        )));
                    }
                    strs.push(arg);
                }
                if strs.is_empty() {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("no {keyword} specified"),
                    )));
                }
                if found && a {
                    self.record(op, &strs.join(" "), origin);
                    self.o.channel_timeouts = strs;
                }
            }
            Op::VersionAddendum => {
                if s.contains('\r') {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::ConfigError,
                        Some(&keyword),
                        Some(origin),
                        format!("Invalid {keyword} argument"),
                    )));
                }
                // `strchr(line, '#')` (readconf.c:2553) only sees the keyword: `strdelim`
                // ended the line after it. A `#` in the value stays.
                let v = s.clone();
                let v = if v.eq_ignore_ascii_case("none") {
                    String::new()
                } else {
                    v
                };
                first!(version_addendum, v.clone(), v);
                av.consume();
            }
            Op::RefuseConnection => {
                let arg = required!("Missing argument.");
                if a {
                    return Err(LineErr::Fatal(fatal(
                        RefusalKind::RefuseConnection,
                        Some(&keyword),
                        Some(origin),
                        format!("RefuseConnection: {arg}"),
                    )));
                }
            }
        }
        if av.remaining() > 0 {
            return bad(format!("keyword {keyword} extra arguments at end of line"));
        }
        Ok(())
    }

    /// The `parse_agent_path` part of `IdentityAgent` and `ForwardAgent`.
    fn agent_path(
        &mut self,
        arg: &str,
        op: Op,
        origin: &Origin,
        a: bool,
        forward: bool,
    ) -> LineResult {
        if expand(arg, true, None, self.env.vars).is_err() {
            return bad(format!("Invalid environment expansion {arg}."));
        }
        if let Some(name) = arg.strip_prefix('$')
            && !name.starts_with('{')
            && !valid_env_name(name)
        {
            return bad(format!("Invalid environment name {arg}."));
        }
        let slot = if forward {
            &mut self.o.forward_agent_sock_path
        } else {
            &mut self.o.identity_agent
        };
        if a && slot.is_none() {
            *slot = Some(arg.to_string());
            self.record(op, arg, origin);
        }
        Ok(())
    }

    /// `parse_jump`.
    fn parse_jump(&mut self, s: &str, strict: bool, active: bool) -> Result<(), ()> {
        if s.eq_ignore_ascii_case("none") {
            if active && self.o.jump.is_none() {
                self.o.jump = Some(Jump::None);
            }
            return Ok(());
        }
        let orig = match s.find('#') {
            Some(i) => s.get(..i).unwrap_or(""),
            None => s,
        };
        let orig = orig.trim_end_matches(|c: char| c.is_ascii() && misc::is_c_space(c as u8));
        let active = active && self.o.proxy_command.is_none() && self.o.jump.is_none();
        let mut first: Option<misc::UserHostPort> = None;
        let mut rest = orig;
        loop {
            let (head, cp) = match rest.rfind(',') {
                Some(i) => (
                    Some(rest.get(..i).unwrap_or("")),
                    rest.get(i + 1..).unwrap_or(""),
                ),
                None => (None, rest),
            };
            let parsed = match parse_ssh_uri(cp)? {
                Some(p) => p,
                None => parse_user_host_port(cp).ok_or(())?,
            };
            if strict
                && (!valid_hostname(&parsed.host)
                    || parsed.user.as_deref().is_some_and(|u| !valid_ruser(u)))
            {
                return Err(());
            }
            first.get_or_insert(parsed);
            match head {
                Some(h) => rest = h,
                None => break,
            }
        }
        if active && let Some(f) = first {
            self.o.jump = Some(Jump::Hosts {
                user: f.user,
                host: f.host,
                port: f.port,
                extra: orig
                    .rfind(',')
                    .map(|i| orig.get(..i).unwrap_or("").to_string()),
            });
            self.o.proxy_command = Some("none".to_string());
        }
        Ok(())
    }

    /// The `oInclude` case.
    fn include(
        &mut self,
        av: &mut Args,
        keyword: &str,
        origin: &Origin,
        active: &mut bool,
        flags: Flags,
        depth: usize,
    ) -> LineResult {
        let mut vanished = false;
        while let Some(arg) = av.next() {
            if arg.is_empty() {
                return bad(format!("keyword {keyword} empty argument"));
            }
            let p = match self.expand_exec_or_include(&arg, flags.final_pass, true, origin)? {
                Ok(p) => p,
                // OpenSSH logs "Unable to expand user config file" and goes on.
                Err(_) => continue,
            };
            if p.starts_with('~') && !flags.user_conf {
                return bad(format!("bad include path {p}."));
            }
            let pattern = if !misc::path_absolute(&p) && !p.starts_with('~') {
                if flags.user_conf {
                    format!("~/.ssh/{p}")
                } else {
                    format!("{SSHDIR}/{p}")
                }
            } else {
                p
            };
            // GLOB_TILDE: a `~` that cannot be expanded stays as written.
            let pattern = if pattern.starts_with('~') {
                tilde_expand(&pattern, &self.env.account).unwrap_or(pattern)
            } else {
                pattern
            };
            let paths = crate::glob::glob(&pattern);
            let oactive = *active;
            for path in paths {
                let sub = Flags {
                    check_perm: true,
                    never_match: flags.never_match || !oactive,
                    ..flags
                };
                match self.read_file_inner(Path::new(&path), sub, active, depth + 1)? {
                    Opened::Yes => {}
                    Opened::Missing => vanished = true,
                    Opened::Error(e) => {
                        return bad(format!("Can't open user config file {path}: {e}"));
                    }
                }
                *active = oactive;
            }
        }
        if vanished {
            return bad("an included file could not be opened");
        }
        Ok(())
    }
}

fn outcome(result: bool, unknown: Option<&'static str>) -> MatchOutcome {
    match (result, unknown) {
        (false, _) => MatchOutcome::False,
        (true, None) => MatchOutcome::True,
        (true, Some(c)) => MatchOutcome::Undeterminable(c),
    }
}

/// The text of a file and its owner and mode (Unix).
fn read_text(path: &Path) -> std::io::Result<(String, Option<(u32, u32)>)> {
    let bytes = std::fs::read(path)?;
    let meta = std::fs::metadata(path)?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        owner_mode(&meta),
    ))
}

/// The owner and mode of a file, on Unix.
pub(crate) fn owner_mode(meta: &std::fs::Metadata) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some((meta.uid(), meta.mode()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// `CHECKPERM`: owned by root or the user, not writable by group or others. Without the
/// user's uid only the mode is checked.
fn perm_ok(uid: u32, mode: u32, me: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        let owner_ok = uid == 0 || me.is_none_or(|m| m == uid);
        owner_ok && mode & GROUP_OTHER_WRITE == 0
    }
    #[cfg(not(unix))]
    {
        let _ = (uid, mode, me);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_and_user_syntax() {
        assert!(valid_hostname("host.example"));
        assert!(!valid_hostname("-oProxyCommand"));
        assert!(!valid_hostname("a;b"));
        assert!(valid_ruser("user name"));
        assert!(!valid_ruser("user -x"));
        assert!(!valid_ruser("a$b"));
    }

    #[test]
    fn forwards() {
        let env = |_: &str| None;
        let f = parse_forward("8080:localhost:80", false, false, &env).unwrap();
        assert_eq!(
            f.connect,
            ForwardEnd::Port {
                host: Some("localhost".to_string()),
                port: 80
            }
        );
        let f = parse_forward("[::1]:8080:[fe80::1]:80", false, false, &env).unwrap();
        assert_eq!(
            f.listen,
            ForwardEnd::Port {
                host: Some("::1".to_string()),
                port: 8080
            }
        );
        assert!(parse_forward("8080", false, false, &env).is_none());
        assert!(parse_forward("1080", true, false, &env).is_some());
        assert!(parse_forward("0", true, false, &env).is_none());
        assert!(parse_forward("/tmp/s:/tmp/t", false, false, &env).is_some());
    }

    #[test]
    fn cidr_lists() {
        assert!(valid_cidr_list("192.0.2.0/24,2001:db8::/32"));
        assert!(!valid_cidr_list("192.0.2.1/24"));
        assert!(!valid_cidr_list("192.0.2.0/33"));
        assert!(!valid_cidr_list("a,,b"));
        assert!(!valid_cidr_list("host.example"));
        // A zero mask length: the whole address is the host part (addr.c:50-56).
        assert!(valid_cidr_list("::/0"));
        assert!(valid_cidr_list("0.0.0.0/0"));
        assert!(!valid_cidr_list("1::/0"));
        assert!(!valid_cidr_list("128.0.0.0/0"));
    }

    #[test]
    fn octal_and_hash() {
        assert_eq!(strtol_octal("0177"), Some(0o177));
        assert_eq!(strtol_octal("0777x"), Some(0o777));
        assert_eq!(strtol_octal("x"), None);
        assert_eq!(
            connection_hash(["a", "b", "c", "d", "e"]),
            "03de6c570bfe24bfc328ccd7ca46b76eadaf4334"
        );
    }
}
