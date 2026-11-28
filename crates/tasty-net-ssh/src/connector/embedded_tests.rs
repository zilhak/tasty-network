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

#[test]
fn the_resolved_target_carries_only_the_values_that_choose_the_target() {
    let dir = Dir::new("sources");
    let (connector, selected) = embedded_with(
        &dir,
        "Host *\n  HostName real.example\n  Port 2222\n  SetEnv TOKEN=s3cret-value\n  SendEnv LANG\n  \
         ConnectionAttempts 2\n  NumberOfPasswordPrompts 1\n",
    );
    let (resolved, _) = connector
        .resolve(
            &selected,
            &SshTarget::parse("h.example"),
            &SshPurpose::Tunnel,
        )
        .expect("resolved")
        .expect("the embedded backend resolves");
    let keywords: Vec<&str> = resolved
        .sources
        .iter()
        .map(|s| s.keyword.as_str())
        .collect();
    assert!(keywords.contains(&"hostname"), "{keywords:?}");
    assert!(keywords.contains(&"port"), "{keywords:?}");
    // The counts that are no settings value can be looked up too.
    let value = |k: &str| {
        resolved
            .sources
            .iter()
            .find(|s| s.keyword == k)
            .map(|s| s.value.clone())
    };
    assert_eq!(
        value("connectionattempts").as_deref(),
        Some("2"),
        "{keywords:?}"
    );
    assert_eq!(
        value("numberofpasswordprompts").as_deref(),
        Some("1"),
        "{keywords:?}"
    );
    assert!(!keywords.contains(&"setenv"), "{keywords:?}");
    assert!(!keywords.contains(&"sendenv"), "{keywords:?}");
    assert!(!format!("{resolved:?}").contains("s3cret"), "{resolved:?}");
}

/// SFTP resolves as the `sftp` subsystem (`ssh -s <target> sftp`): `Match sessiontype
/// subsystem` and `Match command sftp` hold for it and for no other purpose.
#[test]
fn match_sessiontype_subsystem_and_command_sftp_hold_for_sftp_only() {
    let dir = Dir::new("sftp-match");
    let (connector, _) = embedded_with(
        &dir,
        "Match sessiontype subsystem\n  User by-subsystem\nMatch command sftp\n  Port 2201\n\
         Host *\n  User other\n  Port 22\n",
    );
    let seen = |purpose: SshPurpose| {
        let inspection = connector.inspect(Some(&SshTarget::parse("h.example")), purpose);
        let target = inspection.target.expect("target");
        assert_eq!(target.refusal, None);
        let resolved = target.resolved.expect("resolved");
        (resolved.user, resolved.port)
    };
    assert_eq!(
        seen(SshPurpose::Sftp),
        (Some("by-subsystem".to_string()), 2201)
    );
    for purpose in [
        SshPurpose::Tunnel,
        SshPurpose::Discovery,
        SshPurpose::Exec {
            command: "ls".to_string(),
        },
    ] {
        let shown = format!("{purpose:?}");
        assert_eq!(seen(purpose), (Some("other".to_string()), 22), "{shown}");
    }
}

#[test]
fn an_unwritten_count_shows_the_value_in_use_from_the_default() {
    let dir = Dir::new("count-defaults");
    let (connector, selected) = embedded_with(&dir, "Host *\n  Port 2222\n");
    let (resolved, _) = connector
        .resolve(
            &selected,
            &SshTarget::parse("h.example"),
            &SshPurpose::Tunnel,
        )
        .expect("resolved")
        .expect("the embedded backend resolves");
    let entry = |k: &str| {
        resolved
            .sources
            .iter()
            .find(|s| s.keyword == k)
            .map(|s| (s.value.clone(), s.source.clone()))
    };
    // OpenSSH: ConnectionAttempts 1 and NumberOfPasswordPrompts 3 when no line writes them.
    assert_eq!(
        entry("connectionattempts"),
        Some(("1".to_string(), "default".to_string())),
        "{resolved:?}"
    );
    assert_eq!(
        entry("numberofpasswordprompts"),
        Some(("3".to_string(), "default".to_string())),
        "{resolved:?}"
    );
}

/// OpenSSH `sshkey_perm_ok` on desktop Unix: a key file the group or others can read is not
/// used.
#[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
#[test]
fn a_key_file_others_can_read_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("perm");
    let key = dir.0.join("id_test");
    std::fs::write(&key, "not a key").expect("key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let judged =
        || crate::embedded::session::key_permissions_ok(&std::fs::File::open(&key).expect("open"));
    assert!(judged());
    for mode in [0o640, 0o604, 0o660, 0o644] {
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).expect("chmod");
        assert!(!judged(), "{mode:o}");
    }
    assert_eq!(NoteClass::KeyPermissions.name(), "bad_key_permissions");
}

/// A server that takes the TCP connection and never says anything: the session never opens.
fn silent_server() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("addr").port();
    (listener, port)
}

/// The embedded port discovery's run on `connector` for `h.example`.
fn discover_once(
    connector: &SshConnector,
    budget: std::time::Duration,
) -> Result<String, crate::discovery::PortDiscoveryError> {
    discover_on(connector, &SshTarget::parse("h.example"), budget)
}

/// [`discover_once`] for `target`.
fn discover_on(
    connector: &SshConnector,
    target: &SshTarget,
    budget: std::time::Duration,
) -> Result<String, crate::discovery::PortDiscoveryError> {
    use crate::discovery::RemoteRun;
    let run = crate::embedded::discovery::EmbeddedRun {
        ctx: connector.embedded_ctx(),
        target,
    };
    let limits = crate::discovery::Limits::new(connector.settings(), target);
    run.run(&["echo", "x"], budget, "test discovery", limits)
}

#[test]
fn port_discovery_stops_at_its_budget_while_the_session_opens() {
    let (_listener, port) = silent_server();
    let dir = Dir::new("budget");
    // The connect timeout alone would wait a minute.
    let (connector, _) = embedded_with(
        &dir,
        &format!("Host *\n  HostName 127.0.0.1\n  Port {port}\n  ConnectTimeout 60\n"),
    );
    let started = std::time::Instant::now();
    let error =
        discover_once(&connector, std::time::Duration::from_millis(500)).expect_err("no session");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::TimedOut,
        "{error}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn port_discovery_stops_at_a_cancel_while_the_session_opens() {
    let (_listener, port) = silent_server();
    let dir = Dir::new("cancel");
    let (connector, _) = embedded_with(
        &dir,
        &format!("Host *\n  HostName 127.0.0.1\n  Port {port}\n  ConnectTimeout 60\n"),
    );
    let cancel = crate::SshCancel::new();
    let later = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        later.cancel();
    });
    let started = std::time::Instant::now();
    let error = {
        let _scope = cancel.scope();
        discover_once(&connector, std::time::Duration::from_secs(60)).expect_err("cancelled")
    };
    stopper.join().expect("stopper");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::Cancelled,
        "{error}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn an_open_the_server_never_answers_leaves_no_task_on_the_session() {
    use crate::embedded::test_server::ExecReply;
    let dir = Dir::new("never-open");
    let (_server, connector) = on_test_server(&dir, ExecReply::NeverOpen);
    let mut settings = connector.settings().clone();
    settings
        .set(
            "embedded.channel_open_timeout",
            crate::settings::Value::Duration(std::time::Duration::from_millis(300)),
        )
        .expect("channel_open_timeout");
    let connector = SshConnector::new(settings);
    // A tunnel holds the session the discovery shares.
    let held = {
        use crate::embedded::resolve::{Purpose, load, resolve_chain};
        let ctx = connector.embedded_ctx();
        let set = load(ctx.settings).expect("load");
        let chain = resolve_chain(
            &set,
            ctx.settings,
            &SshTarget::parse("h.example"),
            Purpose::Tunnel,
            ctx.verify,
        )
        .expect("chain");
        crate::embedded::session::open(ctx, &chain).expect("session")
    };
    let before = Arc::strong_count(&held.conn.handle);
    for _ in 0..3 {
        let error =
            discover_once(&connector, std::time::Duration::from_secs(2)).expect_err("no channel");
        assert_ne!(
            error.kind,
            crate::discovery::PortDiscoveryFailureKind::Cancelled,
            "{error}"
        );
    }
    // Each open task held the session's handle; once the late wait gives up on it (one more
    // open timeout) the task is gone.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while Arc::strong_count(&held.conn.handle) > before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(Arc::strong_count(&held.conn.handle), before);
}

