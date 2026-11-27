// This file is derived from OpenSSH, modified: a Rust port of parts of misc.c, defines.h
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// misc.c:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2005-2020 Damien Miller.  All rights reserved.
//  * Copyright (c) 2004 Henning Brauer <henning@openbsd.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// defines.h:
// /*
//  * Copyright (c) 1999-2003 Damien Miller.  All rights reserved.
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
//! Helpers of OpenSSH `misc.c`: token splitting, numbers, times, ports, host and user
//! syntax, `~` and `%`/`${}` expansion, domain names and IP QoS names.
//!
//! The C functions work on NUL-terminated byte strings that they change in place. These
//! take `&str` and return new values; every delimiter they look for is ASCII, so slicing at
//! the found positions keeps the strings valid UTF-8.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `WHITESPACE` of `misc.c` and `readconf.c`.
pub(crate) const WHITESPACE: &str = " \t\r\n";

/// The largest port (`a2port`: `strtonum(s, 0, 65535)`).
const PORT_MAX: i64 = 65535;
/// `SSH_TUNID_ANY` (`misc.h`).
pub(crate) const TUNID_ANY: i32 = 0x7fff_ffff;
/// `SSH_TUNID_MAX` (`misc.h`, `SSH_TUNID_ANY - 2`).
const TUNID_MAX: i64 = 0x7fff_fffd;
/// Seconds in a minute, hour, day and week (`convtime_double`).
const SECS_PER_MINUTE: f64 = 60.0;
const SECS_PER_HOUR: f64 = 3600.0;
const SECS_PER_DAY: f64 = 86400.0;
const SECS_PER_WEEK: f64 = 604_800.0;

/// C `isspace` in the C locale.
pub(crate) fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn skip_ws(s: &str) -> &str {
    s.trim_start_matches(|c| WHITESPACE.contains(c))
}

/// `strdelim`: the next token of a configuration line, split on whitespace or one `=`, with
/// a `"` that joins the text up to the next `"`. `rest` is the unread part; `None` is the C
/// `NULL`. A `"` without its pair gives `None` and leaves `rest` empty.
pub(crate) fn strdelim(rest: &mut Option<String>) -> Option<String> {
    let s = rest.take()?;
    let Some(pos) = s.find(|c| WHITESPACE.contains(c) || c == '"' || c == '=') else {
        return Some(s);
    };
    let delim = s.as_bytes().get(pos).copied();
    let after = s.get(pos + 1..).unwrap_or("");
    if delim == Some(b'"') {
        let close = after.find('"')?;
        let token = format!(
            "{}{}",
            s.get(..pos).unwrap_or(""),
            after.get(..close).unwrap_or("")
        );
        *rest = Some(skip_ws(after.get(close + 1..).unwrap_or("")).to_string());
        return Some(token);
    }
    let wspace = delim == Some(b'=');
    let token = s.get(..pos).unwrap_or("").to_string();
    let mut tail = skip_ws(after);
    if !wspace && tail.starts_with('=') {
        tail = skip_ws(tail.get(1..).unwrap_or(""));
    }
    *rest = Some(tail.to_string());
    Some(token)
}

/// `argv_split`: split a string into words with `"`/`'` quotes and `\` escapes. With
/// `terminate_on_comment`, a word that starts with `#` ends the list. `Err` is an unclosed
/// quote.
pub(crate) fn argv_split(s: &str, terminate_on_comment: bool) -> Result<Vec<String>, ()> {
    let b = s.as_bytes();
    let mut argv = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b.get(i).copied().unwrap_or_default();
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        if terminate_on_comment && c == b'#' {
            break;
        }
        let mut quote: Option<u8> = None;
        let mut arg: Vec<u8> = Vec::new();
        while let Some(&c) = b.get(i) {
            if c == b'\\' {
                let next = b.get(i + 1).copied();
                let known = matches!(next, Some(b'\'' | b'"' | b'\\'))
                    || (quote.is_none() && next == Some(b' '));
                if let (true, Some(n)) = (known, next) {
                    i += 1;
                    arg.push(n);
                } else {
                    arg.push(c);
                }
            } else if quote.is_none() && (c == b' ' || c == b'\t') {
                break;
            } else if quote.is_none() && (c == b'"' || c == b'\'') {
                quote = Some(c);
            } else if quote == Some(c) {
                quote = None;
            } else {
                arg.push(c);
            }
            i += 1;
        }
        argv.push(String::from_utf8(arg).unwrap_or_default());
        if i >= b.len() {
            if quote.is_some() {
                return Err(());
            }
            break;
        }
        i += 1;
    }
    Ok(argv)
}

/// `strtonum`: a base-10 integer in `min..=max` with the C `strtoll` leading whitespace and
/// sign. The error is the `strtonum` error string.
pub(crate) fn strtonum(s: &str, min: i64, max: i64) -> Result<i64, &'static str> {
    let t = s.trim_start_matches(|c: char| c.is_ascii() && is_c_space(c as u8));
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..).unwrap_or("")),
        Some(b'+') => (false, t.get(1..).unwrap_or("")),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err("invalid");
    }
    let signed = format!("{}{digits}", if neg { "-" } else { "" });
    let v = match signed.parse::<i64>() {
        Ok(v) => v,
        Err(_) if neg => return Err("too small"),
        Err(_) => return Err("too large"),
    };
    if v < min {
        Err("too small")
    } else if v > max {
        Err("too large")
    } else {
        Ok(v)
    }
}

/// `atoi_err`: a non-negative `int`. The error is the text OpenSSH prints after
/// `integer value`.
pub(crate) fn atoi_err(s: Option<&str>) -> Result<i32, &'static str> {
    let s = match s {
        None | Some("") => return Err("missing"),
        Some(s) => s,
    };
    let v = strtonum(s, 0, i64::from(i32::MAX))?;
    i32::try_from(v).map_err(|_| "too large")
}

