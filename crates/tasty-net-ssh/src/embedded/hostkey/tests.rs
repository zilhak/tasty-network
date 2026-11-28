//! The host key check against known_hosts files written by the tests (docs/design/ssh-backends.md
//! "시험 목록" 호스트 키). Keys are made here; no real host key is in the repository.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use russh::keys::ssh_key::Mpint;
use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData, RsaPublicKey};
use russh::keys::{PublicKey, PublicKeyOrCertificate};
use tasty_net_ssh_config::{ConfigFiles, ConfigInputs, ConfigSet, ResolveContext};

use super::*;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            // A space in the folder name, as a user's temporary folder can have: the
            // configuration lines quote their paths.
            "tasty-net-ssh hostkey-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        Self(dir)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove {}: {e}", self.0.display());
        }
    }
}

fn ed25519(seed: u8) -> PublicKey {
    PublicKey::new(KeyData::Ed25519(Ed25519PublicKey([seed; 32])), "")
}

/// An RSA public key of `bytes` * 8 bits (only its size matters here).
fn rsa(bytes: usize) -> PublicKey {
    let mut n = vec![0xC5; bytes];
    n[bytes - 1] = 0x01;
    let key = RsaPublicKey::new(
        Mpint::from_positive_bytes(&[0x01, 0x00, 0x01]),
        Mpint::from_positive_bytes(&n),
    )
    .expect("rsa");
    PublicKey::new(KeyData::Rsa(key), "")
}

fn line(names: &str, key: &PublicKey) -> String {
    format!(
        "{names} {} {}\n",
        key.key_data().algorithm().as_str(),
        STANDARD.encode(key.to_bytes().expect("blob"))
    )
}

/// A resolution of `h.example` with `config` lines and the test's known_hosts files.
fn check(
    dir: &Dir,
    known: &str,
    config: &str,
    prompt: Option<Arc<dyn SshPrompt>>,
) -> Result<HostCheck, RefusalNote> {
    check_host(dir, "h.example", known, config, prompt)
}

/// [`check`] for the host `host`.
fn check_host(
    dir: &Dir,
    host: &str,
    known: &str,
    config: &str,
    prompt: Option<Arc<dyn SshPrompt>>,
) -> Result<HostCheck, RefusalNote> {
    let kh = dir.write("known_hosts", known);
    let cfg = dir.write(
        "config",
        &format!(
            "UserKnownHostsFile \"{}\"\nGlobalKnownHostsFile none\n{config}\n",
            kh.display()
        ),
    );
    let set = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(cfg),
    })
    .expect("load");
    let res = set.resolve(
        host,
        &ResolveContext {
            local_user: Some("u".into()),
            home: Some(dir.0.to_string_lossy().into_owned()),
            ..ResolveContext::default()
        },
    );
    assert!(res.refusal.is_none(), "{:?}", res.refusal);
    HostCheck::prepare(
        &res,
        prompt,
        Duration::from_secs(5),
        Arc::new(Paused::default()),
        Arc::new(Mutex::new(Vec::new())),
    )
}

fn judge(check: &HostCheck, key: &PublicKey) -> Result<(), HostKeyRefusal> {
    judge_at(check, key, None)
}

/// [`judge`] of a direct connection to `address`.
fn judge_at(
    check: &HostCheck,
    key: &PublicKey,
    address: Option<&str>,
) -> Result<(), HostKeyRefusal> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(check.judge(
            &PublicKeyOrCertificate::PublicKey {
                key: key.clone(),
                hash_alg: None,
            },
            address,
            &super::GivenUp::default(),
        ))
        .map_err(|refused| match refused {
            super::Refused::Key(refusal) => refusal,
            super::Refused::Stopped => panic!("no open to stop"),
        })
}

fn known_file(dir: &Dir) -> String {
    std::fs::read_to_string(dir.0.join("known_hosts")).expect("read")
}