/// A connector for `h.example` on the test server, with its key and known_hosts in `dir`.
fn on_test_server(
    dir: &Dir,
    exec: crate::embedded::test_server::ExecReply,
) -> (crate::embedded::test_server::TestServer, SshConnector) {
    use crate::embedded::test_server::{DirectReply, TestServer};
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start(&key, exec, DirectReply::Refuse(2));
    std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
    let (connector, _) = embedded_with(dir, &server.config(&key, &known));
    (server, connector)
}

/// Whether the server saw a close of every `exec` channel it saw.
fn every_exec_closed(events: &[crate::embedded::test_server::Event]) -> bool {
    use crate::embedded::test_server::Event;
    let execs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Exec(c) => Some(*c),
            _ => None,
        })
        .collect();
    !execs.is_empty() && execs.iter().all(|c| events.contains(&Event::Close(*c)))
}

#[test]
fn port_discovery_closes_its_channel_at_the_budget() {
    use crate::embedded::test_server::ExecReply;
    let dir = Dir::new("close-budget");
    let (server, connector) = on_test_server(&dir, ExecReply::Silent);
    let error =
        discover_once(&connector, std::time::Duration::from_secs(2)).expect_err("no answer");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::TimedOut,
        "{error}"
    );
    assert!(
        server.wait_for(std::time::Duration::from_secs(5), every_exec_closed),
        "{:?}",
        server.events()
    );
}

#[test]
fn port_discovery_closes_a_channel_that_opens_after_its_budget() {
    use crate::embedded::test_server::{Event, ExecReply};
    let dir = Dir::new("close-late");
    let (server, connector) = on_test_server(
        &dir,
        ExecReply::LateOpen(std::time::Duration::from_millis(2500)),
    );
    // A session of the tunnel purpose stays open, as a tunnel holds it, and the discovery
    // shares it (the purposes differ only in the values of each channel).
    let held = {
        use crate::embedded::resolve::{Purpose, load, resolve_chain};
        let ctx = connector.embedded_ctx();
        let set = load(ctx.settings).expect("load");
        let chain = resolve_chain(
            &set,
            ctx.settings,
            &SshTarget::parse("h.example"),
            Purpose::Tunnel,
            ctx.verify,
        )
        .expect("chain");
        crate::embedded::session::open(ctx, &chain).expect("session")
    };
    let error =
        discover_once(&connector, std::time::Duration::from_secs(2)).expect_err("no channel");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::TimedOut,
        "{error}"
    );
    // The confirmation comes after the discovery gave up, and its channel is closed.
    let late_closed = |events: &[Event]| {
        server
            .opened()
            .iter()
            .any(|c| events.contains(&Event::Close(*c)))
    };
    assert!(
        server.wait_for(std::time::Duration::from_secs(10), late_closed),
        "{:?} {:?}",
        server.opened(),
        server.events()
    );
    drop(held);
}

#[test]
fn port_discovery_closes_its_channel_at_a_cancel() {
    use crate::embedded::test_server::ExecReply;
    let dir = Dir::new("close-cancel");
    let (server, connector) = on_test_server(&dir, ExecReply::Silent);
    let cancel = crate::SshCancel::new();
    let later = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        later.cancel();
    });
    let error = {
        let _scope = cancel.scope();
        discover_once(&connector, std::time::Duration::from_secs(60)).expect_err("cancelled")
    };
    stopper.join().expect("stopper");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::Cancelled,
        "{error}"
    );
    assert!(
        server.wait_for(std::time::Duration::from_secs(5), every_exec_closed),
        "{:?}",
        server.events()
    );
}

#[test]
fn port_discovery_closes_its_channel_at_the_output_limit() {
    use crate::embedded::test_server::ExecReply;
    let dir = Dir::new("close-limit");
    let limit = crate::discovery::Limits::new(&SshSettings::new(), &SshTarget::parse("h.example"))
        .output_limit;
    let (server, connector) = on_test_server(&dir, ExecReply::Flood(limit + 1));
    let error =
        discover_once(&connector, std::time::Duration::from_secs(20)).expect_err("too long");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::PortParseFailed,
        "{error}"
    );
    assert!(
        server.wait_for(std::time::Duration::from_secs(5), every_exec_closed),
        "{:?}",
        server.events()
    );
}

/// Opens a tunnel session of `connector` to `h.example`.
fn open_session(connector: &SshConnector) -> Result<(), SshBackendError> {
    open_session_to(connector, &SshTarget::parse("h.example"))
}

/// [`open_session`] to `target`.
fn open_session_to(connector: &SshConnector, target: &SshTarget) -> Result<(), SshBackendError> {
    use crate::embedded::resolve::{Purpose, load, resolve_chain};
    let ctx = connector.embedded_ctx();
    let set = load(ctx.settings)?;
    let chain = resolve_chain(&set, ctx.settings, target, Purpose::Tunnel, ctx.verify)?;
    crate::embedded::session::open(ctx, &chain).map(drop)
}

/// Answers passwords with `password` and passphrases with `passphrase`, and counts the
/// passphrase questions.
#[derive(Default)]
struct CountingPrompt {
    password: Option<&'static str>,
    passphrase: Option<&'static str>,
    passphrases: std::sync::atomic::AtomicUsize,
}

impl crate::SshPrompt for CountingPrompt {
    fn passphrase(&self, _key_file: &std::path::Path) -> Option<String> {
        self.passphrases
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.passphrase.map(str::to_string)
    }

    fn password(&self, _user: &str, _host: &str) -> Option<String> {
        self.password.map(str::to_string)
    }
}

/// A connector on the test server with `rule`, the server's configuration changed by `edit`,
/// and `prompt`.
fn on_auth_server(
    dir: &Dir,
    rule: crate::embedded::test_server::AuthRule,
    edit: impl Fn(String) -> String,
    prompt: Arc<CountingPrompt>,
) -> (crate::embedded::test_server::TestServer, SshConnector) {
    on_auth_server_for(
        dir,
        crate::embedded::test_server::client_key(),
        rule,
        edit,
        prompt,
    )
}

/// [`on_auth_server`] with `client` as the key file `id_test` and the key the server lets in.
fn on_auth_server_for(
    dir: &Dir,
    client: russh::keys::PrivateKey,
    rule: crate::embedded::test_server::AuthRule,
    edit: impl Fn(String) -> String,
    prompt: Arc<CountingPrompt>,
) -> (crate::embedded::test_server::TestServer, SshConnector) {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start_for(
        &key,
        client,
        ExecReply::Silent,
        DirectReply::Refuse(2),
        rule,
    );
    std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
    let (connector, _) = embedded_with(dir, &edit(server.config(&key, &known)));
    (server, connector.with_prompt(prompt))
}

/// Writes `text` as a key file only the user can read.
fn write_key_file(path: &std::path::Path, text: &str) {
    if let Err(e) = std::fs::remove_file(path) {
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path).expect("key file"), text.as_bytes())
        .expect("write key");
}