/// `a2port`: a TCP port `0..=65535`, `None` if invalid. Service names are not looked up
/// (`getservbyname`): this crate does not read the services database.
pub(crate) fn a2port(s: &str) -> Option<u16> {
    strtonum(s, 0, PORT_MAX)
        .ok()
        .and_then(|v| u16::try_from(v).ok())
}

/// `a2tun` for one side: `any` or `0..=SSH_TUNID_MAX`. `None` is `SSH_TUNID_ERR`.
fn a2tun_one(s: &str) -> Option<i32> {
    if s.eq_ignore_ascii_case("any") {
        return Some(TUNID_ANY);
    }
    strtonum(s, 0, TUNID_MAX)
        .ok()
        .and_then(|v| i32::try_from(v).ok())
}

/// `a2tun` with a remote side: `local[:remote]`. `None` is `SSH_TUNID_ERR`.
pub(crate) fn a2tun(s: &str) -> Option<(i32, i32)> {
    match s.split_once(':') {
        None => a2tun_one(s).map(|t| (t, TUNID_ANY)),
        Some((local, remote)) => {
            let remote = a2tun_one(remote)?;
            Some((a2tun_one(local)?, remote))
        }
    }
}

/// `convtime_double`: a time like `90m`, `1h30m` or `1.5s` in seconds, `None` if invalid.
pub(crate) fn convtime_double(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    let mut total = 0.0;
    let mut seen_seconds = false;
    let mut p = 0;
    while p < b.len() {
        let c = b.get(p).copied().unwrap_or_default();
        if !c.is_ascii_digit() && c != b'.' {
            return None;
        }
        let span = b
            .get(p..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == b'.')
            .count();
        let number = s.get(p..p + span).unwrap_or("");
        // strtod reads digits, one '.', digits; the whole [0-9.] run must be that number.
        if number.matches('.').count() > 1 || !number.bytes().any(|c| c.is_ascii_digit()) {
            return None;
        }
        let val: f64 = number.parse().ok()?;
        p += span;
        let unit = b.get(p).copied();
        let multiplier = match unit {
            None | Some(b's' | b'S') => {
                if seen_seconds {
                    return None;
                }
                seen_seconds = true;
                1.0
            }
            Some(b'm' | b'M') => SECS_PER_MINUTE,
            Some(b'h' | b'H') => SECS_PER_HOUR,
            Some(b'd' | b'D') => SECS_PER_DAY,
            Some(b'w' | b'W') => SECS_PER_WEEK,
            Some(_) => return None,
        };
        if number.contains('.') && (multiplier > 1.0 || number.ends_with('.')) {
            return None;
        }
        total += val * multiplier;
        if unit.is_some() {
            p += 1;
        }
    }
    Some(total)
}

/// `convtime`: [`convtime_double`] in whole seconds that fit an `int`, `None` if invalid.
pub(crate) fn convtime(s: &str) -> Option<i32> {
    let v = convtime_double(s)?;
    if v < 0.0 || v > f64::from(i32::MAX) {
        return None;
    }
    // In range: the cast truncates the fraction like the C (int) conversion.
    Some(v as i32)
}

/// `hpdelim2`: split `host[:rest]`, `[host]:rest` or `host/rest`. Returns the host part
/// (brackets kept), the delimiter and the rest. `None` is a `[` without `]` or text after
/// `]` that is not a delimiter.
pub(crate) fn hpdelim2(s: &str) -> Option<(&str, Option<char>, Option<&str>)> {
    let end = if s.starts_with('[') {
        s.find(']')? + 1
    } else {
        s.find([':', '/']).unwrap_or(s.len())
    };
    let host = s.get(..end).unwrap_or("");
    match s.as_bytes().get(end) {
        None => Some((host, None, None)),
        Some(&d @ (b':' | b'/')) => Some((host, Some(char::from(d)), s.get(end + 1..))),
        Some(_) => None,
    }
}

/// `hpdelim`: [`hpdelim2`] that accepts only `:`.
pub(crate) fn hpdelim(s: &str) -> Option<(&str, Option<&str>)> {
    let (host, delim, rest) = hpdelim2(s)?;
    if delim == Some('/') {
        return None;
    }
    Some((host, rest))
}

/// `cleanhostname`: drop the brackets of `[host]`.
pub(crate) fn cleanhostname(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// A user, host and port parsed from a destination.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct UserHostPort {
    pub(crate) user: Option<String>,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

/// `parse_user_host_port`: `[user@]host[:port]`.
pub(crate) fn parse_user_host_port(s: &str) -> Option<UserHostPort> {
    let (user, rest) = match s.rfind('@') {
        Some(at) => {
            let user = s.get(..at).unwrap_or("");
            if user.is_empty() {
                return None;
            }
            (Some(user.to_string()), s.get(at + 1..).unwrap_or(""))
        }
        None => (None, s),
    };
    let (host, port) = hpdelim(rest)?;
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(p) if !p.is_empty() => Some(a2port(p).filter(|p| *p > 0)?),
        _ => None,
    };
    Some(UserHostPort {
        user,
        host: cleanhostname(host).to_string(),
        port,
    })
}

