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
        "Host *\n  HostName real.example\n  Port 2222\n  SetEnv TOKEN=s3cret-value\n  SendEnv LANG\n",
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
    assert!(!keywords.contains(&"setenv"), "{keywords:?}");
    assert!(!keywords.contains(&"sendenv"), "{keywords:?}");
    assert!(!format!("{resolved:?}").contains("s3cret"), "{resolved:?}");
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
    use crate::discovery::RemoteRun;
    let target = SshTarget::parse("h.example");
    let run = crate::embedded::discovery::EmbeddedRun {
        ctx: connector.embedded_ctx(),
        target: &target,
    };
    let limits = crate::discovery::Limits::new(connector.settings(), &target);
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
    use crate::embedded::resolve::{Purpose, load, resolve_chain};
    let ctx = connector.embedded_ctx();
    let set = load(ctx.settings)?;
    let chain = resolve_chain(
        &set,
        ctx.settings,
        &SshTarget::parse("h.example"),
        Purpose::Tunnel,
        ctx.verify,
    )?;
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
    use crate::embedded::test_server::{DirectReply, ExecReply, TestServer};
    let key = dir.0.join("id_test");
    let known = dir.0.join("known_hosts");
    let server = TestServer::start_with(&key, ExecReply::Silent, DirectReply::Refuse(2), rule);
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
    use russh::keys::ssh_key::certificate::{Builder, CertType};
    use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
    let ca = russh::keys::PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[11; 32])),
        "test ca",
    )
    .expect("ca");
    let key = crate::embedded::test_server::client_key();
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
        let verdict = tunnel.listener_verdict();
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
        let verdict = verdict.for_local(stream.local_addr().expect("local address"));
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
            let verdict = tunnel.listener_verdict();
            let stream =
                std::net::TcpStream::connect(("127.0.0.1", tunnel.local_port())).expect("connect");
            let local = stream.local_addr().expect("local address");
            (verdict.for_local(local), local, stream)
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

/// The tunnel keeps a connection's slot only while the connection is open: many forwards
/// that open and end, and connections closed before any verdict names them (as a cancelled
/// attempt does), leave nothing behind; what is kept never exceeds the open connections.
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
    let reached = |want: usize| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while probe.kept() < want && std::time::Instant::now() < end {
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
    assert_eq!(settled(0), 0);
    // Connections that stay open keep their slots, no more.
    let open: Vec<_> = (0..3).map(|_| connect()).collect();
    assert_eq!(reached(open.len()), open.len());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(probe.kept(), open.len());
    drop(open);
    assert_eq!(settled(0), 0);
    // Connections closed after the tunnel accepted them, before a verdict names them (a
    // cancelled attempt).
    for _ in 0..10 {
        let stream = connect();
        assert_eq!(reached(1), 1);
        drop(stream);
        assert_eq!(settled(0), 0);
    }
    // Connections still open when the tunnel is terminated: the aborted forwards release
    // their slots, also while the verdict keeps the marks and the local sockets are open.
    let open: Vec<_> = (0..3).map(|_| connect()).collect();
    assert_eq!(reached(open.len()), open.len());
    tunnel.terminate_and_reap().expect("terminate");
    assert_eq!(settled(0), 0);
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