#[cfg(unix)]
#[test]
fn the_agent_signs_for_a_key_file_it_holds_before_the_file_is_read() {
    use crate::embedded::test_server::{AuthRule, client_key};
    use russh::keys::ssh_key::{Cipher, Kdf, LineEnding};
    let dir = Dir::new("agent");
    let prompt = Arc::new(CountingPrompt::default());
    let socket = dir.0.join("a.sock");
    let quoted = format!("IdentityAgent \"{}\"", socket.display());
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::Key,
        |c| c.replace("IdentityAgent none", &quoted),
        Arc::clone(&prompt),
    );
    // The key file is encrypted; its public part is readable without the passphrase.
    let encrypted = client_key()
        .encrypt_with(
            Cipher::Aes256Ctr,
            Kdf::Bcrypt {
                salt: vec![1; 16],
                rounds: 2,
            },
            7,
            "secret",
        )
        .expect("encrypt");
    write_key_file(
        &dir.0.join("id_test"),
        &encrypted.to_openssh(LineEnding::LF).expect("encode"),
    );
    server.agent(&socket, &[client_key()]);
    open_session(&connector).expect("the agent signs");
    assert_eq!(
        prompt.passphrases.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no passphrase is asked for a key the agent holds"
    );
}

/// A user certificate of the client key, signed by a test CA.
fn user_certificate() -> russh::keys::Certificate {
    certificate_of(&crate::embedded::test_server::client_key())
}

/// A user certificate of `key` for the user `u`, signed by a test CA.
fn certificate_of(key: &russh::keys::PrivateKey) -> russh::keys::Certificate {
    use russh::keys::ssh_key::certificate::{Builder, CertType};
    use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
    let ca = russh::keys::PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[11; 32])),
        "test ca",
    )
    .expect("ca");
    let mut builder = Builder::new(
        vec![5; 16],
        key.public_key().key_data().clone(),
        0,
        u64::MAX,
    )
    .expect("cert");
    builder.cert_type(CertType::User).expect("type");
    builder.valid_principal("u").expect("principal");
    builder.sign(&ca).expect("sign")
}

#[test]
fn a_certificate_outside_pubkey_accepted_algorithms_is_not_offered() {
    use crate::embedded::test_server::{AuthRule, Event};
    let offered = |accepted: &str| {
        let dir = Dir::new("cert");
        let cert = dir.0.join("id_cert.pub");
        std::fs::write(&cert, user_certificate().to_openssh().expect("encode cert"))
            .expect("cert file");
        let line = format!("CertificateFile \"{}\"\n{accepted}", cert.display());
        let (server, connector) = on_auth_server(
            &dir,
            AuthRule::Key,
            |c| format!("{c}  {line}\n"),
            Arc::new(CountingPrompt::default()),
        );
        open_session(&connector).expect("the key gets in");
        server.events().contains(&Event::Cert)
    };
    // Without a restriction the certificate is offered first; with only ssh-ed25519 it is not.
    assert!(offered(""));
    assert!(!offered("  PubkeyAcceptedAlgorithms ssh-ed25519"));
}

#[test]
fn a_refused_tunnel_channel_keeps_its_reason_code() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    use std::io::Read;
    for code in [1, 2, 3, 4] {
        let dir = Dir::new("refused");
        let key = dir.0.join("id_test");
        let known = dir.0.join("known_hosts");
        let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Refuse(code));
        std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
        let (connector, selected) = embedded_with(&dir, &server.config(&key, &known));
        let tunnel = connector
            .tunnel(&selected, &SshTarget::parse("h.example"), 4100, None)
            .expect("tunnel");
        let (verdict, mut stream) = connect_named(&tunnel);
        let mut buf = [0u8; 1];
        let read = stream.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        let dropped = crate::connection_closed_error("the channel closed");
        let refused = verdict.channel_open_refused(&dropped);
        if code == 2 {
            assert_eq!(refused, None);
            assert!(verdict.remote_listener_absent(&dropped));
        } else {
            assert_eq!(refused, Some(crate::ChannelOpenRefused { code }));
            assert!(!verdict.remote_listener_absent(&dropped));
        }
    }
}

#[test]
fn concurrent_forwards_on_one_tunnel_get_only_their_own_refusal_code() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    use std::io::Read;
    let dir = Dir::new("refused-many");
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start(&key, ExecReply::Silent, DirectReply::ByPort);
    std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
    let (connector, selected) = embedded_with(&dir, &server.config(&key, &known));
    let tunnel = connector
        .tunnel(&selected, &SshTarget::parse("h.example"), 4100, None)
        .expect("tunnel");
    // All the connections are open before any is read, so their channels open and are refused
    // at the same time on the one session.
    let streams: Vec<_> = (0..12)
        .map(|_| {
            let (verdict, stream) = connect_named(&tunnel);
            let local = stream.local_addr().expect("local address");
            (verdict, local, stream)
        })
        .collect();
    let dropped = crate::connection_closed_error("the channel closed");
    for (verdict, local, mut stream) in streams {
        let mut buf = [0u8; 1];
        let read = stream.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        // A channel that opened and then ended gives no code either.
        let want = DirectReply::by_port(u32::from(local.port()))
            .map(|code| crate::ChannelOpenRefused { code });
        assert_eq!(verdict.channel_open_refused(&dropped), want, "{local}");
    }
}

/// The tunnel keeps nothing for its connections: many forwards that open and end,
/// connections closed before any verdict names them (as a cancelled attempt does), and named
/// connections that stay open leave nothing behind once accepted; a verdict whose connection
/// never comes is dropped with the verdict.
#[test]
fn ended_forwards_leave_no_slot_in_the_tunnel() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    use std::io::Read;
    let dir = Dir::new("slots-released");
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Open);
    std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
    let (connector, selected) = embedded_with(&dir, &server.config(&key, &known));
    let tunnel = connector
        .tunnel(&selected, &SshTarget::parse("h.example"), 4100, None)
        .expect("tunnel");
    let probe = tunnel.listener_verdict();
    let connect =
        || std::net::TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
    let settled = |want: usize| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while probe.kept() > want && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        probe.kept()
    };
    // Forwards whose channels opened and ended, read to their end without a verdict.
    for _ in 0..40 {
        let mut stream = connect();
        let mut rest = Vec::new();
        let _read = stream.read_to_end(&mut rest);
    }
    assert_eq!(probe.kept(), 0);
    // Named connections that stay open: the accepts take their verdicts' claims.
    let open: Vec<_> = (0..3).map(|_| connect_named(&tunnel)).collect();
    assert_eq!(settled(0), 0);
    drop(open);
    // Connections closed before a verdict names them (a cancelled attempt).
    for _ in 0..10 {
        drop(connect());
    }
    assert_eq!(probe.kept(), 0);
    // A verdict named for a connection that never comes waits until it is dropped.
    let waiting = tunnel
        .listener_verdict()
        .for_local(std::net::SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            9,
        )));
    assert_eq!(probe.kept(), 1);
    drop(waiting);
    // Connections still open when the tunnel is terminated.
    let open: Vec<_> = (0..3).map(|_| connect_named(&tunnel)).collect();
    tunnel.terminate_and_reap().expect("terminate");
    drop(open);
    drop(tunnel);
    assert_eq!(probe.kept(), 0);
}

#[test]
fn a_refusal_of_the_discovery_purpose_only_stays_a_configuration_refusal() {
    let dir = Dir::new("discovery-refused");
    // Only the port discovery meets the security directive: it runs a command, which
    // `Match sessiontype` sees as `exec` (`readconf.c` `match_cfg_line`); the tunnel is `none`.
    let (connector, selected) = embedded_with(
        &dir,
        "Host *\n  HostName 127.0.0.1\n  Port 9\nMatch sessiontype exec\n  ProxyCommand nc %h %p\n",
    );
    let target = SshTarget::parse("h.example");
    assert!(
        connector
            .resolve(&selected, &target, &SshPurpose::Tunnel)
            .is_ok(),
        "the tunnel purpose is not refused"
    );
    let error = connector
        .discover_remote_port(
            &selected,
            &target,
            crate::PortMode::Auto,
            &crate::PortDiscoveryConfig {
                remote_program: "tasty".into(),
                remote_args: vec!["port".into()],
                data_dir: ".tasty".into(),
                port_file: "port".into(),
                explicit_port_file: None,
            },
            crate::decode_port,
        )
        .expect_err("refused");
    match error.config_refused() {
        Some(SshBackendError::ConfigRefused { refusal, .. }) => {
            assert_eq!(
                refusal.keyword.as_deref(),
                Some("proxycommand"),
                "{refusal:?}"
            );
        }
        other => panic!("not a configuration refusal: {other:?} ({error:?})"),
    }
}