fn urldecode(src: &str) -> Option<String> {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = src.get(i + 1..i + 3)?;
                let v = u8::from_str_radix(hex, HEX_RADIX).ok()?;
                if !hex.bytes().all(|c| c.is_ascii_hexdigit()) || v == 0 {
                    return None;
                }
                out.push(v);
                i += 2;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Hex digits are base 16 (`urldecode`).
const HEX_RADIX: u32 = 16;

/// The result of [`parse_uri`]: 1 in C is "not a URI of this scheme".
pub(crate) enum Uri {
    Other,
    Invalid,
    Parsed {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        path: Option<String>,
    },
}

/// `parse_uri`: `<scheme>://[user[;params]@]host[:port][/path]`.
pub(crate) fn parse_uri(scheme: &str, uri: &str) -> Uri {
    let Some(rest) = uri.strip_prefix(scheme).and_then(|r| r.strip_prefix("://")) else {
        return Uri::Other;
    };
    let (user, rest) = match rest.find('@') {
        Some(at) => {
            let info = rest.get(..at).unwrap_or("");
            let name = info.split(';').next().unwrap_or("");
            if name.is_empty() {
                return Uri::Invalid;
            }
            let Some(user) = urldecode(name) else {
                return Uri::Invalid;
            };
            (Some(user), rest.get(at + 1..).unwrap_or(""))
        }
        None => (None, rest),
    };
    let Some((host, delim, tail)) = hpdelim2(rest) else {
        return Uri::Invalid;
    };
    if host.is_empty() {
        return Uri::Invalid;
    }
    let host = cleanhostname(host).to_string();
    if valid_domain(&host, false).is_err() {
        return Uri::Invalid;
    }
    let mut port = None;
    let mut path = None;
    if let Some(mut tail) = tail.filter(|t| !t.is_empty()) {
        if delim == Some(':') {
            let (p, after) = match tail.split_once('/') {
                Some((p, after)) => (p, Some(after)),
                None => (tail, None),
            };
            match a2port(p).filter(|p| *p > 0) {
                Some(p) => port = Some(p),
                None => return Uri::Invalid,
            }
            tail = after.unwrap_or("");
        }
        if !tail.is_empty() {
            match urldecode(tail) {
                Some(p) => path = Some(p),
                None => return Uri::Invalid,
            }
        }
    }
    Uri::Parsed {
        user,
        host,
        port,
        path,
    }
}

/// The local account that `~` and the account tokens refer to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub(crate) user: Option<String>,
    pub(crate) home: Option<String>,
    /// Given only when the ssh folder is not `<home>/.ssh` (`embedded.ssh_dir`).
    pub(crate) ssh_dir: Option<String>,
}

/// Why `~` could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TildeError {
    /// `~user` for a user other than the local account (no user database is read).
    NoSuchUser(String),
    /// The home folder is not known.
    NoHome,
}

/// `tilde_expand`: `~`, `~/path` and `~user/path`. A path that starts with `~/.ssh/` goes
/// to the configured ssh folder when [`Account::ssh_dir`] is given (docs/design/
/// ssh-backends.md "내장 백엔드가 읽는 파일").
pub(crate) fn tilde_expand(filename: &str, account: &Account) -> Result<String, TildeError> {
    let (head, rest) = tilde_split(filename, account)?;
    Ok(format!("{head}{rest}"))
}

/// [`tilde_expand`] in two parts: the folder that replaces the `~` part and the rest of
/// `filename` after it. `glob` needs them apart (`GLOB_TILDE` adds the home after quoting).
pub(crate) fn tilde_split<'a>(
    filename: &'a str,
    account: &Account,
) -> Result<(String, &'a str), TildeError> {
    let Some(copy) = filename.strip_prefix('~') else {
        return Ok((String::new(), filename));
    };
    if let (Some(dir), Some(rest)) = (&account.ssh_dir, filename.strip_prefix("~/.ssh/")) {
        let slash = if dir.ends_with('/') { "" } else { "/" };
        return Ok((format!("{dir}{slash}"), rest));
    }
    let (user, path) = if copy.is_empty() {
        (None, None)
    } else if let Some(p) = copy.strip_prefix('/') {
        let p = p.trim_start_matches('/');
        (None, (!p.is_empty()).then_some(p))
    } else {
        match copy.split_once('/') {
            None => (Some(copy), None),
            Some((u, p)) => {
                let p = p.trim_start_matches('/');
                (Some(u), (!p.is_empty()).then_some(p))
            }
        }
    };
    if let Some(u) = user
        && account.user.as_deref() != Some(u)
    {
        return Err(TildeError::NoSuchUser(u.to_string()));
    }
    let home = account.home.as_deref().ok_or(TildeError::NoHome)?;
    let slash = if home.is_empty() || !home.ends_with('/') {
        "/"
    } else {
        ""
    };
    Ok((format!("{home}{slash}"), path.unwrap_or("")))
}

/// Why `%` or `${}` expansion failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExpandError {
    /// A malformed string (`%` at the end, an unknown `%x`, `${` without `}`, `${}`). OpenSSH
    /// stops with a fatal error for these.
    Invalid(String),
    /// `${NAME}` with no value. OpenSSH returns `NULL` without a parse error.
    MissingVariable(String),
    /// A token whose value the caller did not give (for example `%l` without the local host
    /// name).
    Unavailable(char),
}

/// One `%` token and its value. `None` is a value this crate does not know.
pub(crate) type Token = (char, Option<String>);

/// `vdollar_percent_expand`: `${NAME}` from `env` when `dollar`, and `%x` from `tokens`
/// when given. `%%` is `%`.
pub(crate) fn expand(
    s: &str,
    dollar: bool,
    tokens: Option<&[Token]>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(s.len());
    let mut missing: Option<String> = None;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if dollar && c == '$' && s.get(i + 1..).is_some_and(|r| r.starts_with('{')) {
            let body = s.get(i + 2..).unwrap_or("");
            let Some(end) = body.find('}') else {
                return Err(ExpandError::Invalid(format!(
                    "environment variable '{body}' missing closing '}}'"
                )));
            };
            if end == 0 {
                return Err(ExpandError::Invalid(
                    "zero-length environment variable".to_string(),
                ));
            }
            let name = body.get(..end).unwrap_or("");
            match env(name) {
                Some(v) => out.push_str(&v),
                None => {
                    missing.get_or_insert_with(|| name.to_string());
                }
            }
            let skip_to = i + 2 + end;
            while it.peek().is_some_and(|(j, _)| *j <= skip_to) {
                it.next();
            }
            continue;
        }
        let Some(tokens) = tokens.filter(|_| c == '%') else {
            out.push(c);
            continue;
        };
        let Some((_, key)) = it.next() else {
            return Err(ExpandError::Invalid("invalid format".to_string()));
        };
        if key == '%' {
            out.push('%');
            continue;
        }
        match tokens.iter().find(|(k, _)| *k == key) {
            Some((_, Some(v))) => out.push_str(v),
            Some((_, None)) => return Err(ExpandError::Unavailable(key)),
            None => return Err(ExpandError::Invalid(format!("unknown key %{key}"))),
        }
    }
    match missing {
        Some(name) => Err(ExpandError::MissingVariable(name)),
        None => Ok(out),
    }
}

