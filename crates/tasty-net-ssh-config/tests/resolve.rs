//! Behavior of a resolution that `ssh -G` cannot show: refusals, notes, sources, the host key
//! check this crate does and the `Include` depth limit (docs/design/ssh-backends.md
//! "지시어 분류", "옵션 적용 순서").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use tasty_net_ssh_config::options::{Flag, StrictHostKey};
use tasty_net_ssh_config::{
    CommandLine, CommandLineValue, ConfigFiles, ConfigInputs, ConfigSet, NoteAction, NoteClass,
    Origin, RefusalKind, Resolution, ResolveContext,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A new empty folder for one test.
fn folder() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("tasty-net-ssh-config-{}-{n}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("remove an old test folder");
    }
    std::fs::create_dir_all(&dir).expect("create the test folder");
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).expect("write a test file");
    // Included files are read with OpenSSH's permission check; a umask of 002 would make
    // them group writable and refused.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        const MODE: u32 = 0o644;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(MODE)).expect("file mode");
    }
    p
}

/// The owner of the files this test writes: the `CHECKPERM` of included files compares the
/// owner with the local uid.
#[cfg(unix)]
fn file_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(std::fs::metadata(folder()).expect("test folder").uid())
}

#[cfg(not(unix))]
fn file_owner() -> Option<u32> {
    None
}

fn context(command_line: Vec<CommandLineValue>) -> ResolveContext {
    ResolveContext {
        command_line,
        local_user: Some("tester".to_string()),
        home: Some("/home/tester".to_string()),
        local_uid: file_owner(),
        local_hostname: Some("client.example.com".to_string()),
        ..ResolveContext::default()
    }
}

fn resolve_with(config: &str, target: &str, command_line: Vec<CommandLineValue>) -> Resolution {
    let dir = folder();
    let path = write(&dir, "config", config);
    let set = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(path),
    })
    .expect("load the configuration");
    set.resolve(target, &context(command_line))
}

fn resolve(config: &str, target: &str) -> Resolution {
    resolve_with(config, target, Vec::new())
}

fn option(line: &str, label: &str) -> CommandLineValue {
    CommandLineValue {
        value: CommandLine::Option(line.to_string()),
        label: label.to_string(),
    }
}

fn refusal_kind(r: &Resolution) -> Option<RefusalKind> {
    r.refusal.as_ref().map(|f| f.kind)
}

fn has_note(r: &Resolution, keyword: &str, action: NoteAction, class: NoteClass) -> bool {
    r.notes
        .iter()
        .any(|n| n.keyword == keyword && n.action == action && n.class == class)
}

#[test]
fn unknown_keyword_in_inactive_block_refuses() {
    let r = resolve(
        "Host never\n    NoSuchOption yes\nHost *\n    Port 2222\n",
        "web",
    );
    assert_eq!(refusal_kind(&r), Some(RefusalKind::UnknownKeyword));
    assert!(has_note(
        &r,
        "nosuchoption",
        NoteAction::Refused,
        NoteClass::UnknownKeyword
    ));
}

/// The file, line and name of the first bad line (docs/design/ssh-backends.md "결과 API").
fn refusal_place(r: &Resolution) -> (Option<String>, Option<usize>) {
    let f = r.refusal.as_ref().expect("a refusal");
    let line = match &f.origin {
        Some(Origin::File { line, .. }) => Some(*line),
        _ => None,
    };
    (f.keyword.clone(), line)
}

#[test]
fn refusal_names_the_bad_line() {
    let unknown = resolve("Port 2222\nHost never\n    NoSuchOption yes\n", "web");
    assert_eq!(
        refusal_place(&unknown),
        (Some("nosuchoption".to_string()), Some(3))
    );
    let syntax = resolve("Port 2222\nForwardX11 perhaps\nPort 99999\n", "web");
    assert_eq!(refusal_kind(&syntax), Some(RefusalKind::ConfigError));
    assert_eq!(
        refusal_place(&syntax),
        (Some("forwardx11".to_string()), Some(2))
    );

    let cli = resolve_with("", "web", vec![option("Port=x", "override")]);
    let f = cli.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(f.keyword.as_deref(), Some("port"));
    assert_eq!(
        f.origin,
        Some(Origin::CommandLine {
            label: "override".to_string()
        })
    );
    let cli_unknown = resolve_with("", "web", vec![option("NoSuch=1", "override")]);
    let f = cli_unknown.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::UnknownKeyword);
    assert_eq!(f.keyword.as_deref(), Some("nosuch"));
}