#[test]
fn a_partial_success_starts_the_method_order_over() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("partial");
    let prompt = Arc::new(CountingPrompt {
        password: Some("pw"),
        ..CountingPrompt::default()
    });
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::PasswordThenKey("pw"),
        |c| format!("{c}  PreferredAuthentications publickey,password\n"),
        prompt,
    );
    open_session(&connector).expect("password, then the key again");
    let events = server.events();
    let password = events
        .iter()
        .position(|e| *e == Event::Password)
        .expect("a password");
    assert!(
        events[..password]
            .iter()
            .any(|e| matches!(e, Event::Key(_)))
            && events[password..]
                .iter()
                .any(|e| matches!(e, Event::Key(_))),
        "{events:?}"
    );
}

#[test]
fn a_failure_with_another_list_picks_the_method_again_before_the_next_key() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("list-changes");
    let prompt = Arc::new(CountingPrompt {
        password: Some("pw"),
        ..CountingPrompt::default()
    });
    let second = dir.0.join("id_second");
    write_key_file(
        &second,
        &rsa_key()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode"),
    );
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::PasswordAfterAKey("pw"),
        |c| {
            format!(
                "{c}  IdentityFile \"{}\"\n  PreferredAuthentications publickey,password\n",
                second.display()
            )
        },
        prompt,
    );
    open_session(&connector).expect("the password after the first key");
    let events = server.events();
    // The first key's failure leaves only `password`: the second key is not sent.
    assert!(events.contains(&Event::Password), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Key(name) if name.contains("rsa"))),
        "{events:?}"
    );
}

#[test]
fn a_failure_with_another_list_keeps_the_next_public_key() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("keys-kept");
    let first = dir.0.join("id_first");
    write_key_file(
        &first,
        &rsa_key()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode"),
    );
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::KeyAmidChangingLists,
        |c| {
            // The key the server refuses comes first.
            c.replacen(
                "  IdentityFile ",
                &format!("  IdentityFile \"{}\"\n  IdentityFile ", first.display()),
                1,
            )
        },
        Arc::new(CountingPrompt::default()),
    );
    open_session(&connector).expect("the second key after a failure with another list");
    let events = server.events();
    // `ssh` keeps the keys tried when the list changes without a partial success
    // (`sshconnect2.c` `input_userauth_failure`): the refused key is not offered again.
    let refused = events
        .iter()
        .filter(|e| matches!(e, Event::Key(name) if name.contains("rsa")))
        .count();
    assert_eq!(refused, 1, "{events:?}");
}

/// The key list is made once per authentication (`pubkey_prepare`): an agent key refused and
/// then dropped by the agent does not move the next key, so the key file after it is tried.
#[cfg(unix)]
#[test]
fn a_key_the_agent_drops_after_its_refusal_does_not_skip_the_next_key() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("agent-drops");
    let socket = dir.0.join("a.sock");
    let quoted = format!("IdentityAgent \"{}\"", socket.display());
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::KeyAmidChangingLists,
        |c| {
            c.replace("  IdentitiesOnly yes\n", "  IdentitiesOnly no\n")
                .replace("IdentityAgent none", &quoted)
        },
        Arc::new(CountingPrompt::default()),
    );
    server.agent(&socket, &[rsa_key()]);
    let gone = socket.clone();
    server.on_first_refusal(move || async move {
        let mut agent = russh::keys::agent::client::AgentClient::connect_uds(&gone)
            .await
            .expect("agent");
        // `remove_identity`: the fixed russh's `remove_all_identities` leaves its length field
        // zero (`keys/agent/client.rs:573`–`:579`).
        agent
            .remove_identity(rsa_key().public_key())
            .await
            .expect("forget");
    });
    open_session(&connector).expect("the key file after the dropped agent key");
    let events = server.events();
    let refused = events
        .iter()
        .filter(|e| matches!(e, Event::Key(name) if name.contains("rsa")))
        .count();
    assert_eq!(refused, 1, "{events:?}");
}

/// An agent key added after a refusal would sit in front of the key files (agent keys come
/// first in the list), but the list is made once per authentication (`pubkey_prepare`): the
/// new agent key is not offered, the refused key file is not offered again, and the next key
/// file of the list is tried.
#[cfg(unix)]
#[test]
fn an_agent_key_added_after_a_refusal_does_not_send_the_refused_key_again() {
    use crate::embedded::test_server::{AuthRule, Event};
    use russh::keys::ssh_key::private::{KeypairData, RsaKeypair};
    let dir = Dir::new("agent-adds");
    let socket = dir.0.join("a.sock");
    let first = dir.0.join("id_first");
    write_key_file(
        &first,
        &rsa_key()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode"),
    );
    let quoted = format!("IdentityAgent \"{}\"", socket.display());
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::KeyAmidChangingLists,
        |c| {
            // The refused RSA key file first, then the key file the server lets in.
            c.replacen(
                "  IdentityFile ",
                &format!("  IdentityFile \"{}\"\n  IdentityFile ", first.display()),
                1,
            )
            .replace("  IdentitiesOnly yes\n", "  IdentitiesOnly no\n")
            .replace("IdentityAgent none", &quoted)
        },
        Arc::new(CountingPrompt::default()),
    );
    server.agent(&socket, &[]);
    // Another RSA key, so an offer of it shows as a second RSA key.
    let added = russh::keys::PrivateKey::new(
        KeypairData::Rsa(RsaKeypair::random(&mut TestRng(43), 1024).expect("rsa key")),
        "added",
    )
    .expect("added key");
    let path = socket.clone();
    server.on_first_refusal(move || async move {
        let mut agent = russh::keys::agent::client::AgentClient::connect_uds(&path)
            .await
            .expect("agent");
        agent.add_identity(&added, &[]).await.expect("add");
    });
    open_session(&connector).expect("the key file after the refused one");
    let events = server.events();
    let rsa = events
        .iter()
        .filter(|e| matches!(e, Event::Key(name) if name.contains("rsa")))
        .count();
    assert_eq!(rsa, 1, "{events:?}");
}

/// A certificate file that appears during the authentication is not put in front of the keys
/// (the list is made once), so the refused key is not sent again.
#[test]
fn a_certificate_that_appears_later_does_not_send_a_refused_key_again() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("cert-appears");
    let first = dir.0.join("id_first");
    write_key_file(
        &first,
        &rsa_key()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .expect("encode"),
    );
    let cert = dir.0.join("id_late-cert.pub");
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::KeyAmidChangingLists,
        |c| {
            c.replacen(
                "  IdentityFile ",
                &format!(
                    "  CertificateFile \"{}\"\n  IdentityFile \"{}\"\n  IdentityFile ",
                    cert.display(),
                    first.display()
                ),
                1,
            )
        },
        Arc::new(CountingPrompt::default()),
    );
    let text = certificate_of(&rsa_key())
        .to_openssh()
        .expect("encode cert");
    let late = cert.clone();
    server.on_first_refusal(move || async move {
        std::fs::write(&late, text).expect("cert file");
    });
    open_session(&connector).expect("the second key file");
    let events = server.events();
    let refused = events
        .iter()
        .filter(|e| matches!(e, Event::Key(name) if name.contains("rsa")))
        .count();
    assert_eq!(refused, 1, "{events:?}");
    assert!(!events.contains(&Event::Cert), "{events:?}");
}