/// `valid_domain`: a domain name that starts with a letter, digit or `_` and has only
/// letters, digits, `-`, `_` and single dots. Returns the name with a trailing dot removed,
/// lower-cased when `makelower`.
pub(crate) fn valid_domain(name: &str, makelower: bool) -> Result<String, String> {
    let Some(&first) = name.as_bytes().first() else {
        return Err("empty domain name".to_string());
    };
    if !first.is_ascii_alphanumeric() && first != b'_' {
        return Err(format!(
            "domain name \"{name}\" starts with invalid character"
        ));
    }
    let mut last = None;
    for c in name.bytes().map(|c| c.to_ascii_lowercase()) {
        if last == Some(b'.') && c == b'.' {
            return Err(format!(
                "domain name \"{name}\" contains consecutive separators"
            ));
        }
        if c != b'.' && c != b'-' && !c.is_ascii_alphanumeric() && c != b'_' {
            return Err(format!(
                "domain name \"{name}\" contains invalid characters"
            ));
        }
        last = Some(c);
    }
    let out = if makelower {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    };
    Ok(out.strip_suffix('.').map(str::to_string).unwrap_or(out))
}

/// The address families `getaddrinfo` is asked for (`AddressFamily`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Any,
    Inet,
    Inet6,
}

/// The C library whose `getaddrinfo(AI_NUMERICHOST)` rule decides which names are numeric
/// addresses. `ssh` asks the library of the OS it runs on, and the libraries differ
/// (docs/design/ssh-backends.md "숫자 주소"). The rules are read from the sources of each
/// library; the code is written for this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Libc {
    /// The GNU C Library (Linux): `text_to_binary_address` of `nss/getaddrinfo.c` takes the
    /// `inet_aton` forms (`__inet_aton_exact`), then `inet_pton(AF_INET6)`; asked for IPv4, an
    /// IPv4-mapped IPv6 address is that IPv4 address.
    Glibc,
    /// Android bionic: `explore_numeric` of `libc/dns/net/getaddrinfo.c` takes only
    /// `inet_pton` of the families asked for (its `inet_aton` branch is `#if 0`), and the
    /// OpenBSD `inet_pton` reads a part with leading zeros as decimal.
    Bionic,
    /// Apple Libinfo and Libc (macOS, iOS): `_gai_numerichost` of `si_getaddrinfo.c` takes
    /// `inet_pton(AF_INET)` (leading zeros are decimal), then `_inet_aton_check(strict)` for
    /// the shorter forms, then `inet_pton(AF_INET6)`; asked for one family it refuses an
    /// address of the other.
    Darwin,
}

impl Libc {
    /// Every library with a rule.
    pub(crate) const ALL: &[Libc] = &[Libc::Glibc, Libc::Bionic, Libc::Darwin];

    /// The library of the OS this crate is built for, `None` where this crate has no rule
    /// (Windows, musl, the BSDs).
    pub(crate) const fn current() -> Option<Libc> {
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            Some(Libc::Glibc)
        } else if cfg!(target_os = "android") {
            Some(Libc::Bionic)
        } else if cfg!(target_vendor = "apple") {
            Some(Libc::Darwin)
        } else {
            None
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Libc::Glibc => "glibc",
            Libc::Bionic => "bionic",
            Libc::Darwin => "Darwin",
        }
    }
}

/// How a C library reads a host name with `getaddrinfo(AI_NUMERICHOST)` and writes it back
/// with `getnameinfo(NI_NUMERICHOST)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// A numeric address.
    Address(IpAddr),
    /// Not a numeric address: `ssh` keeps the name as written.
    Name,
    /// Darwin: an IPv6 link-local address, or a node- or link-local multicast address, whose
    /// second word is not zero. Libinfo takes that word as the interface index without a `%`
    /// (`si_addrinfo_v6`), and `getnameinfo` writes the address with the name of that
    /// interface or fails when there is none, so the text depends on the interfaces. The
    /// address is the one `getaddrinfo` gives: a link-local address with its second word
    /// cleared, a multicast address as written. `addr_pton` judges a list entry with it
    /// and needs no interface.
    Scoped(Ipv6Addr),
}

impl Reading {
    fn text(self) -> String {
        match self {
            Reading::Address(a) => numeric_text(a),
            Reading::Name => "not an address".to_string(),
            Reading::Scoped(_) => "an address scoped by its second word".to_string(),
        }
    }
}

/// The readings of `name` by the C library of this OS, or by every library with a rule on
/// an OS without one ([`Libc::current`] is `None`).
pub(crate) fn readings(name: &str, family: Family) -> Vec<(Libc, Reading)> {
    let libcs = match Libc::current() {
        Some(l) => vec![l],
        None => Libc::ALL.to_vec(),
    };
    libcs
        .into_iter()
        .map(|l| (l, numeric_host_of(l, name, family)))
        .collect()
}

/// The address the readings settle on (`None`: not an address), or `None` when they differ
/// or one depends on the interfaces.
pub(crate) fn settled(readings: &[(Libc, Reading)]) -> Option<Option<IpAddr>> {
    let ((_, first), rest) = readings.split_first()?;
    if matches!(first, Reading::Scoped(_)) || rest.iter().any(|(_, r)| r != first) {
        return None;
    }
    Some(match first {
        Reading::Address(a) => Some(*a),
        _ => None,
    })
}