#[test]
fn ignore_unknown_applies_only_before_the_line() {
    let before = resolve("IgnoreUnknown NoSuch*\nNoSuchOption yes\n", "web");
    assert_eq!(refusal_kind(&before), None);
    assert!(has_note(
        &before,
        "nosuchoption",
        NoteAction::Ignored,
        NoteClass::IgnoredUnknownKeyword
    ));

    let after = resolve("NoSuchOption yes\nIgnoreUnknown NoSuch*\n", "web");
    assert_eq!(refusal_kind(&after), Some(RefusalKind::UnknownKeyword));

    // An IgnoreUnknown in a block that does not apply makes no exception.
    let inactive = resolve(
        "Host never\n    IgnoreUnknown NoSuch*\nNoSuchOption yes\n",
        "web",
    );
    assert_eq!(refusal_kind(&inactive), Some(RefusalKind::UnknownKeyword));

    // It has no effect on a known keyword.
    let known = resolve(
        "IgnoreUnknown KnownHostsCommand\nKnownHostsCommand /bin/false\n",
        "web",
    );
    assert_eq!(
        refusal_kind(&known),
        Some(RefusalKind::SecurityNotImplemented)
    );
}

#[test]
fn first_value_wins_and_sources() {
    let r = resolve(
        "Host web\n    Port 2222\nHost *\n    Port 3333\n    User deploy\n",
        "web",
    );
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.options.port, Some(2222));
    assert_eq!(r.options.user.as_deref(), Some("deploy"));
    match r.source_of("port") {
        Origin::File { line, .. } => assert_eq!(line, 2),
        other => panic!("port came from {other:?}"),
    }
    assert_eq!(r.source_of("stricthostkeychecking"), Origin::Default);

    let cli = resolve_with(
        "Host *\n    Port 3333\n",
        "web",
        vec![option("Port=4444", "override")],
    );
    assert_eq!(cli.options.port, Some(4444));
    assert_eq!(
        cli.source_of("port"),
        Origin::CommandLine {
            label: "override".to_string()
        }
    );

    // The user and port of the destination come from the destination.
    let destination = Origin::CommandLine {
        label: "destination".to_string(),
    };
    let target = resolve("User deploy\nPort 3333\n", "alice@web");
    assert_eq!(target.options.user.as_deref(), Some("alice"));
    assert_eq!(target.source_of("user"), destination);
    let uri = resolve("User deploy\nPort 3333\n", "ssh://bob@web:2201");
    assert_eq!(uri.options.port, Some(2201));
    assert_eq!(uri.source_of("port"), destination);
    assert_eq!(uri.source_of("user"), destination);
}

#[test]
fn host_key_check_is_kept() {
    let off = resolve("Host *\n    StrictHostKeyChecking no\n", "web");
    assert_eq!(refusal_kind(&off), None);
    // OpenSSH's value stays in the options (`ssh -G` shows it) ...
    assert_eq!(
        off.options.strict_host_key_checking,
        Some(StrictHostKey::Off)
    );
    // ... and the check this crate does skips it.
    assert_eq!(off.host_key_checking, StrictHostKey::Ask);
    assert!(has_note(
        &off,
        "stricthostkeychecking",
        NoteAction::Shown,
        NoteClass::HostKeyCheckKept
    ));

    let next = resolve(
        "Host web\n    StrictHostKeyChecking off\nHost *\n    StrictHostKeyChecking accept-new\n",
        "web",
    );
    assert_eq!(next.host_key_checking, StrictHostKey::New);
    // The origin is the line of the value in effect, not the skipped first line.
    match &next.host_key_checking_origin {
        Origin::File { line, .. } => assert_eq!(*line, 4),
        other => panic!("host key checking came from {other:?}"),
    }
    assert_eq!(off.host_key_checking_origin, Origin::Default);

    let local = resolve("NoHostAuthenticationForLocalhost yes\n", "localhost");
    assert_eq!(
        local.options.no_host_authentication_for_localhost,
        Some(Flag::Yes)
    );
    assert!(has_note(
        &local,
        "nohostauthenticationforlocalhost",
        NoteAction::Shown,
        NoteClass::HostKeyCheckKept
    ));

    let unwritten = resolve("Port 22\n", "web");
    assert!(unwritten.notes.is_empty(), "notes: {:?}", unwritten.notes);
    assert_eq!(unwritten.host_key_checking, StrictHostKey::Ask);
}