#[test]
fn password_questions_add_up_over_changing_lists() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("passwords-add-up");
    let prompt = Arc::new(CountingPrompt {
        password: Some("wrong"),
        ..CountingPrompt::default()
    });
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::PasswordAmidChangingLists,
        |c| format!("{c}  PreferredAuthentications password\n  NumberOfPasswordPrompts 2\n"),
        prompt,
    );
    let err = open_session(&connector).expect_err("every password fails");
    assert!(matches!(err, SshBackendError::AuthFailed { .. }), "{err:?}");
    // `attempt_passwd` of the authentication context (`sshconnect2.c:1050`): the questions of
    // every entry of the method count against `NumberOfPasswordPrompts`.
    let events = server.events();
    let passwords = events.iter().filter(|e| **e == Event::Password).count();
    assert_eq!(passwords, 2, "{events:?}");
}

/// `connector` with `embedded.auth_timeout` set to `timeout`.
fn with_auth_timeout(
    connector: SshConnector,
    timeout: std::time::Duration,
    prompt: Arc<CountingPrompt>,
) -> SshConnector {
    let mut settings = connector.settings().clone();
    settings
        .set(
            "embedded.auth_timeout",
            crate::settings::Value::Duration(timeout),
        )
        .expect("auth_timeout");
    SshConnector::new(settings).with_prompt(prompt)
}