/// Why the readings of `name` do not settle.
pub(crate) fn unsettled(name: &str, readings: &[(Libc, Reading)]) -> String {
    let each: Vec<String> = readings
        .iter()
        .map(|(l, r)| format!("{} {}", l.name(), r.text()))
        .collect();
    let how = match readings {
        [(l, r)] => format!("{} reads \"{name}\" as {}", l.name(), r.text()),
        _ => format!(
            "the C libraries read \"{name}\" differently ({})",
            each.join(", ")
        ),
    };
    format!("{how}, and this crate cannot tell the address ssh uses on this OS")
}

/// `getaddrinfo(name, ..., AI_NUMERICHOST)` of the C library of this OS: the address `name`
/// is in numeric form, or `None`. A scope (`%`) is not taken. On an OS without a rule the
/// answer is the one every library gives. Where the libraries differ, or the reading
/// depends on the interfaces, it is `Err` with the reason, and the caller refuses.
pub(crate) fn numeric_host(name: &str, family: Family) -> Result<Option<IpAddr>, String> {
    let r = readings(name, family);
    settled(&r).ok_or_else(|| unsettled(name, &r))
}

/// The scope field of a multicast address (the low four bits of the first word).
const MC_SCOPE_MASK: u16 = 0x000f;
/// Multicast scope node-local (`__IPV6_ADDR_SCOPE_NODELOCAL`).
const MC_SCOPE_NODE_LOCAL: u16 = 0x1;
/// Multicast scope link-local (`__IPV6_ADDR_SCOPE_LINKLOCAL`).
const MC_SCOPE_LINK_LOCAL: u16 = 0x2;
/// The flags field of a multicast address (the next four bits of the first word).
const MC_FLAGS_MASK: u16 = 0x00f0;
/// The flags of a unicast-prefix-based multicast address
/// (`IPV6_ADDR_MC_FLAGS_UNICAST_BASED`, transient and prefix).
const MC_FLAGS_UNICAST_BASED: u16 = 0x0030;

/// Whether Libinfo takes the second word of `a` as an interface index: `si_addrinfo_v6`
/// moves it out of a link-local address, and `getnameinfo` reads it from addresses that
/// pass `IN6_IS_ADDR_LINKLOCAL`, `IN6_IS_ADDR_MC_NODELOCAL` or `IN6_IS_ADDR_MC_LINKLOCAL`.
/// The user-space macros of xnu `bsd/netinet6/in6.h` leave out of the link-local multicast
/// test a unicast-prefix-based address (flags `0x30`, `ff32:40:fe80::1234`), whose second
/// word is the prefix length; the node-local test has no flags condition.
///
/// The address `getaddrinfo` gives for such an `a` ([`Reading::Scoped`]), or `None`.
fn darwin_scoped(a: Ipv6Addr) -> Option<Ipv6Addr> {
    let mut words = a.segments();
    let [w0, w1, ..] = words;
    if w1 == 0 {
        return None;
    }
    if a.is_unicast_link_local() {
        // `si_addrinfo_v6` clears the word it took as the interface.
        if let Some(w) = words.get_mut(1) {
            *w = 0;
        }
        return Some(Ipv6Addr::from(words));
    }
    let scope = w0 & MC_SCOPE_MASK;
    let node_local = a.is_multicast() && scope == MC_SCOPE_NODE_LOCAL;
    let link_local = a.is_multicast()
        && w0 & MC_FLAGS_MASK != MC_FLAGS_UNICAST_BASED
        && scope == MC_SCOPE_LINK_LOCAL;
    (node_local || link_local).then_some(a)
}

/// How `libc` reads `name` ([`numeric_host`]).
pub(crate) fn numeric_host_of(libc: Libc, name: &str, family: Family) -> Reading {
    let v4 = match libc {
        Libc::Glibc => inet_aton_glibc(name),
        Libc::Bionic => inet_pton4(name, LeadingZero::Decimal),
        Libc::Darwin => inet_pton4(name, LeadingZero::Decimal).or_else(|| inet_aton_darwin(name)),
    };
    if let Some(a) = v4 {
        // `ssh` does not pass `AI_V4MAPPED`: asked for IPv6, an IPv4 address is refused
        // (glibc `EAI_ADDRFAMILY`, Darwin `-1`); bionic does not read IPv4 for IPv6 at all.
        return if family == Family::Inet6 {
            Reading::Name
        } else {
            Reading::Address(IpAddr::V4(a))
        };
    }
    let Some(a) = inet_pton6(name, libc) else {
        return Reading::Name;
    };
    match family {
        Family::Any | Family::Inet6 => match darwin_scoped(a) {
            Some(given) if libc == Libc::Darwin => Reading::Scoped(given),
            _ => Reading::Address(IpAddr::V6(a)),
        },
        // Only glibc takes an IPv4-mapped IPv6 address for IPv4. Darwin returns `-1`
        // (`EAI_NONAME`), bionic reads only IPv4 for IPv4.
        Family::Inet if libc == Libc::Glibc => a
            .to_ipv4_mapped()
            .map_or(Reading::Name, |v4| Reading::Address(IpAddr::V4(v4))),
        Family::Inet => Reading::Name,
    }
}

/// The zero 16-bit words that start an IPv4-compatible IPv6 address (`::a.b.c.d`).
const V4_COMPAT_ZERO_WORDS: usize = 6;

/// `getnameinfo(NI_NUMERICHOST)`: the `inet_ntop(3)` text of an address. It differs from
/// the Rust form in one place: an IPv6 address whose first six words are zero and whose
/// seventh is not is written with the last 32 bits dotted (`::192.0.2.1`). glibc, bionic
/// (OpenBSD) and Apple Libc `inet_ntop6` give the same text: the Apple test for seven zero
/// words cannot be reached (the seventh word is inside the zero run).
pub(crate) fn numeric_text(a: IpAddr) -> String {
    if let IpAddr::V6(v6) = a {
        let words = v6.segments();
        let (head, tail) = words.split_at(V4_COMPAT_ZERO_WORDS);
        if head.iter().all(|w| *w == 0) && tail.first().is_some_and(|w| *w != 0) {
            let [.., b0, b1, b2, b3] = v6.octets();
            return format!("::{}", Ipv4Addr::new(b0, b1, b2, b3));
        }
    }
    a.to_string()
}