#[test]
fn security_refusals_only_when_applied() {
    for line in [
        "KnownHostsCommand /bin/false",
        "BindInterface eth9",
        "ProxyCommand nc %h %p",
        "VerifyHostKeyDNS yes",
        "PubkeyAuthentication host-bound",
    ] {
        let applied = resolve(&format!("Host web\n    {line}\n"), "web");
        assert_eq!(
            refusal_kind(&applied),
            Some(RefusalKind::SecurityNotImplemented),
            "{line}"
        );
        let other = resolve(&format!("Host other\n    {line}\n"), "web");
        assert_eq!(
            refusal_kind(&other),
            None,
            "{line} in a block that does not apply"
        );
    }
    for line in [
        "ProxyCommand none",
        "VerifyHostKeyDNS no",
        "KnownHostsCommand none",
        "CanonicalizeHostname no",
    ] {
        let r = resolve(&format!("{line}\n"), "web");
        assert_eq!(refusal_kind(&r), None, "{line}");
    }
    // ProxyJump wins over a later ProxyCommand line: that line is not applied.
    let jump = resolve("ProxyJump hop.example.com\nProxyCommand nc %h %p\n", "web");
    assert_eq!(refusal_kind(&jump), None);
}

#[test]
fn shown_notes() {
    let gss = resolve("GSSAPIAuthentication yes\nForwardAgent yes\n", "web");
    assert_eq!(refusal_kind(&gss), None);
    assert!(has_note(
        &gss,
        "gssapiauthentication",
        NoteAction::Shown,
        NoteClass::NotSupported
    ));
    assert!(has_note(
        &gss,
        "forwardagent",
        NoteAction::Shown,
        NoteClass::NotSupported
    ));

    let gss_no = resolve("GSSAPIAuthentication no\n", "web");
    assert!(gss_no.notes.is_empty(), "notes: {:?}", gss_no.notes);

    let pubkey = resolve("PubkeyAuthentication yes\n", "web");
    assert!(has_note(
        &pubkey,
        "pubkeyauthentication",
        NoteAction::Shown,
        NoteClass::PubkeyUnbound
    ));

    let dropped = resolve("Protocol 2\nUseRoaming no\n", "web");
    assert_eq!(refusal_kind(&dropped), None);
    assert!(has_note(
        &dropped,
        "protocol",
        NoteAction::Ignored,
        NoteClass::DroppedByOpenSsh
    ));
    assert!(has_note(
        &dropped,
        "useroaming",
        NoteAction::Shown,
        NoteClass::DroppedByOpenSsh
    ));
}

#[test]
fn undeterminable_match() {
    for criterion in [
        "exec /bin/true",
        "localnetwork 192.0.2.0/24",
        "localnetwork ::/0",
        "localnetwork 0.0.0.0/0",
    ] {
        let r = resolve(&format!("Match {criterion}\n    Port 2222\n"), "web");
        assert_eq!(
            refusal_kind(&r),
            Some(RefusalKind::Undeterminable),
            "{criterion}"
        );
    }
    // An earlier criterion that fails decides without running the program.
    let skipped = resolve("Match host other exec /bin/true\n    Port 2222\n", "web");
    assert_eq!(refusal_kind(&skipped), None);
}