#[test]
fn a_known_key_passes_and_another_key_is_refused_as_changed() {
    let dir = Dir::new("changed");
    let c = check(
        &dir,
        &line("h.example", &ed25519(1)),
        "StrictHostKeyChecking accept-new",
        None,
    )
    .expect("check");
    assert_eq!(judge(&c, &ed25519(1)), Ok(()));
    let refused = judge(&c, &ed25519(2)).expect_err("changed");
    assert!(
        matches!(&refused.reason, HostKeyReason::Changed { line: 1, file } if file.ends_with("known_hosts")),
        "{refused:?}"
    );
    // accept-new does not record over a changed key.
    assert_eq!(known_file(&dir), line("h.example", &ed25519(1)));
}

#[test]
fn a_revoked_line_refuses_even_with_a_matching_line() {
    let dir = Dir::new("revoked");
    let known = format!(
        "@revoked {}{}",
        line("h.example", &ed25519(3)),
        line("h.example", &ed25519(3))
    );
    let c = check(&dir, &known, "StrictHostKeyChecking accept-new", None).expect("check");
    let refused = judge(&c, &ed25519(3)).expect_err("revoked");
    assert!(
        matches!(refused.reason, HostKeyReason::Revoked { line: Some(1), .. }),
        "{refused:?}"
    );
}

#[test]
fn a_cert_authority_line_is_not_trusted_and_noted() {
    let dir = Dir::new("ca");
    let known = format!("@cert-authority {}", line("h.example", &ed25519(4)));
    let c = check(&dir, &known, "StrictHostKeyChecking yes", None).expect("check");
    let refused = judge(&c, &ed25519(4)).expect_err("unknown");
    assert_eq!(refused.reason, HostKeyReason::Unknown);
    let notes = c.notes.lock().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n.class == NoteClass::CertAuthorityDistrusted && n.line == Some(1)),
        "{notes:?}"
    );
}

#[test]
fn accept_new_records_the_key_plain_or_hashed() {
    for hashed in [false, true] {
        let dir = Dir::new("record");
        let config = format!(
            "StrictHostKeyChecking accept-new\nHashKnownHosts {}",
            if hashed { "yes" } else { "no" }
        );
        let c = check(&dir, "", &config, None).expect("check");
        assert_eq!(judge(&c, &ed25519(5)), Ok(()));
        let written = known_file(&dir);
        if hashed {
            assert!(written.starts_with("|1|"), "{written}");
            assert!(!written.contains("h.example"), "{written}");
        } else {
            assert_eq!(written, line("h.example", &ed25519(5)));
        }
        // The next connection finds the recorded key, and another key is changed.
        let c = check(&dir, &written, &config, None).expect("check");
        assert_eq!(judge(&c, &ed25519(5)), Ok(()));
        assert!(matches!(
            judge(&c, &ed25519(6)).expect_err("changed").reason,
            HostKeyReason::Changed { .. }
        ));
    }
}

#[test]
fn ask_without_an_answer_refuses_and_with_yes_records() {
    struct Yes;
    impl SshPrompt for Yes {
        fn confirm_host_key(&self, q: &HostKeyQuestion) -> bool {
            q.host == "h.example" && q.fingerprint.starts_with("SHA256:")
        }
    }
    let dir = Dir::new("ask");
    let c = check(&dir, "", "StrictHostKeyChecking ask", None).expect("check");
    assert_eq!(
        judge(&c, &ed25519(7)).expect_err("no prompt").reason,
        HostKeyReason::Unknown
    );
    assert_eq!(known_file(&dir), "");
    let c = check(&dir, "", "StrictHostKeyChecking ask", Some(Arc::new(Yes))).expect("check");
    assert_eq!(judge(&c, &ed25519(7)), Ok(()));
    assert_eq!(known_file(&dir), line("h.example", &ed25519(7)));
    // BatchMode yes asks nothing.
    let dir = Dir::new("batch");
    let c = check(
        &dir,
        "",
        "StrictHostKeyChecking ask\nBatchMode yes",
        Some(Arc::new(Yes)),
    )
    .expect("check");
    assert_eq!(
        judge(&c, &ed25519(7)).expect_err("batch").reason,
        HostKeyReason::Unknown
    );
}

