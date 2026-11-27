// This file is derived from OpenSSH, modified: a Rust port of parts of openbsd-compat/glob.c,
// openbsd-compat/charclass.h of OpenSSH portable 10.6p2 (commit
// d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The notices of those files follow as written
// there.
//
// openbsd-compat/glob.c:
// /*
//  * Copyright (c) 1989, 1993
//  *	The Regents of the University of California.  All rights reserved.
//  *
//  * This code is derived from software contributed to Berkeley by
//  * Guido van Rossum.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  * 3. Neither the name of the University nor the names of its contributors
//  *    may be used to endorse or promote products derived from this software
//  *    without specific prior written permission.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE REGENTS AND CONTRIBUTORS ``AS IS'' AND
//  * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
//  * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
//  * ARE DISCLAIMED.  IN NO EVENT SHALL THE REGENTS OR CONTRIBUTORS BE LIABLE
//  * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
//  * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
//  * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
//  * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
//  * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
//  * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
//  * SUCH DAMAGE.
//  */
//
// openbsd-compat/charclass.h:
// /*
//  * Public domain, 2008, Todd C. Miller <Todd.Miller@courtesan.com>
//  *
//  * $OpenBSD: charclass.h,v 1.1 2008/10/01 23:04:13 millert Exp $
//  */
//! The `glob(pattern, GLOB_TILDE, NULL, &gl)` call of `Include` (`readconf.c:2115`), as
//! `openbsd-compat/glob.c` does it (portable OpenSSH builds that file on Linux and macOS,
//! whose `glob` has no `gl_matchc`): `glob` (`:179`–`:230`) quoting, `glob0` (`:472`–`:594`)
//! compiling, `glob1`–`glob3` (`:611`–`:780`) walking the path parts, `match` (`:903`–`:969`)
//! without backtracking, and `g_charclass` (`:438`–`:463`) with the classes of `charclass.h`.
//! `!` is the only negation of a set; `*` does not match a leading `.`; `.` and `..` are
//! names like others; a part without pattern characters is kept as written (so a trailing
//! `/` needs a folder); the results are sorted by bytes (`strcmp`). The flags of the call
//! leave out `GLOB_BRACE`, `GLOB_NOCHECK`, `GLOB_MARK` and the limits. For `GLOB_TILDE` the
//! caller gives the configured home, and it is put in front after quoting, where
//! `globtilde` (`:364`–`:421`) puts it.

use std::path::PathBuf;

/// `Char`: a pattern byte with the `M_QUOTE` (pattern character) and `M_PROTECT` (escaped)
/// bits.
type Char = u16;

const M_QUOTE: Char = 0x8000;
const M_PROTECT: Char = 0x4000;
const M_ASCII: Char = 0x00ff;
const EOS: Char = 0;
const DOT: Char = b'.' as Char;
const LBRACKET: Char = b'[' as Char;
const NOT: Char = b'!' as Char;
const QUESTION: Char = b'?' as Char;
const QUOTE: Char = b'\\' as Char;
const RANGE: Char = b'-' as Char;
const RBRACKET: Char = b']' as Char;
const SEP: Char = b'/' as Char;
const STAR: Char = b'*' as Char;
const COLON: Char = b':' as Char;

/// `CHAR(c)`.
const fn char_of(c: Char) -> Char {
    c & M_ASCII
}

/// `META(c)`.
const fn meta(c: u8) -> Char {
    c as Char | M_QUOTE
}

const M_ALL: Char = meta(b'*');
const M_END: Char = meta(b']');
const M_NOT: Char = meta(b'!');
const M_ONE: Char = meta(b'?');
const M_RNG: Char = meta(b'-');
const M_SET: Char = meta(b'[');
const M_CLASS: Char = meta(b':');

fn is_meta(c: Char) -> bool {
    c & M_QUOTE != 0
}

/// `PATH_MAX` of the C library: a pattern this long matches nothing and a path may not
/// reach it. 1024 on the BSD family (macOS, iOS, FreeBSD), 4096 elsewhere.
pub(crate) const PATH_MAX: usize = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)) {
    1024
} else {
    4096
};

/// The places `glob3` needs after a name: its `EOS` and `pathend_last`, which is one before
/// the end of `pathbuf[PATH_MAX]` (`glob.c:619`, `:755`-`:760`).
const NAME_ROOM: usize = 2;

/// The `isctype` test of one class.
type ClassTest = fn(u8) -> bool;

