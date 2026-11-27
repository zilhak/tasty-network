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

/// `getaddrinfo(name, ..., AI_NUMERICHOST)`: the address `name` is in numeric form, or
/// `None`. IPv4 takes the `inet_aton(3)` forms (`a.b.c.d`, `a.b.c`, `a.b`, `a`; each part
/// decimal, `0x` hexadecimal or `0` octal), IPv6 the `inet_pton(3)` form without a scope
/// (`%`). Asked for IPv4, an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is that IPv4
/// address, as glibc gives it. Written for this crate from those manual pages.
pub(crate) fn numeric_host(name: &str, family: Family) -> Option<std::net::IpAddr> {
    if family != Family::Inet6
        && let Some(a) = inet_aton(name)
    {
        return Some(std::net::IpAddr::V4(a));
    }
    let a = name.parse::<std::net::Ipv6Addr>().ok()?;
    match family {
        Family::Inet => a.to_ipv4_mapped().map(std::net::IpAddr::V4),
        Family::Any | Family::Inet6 => Some(std::net::IpAddr::V6(a)),
    }
}

/// The zero 16-bit words that start an IPv4-compatible IPv6 address (`::a.b.c.d`).
const V4_COMPAT_ZERO_WORDS: usize = 6;

/// `getnameinfo(NI_NUMERICHOST)`: the `inet_ntop(3)` text of an address. It differs from
/// the Rust form in one place: an IPv6 address whose first six words are zero and whose
/// seventh is not is written with the last 32 bits dotted (`::192.0.2.1`). Written for this
/// crate from the manual page.
pub(crate) fn numeric_text(a: std::net::IpAddr) -> String {
    if let std::net::IpAddr::V6(v6) = a {
        let words = v6.segments();
        let (head, tail) = words.split_at(V4_COMPAT_ZERO_WORDS);
        if head.iter().all(|w| *w == 0) && tail.first().is_some_and(|w| *w != 0) {
            let [.., b0, b1, b2, b3] = v6.octets();
            return format!("::{}", std::net::Ipv4Addr::new(b0, b1, b2, b3));
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

/// `inet_aton(3)` of the whole string.
fn inet_aton(s: &str) -> Option<std::net::Ipv4Addr> {
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
    Some(std::net::Ipv4Addr::from(value))
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