#[test]
fn authentication_that_never_ends_stops_at_the_auth_timeout() {
    use crate::embedded::test_server::AuthRule;
    let dir = Dir::new("endless");
    let prompt = Arc::new(CountingPrompt::default());
    let (_server, connector) =
        on_auth_server(&dir, AuthRule::EndlessPartial, |c| c, Arc::clone(&prompt));
    // `ssh.connect_timeout` (10 s by default) does not bound authentication.
    let connector = with_auth_timeout(connector, std::time::Duration::from_secs(1), prompt);
    let started = std::time::Instant::now();
    let err = open_session(&connector).expect_err("a server without an end");
    assert!(
        matches!(err, crate::SshBackendError::TimedOut { .. }),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn authentication_longer_than_the_connect_timeout_gets_in_within_the_auth_timeout() {
    use crate::embedded::test_server::AuthRule;
    let dir = Dir::new("slow-auth");
    let (_server, connector) = on_auth_server(
        &dir,
        AuthRule::SlowKey(std::time::Duration::from_secs(2)),
        |c| c,
        Arc::new(CountingPrompt::default()),
    );
    // `ConnectTimeout` bounds the TCP connection and the banner only, as in OpenSSH.
    let mut target = SshTarget::parse("h.example");
    target.extra_options.push("ConnectTimeout=1".into());
    open_session_to(&connector, &target).expect("the slow server lets the key in");
}

#[test]
fn an_encrypted_pkcs8_key_file_asks_for_its_passphrase() {
    use crate::embedded::test_server::{AuthRule, client_key};
    let dir = Dir::new("pkcs8");
    let prompt = Arc::new(CountingPrompt {
        passphrase: Some("secret"),
        ..CountingPrompt::default()
    });
    let (_server, connector) = on_auth_server(&dir, AuthRule::Key, |c| c, Arc::clone(&prompt));
    let mut pem = Vec::new();
    russh::keys::encode_pkcs8_pem_encrypted(&client_key(), b"secret", 2, &mut pem).expect("encode");
    write_key_file(
        &dir.0.join("id_test"),
        std::str::from_utf8(&pem).expect("pem text"),
    );
    open_session(&connector).expect("the key gets in with its passphrase");
    assert_eq!(
        prompt.passphrases.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the passphrase is asked once"
    );
}

/// The note of a key file skipped for its permissions reaches the caller with the failure.
#[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
#[test]
fn a_failed_authentication_carries_the_note_of_a_key_file_not_used() {
    use crate::embedded::test_server::AuthRule;
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("perm-failure");
    let (_server, connector) = on_auth_server(
        &dir,
        AuthRule::Key,
        |c| c,
        Arc::new(CountingPrompt::default()),
    );
    std::fs::set_permissions(
        dir.0.join("id_test"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");
    match open_session(&connector) {
        Err(SshBackendError::AuthFailed { notes, .. }) => assert!(
            notes
                .iter()
                .any(|n| n.class == NoteClass::KeyPermissions && n.keyword == "identityfile"),
            "{notes:?}"
        ),
        other => panic!("not an authentication failure: {other:?}"),
    }
}

/// An `IdentityFile` that names a public key file finds the agent's key by that public key
/// (`sshkey_load_public` reads the named file first), with `IdentitiesOnly`.
#[cfg(unix)]
#[test]
fn an_identity_file_that_names_a_public_key_signs_with_the_agent() {
    use crate::embedded::test_server::{AuthRule, client_key};
    let dir = Dir::new("agent-pub");
    let socket = dir.0.join("a.sock");
    let private = dir.0.join("id_test");
    let public = dir.0.join("named.pub");
    let (from, to) = (
        format!("\"{}\"", private.display()),
        format!("\"{}\"", public.display()),
    );
    let agent_line = format!("IdentityAgent \"{}\"", socket.display());
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::Key,
        |c| {
            c.replace(&from, &to)
                .replace("IdentityAgent none", &agent_line)
        },
        Arc::new(CountingPrompt::default()),
    );
    let config = std::fs::read_to_string(dir.0.join("config")).expect("config");
    assert!(
        config.contains(&to),
        "the configuration names the public file"
    );
    std::fs::write(
        &public,
        client_key()
            .public_key()
            .to_openssh()
            .expect("encode public"),
    )
    .expect("public file");
    // No private key file: only the agent can sign.
    std::fs::remove_file(&private).expect("remove the private file");
    server.agent(&socket, &[client_key()]);
    open_session(&connector).expect("the agent signs for the named public key");
}

/// A deterministic random source for test keys (splitmix64): the keys are made in the test.
struct TestRng(u64);

impl russh::keys::ssh_key::rand_core::TryRng for TestRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok((self.try_next_u64()? >> 32) as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Ok(z ^ (z >> 31))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dst.chunks_mut(8) {
            let word = self.try_next_u64()?.to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}

impl russh::keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

/// A 1024-bit RSA test key, made once per test process.
fn rsa_key() -> russh::keys::PrivateKey {
    use russh::keys::ssh_key::private::{KeypairData, RsaKeypair};
    static KEY: std::sync::OnceLock<russh::keys::PrivateKey> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let pair = RsaKeypair::random(&mut TestRng(41), 1024).expect("rsa key");
        russh::keys::PrivateKey::new(KeypairData::Rsa(pair), "tasty-net-ssh-rsa-test")
            .expect("rsa test key")
    })
    .clone()
}

/// A key file signs an RSA certificate with the SHA-2 hash that the certificate name was
/// judged with, so a `PubkeyAcceptedAlgorithms` with only the SHA-2 certificate names gets in.
#[test]
fn an_rsa_certificate_is_signed_with_the_sha2_hash_of_its_name() {
    use crate::embedded::test_server::{AuthRule, Event};
    let dir = Dir::new("rsa-cert");
    let key = rsa_key();
    let cert = dir.0.join("id_cert.pub");
    std::fs::write(
        &cert,
        certificate_of(&key).to_openssh().expect("encode cert"),
    )
    .expect("cert file");
    let line = format!(
        "CertificateFile \"{}\"\n  PubkeyAcceptedAlgorithms rsa-sha2-512-cert-v01@openssh.com,rsa-sha2-256-cert-v01@openssh.com",
        cert.display()
    );
    let (server, connector) = on_auth_server_for(
        &dir,
        key,
        AuthRule::Cert,
        |c| format!("{c}  {line}\n"),
        Arc::new(CountingPrompt::default()),
    );
    open_session(&connector).expect("the RSA certificate gets in");
    let events = server.events();
    assert!(events.contains(&Event::Cert), "{events:?}");
    // The server refuses a request that names SHA-1, and saw only SHA-2 names.
    let requested: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Requested(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(!requested.is_empty(), "{events:?}");
    assert!(
        requested
            .iter()
            .all(|name| *name == "rsa-sha2-512" || *name == "rsa-sha2-256"),
        "{events:?}"
    );
}

/// The signature the key file signer appends names the RSA hash of the request.
#[test]
fn the_key_file_signer_uses_the_hash_it_is_given() {
    use russh::Signer as _;
    use russh::keys::HashAlg;
    use russh::keys::ssh_encoding::Decode as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let key = Arc::new(rsa_key());
    let identity = russh::keys::agent::AgentIdentity::PublicKey {
        key: key.public_key().clone(),
        comment: String::new(),
    };
    for (hash, name) in [
        (Some(HashAlg::Sha512), "rsa-sha2-512"),
        (Some(HashAlg::Sha256), "rsa-sha2-256"),
        (None, "ssh-rsa"),
    ] {
        let mut signer = crate::embedded::session::KeySigner(Arc::clone(&key));
        let out = runtime
            .block_on(signer.auth_sign(&identity, hash, b"data".to_vec()))
            .expect("signed");
        assert_eq!(&out[..4], b"data");
        let mut rest = &out[4..];
        let blob = Vec::<u8>::decode(&mut rest).expect("signature blob");
        let mut blob = &blob[..];
        assert_eq!(String::decode(&mut blob).expect("name"), name);
    }
}

/// The agent signs a certificate whose plain key it holds, with that plain key as the signing
/// identity (`sign_and_send_pubkey`), with no key file to fall back on.
#[cfg(unix)]
#[test]
fn a_certificate_is_signed_with_the_agent_plain_key() {
    use crate::embedded::test_server::{AuthRule, Event, client_key};
    let dir = Dir::new("agent-cert");
    let socket = dir.0.join("a.sock");
    let cert = dir.0.join("id_cert.pub");
    std::fs::write(&cert, user_certificate().to_openssh().expect("encode cert"))
        .expect("cert file");
    let lines = format!(
        "CertificateFile \"{}\"\n  IdentityAgent \"{}\"",
        cert.display(),
        socket.display()
    );
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::Cert,
        |c| format!("{}  {lines}\n", c.replace("  IdentityAgent none\n", "")),
        Arc::new(CountingPrompt::default()),
    );
    std::fs::remove_file(dir.0.join("id_test")).expect("remove the private file");
    server.agent(&socket, &[client_key()]);
    open_session(&connector).expect("the agent signs the certificate");
    assert!(server.events().contains(&Event::Cert));
}

/// The key file that signs a `CertificateFile` certificate is the first `IdentityFile` with
/// the certificate's public key, as in `ssh` (`sshconnect2.c:1355`–`:1378`): when it cannot
/// sign (here a public key only), the certificate is not tried with a later file of the same
/// key; the key itself still gets in.
#[test]
fn a_certificate_whose_first_key_file_cannot_sign_is_not_tried_with_a_later_one() {
    use crate::embedded::test_server::{AuthRule, Event, client_key};
    let cert_sent = |public_first: bool| {
        let dir = Dir::new("cert-first-file");
        let cert = dir.0.join("id_cert.pub");
        std::fs::write(&cert, user_certificate().to_openssh().expect("encode cert"))
            .expect("cert file");
        let public = dir.0.join("id_public_only");
        std::fs::write(
            &public,
            client_key()
                .public_key()
                .to_openssh()
                .expect("encode public key"),
        )
        .expect("public file");
        let first = if public_first {
            format!("  IdentityFile \"{}\"\n", public.display())
        } else {
            String::new()
        };
        let (server, connector) = on_auth_server(
            &dir,
            AuthRule::Key,
            |c| {
                let c = c.replacen("  IdentityFile ", &format!("{first}  IdentityFile "), 1);
                format!("{c}  CertificateFile \"{}\"\n", cert.display())
            },
            Arc::new(CountingPrompt::default()),
        );
        open_session(&connector).expect("the key gets in");
        server.events().contains(&Event::Cert)
    };
    // The private key file alone signs the certificate; behind a public key only it does not.
    assert!(cert_sent(false));
    assert!(!cert_sent(true));
}

/// An agent that refuses to sign a certificate's plain key does not end the certificate: its
/// key file signs it, also when the agent holds that key file.
#[cfg(unix)]
#[test]
fn a_certificate_the_agent_refuses_to_sign_is_signed_by_its_key_file() {
    use crate::embedded::test_server::{AuthRule, Event, client_key};
    let dir = Dir::new("agent-refuses");
    let socket = dir.0.join("a.sock");
    let cert = dir.0.join("id_cert.pub");
    std::fs::write(&cert, user_certificate().to_openssh().expect("encode cert"))
        .expect("cert file");
    let lines = format!(
        "CertificateFile \"{}\"\n  IdentityAgent \"{}\"",
        cert.display(),
        socket.display()
    );
    let (server, connector) = on_auth_server(
        &dir,
        AuthRule::Cert,
        |c| format!("{}  {lines}\n", c.replace("  IdentityAgent none\n", "")),
        Arc::new(CountingPrompt::default()),
    );
    server.refusing_agent(&socket, &[client_key()]);
    open_session(&connector).expect("the key file signs the certificate");
    assert!(server.events().contains(&Event::Cert));
}

/// A certificate smaller than `RequiredRSASize` is not offered: the key file signs it without
/// the setting and nothing is sent with it.
#[cfg(unix)]
#[test]
fn a_certificate_smaller_than_required_rsa_size_is_not_offered() {
    use crate::embedded::test_server::{AuthRule, Event};
    let offered = |required: &str| {
        let dir = Dir::new("rsa-size");
        let key = rsa_key();
        let cert = dir.0.join("id_cert.pub");
        std::fs::write(
            &cert,
            certificate_of(&key).to_openssh().expect("encode cert"),
        )
        .expect("cert file");
        let lines = format!("CertificateFile \"{}\"\n{required}", cert.display());
        let (server, connector) = on_auth_server_for(
            &dir,
            key,
            AuthRule::Key,
            |c| format!("{c}  {lines}\n"),
            Arc::new(CountingPrompt::default()),
        );
        let _attempted = open_session(&connector).is_ok();
        server.events().contains(&Event::Cert)
    };
    assert!(offered(""));
    assert!(!offered("  RequiredRSASize 2048"));
}

/// Makes a FIFO at `path` (`mkfifo`) only the user can read and write: a file that does not
/// answer, as a network file system that hangs.
#[cfg(unix)]
fn make_fifo(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    const OWNER_ONLY: u32 = 0o600;
    let made = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY)).expect("fifo mode");
}

/// Opens the FIFO `path` to write, which returns once something opened it to read: a read of
/// it has started then, and waits for data until the returned end is dropped (its end of file).
/// Fails after `limit` when nothing opens it.
#[cfg(unix)]
fn reader_of(path: &std::path::Path, limit: std::time::Duration) -> std::fs::File {
    let (tx, rx) = std::sync::mpsc::channel();
    let fifo = path.to_path_buf();
    std::thread::spawn(move || {
        let opened = std::fs::OpenOptions::new().write(true).open(&fifo);
        // The test may have given up waiting already.
        tx.send(opened).unwrap_or_default();
    });
    rx.recv_timeout(limit)
        .expect("nothing opened the FIFO to read")
        .expect("open the FIFO to write")
}

/// An embedded connector reading only the configuration file `config`, with its home in `dir`.
#[cfg(unix)]
fn embedded_on_file(dir: &Dir, config: &std::path::Path) -> SshConnector {
    let mut s = settings("embedded");
    s.set_text("embedded.config_file", &config.to_string_lossy())
        .expect("config_file");
    s.set_text("embedded.system_config_file", "none")
        .expect("system");
    s.set_text("embedded.home", &dir.0.to_string_lossy())
        .expect("home");
    s.set_text("embedded.local_user", "u").expect("user");
    SshConnector::new(s)
}