#[test]
fn revoked_host_keys_files_are_read_before_the_check() {
    let dir = Dir::new("revoked-file");
    let known = line("h.example", &ed25519(8));
    // A missing list refuses the host key.
    let missing = dir.0.join("missing.keys");
    let c = check(
        &dir,
        &known,
        &format!("RevokedHostKeys \"{}\"", missing.display()),
        None,
    )
    .expect("check");
    assert_eq!(
        judge(&c, &ed25519(8)).expect_err("unreadable").reason,
        HostKeyReason::RevokedListUnreadable { file: missing }
    );
    // A list with the key refuses it; one without passes.
    let list = dir.write("revoked.keys", line("", &ed25519(8)).trim_start());
    let c = check(
        &dir,
        &known,
        &format!("RevokedHostKeys \"{}\"", list.display()),
        None,
    )
    .expect("check");
    assert!(matches!(
        judge(&c, &ed25519(8)).expect_err("revoked").reason,
        HostKeyReason::Revoked { line: None, .. }
    ));
    let other = dir.write("other.keys", line("", &ed25519(9)).trim_start());
    let c = check(
        &dir,
        &known,
        &format!("RevokedHostKeys \"{}\"", other.display()),
        None,
    )
    .expect("check");
    assert_eq!(judge(&c, &ed25519(8)), Ok(()));
    // A binary KRL is refused before connecting.
    let krl = dir.0.join("revoked.krl");
    std::fs::write(&krl, [KRL_MAGIC, b"rest"].concat()).expect("krl");
    let refusal = check(
        &dir,
        &known,
        &format!("RevokedHostKeys \"{}\"", krl.display()),
        None,
    )
    .err()
    .expect("refused");
    assert_eq!(refusal.kind, "security_not_implemented");
    assert_eq!(refusal.keyword.as_deref(), Some("revokedhostkeys"));
}

#[test]
fn a_small_rsa_host_key_is_refused() {
    let dir = Dir::new("rsa");
    let small = rsa(64);
    let c = check(&dir, &line("h.example", &small), "", None).expect("check");
    assert_eq!(
        judge(&c, &small).expect_err("small").reason,
        HostKeyReason::RsaTooSmall {
            bits: 512,
            required: 1024
        }
    );
    let big = rsa(256);
    let c = check(&dir, &line("h.example", &big), "", None).expect("check");
    assert_eq!(judge(&c, &big), Ok(()));
}

#[test]
fn the_record_goes_to_the_first_user_file() {
    let dir = Dir::new("first");
    let second = dir.write("second", "");
    let first = dir.0.join("sub").join("first");
    let cfg = dir.write(
        "config",
        &format!(
            "UserKnownHostsFile \"{}\" \"{}\"\nGlobalKnownHostsFile none\nStrictHostKeyChecking accept-new\n",
            first.display(),
            second.display()
        ),
    );
    let set = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(cfg),
    })
    .expect("load");
    let res = set.resolve(
        "h.example",
        &ResolveContext {
            local_user: Some("u".into()),
            ..ResolveContext::default()
        },
    );
    let c = HostCheck::prepare(
        &res,
        None,
        Duration::from_secs(5),
        Arc::new(Paused::default()),
        Arc::new(Mutex::new(Vec::new())),
    )
    .expect("check");
    assert_eq!(judge(&c, &ed25519(10)), Ok(()));
    // The folder of the first file is made, as `ssh` makes `~/.ssh`, readable by the user
    // only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(first.parent().expect("parent"))
            .expect("folder")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }
    assert_eq!(
        std::fs::read_to_string(&first).expect("first"),
        line("h.example", &ed25519(10))
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&second)).expect("second"),
        ""
    );
}

