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
            "tasty-net-ssh-hostkey-{}-{}-{name}",
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
    let kh = dir.write("known_hosts", known);
    let cfg = dir.write(
        "config",
        &format!(
            "UserKnownHostsFile {}\nGlobalKnownHostsFile none\n{config}\n",
            kh.display()
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(check.judge(
            &PublicKeyOrCertificate::PublicKey {
                key: key.clone(),
                hash_alg: None,
            },
            None,
        ))
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
        &format!("RevokedHostKeys {}", missing.display()),
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
        &format!("RevokedHostKeys {}", list.display()),
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
        &format!("RevokedHostKeys {}", other.display()),
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
        &format!("RevokedHostKeys {}", krl.display()),
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
            "UserKnownHostsFile {} {}\nGlobalKnownHostsFile none\nStrictHostKeyChecking accept-new\n",
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
    // The folder of the first file is made, as `ssh` makes `~/.ssh`.
    assert_eq!(
        std::fs::read_to_string(&first).expect("first"),
        line("h.example", &ed25519(10))
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&second)).expect("second"),
        ""
    );
}
