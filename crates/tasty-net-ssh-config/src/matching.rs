// This file is derived from OpenSSH, modified: a Rust port of parts of match.c
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// match.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Simple pattern matching, with '*' and '?' as wildcards.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2026 Damien Miller.  All rights reserved.
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
//! Patterns of OpenSSH `match.c`: `*`/`?` wildcards, comma lists with `!` negation, and
//! the list filters and intersection that algorithm lists use.

/// The buffer of one subpattern in `match_pattern_list` (`char sub[1024]`). A subpattern of
/// this length or longer gives "no match".
const SUBPATTERN_BUFFER: usize = 1024;
/// `MAX_PROP` of `match_list`: the server list is read up to this many names.
const MAX_PROPOSALS: usize = 40;

/// `match_pattern`: whether `s` matches `pattern`, where `*` matches any run and `?` any
/// one byte. The match is on bytes and case-sensitive.
pub(crate) fn match_pattern(s: &str, pattern: &str) -> bool {
    match_pattern_bytes(s.as_bytes(), pattern.as_bytes())
}

/// [`match_pattern`] on bytes, as the C function sees them.
fn match_pattern_bytes(s: &[u8], p: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    let closure = |states: &mut Vec<bool>| {
        for i in 0..p.len() {
            if states.get(i).copied().unwrap_or_default()
                && p.get(i) == Some(&b'*')
                && let Some(next) = states.get_mut(i + 1)
            {
                *next = true;
            }
        }
    };
    let mut states = vec![false; p.len() + 1];
    if let Some(first) = states.first_mut() {
        *first = true;
    }
    closure(&mut states);
    for &c in s {
        let mut next = vec![false; p.len() + 1];
        for (i, pc) in p.iter().enumerate() {
            if !states.get(i).copied().unwrap_or_default() {
                continue;
            }
            let target = if *pc == b'*' {
                Some(i)
            } else if *pc == b'?' || *pc == c {
                Some(i + 1)
            } else {
                None
            };
            if let Some(slot) = target.and_then(|t| next.get_mut(t)) {
                *slot = true;
            }
        }
        closure(&mut next);
        states = next;
        if !states.iter().any(|s| *s) {
            return false;
        }
    }
    states.last().copied().unwrap_or_default()
}

/// The result of [`match_pattern_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListMatch {
    /// A `!` subpattern matched (`-1`).
    Negated,
    /// No subpattern matched (`0`).
    None,
    /// A subpattern matched and no `!` subpattern did (`1`).
    Positive,
}

/// `match_pattern_list`: match `s` against a comma list of subpatterns, each maybe preceded
/// by `!`. With `dolower` the subpatterns are lower-cased first (not `s`).
pub(crate) fn match_pattern_list(s: &str, pattern: &str, dolower: bool) -> ListMatch {
    match_pattern_list_bytes(s.as_bytes(), pattern.as_bytes(), dolower)
}

/// [`match_pattern_list`] on bytes: a pattern that is not UTF-8 (a known_hosts host field) is
/// matched byte for byte, as the C function matches it.
fn match_pattern_list_bytes(s: &[u8], b: &[u8], dolower: bool) -> ListMatch {
    let mut got_positive = false;
    let mut i = 0;
    while i < b.len() {
        let negated = b.get(i) == Some(&b'!');
        if negated {
            i += 1;
        }
        let start = i;
        while i < b.len() && i - start < SUBPATTERN_BUFFER - 1 && b.get(i) != Some(&b',') {
            i += 1;
        }
        if i - start >= SUBPATTERN_BUFFER - 1 {
            return ListMatch::None;
        }
        let raw = b.get(start..i).unwrap_or_default();
        if b.get(i) == Some(&b',') {
            i += 1;
        }
        let sub: Vec<u8> = if dolower {
            raw.to_ascii_lowercase()
        } else {
            raw.to_vec()
        };
        if match_pattern_bytes(s, &sub) {
            if negated {
                return ListMatch::Negated;
            }
            got_positive = true;
        }
    }
    if got_positive {
        ListMatch::Positive
    } else {
        ListMatch::None
    }
}