#[test]
fn records_from_many_threads_keep_whole_lines() {
    let dir = Dir::new("concurrent");
    let file = dir.0.join("known_hosts");
    let writers: Vec<_> = (0..8u8)
        .map(|n| {
            let file = file.clone();
            std::thread::spawn(move || {
                for i in 0..25u8 {
                    let host = format!("h{n}-{i}.example");
                    let key = ed25519(n.wrapping_mul(31).wrapping_add(i));
                    let lines = host_lines(&[host], &key, false).expect("lines");
                    append(&file, &lines).expect("append");
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
    let text = known_file(&dir);
    assert_eq!(text.lines().count(), 200, "{text}");
    for line in text.lines() {
        assert_eq!(line.split(' ').count(), 3, "{line}");
        assert!(
            line.starts_with('h') && line.contains(".example ssh-ed25519 "),
            "{line}"
        );
    }
}

#[test]
fn a_first_record_wins_over_checks_prepared_before_it() {
    let dir = Dir::new("first-record");
    // Three connections read the same empty files before any of them records.
    let first = check(&dir, "", "StrictHostKeyChecking accept-new", None).expect("check");
    let other = check(&dir, "", "StrictHostKeyChecking accept-new", None).expect("check");
    let same = check(&dir, "", "StrictHostKeyChecking accept-new", None).expect("check");
    assert_eq!(judge(&first, &ed25519(11)), Ok(()));
    // Another key for the host is now a changed key, not a second new key.
    let refused = judge(&other, &ed25519(12)).expect_err("changed");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 1, .. }),
        "{refused:?}"
    );
    // The same key passes and is not written twice.
    assert_eq!(judge(&same, &ed25519(11)), Ok(()));
    assert_eq!(known_file(&dir), line("h.example", &ed25519(11)));
}

#[test]
fn a_first_record_of_an_address_wins_over_checks_prepared_before_it() {
    let dir = Dir::new("first-address");
    let config = "StrictHostKeyChecking accept-new\nCheckHostIP yes";
    // `c.example` is known; the address is not. All four read the files before any records.
    let known = line("c.example", &ed25519(13));
    let a = check_host(&dir, "a.example", &known, config, None).expect("check");
    let b = check_host(&dir, "b.example", &known, config, None).expect("check");
    let c = check_host(&dir, "c.example", &known, config, None).expect("check");
    let d = check_host(&dir, "d.example", &known, config, None).expect("check");
    let address = Some("192.0.2.7");
    assert_eq!(judge_at(&a, &ed25519(11), address), Ok(()));
    // A new host at the recorded address with another key: the address's key is changed.
    let refused = judge_at(&b, &ed25519(12), address).expect_err("changed address");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 2, .. }),
        "{refused:?}"
    );
    // A known host whose address only is new: the address is judged again too.
    let refused = judge_at(&c, &ed25519(13), address).expect_err("changed address");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 2, .. }),
        "{refused:?}"
    );
    // The same key at the address is written for the new name only.
    assert_eq!(judge_at(&d, &ed25519(11), address), Ok(()));
    assert_eq!(
        known_file(&dir),
        format!(
            "{known}{}{}",
            line("a.example,192.0.2.7", &ed25519(11)),
            line("d.example", &ed25519(11))
        )
    );
}

#[test]
fn the_address_record_of_a_name_with_a_port_is_judged_again() {
    let dir = Dir::new("first-port");
    let config = "StrictHostKeyChecking accept-new\nCheckHostIP yes\nPort 2222";
    // `[h.example]:2222` is not known and `h.example` is, so the name passes by its lookup
    // without the port and only the address `[192.0.2.8]:2222` is new.
    let known = line("h.example", &ed25519(11));
    let c = check(&dir, &known, config, None).expect("check");
    // Another connection records the address with another key after the files were read.
    std::fs::write(
        dir.0.join("known_hosts"),
        format!("{known}{}", line("[192.0.2.8]:2222", &ed25519(12))),
    )
    .expect("known_hosts");
    let refused = judge_at(&c, &ed25519(11), Some("192.0.2.8")).expect_err("changed address");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 2, .. }),
        "{refused:?}"
    );
}