/// The largest value of the last part of an `inet_aton` address with 1 to 4 parts.
const INET_ATON_LAST_MAX: [u32; 4] = [u32::MAX, 0x00ff_ffff, 0xffff, 0xff];
/// The largest value of a part before the last.
const INET_ATON_PART_MAX: u32 = 0xff;
const OCTAL_RADIX: u32 = 8;
const DECIMAL_RADIX: u32 = 10;
/// The bits of one byte of an IPv4 address.
const BITS_PER_PART: usize = 8;
/// The bits of one hexadecimal digit.
const BITS_PER_HEX_DIGIT: u32 = 4;
/// glibc and bionic `inet_pton6` take at most four hexadecimal digits in a group.
const GROUP_DIGITS_MAX: usize = 4;
/// The largest value of a 16-bit IPv6 group.
const GROUP_MAX: u32 = 0xffff;

/// How `inet_pton4` reads a part with leading zeros.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeadingZero {
    /// As decimal (OpenBSD and Apple `inet_pton4`).
    Decimal,
    /// Refused (glibc `inet_pton4`).
    Refused,
}

/// `inet_pton(AF_INET)` of the whole string: four decimal parts of at most 255.
fn inet_pton4(s: &str, leading_zero: LeadingZero) -> Option<Ipv4Addr> {
    let mut out = Ipv4Addr::UNSPECIFIED.octets();
    let mut parts = s.split('.');
    for byte in &mut out {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if leading_zero == LeadingZero::Refused && part.len() > 1 && part.starts_with('0') {
            return None;
        }
        // Leading zeros keep the value small; a long part is refused digit by digit.
        let mut v: u32 = 0;
        for c in part.bytes() {
            v = v * DECIMAL_RADIX + u32::from(c - b'0');
            if v > INET_ATON_PART_MAX {
                return None;
            }
        }
        *byte = u8::try_from(v).ok()?;
    }
    parts.next().is_none().then(|| Ipv4Addr::from(out))
}

/// glibc `__inet_aton_exact` (`resolv/inet_addr.c`): `a.b.c.d`, `a.b.c`, `a.b` or `a`, each
/// part read by `strtoul(.., 0)` (decimal, `0x` hexadecimal or `0` octal) and the whole
/// value within 32 bits.
fn inet_aton_glibc(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    let last_max = *INET_ATON_LAST_MAX.get(parts.len().wrapping_sub(1))?;
    let mut value: u32 = 0;
    for (i, part) in parts.iter().enumerate() {
        let (digits, radix) = if let Some(h) = part.strip_prefix("0x").or(part.strip_prefix("0X")) {
            (h, HEX_RADIX)
        } else if part.len() > 1 && part.starts_with('0') {
            (part.get(1..).unwrap_or(""), OCTAL_RADIX)
        } else {
            (*part, DECIMAL_RADIX)
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let v = u32::from_str_radix(digits, radix).ok()?;
        if i + 1 < parts.len() {
            if v > INET_ATON_PART_MAX {
                return None;
            }
            // Parts before the last are the leading bytes.
            let shift = BITS_PER_PART * (INET_ATON_LAST_MAX.len() - 1 - i);
            value |= v << shift;
        } else {
            if v > last_max {
                return None;
            }
            value |= v;
        }
    }
    Some(Ipv4Addr::from(value))
}

/// Apple `_inet_aton_check(cp, addr, 1)` (Libc `net/FreeBSD/inet_addr.c`): the parts of
/// `inet_aton`, where it differs from glibc in three places. A part is read into a 64-bit
/// `u_long` that wraps, a one-part address keeps the low 32 bits of it, and a `0x` with no
/// digit before a `.` is a zero part.
fn inet_aton_darwin(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    let last_max = *INET_ATON_LAST_MAX.get(parts.len().wrapping_sub(1))?;
    let mut value: u32 = 0;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if !part.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let (digits, radix) = if let Some(h) = part.strip_prefix("0x").or(part.strip_prefix("0X")) {
            (h, HEX_RADIX)
        } else if let Some(o) = part.strip_prefix('0') {
            (o, OCTAL_RADIX)
        } else {
            (*part, DECIMAL_RADIX)
        };
        // `0x` alone is no digit: refused as the last part, zero before a `.`.
        if radix == HEX_RADIX && digits.is_empty() && last {
            return None;
        }
        let mut v: u64 = 0;
        for c in digits.chars() {
            let d = c.to_digit(radix)?;
            v = v.wrapping_mul(u64::from(radix)).wrapping_add(u64::from(d));
        }
        if !last {
            let byte = u32::try_from(v).ok().filter(|b| *b <= INET_ATON_PART_MAX)?;
            let shift = BITS_PER_PART * (INET_ATON_LAST_MAX.len() - 1 - i);
            value |= byte << shift;
        } else if parts.len() == 1 {
            // `htonl(val)` keeps the low 32 bits.
            let [b0, b1, b2, b3, ..] = v.to_le_bytes();
            value = u32::from_le_bytes([b0, b1, b2, b3]);
        } else {
            value |= u32::try_from(v).ok().filter(|l| *l <= last_max)?;
        }
    }
    Some(Ipv4Addr::from(value))
}