/// `match_hostname`: [`match_pattern_list`] with the host and patterns lower-cased.
pub(crate) fn match_hostname(host: &str, pattern: &str) -> ListMatch {
    match_hostname_bytes(host, pattern.as_bytes())
}

/// [`match_hostname`] with the patterns as bytes (a known_hosts host field, which need not be
/// UTF-8).
pub(crate) fn match_hostname_bytes(host: &str, pattern: &[u8]) -> ListMatch {
    match_pattern_list_bytes(host.to_ascii_lowercase().as_bytes(), pattern, true)
}

/// `match_list`: the first name of `client` that is also in `server`. Both lists stop at
/// their first empty name; `server` is read up to [`MAX_PROPOSALS`] names.
pub(crate) fn match_list(client: &str, server: &str) -> Option<String> {
    let server: Vec<&str> = server
        .split(',')
        .take_while(|p| !p.is_empty())
        .take(MAX_PROPOSALS)
        .collect();
    client
        .split(',')
        .take_while(|p| !p.is_empty())
        .find(|p| server.contains(p))
        .map(str::to_string)
}

fn filter_list(proposal: &str, filter: &str, denylist: bool) -> String {
    let kept: Vec<&str> = proposal
        .split(',')
        .filter(|cp| {
            let positive = match_pattern_list(cp, filter, false) == ListMatch::Positive;
            positive != denylist
        })
        .collect();
    // `strlcat` appends ",name" only after a non-empty result, so empty names never get a
    // separator in front of them while the result is still empty.
    let mut out = String::new();
    for cp in kept {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(cp);
    }
    out
}

/// `match_filter_denylist`: the names of `proposal` that do not match `filter`.
pub(crate) fn match_filter_denylist(proposal: &str, filter: &str) -> String {
    filter_list(proposal, filter, true)
}

/// `match_filter_allowlist`: the names of `proposal` that match `filter`.
pub(crate) fn match_filter_allowlist(proposal: &str, filter: &str) -> String {
    filter_list(proposal, filter, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(match_pattern("", ""));
        assert!(!match_pattern("a", ""));
        assert!(match_pattern("", "*"));
        assert!(match_pattern("host.example", "*.example"));
        assert!(match_pattern("abc", "a?c"));
        assert!(!match_pattern("ac", "a?c"));
        assert!(match_pattern("aXbXc", "a*b*c"));
        assert!(!match_pattern("Host", "host"));
    }

    #[test]
    fn lists() {
        assert_eq!(match_pattern_list("a", "b,a", false), ListMatch::Positive);
        assert_eq!(match_pattern_list("a", "*,!a", false), ListMatch::Negated);
        assert_eq!(match_pattern_list("c", "a,b", false), ListMatch::None);
        assert_eq!(match_pattern_list("a", "A", true), ListMatch::Positive);
        assert_eq!(match_pattern_list("", "a,", false), ListMatch::None);
        assert_eq!(match_pattern_list("", "a,,b", false), ListMatch::Positive);
        assert_eq!(match_hostname("HOST", "host"), ListMatch::Positive);
        let long = "x".repeat(SUBPATTERN_BUFFER);
        assert_eq!(match_pattern_list(&long, "*", false), ListMatch::Positive);
        assert_eq!(match_pattern_list(&long, &long, false), ListMatch::None);
    }

    #[test]
    fn filters() {
        assert_eq!(match_filter_allowlist("a,b,c", "a,c"), "a,c");
        assert_eq!(match_filter_denylist("a,b,c", "b*"), "a,c");
        assert_eq!(match_list("x,b,a", "a,b").as_deref(), Some("b"));
        assert_eq!(match_list("x,,a", "a"), None);
    }
}