#[test]
fn a_known_name_is_judged_again_when_only_its_address_is_written() {
    let config = "StrictHostKeyChecking accept-new\nCheckHostIP yes";
    let known = line("h.example", &ed25519(11));
    let address = Some("192.0.2.10");
    // The name's key changes after the files were read.
    let dir = Dir::new("name-changed");
    let c = check(&dir, &known, config, None).expect("check");
    std::fs::write(dir.0.join("known_hosts"), line("h.example", &ed25519(12))).expect("rewrite");
    let refused = judge_at(&c, &ed25519(11), address).expect_err("changed name");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 1, .. }),
        "{refused:?}"
    );
    assert_eq!(known_file(&dir), line("h.example", &ed25519(12)));
    // The name's key is revoked after the files were read.
    let dir = Dir::new("name-revoked");
    let c = check(&dir, &known, config, None).expect("check");
    let revoked = format!("{known}@revoked {}", line("h.example", &ed25519(11)));
    std::fs::write(dir.0.join("known_hosts"), &revoked).expect("revoke");
    let refused = judge_at(&c, &ed25519(11), address).expect_err("revoked name");
    assert!(
        matches!(refused.reason, HostKeyReason::Revoked { .. }),
        "{refused:?}"
    );
    assert_eq!(known_file(&dir), revoked);
}

#[test]
fn a_known_address_is_judged_again_when_only_the_name_is_written() {
    let dir = Dir::new("address-changed");
    let config = "StrictHostKeyChecking accept-new\nCheckHostIP yes";
    let c = check(&dir, &line("192.0.2.11", &ed25519(11)), config, None).expect("check");
    // The address's key changes after the files were read.
    std::fs::write(dir.0.join("known_hosts"), line("192.0.2.11", &ed25519(12))).expect("rewrite");
    let refused = judge_at(&c, &ed25519(11), Some("192.0.2.11")).expect_err("changed address");
    assert!(
        matches!(refused.reason, HostKeyReason::Changed { line: 1, .. }),
        "{refused:?}"
    );
    assert_eq!(known_file(&dir), line("192.0.2.11", &ed25519(12)));
}

/// A name known when the files were read whose line is removed before the record (only its
/// address was to be written) is a new host: the checking mode decides it as any new host.
#[test]
fn a_known_name_removed_before_the_record_is_decided_by_the_mode() {
    struct Yes;
    impl SshPrompt for Yes {
        fn confirm_host_key(&self, q: &HostKeyQuestion) -> bool {
            q.host == "h.example"
        }
    }
    let known = line("h.example", &ed25519(11));
    let address = Some("192.0.2.12");
    let run = |name: &str, mode: &str, prompt: Option<Arc<dyn SshPrompt>>, now: &str| {
        let dir = Dir::new(name);
        let config = format!("StrictHostKeyChecking {mode}\nCheckHostIP yes");
        let c = check(&dir, &known, &config, prompt).expect("check");
        std::fs::write(dir.0.join("known_hosts"), now).expect("rewrite");
        (judge_at(&c, &ed25519(11), address), known_file(&dir))
    };
    // `yes` refuses the host it does not know now, and writes nothing.
    let (verdict, file) = run("removed-yes", "yes", None, "");
    assert_eq!(verdict.expect_err("unknown").reason, HostKeyReason::Unknown);
    assert_eq!(file, "");
    // `ask` without a prompt refuses; with a yes it writes both names.
    let (verdict, file) = run("removed-ask-none", "ask", None, "");
    assert_eq!(verdict.expect_err("unknown").reason, HostKeyReason::Unknown);
    assert_eq!(file, "");
    let (verdict, file) = run("removed-ask-yes", "ask", Some(Arc::new(Yes)), "");
    assert_eq!(verdict, Ok(()));
    assert_eq!(file, line("h.example,192.0.2.12", &ed25519(11)));
    // `accept-new` writes the name as well as the address.
    let (verdict, file) = run("removed-new", "accept-new", None, "");
    assert_eq!(verdict, Ok(()));
    assert_eq!(file, line("h.example,192.0.2.12", &ed25519(11)));
    // A `@cert-authority` line that is not trusted leaves the name unknown in the same way.
    let ca = format!("@cert-authority {}", line("h.example", &ed25519(20)));
    let (verdict, file) = run("removed-ca-yes", "yes", None, &ca);
    assert_eq!(verdict.expect_err("unknown").reason, HostKeyReason::Unknown);
    assert_eq!(file, ca);
}