/// `cclasses[]` of `charclass.h`, with the C locale's `is*` functions.
const CLASSES: &[(&str, ClassTest)] = &[
    ("alnum", |c| c.is_ascii_alphanumeric()),
    ("alpha", |c| c.is_ascii_alphabetic()),
    ("blank", |c| c == b' ' || c == b'\t'),
    ("cntrl", |c| c.is_ascii_control()),
    ("digit", |c| c.is_ascii_digit()),
    ("graph", |c| c.is_ascii_graphic()),
    ("lower", |c| c.is_ascii_lowercase()),
    ("print", |c| c.is_ascii_graphic() || c == b' '),
    ("punct", |c| c.is_ascii_punctuation()),
    ("space", |c| c == b' ' || (b'\t'..=b'\r').contains(&c)),
    ("upper", |c| c.is_ascii_uppercase()),
    ("xdigit", |c| c.is_ascii_hexdigit()),
];

/// `glob` returned an error other than `GLOB_NOMATCH` (a path reached `PATH_MAX`):
/// `readconf.c:2121`-`:2124` stops the line with "glob failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlobFailed;

/// The paths that match `pattern`, sorted. An empty list is `GLOB_NOMATCH`. Only the
/// Unix tests use it.
#[cfg(all(test, unix))]
pub(crate) fn glob(pattern: &str) -> Result<Vec<PathBuf>, GlobFailed> {
    glob_tilde("", pattern, pattern.len())
}

/// [`glob`] of `pattern` after `home`, the folder `GLOB_TILDE` put in place of a leading
/// `~` part; `written` is the length of the pattern as written, with its `~` part. In the
/// order of the original: `glob` returns `GLOB_NOMATCH` when the written pattern reaches
/// `PATH_MAX` (`glob.c:200`) and protects the quoted characters (`:206`-`:219`), then
/// `globtilde` (`:364`-`:421`) puts the home in front, so a `\` in `home` is a plain
/// character, and cuts the result to `PATH_MAX - 1` characters without an error.
pub(crate) fn glob_tilde(
    home: &str,
    pattern: &str,
    written: usize,
) -> Result<Vec<PathBuf>, GlobFailed> {
    let bytes = pattern.as_bytes();
    if written >= PATH_MAX {
        return Ok(Vec::new());
    }
    let mut patbuf: Vec<Char> = home.bytes().map(Char::from).collect();
    // Protect the quoted characters.
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        i += 1;
        let c = Char::from(b);
        if c == QUOTE {
            match bytes.get(i) {
                Some(&n) => {
                    i += 1;
                    patbuf.push(Char::from(n) | M_PROTECT);
                }
                None => patbuf.push(QUOTE | M_PROTECT),
            }
        } else {
            patbuf.push(c);
        }
    }
    patbuf.truncate(PATH_MAX - 1);
    let Some(compiled) = glob0(&patbuf) else {
        return Ok(Vec::new());
    };
    let mut found: Vec<Vec<u8>> = Vec::new();
    if compiled.is_empty() {
        // `glob1`: a null pathname is invalid.
        return Ok(Vec::new());
    }
    glob2(&mut Vec::new(), &compiled, &mut found)?;
    found.sort();
    Ok(found.into_iter().map(path_of).collect())
}

/// `g_strchr` from `from`: whether `ch` comes before the end.
fn has_char(p: &[Char], from: usize, ch: Char) -> bool {
    p.get(from..).unwrap_or_default().contains(&ch)
}

/// `g_charclass` at `*q` (the `:` after `[`). `Some(true)`: a class was compiled and `*q`
/// moved past `:]`; `Some(false)`: not a class; `None`: an unknown class name.
fn charclass(p: &[Char], q: &mut usize, out: &mut Vec<Char>) -> Option<bool> {
    let start = *q + 1;
    let Some(colon) = p
        .get(start..)
        .unwrap_or_default()
        .iter()
        .position(|&c| c == COLON)
        .map(|k| start + k)
    else {
        return Some(false);
    };
    if p.get(colon + 1) != Some(&RBRACKET) {
        return Some(false);
    }
    let name: Vec<Char> = p.get(start..colon).unwrap_or_default().to_vec();
    let index = CLASSES.iter().position(|(n, _)| {
        n.len() == name.len() && n.bytes().zip(&name).all(|(a, b)| Char::from(a) == *b)
    })?;
    out.push(M_CLASS);
    out.push(Char::try_from(index).ok()?);
    *q = colon + 2;
    Some(true)
}

