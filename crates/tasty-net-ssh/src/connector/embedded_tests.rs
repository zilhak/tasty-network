//! Backend selection with the embedded backend built, and its resolution before connecting
//! (docs/design/ssh-backends.md "시험 목록" 폴백 선택, 미지원 지시어). Runs on every OS.

use std::path::PathBuf;

use super::*;
use crate::BackendReason;
use crate::inspect::{NoteAction, NoteClass, RefusalNote};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tasty-net-ssh-embedded-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove {}: {e}", self.0.display());
        }
    }
}

fn settings(backend: &str) -> SshSettings {
    let mut settings = SshSettings::new();
    settings.set_text("ssh.backend", backend).expect("backend");
    settings
}

#[cfg(feature = "system-ssh")]
#[test]
fn auto_falls_back_to_embedded_only_when_the_program_file_is_missing() {
    let missing = std::env::temp_dir().join("tasty-net-ssh-embedded-no-such-ssh");
    let connector = SshConnector::new(settings("auto")).with_launcher(SshLauncher::new(&missing));
    let selected = connector.select(&SshTarget::parse("h")).expect("fallback");
    assert_eq!(selected.backend, Some(SshBackendKind::Embedded));
    assert_eq!(
        selected.reason,
        BackendReason::NotFound {
            searched: vec![missing.clone()]
        }
    );
    // The reason names what was looked at.
    assert_eq!(selected.reason.to_string(), "not_found (1 files looked at)");
    assert_eq!(selected.program(), None);
    let inspection = connector.inspect(None, SshPurpose::Tunnel);
    assert_eq!(
        inspection
            .backend
            .as_ref()
            .map(|s| (s.backend, s.reason.clone())),
        Ok((Some(SshBackendKind::Embedded), selected.reason.clone()))
    );

    // `system` does not fall back.
    let forced = SshConnector::new(settings("system")).with_launcher(SshLauncher::new(&missing));
    assert_eq!(
        forced.select(&SshTarget::parse("h")),
        Err(SshBackendError::NotFound {
            searched: vec![missing.clone()]
        })
    );

    // A found file keeps the system backend.
    let exe = std::env::current_exe().expect("exe");
    let found = SshConnector::new(settings("auto")).with_launcher(SshLauncher::new(&exe));
    let selected = found.select(&SshTarget::parse("h")).expect("found");
    assert_eq!(selected.backend, Some(SshBackendKind::System));
    assert_eq!(selected.reason, BackendReason::Found { path: exe });
}

#[cfg(not(feature = "system-ssh"))]
#[test]
fn auto_without_the_system_backend_built_is_embedded() {
    let connector = SshConnector::new(settings("auto"));
    let selected = connector.select(&SshTarget::parse("h")).expect("embedded");
    assert_eq!(selected.backend, Some(SshBackendKind::Embedded));
    assert_eq!(selected.reason, BackendReason::SystemNotBuilt);
    assert!(
        SshSettings::new()
            .set_text("ssh.backend", "system")
            .is_err()
    );
}

#[test]
fn embedded_forced_by_the_setting_is_configured() {
    let connector = SshConnector::new(settings("embedded"));
    #[cfg(feature = "system-ssh")]
    let connector =
        connector.with_launcher(SshLauncher::new(std::env::current_exe().expect("exe")));
    let selected = connector.select(&SshTarget::parse("h")).expect("embedded");
    assert_eq!(selected.backend, Some(SshBackendKind::Embedded));
    assert_eq!(selected.reason, BackendReason::Configured);
}

/// An embedded connector reading only `config` (no system file), with its home in `dir`.
fn embedded_with(dir: &Dir, config: &str) -> (SshConnector, SelectedBackend) {
    let file = dir.0.join("config");
    std::fs::write(&file, config).expect("config");
    let mut s = settings("embedded");
    s.set_text("embedded.config_file", &file.to_string_lossy())
        .expect("config_file");
    s.set_text("embedded.system_config_file", "none")
        .expect("system");
    s.set_text("embedded.home", &dir.0.to_string_lossy())
        .expect("home");
    s.set_text("embedded.local_user", "u").expect("user");
    let connector = SshConnector::new(s);
    let selected = connector.select(&SshTarget::parse("h")).expect("embedded");
    (connector, selected)
}

fn refusal(connector: &SshConnector, selected: &SelectedBackend) -> RefusalNote {
    match connector.resolve(
        selected,
        &SshTarget::parse("h.example"),
        &SshPurpose::Tunnel,
    ) {
        Err(SshBackendError::ConfigRefused { refusal, .. }) => *refusal,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn unsupported_directives_are_shown_and_host_key_checking_is_kept() {
    let dir = Dir::new("shown");
    let (connector, selected) = embedded_with(
        &dir,
        "Host *\n  GSSAPIAuthentication yes\n  StrictHostKeyChecking no\n  ChannelTimeout session=5\n",
    );
    let (resolved, notes) = connector
        .resolve(
            &selected,
            &SshTarget::parse("h.example"),
            &SshPurpose::Tunnel,
        )
        .expect("resolved")
        .expect("the embedded backend resolves");
    assert_eq!(resolved.hostname, "h.example");
    assert_eq!(resolved.user.as_deref(), Some("u"));
    let has = |keyword: &str, class: NoteClass, line: usize| {
        notes.iter().any(|n| {
            n.keyword.eq_ignore_ascii_case(keyword)
                && n.class == class
                && n.action == NoteAction::Shown
                && n.line == Some(line)
        })
    };
    assert!(
        has("gssapiauthentication", NoteClass::NotSupported, 2),
        "{notes:?}"
    );
    assert!(
        has("stricthostkeychecking", NoteClass::HostKeyCheckKept, 3),
        "{notes:?}"
    );
    // The channel timers are not in this stage: shown rather than dropped.
    assert!(
        has("channeltimeout", NoteClass::NotSupported, 4),
        "{notes:?}"
    );
}

#[test]
fn security_directives_the_backend_lacks_refuse_before_connecting() {
    for (line, keyword) in [
        ("ProxyCommand nc %h %p", "proxycommand"),
        ("KnownHostsCommand /bin/true", "knownhostscommand"),
        ("VerifyHostKeyDNS yes", "verifyhostkeydns"),
        ("PubkeyAuthentication host-bound", "pubkeyauthentication"),
    ] {
        let dir = Dir::new("refused");
        let (connector, selected) = embedded_with(&dir, &format!("{line}\n"));
        let refusal = refusal(&connector, &selected);
        assert_eq!(
            refusal.kind, "security_not_implemented",
            "{line}: {refusal}"
        );
        assert_eq!(
            refusal.keyword.map(|k| k.to_ascii_lowercase()).as_deref(),
            Some(keyword),
            "{line}"
        );
        // The settings query shows the same refusal without connecting.
        let inspection =
            connector.inspect(Some(&SshTarget::parse("h.example")), SshPurpose::Tunnel);
        assert_eq!(
            inspection.target.and_then(|t| t.refusal).map(|r| r.kind),
            Some("security_not_implemented"),
            "{line}"
        );
    }
}

#[test]
fn an_unknown_keyword_in_a_file_refuses_like_openssh() {
    let dir = Dir::new("unknown");
    let (connector, selected) = embedded_with(&dir, "NoSuchKeyword yes\n");
    let refusal = refusal(&connector, &selected);
    assert_eq!(refusal.kind, "unknown_keyword", "{refusal}");
}
