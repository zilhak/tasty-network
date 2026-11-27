//! `ssh-keygen -F` comparison of known_hosts reading (docs/design/ssh-backends.md "시험 목록",
//! "호스트 키 판정").
//!
//! The test makes keys with the reference `ssh-keygen`, writes `tests/data/known-hosts/
//! known_hosts.in` with them, appends the lines of `to_hash.in` after `ssh-keygen -H` hashed
//! them, and looks every name of `queries` up twice:
//!
//! - `ssh-keygen -F <name> -f <file>`: the lines it lists (line number and marker) are the
//!   [`HostKeyEntry`] values with a `key_type`, and it exits with 0 when there are some and
//!   with 1 when there are none (any other exit or a signal fails the test);
//! - `ssh-keygen -l -E sha1 -F <name> -f <file>`: the lines it lists, with the key type, the
//!   SHA-1 fingerprint and the comment, are the entries with a `key`.
//!
//! A name both list nothing for is counted as rejected alike: `ssh-keygen` exits with 1 and the
//! crate gives no entry. The reference client is the one of `tests/matches_openssh.rs`
//! (`$TASTY_OPENSSH_REF_DIR/bin/ssh-keygen` or `target/tools/openssh-10.6p2/bin/ssh-keygen`),
//! checked by the `ssh -V` next to it; without it the test fails when
//! `TASTY_OPENSSH_REF_REQUIRED` is not empty and is skipped otherwise. Windows does not
//! compare.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use sha1::Digest as _;
use tasty_net_ssh_config::settings::OPENSSH_VERSION;
use tasty_net_ssh_config::{HostKeyEntry, HostKeyQuery, KnownHosts, Marker};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reference_bin() -> PathBuf {
    match std::env::var_os("TASTY_OPENSSH_REF_DIR") {
        Some(dir) => PathBuf::from(dir).join("bin"),
        None => manifest_dir().join("../../target/tools/openssh-10.6p2/bin"),
    }
}

fn run(program: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(program)
        .env_clear()
        .args(args)
        .output()
        .expect("run the reference ssh-keygen")
}

fn path_arg(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 temporary path")
}

/// `<type> <base64>` of the `.pub` file `ssh-keygen` wrote.
fn public_line(path: &Path) -> String {
    let text = std::fs::read_to_string(path).expect("a public key file");
    let mut words = text.split_whitespace();
    format!(
        "{} {}",
        words.next().expect("the key type"),
        words.next().expect("the key")
    )
}