/// The port discovery of the connector `on` makes for a FIFO stops at its budget and at a
/// cancel once the read of the FIFO started. Each step has its own FIFO in `dir`
/// (`<name>-budget`, `<name>-cancel`), so the cancel is seen while its own read waits.
#[cfg(unix)]
fn fifo_reads_stop_at_the_budget_and_a_cancel(
    dir: &Dir,
    name: &str,
    on: impl Fn(&std::path::Path) -> SshConnector,
) {
    use crate::discovery::PortDiscoveryFailureKind;
    let limit = std::time::Duration::from_secs(10);

    let fifo = dir.0.join(format!("{name}-budget"));
    make_fifo(&fifo);
    let connector = on(&fifo);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let out = discover_once(&connector, std::time::Duration::from_millis(500));
        tx.send((out, started.elapsed())).unwrap_or_default();
    });
    let end = reader_of(&fifo, limit);
    let (out, elapsed) = rx
        .recv_timeout(limit)
        .expect("the budget did not stop the read");
    let error = out.expect_err("no answer");
    assert_eq!(error.kind, PortDiscoveryFailureKind::TimedOut, "{error}");
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    drop(end);

    let fifo = dir.0.join(format!("{name}-cancel"));
    make_fifo(&fifo);
    let connector = on(&fifo);
    let cancel = crate::SshCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let scoped = cancel.clone();
    std::thread::spawn(move || {
        let _scope = scoped.scope();
        let out = discover_once(&connector, std::time::Duration::from_secs(60));
        tx.send(out).unwrap_or_default();
    });
    let end = reader_of(&fifo, limit);
    cancel.cancel();
    let error = rx
        .recv_timeout(limit)
        .expect("the cancel did not stop the read")
        .expect_err("cancelled");
    assert_eq!(error.kind, PortDiscoveryFailureKind::Cancelled, "{error}");
    drop(end);
}

/// The configuration files are read under the discovery budget and the cancel: a file that
/// does not answer stops neither (docs/design/ssh-backends.md "내장 백엔드: 세션 하나에 채널
/// 여럿").
#[cfg(unix)]
#[test]
fn a_configuration_file_that_does_not_answer_stops_neither_the_budget_nor_a_cancel() {
    let dir = Dir::new("config-fifo");
    fifo_reads_stop_at_the_budget_and_a_cancel(&dir, "config", |fifo| embedded_on_file(&dir, fifo));
}

/// The known_hosts files read before connecting are read under the discovery budget and the
/// cancel too.
#[cfg(unix)]
#[test]
fn a_known_hosts_file_that_does_not_answer_stops_neither_the_budget_nor_a_cancel() {
    let (_listener, port) = silent_server();
    let dir = Dir::new("known-hosts-fifo");
    fifo_reads_stop_at_the_budget_and_a_cancel(&dir, "known_hosts", |fifo| {
        let config = std::path::PathBuf::from(format!("{}.config", fifo.display()));
        std::fs::write(
            &config,
            format!(
                "Host *\n  HostName 127.0.0.1\n  Port {port}\n  UserKnownHostsFile \"{}\"\n  \
                 ConnectTimeout 60\n",
                fifo.display()
            ),
        )
        .expect("config");
        embedded_on_file(&dir, &config)
    });
}

/// The text of the first known_hosts file of [`RecordCase`], which a record would change.
#[cfg(unix)]
const RECORD_BEFORE: &str = "# the first file\n";

/// A test server whose host is new to two known_hosts files under `accept-new`: the first one
/// (writable, holding [`RECORD_BEFORE`]), where the key is recorded, and a second one that is a
/// link to a FIFO. The read before connecting reads the FIFO `early`; the record's read again
/// (under the record lock) reads the FIFO `again`, once [`RecordCase::reread_waits`] moved the
/// link, so each read is caught on its own FIFO.
#[cfg(unix)]
struct RecordCase {
    _server: crate::embedded::test_server::TestServer,
    first: std::path::PathBuf,
    link: std::path::PathBuf,
    early: std::path::PathBuf,
    again: std::path::PathBuf,
    config: String,
}

#[cfg(unix)]
impl RecordCase {
    fn new(dir: &Dir) -> Self {
        use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
        let key = dir.0.join("id_test");
        let first = dir.0.join("known_hosts");
        let link = dir.0.join("known_hosts_link");
        let early = dir.0.join("known_hosts_early");
        let again = dir.0.join("known_hosts_again");
        let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Refuse(2));
        std::fs::write(&first, RECORD_BEFORE).expect("known_hosts");
        make_fifo(&early);
        make_fifo(&again);
        std::os::unix::fs::symlink(&early, &link).expect("link");
        let config = server
            .config(&key, &first)
            .replace(
                "StrictHostKeyChecking yes",
                "StrictHostKeyChecking accept-new",
            )
            .replace(
                &format!("UserKnownHostsFile \"{}\"", first.display()),
                &format!(
                    "UserKnownHostsFile \"{}\" \"{}\"",
                    first.display(),
                    link.display()
                ),
            );
        assert!(config.contains("accept-new") && config.contains("known_hosts_link"));
        Self {
            _server: server,
            first,
            link,
            early,
            again,
            config,
        }
    }

    /// Ends the read before connecting (the end of the FIFO `early`), points the link at the
    /// FIFO `again` and returns once the record's read again waits on it: its writing end,
    /// whose drop ends that read.
    fn reread_waits(&self, limit: std::time::Duration) -> std::fs::File {
        drop(reader_of(&self.early, limit));
        let moved = self.link.with_extension("moved");
        std::os::unix::fs::symlink(&self.again, &moved).expect("link");
        std::fs::rename(&moved, &self.link).expect("move the link");
        reader_of(&self.again, limit)
    }

    /// Waits for the record to end and checks the first file is as it was.
    fn first_file_unchanged(&self) {
        crate::embedded::hostkey::records_done();
        assert_eq!(
            std::fs::read_to_string(&self.first).expect("known_hosts"),
            RECORD_BEFORE
        );
    }
}

/// An `accept-new` record whose read of the known_hosts files again (under the record lock)
/// ends after the discovery was cancelled writes nothing.
#[cfg(unix)]
#[test]
fn a_host_key_is_not_recorded_after_the_discovery_is_cancelled() {
    let limit = std::time::Duration::from_secs(10);
    let dir = Dir::new("record-cancel");
    let case = RecordCase::new(&dir);
    let (connector, _) = embedded_with(&dir, &case.config);
    let cancel = crate::SshCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let scoped = cancel.clone();
    std::thread::spawn(move || {
        let _scope = scoped.scope();
        let out = discover_once(&connector, std::time::Duration::from_secs(60));
        tx.send(out).unwrap_or_default();
    });
    let again = case.reread_waits(limit);
    cancel.cancel();
    let error = rx
        .recv_timeout(limit)
        .expect("the cancel did not stop the discovery")
        .expect_err("cancelled");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::Cancelled,
        "{error}"
    );
    drop(again);
    case.first_file_unchanged();
}

/// An `accept-new` record whose read again ends after the key exchange's own deadline
/// (`embedded.auth_timeout`) gave the handshake up writes nothing either, although the
/// discovery's budget and cancel did not stop.
#[cfg(unix)]
#[test]
fn a_host_key_is_not_recorded_after_the_key_exchange_deadline() {
    let limit = std::time::Duration::from_secs(10);
    let budget = std::time::Duration::from_secs(60);
    let dir = Dir::new("record-auth-timeout");
    let case = RecordCase::new(&dir);
    let (connector, _) = embedded_with(&dir, &case.config);
    let mut settings = connector.settings().clone();
    settings
        .set(
            "embedded.auth_timeout",
            crate::settings::Value::Duration(std::time::Duration::from_secs(1)),
        )
        .expect("auth_timeout");
    let connector = SshConnector::new(settings);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let out = discover_once(&connector, budget);
        tx.send((out, started.elapsed())).unwrap_or_default();
    });
    let again = case.reread_waits(limit);
    let (out, elapsed) = rx
        .recv_timeout(limit)
        .expect("the key exchange deadline did not stop the discovery");
    let error = out.expect_err("the key exchange did not end");
    assert_ne!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::Cancelled,
        "{error}"
    );
    assert!(elapsed < budget, "{elapsed:?}");
    drop(again);
    case.first_file_unchanged();
}

