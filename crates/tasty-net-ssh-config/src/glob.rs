//! The `glob(3)` call of `Include` (`readconf.c:2109`, `GLOB_TILDE`): `*`, `?` and `[...]`
//! in each path part, a leading `.` matched only literally, `\` escapes, results sorted by
//! bytes (the C collation of `ssh`). A path without patterns is returned only if it exists.
//! The `~` of the path is expanded by the caller.
//!
//! This file is not derived from a `glob(3)` implementation (`openbsd-compat/glob.c` or a
//! C library); it is written for this crate to give the same results for the patterns
//! `Include` takes, so it carries the crate license only.

use std::path::Path;

/// Whether a path part has a pattern character that is not escaped.
fn has_magic(part: &str) -> bool {
    let mut escaped = false;
    for c in part.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

fn unescape(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut it = part.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A `[...]` set at the start of `p` (after the `[`). Returns whether `c` is in it and the
/// rest of the pattern, or `None` if the set is not closed (then `[` is literal).
fn bracket(p: &[char], c: char) -> Option<(bool, &[char])> {
    let (negate, mut i) = match p.first() {
        Some('!' | '^') => (true, 1),
        _ => (false, 0),
    };
    let mut found = false;
    let mut first = true;
    loop {
        let lo = *p.get(i)?;
        if lo == ']' && !first {
            return Some((found != negate, p.get(i + 1..).unwrap_or_default()));
        }
        first = false;
        let (lo, step) = if lo == '\\' {
            (*p.get(i + 1)?, i + 2)
        } else {
            (lo, i + 1)
        };
        i = step;
        if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|h| *h != ']') {
            let hi = *p.get(i + 1)?;
            i += 2;
            if lo <= c && c <= hi {
                found = true;
            }
        } else if lo == c {
            found = true;
        }
    }
}

/// `fnmatch(pattern, name, FNM_PERIOD)` for one path part.
fn fnmatch(p: &[char], n: &[char], at_start: bool) -> bool {
    let leading_dot = at_start && n.first() == Some(&'.');
    match p.first() {
        None => n.is_empty(),
        Some('*') => {
            if leading_dot {
                return false;
            }
            let rest = p.get(1..).unwrap_or_default();
            (0..=n.len()).any(|k| fnmatch(rest, n.get(k..).unwrap_or_default(), false))
        }
        Some('?') => {
            !n.is_empty()
                && !leading_dot
                && fnmatch(
                    p.get(1..).unwrap_or_default(),
                    n.get(1..).unwrap_or_default(),
                    false,
                )
        }
        Some('[') => {
            let Some(&c) = n.first() else {
                return false;
            };
            match bracket(p.get(1..).unwrap_or_default(), c) {
                Some((true, rest)) if !leading_dot => {
                    fnmatch(rest, n.get(1..).unwrap_or_default(), false)
                }
                Some(_) => false,
                None => {
                    c == '['
                        && fnmatch(
                            p.get(1..).unwrap_or_default(),
                            n.get(1..).unwrap_or_default(),
                            false,
                        )
                }
            }
        }
        Some('\\') if p.len() > 1 => {
            n.first() == p.get(1)
                && fnmatch(
                    p.get(2..).unwrap_or_default(),
                    n.get(1..).unwrap_or_default(),
                    false,
                )
        }
        Some(&c) => {
            n.first() == Some(&c)
                && fnmatch(
                    p.get(1..).unwrap_or_default(),
                    n.get(1..).unwrap_or_default(),
                    false,
                )
        }
    }
}

/// The paths that match `pattern`, sorted. An empty list is `GLOB_NOMATCH`.
pub(crate) fn glob(pattern: &str) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let parts: Vec<&str> = pattern.split('/').filter(|p| !p.is_empty()).collect();
    let mut found: Vec<String> = vec![if absolute {
        "/".to_string()
    } else {
        String::new()
    }];
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = Vec::new();
        for base in &found {
            let join = |name: &str| {
                if base.is_empty() || base.ends_with('/') {
                    format!("{base}{name}")
                } else {
                    format!("{base}/{name}")
                }
            };
            if !has_magic(part) {
                let candidate = join(&unescape(part));
                let path = Path::new(&candidate);
                let keep = if last {
                    path.symlink_metadata().is_ok()
                } else {
                    path.is_dir()
                };
                if keep {
                    next.push(candidate);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let pat: Vec<char> = part.chars().collect();
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let chars: Vec<char> = name.chars().collect();
                if !fnmatch(&pat, &chars, true) {
                    continue;
                }
                let candidate = join(&name);
                if last || Path::new(&candidate).is_dir() {
                    next.push(candidate);
                }
            }
        }
        found = next;
    }
    if parts.is_empty() {
        found.retain(|p| !p.is_empty() && Path::new(p).exists());
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> bool {
        let p: Vec<char> = p.chars().collect();
        let n: Vec<char> = n.chars().collect();
        fnmatch(&p, &n, true)
    }

    #[test]
    fn matching() {
        assert!(m("*.conf", "a.conf"));
        assert!(!m("*.conf", ".a.conf"));
        assert!(m(".*", ".a"));
        assert!(m("a?c", "abc"));
        assert!(m("[a-c]x", "bx"));
        assert!(!m("[!a-c]x", "bx"));
        assert!(m("[]]", "]"));
        assert!(m("\\*", "*"));
        assert!(!m("\\*", "a"));
        assert!(m("[x", "[x"));
    }

    #[test]
    fn lists_files_sorted() {
        let dir =
            std::env::temp_dir().join(format!("tasty-ssh-config-glob-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        for f in ["b.conf", "a.conf", ".h.conf", "d/c.conf"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let base = dir.to_str().unwrap();
        let got = glob(&format!("{base}/*.conf"));
        assert_eq!(
            got,
            vec![format!("{base}/a.conf"), format!("{base}/b.conf")]
        );
        assert_eq!(
            glob(&format!("{base}/*/c.conf")),
            vec![format!("{base}/d/c.conf")]
        );
        assert_eq!(
            glob(&format!("{base}/a.conf")),
            vec![format!("{base}/a.conf")]
        );
        assert!(glob(&format!("{base}/none.conf")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