/// `glob0` without `globtilde`: compile the pattern. `None` is `GLOB_NOMATCH` (an unknown
/// character class).
fn glob0(pattern: &[Char]) -> Option<Vec<Char>> {
    let at = |k: usize| pattern.get(k).copied().unwrap_or(EOS);
    let mut out: Vec<Char> = Vec::with_capacity(pattern.len());
    let mut q = 0;
    while q < pattern.len() {
        let c = at(q);
        q += 1;
        match c {
            LBRACKET => {
                let c = at(q);
                if c == NOT {
                    q += 1;
                }
                if at(q) == EOS || !has_char(pattern, q + 1, RBRACKET) {
                    out.push(LBRACKET);
                    if c == NOT {
                        q -= 1;
                    }
                    continue;
                }
                out.push(M_SET);
                if c == NOT {
                    out.push(M_NOT);
                }
                let mut c = at(q);
                q += 1;
                loop {
                    if c == LBRACKET && at(q) == COLON {
                        let mut class = Some(true);
                        while c == LBRACKET && at(q) == COLON {
                            class = charclass(pattern, &mut q, &mut out);
                            if class != Some(true) {
                                break;
                            }
                            c = at(q);
                            q += 1;
                        }
                        class?;
                        if c == RBRACKET {
                            break;
                        }
                    }
                    out.push(char_of(c));
                    if at(q) == RANGE && at(q + 1) != RBRACKET {
                        out.push(M_RNG);
                        out.push(char_of(at(q + 1)));
                        q += 2;
                    }
                    // The C code reads past the end here when a class used up the last `]`;
                    // the set ends with the pattern instead.
                    if q >= pattern.len() {
                        break;
                    }
                    c = at(q);
                    q += 1;
                    if c == RBRACKET {
                        break;
                    }
                }
                out.push(M_END);
            }
            QUESTION => out.push(M_ONE),
            // Collapse adjacent stars to one.
            STAR => {
                if out.last() != Some(&M_ALL) {
                    out.push(M_ALL);
                }
            }
            _ => out.push(char_of(c)),
        }
    }
    Some(out)
}

/// `g_Ctoc`: the path bytes.
fn bytes_of(p: &[Char]) -> Vec<u8> {
    p.iter().map(|&c| (c & M_ASCII) as u8).collect()
}

#[cfg(unix)]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(unix)]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// `glob2`: copy path parts without pattern characters to `path`, then expand the first
/// part with them (`glob3`). At the end of the pattern the path is kept if `lstat` finds it.
fn glob2(
    path: &mut Vec<Char>,
    pattern: &[Char],
    found: &mut Vec<Vec<u8>>,
) -> Result<(), GlobFailed> {
    let mut pattern = pattern;
    loop {
        if pattern.is_empty() {
            if path_of(bytes_of(path)).symlink_metadata().is_ok() {
                found.push(bytes_of(path));
            }
            return Ok(());
        }
        // Find the end of the next part.
        let end = pattern
            .iter()
            .position(|&c| c == SEP)
            .unwrap_or(pattern.len());
        let (part, rest) = pattern.split_at(end);
        if path.len() + part.len() >= PATH_MAX {
            return Err(GlobFailed);
        }
        if part.iter().any(|&c| is_meta(c)) {
            return glob3(path, part, rest, found);
        }
        path.extend_from_slice(part);
        let seps = rest.iter().take_while(|&&c| c == SEP).count();
        if path.len() + seps >= PATH_MAX {
            return Err(GlobFailed);
        }
        path.extend_from_slice(rest.get(..seps).unwrap_or_default());
        pattern = rest.get(seps..).unwrap_or_default();
    }
}