/// Cancels `cancel` from inside the host key question, then trusts the key.
#[cfg(unix)]
struct CancellingPrompt {
    cancel: crate::SshCancel,
    asked: std::sync::atomic::AtomicUsize,
}

#[cfg(unix)]
impl crate::SshPrompt for CancellingPrompt {
    fn confirm_host_key(&self, _question: &crate::prompt::HostKeyQuestion) -> bool {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cancel.cancel();
        true
    }
}

/// A host key trusted in its question after the discovery was cancelled there is not
/// recorded, and the connection ends: the session is neither put in the pool nor shown to the
/// session hook, even when it would open before the wait looks at the cancel again
/// (`discovery.poll_max` at its longest).
#[cfg(unix)]
#[test]
fn a_session_whose_discovery_was_cancelled_in_the_host_key_question_is_not_kept() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    let limit = std::time::Duration::from_secs(10);
    let dir = Dir::new("record-ask-cancel");
    let key = dir.0.join("id_test");
    let first = dir.0.join("known_hosts");
    let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Refuse(2));
    std::fs::write(&first, RECORD_BEFORE).expect("known_hosts");
    let config = server
        .config(&key, &first)
        .replace("StrictHostKeyChecking yes", "StrictHostKeyChecking ask");
    assert!(config.contains("StrictHostKeyChecking ask"));
    let (connector, _) = embedded_with(&dir, &config);
    let mut settings = connector.settings().clone();
    settings
        .set(
            "discovery.poll_max",
            crate::settings::Value::Duration(std::time::Duration::from_secs(1)),
        )
        .expect("poll_max");
    let cancel = crate::SshCancel::new();
    let prompt = Arc::new(CancellingPrompt {
        cancel: cancel.clone(),
        asked: std::sync::atomic::AtomicUsize::new(0),
    });
    let hooked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(
        SshConnector::new(settings)
            .with_prompt(Arc::clone(&prompt) as Arc<dyn crate::SshPrompt>)
            .with_session_hook(crate::embedded::SessionHook::new({
                let hooked = Arc::clone(&hooked);
                move |_| {
                    hooked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let (scoped, on) = (cancel.clone(), Arc::clone(&connector));
    std::thread::spawn(move || {
        let _scope = scoped.scope();
        let out = discover_once(&on, std::time::Duration::from_secs(60));
        tx.send(out).unwrap_or_default();
    });
    let error = rx
        .recv_timeout(limit)
        .expect("the discovery did not end")
        .expect_err("cancelled");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::Cancelled,
        "{error}"
    );
    assert_eq!(prompt.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(hooked.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(connector.embedded_ctx().shared.pooled(), 0);
    crate::embedded::hostkey::records_done();
    assert_eq!(
        std::fs::read_to_string(&first).expect("known_hosts"),
        RECORD_BEFORE
    );
}

/// A key file read during authentication waits on a blocking thread: with one runtime worker
/// another session still opens meanwhile, and the discovery that waits for the file stops at
/// its budget.
#[cfg(unix)]
#[test]
fn a_key_file_that_does_not_answer_holds_no_runtime_worker() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    let limit = std::time::Duration::from_secs(10);
    let budget = std::time::Duration::from_secs(5);
    let dir = Dir::new("key-fifo");
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Refuse(2));
    std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
    let fifo = dir.0.join("id_fifo");
    make_fifo(&fifo);
    // `stuck.example` tries the FIFO first; `other.example` has only the key file.
    let config = format!(
        "Host stuck.example\n  IdentityFile \"{}\"\n{}",
        fifo.display(),
        server.config(&key, &known)
    );
    let (connector, _) = embedded_with(&dir, &config);
    let mut settings = connector.settings().clone();
    settings
        .set(
            "embedded.runtime.worker_threads",
            crate::settings::Value::Count(1),
        )
        .expect("worker_threads");
    let connector = SshConnector::new(settings);

    let (tx, rx) = std::sync::mpsc::channel();
    let run = connector.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let out = discover_on(&run, &SshTarget::parse("stuck.example"), budget);
        tx.send((out, started.elapsed())).unwrap_or_default();
    });
    let end = reader_of(&fifo, limit);
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let other = connector.clone();
    std::thread::spawn(move || {
        let opened = open_session_to(&other, &SshTarget::parse("other.example"));
        opened_tx.send(opened).unwrap_or_default();
    });
    opened_rx
        .recv_timeout(limit)
        .expect("the other session waited for the key file read")
        .expect("the other session opens");
    let (out, elapsed) = rx
        .recv_timeout(limit)
        .expect("the budget did not stop the key file read");
    let error = out.expect_err("no key");
    assert_eq!(
        error.kind,
        crate::discovery::PortDiscoveryFailureKind::TimedOut,
        "{error}"
    );
    // The budget, and what stopping takes on a loaded machine.
    assert!(
        elapsed < budget + std::time::Duration::from_secs(3),
        "{elapsed:?}"
    );
    drop(end);
}

// Retrying a refused tunnel channel by its reason code (docs/design/ssh-backends.md "리스너
// 판정", conductor M4 2단계 D6).

/// A channel the server refuses with code 1 or 3 is not retried, with code 4 it is: the code
/// the verdict gives for the connection decides.
#[test]
fn a_refused_tunnel_channel_is_retried_only_for_a_passing_reason() {
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    use std::io::Read;
    for (code, retried) in [(1, false), (3, false), (4, true)] {
        let dir = Dir::new("refused-retry");
        let key = dir.0.join("id_test");
        let known = dir.0.join("known_hosts");
        let server = TestServer::start(&key, ExecReply::Silent, DirectReply::Refuse(code));
        std::fs::write(&known, format!("{}\n", server.known_hosts_line())).expect("known_hosts");
        let (connector, selected) = embedded_with(&dir, &server.config(&key, &known));
        let tunnel = connector
            .tunnel(&selected, &SshTarget::parse("h.example"), 4100, None)
            .expect("tunnel");
        let (verdict, mut stream) = connect_named(&tunnel);
        let mut buf = [0u8; 1];
        let read = stream.read(&mut buf);
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        let dropped = crate::connection_closed_error("the channel closed");
        let refused = verdict
            .channel_open_refused(&dropped)
            .unwrap_or_else(|| panic!("code {code} gives no refusal"));
        assert_eq!(refused, crate::ChannelOpenRefused { code });
        assert_eq!(refused.is_retryable(), retried, "code {code}");
    }
}

// Naming the local connection before it is made (docs/design/ssh-backends.md "리스너 판정",
// conductor M4 2단계 D7).

/// A local connection to `tunnel` and its verdict, made as the transport makes them: the
/// verdict is taken, the socket is bound to a loopback port, the verdict names that address,
/// and then the socket connects.
fn connect_named(tunnel: &crate::SshTunnel) -> (crate::ListenerVerdict, std::net::TcpStream) {
    use std::net::{Ipv4Addr, SocketAddr};
    let verdict = tunnel.listener_verdict();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4().expect("socket");
        socket
            .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("bind");
        let verdict = verdict.for_local(socket.local_addr().expect("local address"));
        let stream = socket
            .connect(SocketAddr::from((Ipv4Addr::LOCALHOST, tunnel.local_port())))
            .await
            .expect("connect");
        let stream = stream.into_std().expect("std stream");
        stream.set_nonblocking(false).expect("blocking");
        (verdict, stream)
    })
}