/// `inet_pton(AF_INET6)` of `libc`, without a scope: groups of hexadecimal digits split by
/// `:`, one `::` for one or more zero groups, and a dotted IPv4 tail for the last two groups.
/// glibc and bionic take at most four digits in a group, Apple any number whose value fits
/// in 16 bits; the tail is read by the `inet_pton4` of that library (glibc refuses leading
/// zeros there), after its first part was counted as group digits.
fn inet_pton6(s: &str, libc: Libc) -> Option<Ipv6Addr> {
    // The dotted tail is the last piece; the colon before it is not part of a group.
    let (body, v4) = match s.rsplit_once(':') {
        Some((_, last)) if last.contains('.') => {
            let leading_zero = if libc == Libc::Glibc {
                LeadingZero::Refused
            } else {
                LeadingZero::Decimal
            };
            // The digits before the first `.` are read as a group first, so glibc and bionic
            // stop at a fifth digit there; Apple only needs the value as hexadecimal to fit in
            // 16 bits, which every part of at most 255 does.
            let first = last.split('.').next().unwrap_or("");
            if libc != Libc::Darwin && first.len() > GROUP_DIGITS_MAX {
                return None;
            }
            let a = inet_pton4(last, leading_zero)?;
            let body = s.get(..s.len() - last.len())?;
            let body = if body.ends_with("::") {
                body
            } else {
                body.strip_suffix(':')?
            };
            (body, Some(a))
        }
        _ => (s, None),
    };
    let groups = |t: &str| -> Option<Vec<u16>> {
        if t.is_empty() {
            return Some(Vec::new());
        }
        t.split(':')
            .map(|piece| {
                if piece.is_empty() || (libc != Libc::Darwin && piece.len() > GROUP_DIGITS_MAX) {
                    return None;
                }
                let mut v: u32 = 0;
                for c in piece.chars() {
                    v = (v << BITS_PER_HEX_DIGIT) | c.to_digit(HEX_RADIX)?;
                    if v > GROUP_MAX {
                        return None;
                    }
                }
                u16::try_from(v).ok()
            })
            .collect()
    };
    let (mut head, mut tail, gap) = match body.split_once("::") {
        Some((h, t)) => (groups(h)?, groups(t)?, true),
        None => (groups(body)?, Vec::new(), false),
    };
    if let Some(a) = v4 {
        let [b0, b1, b2, b3] = a.octets();
        let end = if gap { &mut tail } else { &mut head };
        end.push(u16::from_be_bytes([b0, b1]));
        end.push(u16::from_be_bytes([b2, b3]));
    }
    let words = Ipv6Addr::UNSPECIFIED.segments().len();
    if gap {
        // `::` stands for at least one group.
        if head.len() + tail.len() >= words {
            return None;
        }
        head.resize(words - tail.len(), 0);
        head.extend(tail);
    }
    if head.len() != words {
        return None;
    }
    let mut out = Ipv6Addr::UNSPECIFIED.octets();
    for (bytes, g) in out.chunks_exact_mut(std::mem::size_of::<u16>()).zip(&head) {
        bytes.copy_from_slice(&g.to_be_bytes());
    }
    Some(Ipv6Addr::from(out))
}

/// `valid_env_name`: one or more letters, digits or `_`.
pub(crate) fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// `parse_pattern_interval`: `type=time` with a non-empty type.
pub(crate) fn parse_pattern_interval(s: &str) -> bool {
    match s.split_once('=') {
        Some((kind, time)) => !kind.is_empty() && convtime_double(time).is_some(),
        None => false,
    }
}