/// The name removed and the address key changed or revoked in the same update of the files:
/// the address is judged before the checking mode decides the name, so the connection is
/// refused as changed or revoked and nothing is asked.
#[test]
fn an_address_changed_with_the_name_removed_is_refused_before_the_mode() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counting(AtomicUsize);
    impl SshPrompt for Counting {
        fn confirm_host_key(&self, _: &HostKeyQuestion) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }
    let known = line("h.example", &ed25519(11));
    let address = Some("192.0.2.13");
    let changed = line("192.0.2.13", &ed25519(12));
    let revoked = format!("@revoked {}", line("192.0.2.13", &ed25519(11)));
    for mode in ["yes", "ask"] {
        for now in [&changed, &revoked] {
            let dir = Dir::new(&format!("removed-address-{mode}"));
            let prompt = Arc::new(Counting(AtomicUsize::new(0)));
            let config = format!("StrictHostKeyChecking {mode}\nCheckHostIP yes");
            let c = check(&dir, &known, &config, Some(prompt.clone())).expect("check");
            std::fs::write(dir.0.join("known_hosts"), now).expect("rewrite");
            let refused = judge_at(&c, &ed25519(11), address).expect_err("refused");
            if now == &changed {
                assert!(
                    matches!(refused.reason, HostKeyReason::Changed { line: 1, .. }),
                    "{mode}: {refused:?}"
                );
            } else {
                assert!(
                    matches!(refused.reason, HostKeyReason::Revoked { .. }),
                    "{mode}: {refused:?}"
                );
            }
            assert_eq!(prompt.0.load(Ordering::SeqCst), 0, "{mode}: asked");
            assert_eq!(&known_file(&dir), now);
        }
    }
}

/// A name with a port that passed by its lookup without the port: the record judges it again
/// with the same judgement as before connecting, on the files as they are then. The lines the
/// lookup without the port found are replaced in one update by a removed host line and a
/// changed or `@revoked` key of the address without its port. That is a new host for that
/// judgement, as for a new connection to these files (`ssh` likewise, `sshconnect.c:1181`–
/// `:1196`): `yes` refuses it as unknown, `ask` asks once and with a yes writes both names
/// with the port.
#[test]
fn a_name_passed_without_its_port_is_judged_again_as_at_the_start() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counting(AtomicUsize);
    impl SshPrompt for Counting {
        fn confirm_host_key(&self, _: &HostKeyQuestion) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }
    let known = format!(
        "{}{}",
        line("h.example", &ed25519(11)),
        line("192.0.2.14", &ed25519(11))
    );
    let address = Some("192.0.2.14");
    let revoked = format!("@revoked {}", line("192.0.2.14", &ed25519(11)));
    let changed = line("192.0.2.14", &ed25519(12));
    for now in [&revoked, &changed] {
        for mode in ["yes", "ask"] {
            let config = format!("StrictHostKeyChecking {mode}\nCheckHostIP yes\nPort 2222");
            let dir = Dir::new(&format!("port-rejudged-{mode}"));
            let prompt = Arc::new(Counting(AtomicUsize::new(0)));
            let c = check(&dir, &known, &config, Some(prompt.clone())).expect("check");
            std::fs::write(dir.0.join("known_hosts"), now).expect("rewrite");
            let verdict = judge_at(&c, &ed25519(11), address);
            // A new connection to the files as they are now judges the same.
            let fresh_dir = Dir::new(&format!("port-fresh-{mode}"));
            let fresh = check(&fresh_dir, now, &config, None).expect("check");
            let fresh = judge_at(&fresh, &ed25519(11), address);
            if mode == "yes" {
                assert_eq!(verdict.expect_err("unknown").reason, HostKeyReason::Unknown);
                assert_eq!(prompt.0.load(Ordering::SeqCst), 0);
                assert_eq!(&known_file(&dir), now);
                assert_eq!(fresh.expect_err("unknown").reason, HostKeyReason::Unknown);
            } else {
                assert_eq!(verdict, Ok(()));
                assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
                assert_eq!(
                    known_file(&dir),
                    format!(
                        "{now}{}",
                        line("[h.example]:2222,[192.0.2.14]:2222", &ed25519(11))
                    )
                );
                // Without a prompt `ask` refuses the new host, as `yes` does.
                assert_eq!(fresh.expect_err("unknown").reason, HostKeyReason::Unknown);
            }
        }
    }
}