/// `glob3`: the names of the folder `path` that match `part`, each followed by the rest.
fn glob3(
    path: &mut Vec<Char>,
    part: &[Char],
    rest: &[Char],
    found: &mut Vec<Vec<u8>>,
) -> Result<(), GlobFailed> {
    let dir = if path.is_empty() {
        PathBuf::from(".")
    } else {
        path_of(bytes_of(path))
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    // `readdir` also gives `.` and `..`. Each name is judged as it is read; a read error
    // ends the folder as it ends `readdir`.
    let names = [b".".to_vec(), b"..".to_vec()].into_iter().chain(
        entries
            .map_while(Result::ok)
            .map(|e| name_bytes(&e.file_name())),
    );
    let base = path.len();
    for name in names {
        // An initial `.` must be matched literally.
        if name.first() == Some(&b'.') && part.first() != Some(&DOT) {
            continue;
        }
        // The name and its `EOS` are copied up to `pathend_last` (`pathbuf + PATH_MAX - 1`),
        // and reaching it is an error, matched or not.
        if base + name.len() + NAME_ROOM >= PATH_MAX {
            return Err(GlobFailed);
        }
        let name: Vec<Char> = name.iter().map(|&b| Char::from(b)).collect();
        if !matches(&name, part) {
            continue;
        }
        path.truncate(base);
        path.extend_from_slice(&name);
        let r = glob2(path, rest, found);
        path.truncate(base);
        r?;
    }
    Ok(())
}

/// `match`: whether `name` matches the compiled part `pat`. Each `*` keeps one place to
/// come back to (the algorithm of <https://research.swtch.com/glob>), so a pattern with many
/// `*` does not take exponential time.
fn matches(name: &[Char], pat: &[Char]) -> bool {
    let (mut n, mut p) = (0usize, 0usize);
    let mut next: Option<(usize, usize)> = None;
    let at = |s: &[Char], k: usize| s.get(k).copied().unwrap_or(EOS);
    'outer: loop {
        let mut failed = false;
        while p < pat.len() {
            let c = at(pat, p);
            p += 1;
            match c {
                M_ALL => {
                    while at(pat, p) == M_ALL && p < pat.len() {
                        p += 1;
                    }
                    if p == pat.len() {
                        return true;
                    }
                    if n >= name.len() {
                        return false;
                    }
                    next = Some((p - 1, n + 1));
                }
                M_ONE => {
                    if n >= name.len() {
                        failed = true;
                        break;
                    }
                    n += 1;
                }
                M_SET => {
                    if n >= name.len() {
                        failed = true;
                        break;
                    }
                    let k = at(name, n);
                    n += 1;
                    let negate = at(pat, p) == M_NOT;
                    if negate {
                        p += 1;
                    }
                    let mut ok = false;
                    loop {
                        let c = at(pat, p);
                        p += 1;
                        if c == M_END || p > pat.len() {
                            break;
                        }
                        if c == M_CLASS {
                            let index = usize::from(at(pat, p));
                            if let Some((_, is)) = CLASSES.get(index)
                                && u8::try_from(k).is_ok_and(is)
                            {
                                ok = true;
                            }
                            p += 1;
                        }
                        if at(pat, p) == M_RNG {
                            if c <= k && k <= at(pat, p + 1) {
                                ok = true;
                            }
                            p += 2;
                        } else if c == k {
                            ok = true;
                        }
                    }
                    if ok == negate {
                        failed = true;
                        break;
                    }
                }
                _ => {
                    if n >= name.len() || at(name, n) != c {
                        failed = true;
                        break;
                    }
                    n += 1;
                }
            }
        }
        if !failed && n >= name.len() {
            return true;
        }
        match next {
            Some((np, nn)) => {
                p = np;
                n = nn;
                continue 'outer;
            }
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> bool {
        let quoted: Vec<Char> = p.bytes().map(Char::from).collect();
        let compiled = glob0(&quoted).expect("compiles");
        let name: Vec<Char> = n.bytes().map(Char::from).collect();
        matches(&name, &compiled)
    }

    #[test]
    fn matching() {
        assert!(m("*.conf", "a.conf"));
        assert!(m("a?c", "abc"));
        assert!(m("[a-c]x", "bx"));
        assert!(!m("[!a-c]x", "bx"));
        // `^` is not a negation (glob.c:92).
        assert!(m("[^a]x", "ax"));
        assert!(!m("[^a]x", "bx"));
        assert!(m("[]]", "]"));
        assert!(m("[x", "[x"));
        assert!(m("[[:digit:]]*", "1.conf"));
        assert!(!m("[[:digit:]]*", "a.conf"));
        assert!(m("[[:alpha:][:digit:]]", "q"));
        assert!(m("a*b*c", "aXbYc"));
        assert!(!m("a*b*c", "aXbY"));
    }

    #[test]
    fn unknown_class_matches_nothing() {
        let quoted: Vec<Char> = "[[:nosuch:]]".bytes().map(Char::from).collect();
        assert_eq!(glob0(&quoted), None);
    }

    #[test]
    fn many_stars_do_not_backtrack() {
        // A name of 100 `a` and a pattern with many `*`: linear in the name per `*`.
        let name = "a".repeat(100);
        let pattern = format!("{}y", "a*".repeat(30));
        // The backtracking version does not finish this in a test run.
        assert!(!m(&pattern, &name));
        assert!(m(&format!("{}a", "a*".repeat(30)), &name));
    }

    // Unix only: the glob sees only `/` as a separator (portable OpenSSH), and a Windows
    // temporary path has `\`.
    #[cfg(unix)]
    #[test]
    fn lists_files_sorted() {
        let dir =
            std::env::temp_dir().join(format!("tasty-ssh-config-glob-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        for f in ["b.conf", "a.conf", ".h.conf", "d/c.conf", "1.conf"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let base = dir.to_str().unwrap();
        let got = |p: &str| -> Vec<String> {
            glob(&format!("{base}/{p}"))
                .unwrap()
                .into_iter()
                .map(|p| p.display().to_string())
                .collect()
        };
        let full = |names: &[&str]| -> Vec<String> {
            names.iter().map(|n| format!("{base}/{n}")).collect()
        };
        assert_eq!(got("*.conf"), full(&["1.conf", "a.conf", "b.conf"]));
        assert_eq!(got("*/c.conf"), full(&["d/c.conf"]));
        assert_eq!(got("a.conf"), full(&["a.conf"]));
        assert!(got("none.conf").is_empty());
        // A folder is a result like a file.
        assert_eq!(got("[d]"), full(&["d"]));
        // A trailing `/` needs a folder; an empty part stays.
        assert!(got("a.conf/").is_empty());
        assert_eq!(got("d//c.conf"), full(&["d//c.conf"]));
        // `.*` also gives `.` and `..`, as `readdir` does.
        assert_eq!(got(".*"), full(&[".", "..", ".h.conf"]));
        assert_eq!(got("[[:digit:]]*"), full(&["1.conf"]));
        // An escaped pattern character is literal.
        assert!(got("\\*.conf").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Unix only: the folders are named with `/`.
    #[cfg(unix)]
    #[test]
    fn path_length_bounds() {
        const PART: usize = 100;
        const WIDE: usize = 200;
        // The canonical folder: a symbolic link on the way (macOS `/var`) would count in the
        // length the kernel checks.
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tasty-ssh-config-glen-{}", std::process::id()));
        // Folders deep enough that a name of more than `PART` bytes reaches the bound.
        let mut dir = root.clone();
        while PATH_MAX - NAME_ROOM - (dir.as_os_str().len() + 1) > WIDE {
            dir.push("a".repeat(PART));
        }
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.to_str().unwrap().to_string();
        // `base/` and a name of this length reach `pathend_last`.
        let at_bound = PATH_MAX - NAME_ROOM - (base.len() + 1);
        std::fs::write(dir.join("b".repeat(at_bound - 1)), "").unwrap();
        let star = format!("{base}/*");
        assert_eq!(glob(&star).unwrap().len(), 1);
        std::fs::write(dir.join("c".repeat(at_bound)), "").unwrap();
        assert_eq!(glob(&star), Err(GlobFailed));

        // A pattern of `PATH_MAX - 1` bytes without pattern characters is copied and looked
        // up, so the file it names is found; one of `PATH_MAX` bytes matches nothing.
        let literal = |len: usize| format!("{base}/{}", "d".repeat(len - base.len() - 1));
        let longest = literal(PATH_MAX - 1);
        std::fs::write(&longest, "").unwrap();
        assert_eq!(glob(&longest), Ok(vec![PathBuf::from(&longest)]));
        assert_eq!(glob(&literal(PATH_MAX)), Ok(Vec::new()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Unix only: the folders are named with `/`.
    #[cfg(unix)]
    #[test]
    fn home_cut_at_path_max() {
        const PART: usize = 100;
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tasty-ssh-config-gtil-{}", std::process::id()));
        // A folder whose path is `PATH_MAX - 3` bytes, so `<folder>/f` is `PATH_MAX - 1`.
        let want = PATH_MAX - 3;
        let mut dir = root.as_os_str().to_str().unwrap().to_string();
        while want - dir.len() > 2 * (PART + 1) {
            dir.push('/');
            dir.push_str(&"a".repeat(PART));
        }
        let last = want - dir.len() - 1;
        dir.push('/');
        dir.push_str(&"b".repeat(last));
        assert_eq!(dir.len(), want);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/f"), "").unwrap();
        let home = format!("{dir}/");
        // The home and the rest are cut to `PATH_MAX - 1` characters and the search goes on.
        assert_eq!(
            glob_tilde(&home, "fgh", "~/fgh".len()),
            Ok(vec![PathBuf::from(format!("{dir}/f"))])
        );
        // The written pattern, not the one with the home, decides `GLOB_NOMATCH`.
        assert_eq!(glob_tilde(&home, "fgh", PATH_MAX), Ok(Vec::new()));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