#[test]
fn refuse_connection() {
    let r = resolve("Host web\n    RefuseConnection \"not here\"\n", "web");
    assert_eq!(refusal_kind(&r), Some(RefusalKind::RefuseConnection));
    let other = resolve("Host other\n    RefuseConnection \"not here\"\n", "web");
    assert_eq!(refusal_kind(&other), None);
}

// Unix only: `Include` sees only `/` as a separator (portable OpenSSH), so a Windows temporary
// path such as `C:\...` is relative there.
#[cfg(unix)]
/// A chain of `files` included files below the top file.
fn include_chain(files: usize) -> Resolution {
    let dir = folder();
    for i in 1..=files {
        let text = if i == files {
            "Port 2222\n".to_string()
        } else {
            format!("Include {}\n", dir.join(format!("f{}", i + 1)).display())
        };
        write(&dir, &format!("f{i}"), &text);
    }
    let top = write(
        &dir,
        "config",
        &format!("Include {}\n", dir.join("f1").display()),
    );
    ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()))
}

// Unix only: `Include` sees only `/` as a separator (portable OpenSSH), so a Windows temporary
// path such as `C:\...` is relative there.
#[cfg(unix)]
#[test]
fn include_depth_limit() {
    let depth = tasty_net_ssh_config::settings::FIXED
        .iter()
        .find(|f| f.key == "config.include_max_depth")
        .expect("the fixed value")
        .value
        .parse::<usize>()
        .expect("a number");
    // The top file has depth 0, so `depth` included files still read.
    let ok = include_chain(depth);
    assert_eq!(refusal_kind(&ok), None);
    assert_eq!(ok.options.port, Some(2222));

    let deep = include_chain(depth + 1);
    assert_eq!(refusal_kind(&deep), Some(RefusalKind::ConfigError));
    let message = deep.refusal.map(|r| r.message).unwrap_or_default();
    assert!(
        message.contains("Too many recursive configuration includes"),
        "{message}"
    );
}

// Unix only: `Include` sees only `/` as a separator (portable OpenSSH), so a Windows temporary
// path such as `C:\...` is relative there.
#[cfg(unix)]
#[test]
fn include_cycle_stops_at_the_limit() {
    let dir = folder();
    let a = dir.join("a");
    let b = dir.join("b");
    write(&dir, "a", &format!("Include {}\n", b.display()));
    write(&dir, "b", &format!("Include {}\n", a.display()));
    let top = write(&dir, "config", &format!("Include {}\n", a.display()));
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()));
    assert_eq!(refusal_kind(&r), Some(RefusalKind::ConfigError));
    let message = r.refusal.map(|r| r.message).unwrap_or_default();
    assert!(
        message.contains("Too many recursive configuration includes"),
        "{message}"
    );
}

#[test]
fn environment_and_tokens() {
    let mut ctx = context(Vec::new());
    ctx.env.insert("NAME".to_string(), "value".to_string());
    let dir = folder();
    let top = write(
        &dir,
        "config",
        "Host web\n    HostName %h.example.com\n    User ${NAME}\n    ControlPath ~/.ssh/cm-%r@%h:%p\n",
    );
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &ctx);
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.host, "web.example.com");
    assert_eq!(r.options.user.as_deref(), Some("value"));
    assert_eq!(
        r.options.control_path.as_deref(),
        Some("/home/tester/.ssh/cm-value@web.example.com:22")
    );

    // A missing variable in User stops the resolution as OpenSSH does.
    let missing = resolve("User ${TASTY_NOT_SET}\n", "web");
    assert_eq!(refusal_kind(&missing), Some(RefusalKind::ConfigError));
}

#[test]
fn missing_config_file() {
    let dir = folder();
    let err = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(dir.join("absent")),
    });
    assert!(err.is_err());
    let none = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Default {
            user: Some(dir.join("absent")),
            system: None,
        },
    })
    .expect("a missing user file is skipped");
    assert_eq!(
        refusal_kind(&none.resolve("web", &context(Vec::new()))),
        None
    );
}