/// A prompt that counts its questions, writes `text` to the known_hosts file of `dir` while it
/// is asked (another program saving a line then), and accepts.
struct WritingPrompt {
    file: PathBuf,
    text: String,
    asked: std::sync::atomic::AtomicUsize,
}

impl SshPrompt for WritingPrompt {
    fn confirm_host_key(&self, _: &HostKeyQuestion) -> bool {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(&self.file, &self.text).expect("write while asked");
        true
    }
}

/// The notes of `c` with the file shown by its name only, to compare two folders.
fn notes_of(c: &HostCheck) -> Vec<(String, NoteClass, Option<usize>, Option<String>)> {
    c.notes
        .lock()
        .expect("notes")
        .iter()
        .map(|n| {
            (
                n.keyword.clone(),
                n.class,
                n.line,
                n.file
                    .as_ref()
                    .and_then(|f| f.file_name())
                    .map(|f| f.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// The notes and the result follow the judgement under the record lock, also after the
/// checking mode accepted the key: a `@cert-authority` line written while the question was
/// open is noted as a new connection to that file notes it, and the question is asked once.
#[test]
fn a_cert_authority_line_written_during_the_question_is_noted() {
    let dir = Dir::new("ca-during");
    let ca = format!("@cert-authority {}", line("h.example", &ed25519(21)));
    let prompt = Arc::new(WritingPrompt {
        file: dir.0.join("known_hosts"),
        text: ca.clone(),
        asked: Default::default(),
    });
    let c = check(&dir, "", "StrictHostKeyChecking ask", Some(prompt.clone())).expect("check");
    assert_eq!(judge(&c, &ed25519(11)), Ok(()));
    assert_eq!(prompt.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        known_file(&dir),
        format!("{ca}{}", line("h.example", &ed25519(11)))
    );
    let notes = notes_of(&c);
    assert_eq!(
        notes,
        vec![(
            "@cert-authority".to_string(),
            NoteClass::CertAuthorityDistrusted,
            Some(1),
            Some("known_hosts".to_string())
        )]
    );
    // A new connection to the file as it was under the lock.
    let fresh_dir = Dir::new("ca-fresh");
    let yes = Arc::new(WritingPrompt {
        file: fresh_dir.0.join("unused"),
        text: String::new(),
        asked: Default::default(),
    });
    let fresh = check(&fresh_dir, &ca, "StrictHostKeyChecking ask", Some(yes)).expect("check");
    assert_eq!(judge(&fresh, &ed25519(11)), Ok(()));
    assert_eq!(notes_of(&fresh), notes);
}

/// The other way: a `@cert-authority` line noted before the question, and the host's key saved
/// by another program while the question was open. Under the lock the host is known, so the
/// key is not written again and nothing is noted, as for a new connection to that file.
#[test]
fn a_key_saved_during_the_question_leaves_the_notes_of_a_known_host() {
    let dir = Dir::new("saved-during");
    let ca = format!("@cert-authority {}", line("h.example", &ed25519(21)));
    let saved = format!("{ca}{}", line("h.example", &ed25519(11)));
    let prompt = Arc::new(WritingPrompt {
        file: dir.0.join("known_hosts"),
        text: saved.clone(),
        asked: Default::default(),
    });
    let c = check(&dir, &ca, "StrictHostKeyChecking ask", Some(prompt.clone())).expect("check");
    assert_eq!(judge(&c, &ed25519(11)), Ok(()));
    assert_eq!(prompt.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(known_file(&dir), saved);
    assert_eq!(notes_of(&c), Vec::new());
    let fresh_dir = Dir::new("saved-fresh");
    let fresh = check(&fresh_dir, &saved, "StrictHostKeyChecking ask", None).expect("check");
    assert_eq!(judge(&fresh, &ed25519(11)), Ok(()));
    assert_eq!(notes_of(&fresh), Vec::new());
}