/// The SHA-1 fingerprint `ssh-keygen -l -E sha1` prints: of the plain key, also for a
/// certificate (`sshkey_fingerprint_raw` serializes it plain). The certificates here are
/// Ed25519 ones.
fn fingerprint(blob: &[u8], short_type: &str) -> String {
    fn string(b: &[u8]) -> (&[u8], &[u8]) {
        let (len, rest) = b.split_at(4);
        let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
        rest.split_at(len)
    }
    let plain = if short_type.ends_with("-CERT") {
        assert_eq!(
            short_type, "ED25519-CERT",
            "only Ed25519 certificates are made"
        );
        let (_, rest) = string(blob);
        let (_, rest) = string(rest);
        let (pk, _) = string(rest);
        let mut out = Vec::new();
        for part in [b"ssh-ed25519".as_slice(), pk] {
            out.extend_from_slice(&u32::try_from(part.len()).unwrap().to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    } else {
        blob.to_vec()
    };
    format!(
        "SHA1:{}",
        STANDARD_NO_PAD.encode(sha1::Sha1::digest(&plain))
    )
}

fn marker_word(m: Marker) -> &'static str {
    match m {
        Marker::None => "",
        Marker::Revoked => "REVOKED",
        Marker::CertAuthority => "CA",
    }
}

/// The lines `ssh-keygen -F` lists: `(line, marker word, the line after the header)`.
fn listed(stdout: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut lines = stdout.lines();
    while let Some(l) = lines.next() {
        let Some(rest) = l.strip_prefix("# Host ") else {
            panic!("unexpected ssh-keygen output line {l:?}");
        };
        let (_, place) = rest.split_once(" found: line ").expect("a found line");
        let mut words = place.split_whitespace();
        let line = words.next().expect("the line").parse().expect("a number");
        let marker = words.next().unwrap_or_default().to_string();
        out.push((line, marker, lines.next().unwrap_or_default().to_string()));
    }
    out
}

/// `ssh-keygen -F` exits with 0 when it lists lines and 1 when it lists none
/// (`ssh-keygen.c:1383`; an error is `fatal`, 255); a signal has no code.
fn exit_agrees(status: std::process::ExitStatus, lists: bool) -> bool {
    status.code() == Some(if lists { 0 } else { 1 })
}

#[test]
fn matches_ssh_keygen_f() {
    if cfg!(windows) {
        println!("ssh-keygen -F comparison: not run on Windows");
        return;
    }
    let bin = reference_bin();
    let keygen = bin.join("ssh-keygen");
    if !keygen.exists() || !bin.join("ssh").exists() {
        let msg = format!(
            "ssh-keygen -F comparison: no reference client in {} (bash scripts/install-openssh-ref.sh)",
            bin.display()
        );
        if std::env::var_os("TASTY_OPENSSH_REF_REQUIRED").is_some_and(|v| !v.is_empty()) {
            panic!("{msg}");
        }
        println!("{msg}; skipped");
        return;
    }
    let version = run(&bin.join("ssh"), &["-V"]);
    let version = String::from_utf8_lossy(&version.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    println!("reference client: {version}");
    assert!(
        version.starts_with(OPENSSH_VERSION),
        "reference client version differs: want {OPENSSH_VERSION}, got {version:?}"
    );

    let dir = std::env::temp_dir().join(format!(
        "tasty-net-ssh-config-known-hosts-{}",
        std::process::id()
    ));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("remove an old copy");
    }
    std::fs::create_dir_all(&dir).expect("a temporary folder");

    // The keys: made here, never real host keys.
    let mut keys: Vec<(String, String)> = Vec::new();
    for (name, args) in [
        ("ED1", &["-t", "ed25519"][..]),
        ("ED2", &["-t", "ed25519"]),
        ("EC1", &["-t", "ecdsa", "-b", "256"]),
        ("EC2", &["-t", "ecdsa", "-b", "384"]),
        ("RS1", &["-t", "rsa", "-b", "2048"]),
        ("CA", &["-t", "ed25519"]),
    ] {
        let file = dir.join(name);
        let mut all = vec!["-q", "-N", "", "-C", "", "-f", path_arg(&file)];
        all.extend_from_slice(args);
        let out = run(&keygen, &all);
        assert!(out.status.success(), "ssh-keygen {all:?}: {out:?}");
        keys.push((name.to_string(), public_line(&file.with_extension("pub"))));
    }
    let signed = run(
        &keygen,
        &[
            "-q",
            "-s",
            path_arg(&dir.join("CA")),
            "-I",
            "test",
            "-h",
            "-n",
            "certhost.example",
            path_arg(&dir.join("ED1.pub")),
        ],
    );
    assert!(signed.status.success(), "ssh-keygen -s: {signed:?}");
    keys.push((
        "ED1CERT".to_string(),
        public_line(&dir.join("ED1-cert.pub")),
    ));
    let fill = |text: &str| {
        let mut text = text.to_string();
        for (name, line) in &keys {
            let b64 = line.split_once(' ').expect("type and key").1;
            text = text
                .replace(&format!("@{name}_B64@"), b64)
                .replace(&format!("@{name}@"), line);
        }
        assert!(!text.contains("@ED") && !text.contains("@EC") && !text.contains("@RS"));
        text
    };

    let data = manifest_dir().join("tests/data/known-hosts");
    let read = |name: &str| std::fs::read_to_string(data.join(name)).expect("a data file");
    let to_hash = dir.join("to_hash");
    std::fs::write(&to_hash, fill(&read("to_hash.in"))).expect("write to_hash");
    let hashed = run(&keygen, &["-H", "-f", path_arg(&to_hash)]);
    assert!(hashed.status.success(), "ssh-keygen -H: {hashed:?}");
    let hashed = std::fs::read_to_string(&to_hash).expect("the hashed file");
    assert!(hashed.contains("|1|"), "ssh-keygen -H hashed nothing");
    let file = dir.join("known_hosts");
    std::fs::write(&file, fill(&read("known_hosts.in")) + &hashed).expect("write known_hosts");
    let known = KnownHosts::load(std::slice::from_ref(&file));

    let (mut queries, mut listed_f, mut listed_l, mut rejected_alike) = (0, 0, 0, 0);
    let mut failures = Vec::new();
    for name in read("queries")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        queries += 1;
        let entries: Vec<HostKeyEntry> = known.entries(&HostKeyQuery {
            name: name.to_string(),
        });

        let f = run(&keygen, &["-F", name, "-f", path_arg(&file)]);
        let want_f: Vec<(usize, String)> = listed(&String::from_utf8_lossy(&f.stdout))
            .into_iter()
            .map(|(n, m, _)| (n, m))
            .collect();
        let got_f: Vec<(usize, String)> = entries
            .iter()
            .filter(|e| e.key_type.is_some())
            .map(|e| (e.line, marker_word(e.marker).to_string()))
            .collect();
        if want_f != got_f || !exit_agrees(f.status, !want_f.is_empty()) {
            failures.push(format!(
                "-F {name}: ssh-keygen {want_f:?} (exit {:?}), crate {got_f:?}",
                f.status.code()
            ));
        }
        listed_f += want_f.len();

        let l = run(
            &keygen,
            &["-l", "-E", "sha1", "-F", name, "-f", path_arg(&file)],
        );
        let want_l = listed(&String::from_utf8_lossy(&l.stdout));
        let got_l: Vec<(usize, String, String)> = entries
            .iter()
            .filter_map(|e| {
                let k = e.key.as_ref()?;
                let fp = fingerprint(&k.blob, k.short_type);
                let comment = if k.comment.is_empty() {
                    String::new()
                } else {
                    format!(" {}", k.comment)
                };
                Some((
                    e.line,
                    marker_word(e.marker).to_string(),
                    format!("{name} {} {fp}{comment}", k.short_type),
                ))
            })
            .collect();
        if want_l != got_l || !exit_agrees(l.status, !want_l.is_empty()) {
            failures.push(format!(
                "-l -F {name}: ssh-keygen {want_l:?} (exit {:?}), crate {got_l:?}",
                l.status.code()
            ));
        }
        listed_l += want_l.len();
        if f.status.code() == Some(1) && l.status.code() == Some(1) && entries.is_empty() {
            rejected_alike += 1;
        }
    }
    // The base64 of every listed key is the one written (a sanity check of the decoder).
    for e in known.entries(&HostKeyQuery {
        name: "plain.example".to_string(),
    }) {
        if let Some(k) = e.key {
            assert!(
                fill(&read("known_hosts.in")).contains(&STANDARD.encode(&k.blob)),
                "line {}",
                e.line
            );
        }
    }
    std::fs::remove_dir_all(&dir).expect("remove the temporary folder");
    println!(
        "ssh-keygen -F comparison: {queries} names, {listed_f} lines listed by -F, {listed_l} by -l -F, \
         {rejected_alike} names rejected alike, {} differences",
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