/// `lookup_setenv_in_list`: whether `NAME=` of `env` is already in `envs`.
pub(crate) fn setenv_in_list(env: &str, envs: &[String]) -> bool {
    let Some((name, _)) = env.split_once('=') else {
        return false;
    };
    envs.iter().any(|e| {
        e.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

/// A parsed `IPQoS` value: `none` (`INT_MAX`) or a DSCP/TOS value.
pub(crate) const IPQOS_NONE: i32 = i32::MAX;
/// The deprecated names `lowdelay`, `throughput`, `reliability` (`INT_MIN`).
pub(crate) const IPQOS_DEPRECATED: i32 = i32::MIN;
/// The largest numeric `IPQoS` value (`strtonum(cp, 0, 255)`).
const IPQOS_MAX: i64 = 255;

/// `ipqos[]` of `misc.c` with the `IPTOS_DSCP_*` values of `defines.h`.
const IPQOS: &[(&str, i32)] = &[
    ("none", IPQOS_NONE),
    ("af11", 0x28),
    ("af12", 0x30),
    ("af13", 0x38),
    ("af21", 0x48),
    ("af22", 0x50),
    ("af23", 0x58),
    ("af31", 0x68),
    ("af32", 0x70),
    ("af33", 0x78),
    ("af41", 0x88),
    ("af42", 0x90),
    ("af43", 0x98),
    ("cs0", 0x00),
    ("cs1", 0x20),
    ("cs2", 0x40),
    ("cs3", 0x60),
    ("cs4", 0x80),
    ("cs5", 0xa0),
    ("cs6", 0xc0),
    ("cs7", 0xe0),
    ("ef", 0xb8),
    ("le", 0x04),
    ("va", 0xb0),
    ("lowdelay", IPQOS_DEPRECATED),
    ("throughput", IPQOS_DEPRECATED),
    ("reliability", IPQOS_DEPRECATED),
];

/// `IPTOS_DSCP_EF`, the default interactive `IPQoS` (`fill_default_options`).
pub(crate) const IPTOS_DSCP_EF: i32 = 0xb8;
/// `IPTOS_DSCP_CS0`, the default bulk `IPQoS` (`fill_default_options`).
pub(crate) const IPTOS_DSCP_CS0: i32 = 0x00;

/// `parse_ipqos`: a name of [`IPQOS`] or `0..=255`.
pub(crate) fn parse_ipqos(s: Option<&str>) -> Option<i32> {
    let s = s?;
    if let Some((_, v)) = IPQOS.iter().find(|(n, _)| n.eq_ignore_ascii_case(s)) {
        return Some(*v);
    }
    strtonum(s, 0, IPQOS_MAX)
        .ok()
        .and_then(|v| i32::try_from(v).ok())
}

/// `iptos2str`: the first name of the value, or `0x%02x`.
pub(crate) fn iptos2str(v: i32) -> String {
    match IPQOS.iter().find(|(_, x)| *x == v) {
        Some((n, _)) => (*n).to_string(),
        None => format!("0x{v:02x}"),
    }
}

/// `match_pattern` style test whether a path is absolute (`path_absolute`).
pub(crate) fn path_absolute(p: &str) -> bool {
    p.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delim_all(line: &str) -> Vec<Option<String>> {
        let mut rest = Some(line.to_string());
        let mut out = Vec::new();
        while rest.is_some() {
            out.push(strdelim(&mut rest));
        }
        out
    }

    #[test]
    fn strdelim_splits_on_space_or_one_equals() {
        assert_eq!(
            delim_all("Host=foo"),
            vec![Some("Host".to_string()), Some("foo".to_string())]
        );
        let mut rest = Some("Port = 22".to_string());
        assert_eq!(strdelim(&mut rest).as_deref(), Some("Port"));
        assert_eq!(rest.as_deref(), Some("22"));
        let mut rest = Some("Port==22".to_string());
        assert_eq!(strdelim(&mut rest).as_deref(), Some("Port"));
        assert_eq!(rest.as_deref(), Some("=22"));
        let mut rest = Some("\"Ho st\" x".to_string());
        assert_eq!(strdelim(&mut rest).as_deref(), Some("Ho st"));
        let mut rest = Some("\"Host x".to_string());
        assert_eq!(strdelim(&mut rest), None);
        assert_eq!(rest, None);
    }

    #[test]
    fn argv_split_quotes_and_escapes() {
        assert_eq!(
            argv_split("a \"b c\" 'd e' f\\ g # x", true).unwrap(),
            vec!["a", "b c", "d e", "f g"]
        );
        assert_eq!(argv_split("a\\x", false).unwrap(), vec!["a\\x"]);
        assert!(argv_split("\"open", false).is_err());
        assert_eq!(argv_split("  ", false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn numbers_and_times() {
        assert_eq!(strtonum(" +22", 0, 100), Ok(22));
        assert_eq!(strtonum("22x", 0, 100), Err("invalid"));
        assert_eq!(strtonum("101", 0, 100), Err("too large"));
        assert_eq!(a2port("65536"), None);
        assert_eq!(convtime("1h30m"), Some(5400));
        assert_eq!(convtime("1.5"), Some(1));
        assert_eq!(convtime("1.5m"), None);
        assert_eq!(convtime("1s2s"), None);
        assert_eq!(convtime("1."), None);
        assert_eq!(convtime("."), None);
        assert_eq!(a2tun("1:any"), Some((1, TUNID_ANY)));
    }

    #[test]
    fn expansion() {
        let env = |n: &str| (n == "H").then(|| "/h".to_string());
        let toks: Vec<Token> = vec![('h', Some("host".to_string())), ('l', None)];
        assert_eq!(
            expand("${H}/%h/%%", true, Some(&toks), &env).unwrap(),
            "/h/host/%"
        );
        assert_eq!(
            expand("%l", false, Some(&toks), &env),
            Err(ExpandError::Unavailable('l'))
        );
        assert!(matches!(
            expand("%q", false, Some(&toks), &env),
            Err(ExpandError::Invalid(_))
        ));
        assert_eq!(
            expand("${X}", true, None, &env),
            Err(ExpandError::MissingVariable("X".to_string()))
        );
        assert_eq!(expand("%h", true, None, &env).unwrap(), "%h");
    }

    #[test]
    fn tilde() {
        let a = Account {
            user: Some("me".to_string()),
            home: Some("/home/me".to_string()),
            ssh_dir: None,
        };
        assert_eq!(tilde_expand("~", &a).unwrap(), "/home/me/");
        assert_eq!(tilde_expand("~//x", &a).unwrap(), "/home/me/x");
        assert_eq!(tilde_expand("~me/x", &a).unwrap(), "/home/me/x");
        assert_eq!(
            tilde_expand("~other/x", &a),
            Err(TildeError::NoSuchUser("other".to_string()))
        );
        let b = Account {
            ssh_dir: Some("/app/ssh".to_string()),
            ..a
        };
        assert_eq!(tilde_expand("~/.ssh/id", &b).unwrap(), "/app/ssh/id");
        assert_eq!(tilde_expand("~/x", &b).unwrap(), "/home/me/x");
    }

    #[test]
    fn hosts_and_uris() {
        let u = parse_user_host_port("u@[::1]:2222").unwrap();
        assert_eq!(
            (u.user.as_deref(), u.host.as_str(), u.port),
            (Some("u"), "::1", Some(2222))
        );
        assert!(parse_user_host_port("@h").is_none());
        assert!(parse_user_host_port("h:0").is_none());
        match parse_uri("ssh", "ssh://u%41@host.:22") {
            Uri::Parsed {
                user,
                host,
                port,
                path,
            } => {
                assert_eq!(user.as_deref(), Some("uA"));
                assert_eq!(host, "host.");
                assert_eq!(port, Some(22));
                assert_eq!(path, None);
            }
            _ => panic!("not parsed"),
        }
        assert!(matches!(parse_uri("ssh", "host"), Uri::Other));
        assert!(matches!(parse_uri("ssh", "ssh://h/p"), Uri::Parsed { .. }));
    }

    #[test]
    fn domains_and_qos() {
        assert_eq!(valid_domain("Ex.COM.", true).unwrap(), "ex.com");
        assert!(valid_domain("a..b", false).is_err());
        assert_eq!(parse_ipqos(Some("AF21")), Some(0x48));
        assert_eq!(iptos2str(0x48), "af21");
        assert_eq!(iptos2str(7), "0x07");
        assert!(setenv_in_list("A=2", &["A=1".to_string()]));
        assert!(!setenv_in_list("A=2", &["AB=1".to_string()]));
    }
}
