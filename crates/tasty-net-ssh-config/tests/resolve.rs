//! Behavior of a resolution that `ssh -G` cannot show: refusals, notes, sources, the host key
//! check this crate does and the `Include` depth limit (docs/design/ssh-backends.md
//! "지시어 분류", "옵션 적용 순서").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use tasty_net_ssh_config::options::{Flag, ProvidedBy, StrictHostKey};
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

// Unix only: folder modes.
#[cfg(unix)]
#[test]
fn an_unreadable_folder_is_an_open_error() {
    use std::os::unix::fs::PermissionsExt as _;
    const NO_READ: u32 = 0o300;
    const OPEN: u32 = 0o755;
    let dir = folder();
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).expect("a folder");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(NO_READ)).expect("mode");
    // A user who can open it anyway (root) cannot see the case.
    if std::fs::File::open(&sub).is_ok() {
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(OPEN)).expect("mode");
        return;
    }
    let top = write(&dir, "config", &format!("Include {}\n", sub.display()));
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()));
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(OPEN)).expect("mode");
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(f.keyword.as_deref(), Some("include"));
    assert!(
        f.message.contains("Can't open user config file"),
        "{}",
        f.message
    );
}

// Unix only: the Include names an absolute path.
#[cfg(unix)]
#[test]
fn include_refusal_names_its_own_line() {
    // The included file is read first and its last keyword is `user`; the empty argument
    // after it is the bad part of the Include line in the parent.
    let dir = folder();
    let good = write(&dir, "good.conf", "Port 2201\nUser alice\n");
    let top = write(
        &dir,
        "config",
        &format!("Port 2222\nInclude {} \"\"\n", good.display()),
    );
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top.clone()),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()));
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(f.keyword.as_deref(), Some("include"));
    assert_eq!(f.origin, Some(Origin::File { path: top, line: 2 }));
}

// The owner is judged by `resolve` with the local uid: a user file whose mode passes but whose
// owner is neither root nor the local user is refused (docs/design/ssh-backends.md "파일 읽기").
#[cfg(unix)]
#[test]
fn a_user_file_of_another_owner_is_refused() {
    let dir = folder();
    let path = write(&dir, "config", "Port 2201\n");
    let owner = file_owner().expect("the file owner");
    // A file owned by root passes for every local uid.
    if owner == 0 {
        return;
    }
    let run = |uid: u32| {
        let mut ctx = context(Vec::new());
        ctx.local_uid = Some(uid);
        ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Default {
                user: Some(path.clone()),
                system: None,
            },
        })
        .expect("load")
        .resolve("web", &ctx)
    };
    let own = run(owner);
    assert_eq!(refusal_kind(&own), None);
    assert_eq!(own.options.port, Some(2201));
    let other = run(owner.wrapping_add(1).max(1));
    let f = other.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert!(
        f.message.contains("Bad owner or permissions"),
        "{}",
        f.message
    );
    assert_eq!(other.options.port, None);
}

// Linux only: a FIFO opened for reading and writing does not block there.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_file_is_not_read() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    const GROUP_WRITABLE: u32 = 0o662;
    let dir = folder();
    let fifo = dir.join("config");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(GROUP_WRITABLE))
        .expect("fifo mode");
    // This end keeps the FIFO open, so opening it to read does not block and reading it would
    // wait for more.
    let mut end = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .expect("open the fifo");
    end.write_all(b"Port 1\n").expect("write the fifo");
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = context(Vec::new());
    let path = fifo.clone();
    std::thread::spawn(move || {
        let r = ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Default {
                user: Some(path),
                system: None,
            },
        })
        .expect("load")
        .resolve("web", &ctx);
        let _sent = tx.send(r);
    });
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let r = rx
        .recv_timeout(WAIT)
        .expect("the resolution reads the refused file");
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert!(
        f.message.contains("Bad owner or permissions"),
        "{}",
        f.message
    );
    // What was written is still in the FIFO: nobody read it.
    let mut left = [0u8; 7];
    end.read_exact(&mut left).expect("read the fifo");
    assert_eq!(&left, b"Port 1\n");
}

// Linux only: FIFOs made with mkfifo.
#[cfg(target_os = "linux")]
#[test]
fn files_that_pass_are_read_from_fifos() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    const OWNER_ONLY: u32 = 0o600;
    let dir = folder();
    let fifo = |name: &str, text: String| {
        let path = dir.join(name);
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(made.success());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(OWNER_ONLY))
            .expect("fifo mode");
        // The writer opens when a reader does, writes and closes, which ends the text.
        let writer = path.clone();
        std::thread::spawn(move || {
            let mut end = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer)
                .expect("open the fifo to write");
            end.write_all(text.as_bytes()).expect("write the fifo");
        });
        path
    };
    let inner = fifo("inner", "User fromfifo\n".to_string());
    let top = fifo(
        "config",
        format!("Port 2201\nInclude {}\n", inner.display()),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = context(Vec::new());
    std::thread::spawn(move || {
        let r = ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Only(top),
        })
        .expect("load")
        .resolve("web", &ctx);
        let _sent = tx.send(r);
    });
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let r = rx.recv_timeout(WAIT).expect("the resolution ends");
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.options.port, Some(2201));
    assert_eq!(r.options.user.as_deref(), Some("fromfifo"));
}

#[test]
fn service_name_port_is_refused() {
    // OpenSSH looks `ssh` up in the services database (`a2port`, `misc.c:592`-`:604`); this
    // crate takes numeric ports only, so the line is a configuration error.
    let r = resolve("Host *\n    Port ssh\n", "web");
    assert_eq!(refusal_kind(&r), Some(RefusalKind::ConfigError));
    assert_eq!(refusal_place(&r), (Some("port".to_string()), Some(2)));
    let cli = resolve_with("", "web", vec![option("Port=ssh", "override")]);
    let f = cli.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(f.keyword.as_deref(), Some("port"));
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
fn identity_file_limit() {
    // 100 files (SSH_MAX_IDENTITY_FILES), then a duplicate: the limit comes first.
    let mut config: String = (0..100).map(|i| format!("IdentityFile /k{i}\n")).collect();
    let full = resolve(&config, "web");
    assert_eq!(refusal_kind(&full), None);
    assert_eq!(full.options.identity_files.len(), 100);
    config.push_str("IdentityFile /k0\n");
    let over = resolve(&config, "web");
    assert_eq!(refusal_kind(&over), Some(RefusalKind::ConfigError));

    // `-i` goes through the same limit.
    let dir = folder();
    let mut command_line: Vec<CommandLineValue> = (0..100)
        .map(|i| CommandLineValue {
            value: CommandLine::Identity(write(&dir, &format!("k{i}"), "").display().to_string()),
            label: "target".to_string(),
        })
        .collect();
    let ok = resolve_with("", "web", command_line.clone());
    assert_eq!(refusal_kind(&ok), None);
    command_line.push(command_line[0].clone());
    let over_cli = resolve_with("", "web", command_line);
    let f = over_cli.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert!(
        f.message.contains("Too many identity files"),
        "{}",
        f.message
    );
}

#[test]
fn identity_file_duplicates_by_origin() {
    // `-o` is a user configuration (2), as the user file: one entry.
    let o = resolve_with(
        "IdentityFile /k\n",
        "web",
        vec![option("IdentityFile=/k", "override")],
    );
    assert_eq!(
        o.options.identity_files,
        vec![("/k".to_string(), ProvidedBy::UserConfig)]
    );
    // `-i` (1) is not a duplicate of the same file in the user file (2).
    let dir = folder();
    let key = write(&dir, "key", "").display().to_string();
    let i = resolve_with(
        &format!("IdentityFile {key}\n"),
        "web",
        vec![CommandLineValue {
            value: CommandLine::Identity(key.clone()),
            label: "target".to_string(),
        }],
    );
    assert_eq!(
        i.options.identity_files,
        vec![
            (key.clone(), ProvidedBy::IdentityOption),
            (key, ProvidedBy::UserConfig)
        ]
    );
}

#[test]
fn no_command_sets_request_tty() {
    use tasty_net_ssh_config::options::{RequestTty, YesNoAsk};
    let n = CommandLineValue {
        value: CommandLine::NoCommand,
        label: "forced".to_string(),
    };
    let r = resolve_with(
        "UpdateHostKeys ask\n",
        "web",
        vec![option("RequestTTY=force", "target"), n],
    );
    assert_eq!(r.options.request_tty, Some(RequestTty::No));
    assert_eq!(
        r.source_of("requesttty"),
        Origin::CommandLine {
            label: "forced".to_string()
        }
    );
    // `RequestTTY no` turns `UpdateHostKeys ask` off (ssh.c:1344-1351).
    assert_eq!(r.options.update_hostkeys, Some(YesNoAsk::No));
}

#[test]
fn jump_command_passes_config_none() {
    let set = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::None,
    })
    .expect("no files");
    let r = set.resolve("web", &context(vec![option("ProxyJump=hop", "target")]));
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(
        r.options.proxy_command.as_deref(),
        Some("ssh -F none -W '[%h]:%p' hop")
    );
}

#[test]
fn host_key_algorithms_that_do_not_assemble() {
    let r = resolve("HostKeyAlgorithms !ssh-rsa\n", "web");
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(f.keyword.as_deref(), Some("hostkeyalgorithms"));
    let ok = resolve("HostKeyAlgorithms ^ssh-ed25519\n", "web");
    assert_eq!(refusal_kind(&ok), None);
}

// Unix only: a folder opens and reads as an empty file there, as in OpenSSH.
#[cfg(unix)]
#[test]
fn include_of_a_folder_reads_nothing() {
    let dir = folder();
    std::fs::create_dir_all(dir.join("sub")).expect("a folder");
    // The permission check also applies to the folder; a umask of 002 would refuse it.
    {
        use std::os::unix::fs::PermissionsExt as _;
        const MODE: u32 = 0o755;
        std::fs::set_permissions(dir.join("sub"), std::fs::Permissions::from_mode(MODE))
            .expect("folder mode");
    }
    write(&dir, "sub/inner", "Port 1\n");
    let top = write(
        &dir,
        "config",
        &format!("Include {}\nPort 2222\n", dir.join("sub").display()),
    );
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()));
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.options.port, Some(2222));
}

// Unix only: the home is a temporary path, and `Include` sees only `/` as a separator.
#[cfg(unix)]
#[test]
fn relative_include_goes_under_the_ssh_folder() {
    let home = folder();
    std::fs::create_dir_all(home.join(".ssh")).expect("the ssh folder");
    write(&home, ".ssh/sub.conf", "Port 2222\n");
    let top = write(&home, "config", "Include sub.conf\n");
    let mut ctx = context(Vec::new());
    ctx.home = Some(home.display().to_string());
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top),
    })
    .expect("load")
    .resolve("web", &ctx);
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.options.port, Some(2222));
}

// Unix only: a file name with `\` and a home given with `/`.
#[cfg(unix)]
#[test]
fn a_backslash_in_the_home_is_a_plain_character() {
    // `globtilde` adds the home after the quoted characters are protected, so its `\` is
    // not an escape. Both the home and the ssh folder are put in that way.
    for ssh_dir in [false, true] {
        let home = folder().join("back\\slash");
        std::fs::create_dir_all(home.join(".ssh")).expect("the ssh folder");
        write(&home, ".ssh/sub.conf", "Port 2222\n");
        let top = write(&home, "config", "Include sub.conf\n");
        let mut ctx = context(Vec::new());
        ctx.home = Some(home.display().to_string());
        if ssh_dir {
            ctx.ssh_dir = Some(home.join(".ssh").display().to_string());
        }
        let r = ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Only(top),
        })
        .expect("load")
        .resolve("web", &ctx);
        assert_eq!(refusal_kind(&r), None);
        assert_eq!(r.options.port, Some(2222), "ssh_dir given: {ssh_dir}");
    }
}

#[test]
fn paths_under_an_unknown_home() {
    let mut ctx = context(Vec::new());
    ctx.home = None;
    let run = |config: &str| {
        let dir = folder();
        let top = write(&dir, "config", config);
        ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Only(top),
        })
        .expect("load")
        .resolve("web", &ctx)
    };
    // An Include under the home matches no file; the line goes on.
    let include = run("Include ~/.ssh/other.conf\nPort 2222\n");
    assert_eq!(refusal_kind(&include), None);
    assert_eq!(include.options.port, Some(2222));
    // A known hosts file under the home is missing, not a `~` folder of the working directory.
    let hosts = run("UserKnownHostsFile ~/.ssh/known_hosts /etc/ssh/extra_known_hosts\n");
    assert_eq!(refusal_kind(&hosts), None);
    assert_eq!(
        hosts.options.user_hostfiles,
        vec!["/etc/ssh/extra_known_hosts".to_string()]
    );
    // A socket under the home is missing: no control socket, no agent.
    let sockets = run(
        "ControlPath ~/.ssh/cm-%C\nIdentityAgent ~/.ssh/agent.sock\nForwardAgent ~/.ssh/fwd.sock\n",
    );
    assert_eq!(refusal_kind(&sockets), None);
    assert_eq!(sockets.options.control_path, None);
    assert_eq!(sockets.options.identity_agent.as_deref(), Some("none"));
    assert_eq!(sockets.options.forward_agent_sock_path, None);
    // A revoked keys file `ssh` cannot read stops it, so it cannot be skipped.
    let revoked = run("RevokedHostKeys ~/.ssh/revoked\n");
    let f = revoked.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::Undeterminable);
    assert_eq!(f.keyword.as_deref(), Some("revokedhostkeys"));
}

#[test]
fn a_line_ends_at_its_first_nul() {
    // OpenSSH handles each line as a C string: what follows a NUL is not in the line.
    let r = resolve("User nulled\0Port 2201\nHostName after.example\n", "web");
    assert_eq!(refusal_kind(&r), None);
    assert_eq!(r.options.user.as_deref(), Some("nulled"));
    assert_ne!(r.options.port, Some(2201));
    assert_eq!(r.options.hostname.as_deref(), Some("after.example"));
    // The line numbers stay: the bad line after a cut line is still line 2.
    let dir = folder();
    let top = write(&dir, "config", "User nulled\0Port 2201\nNoSuchKeyword x\n");
    let bad = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(top.clone()),
    })
    .expect("load")
    .resolve("web", &context(Vec::new()));
    let f = bad.refusal.expect("a refusal");
    assert_eq!(f.origin, Some(Origin::File { path: top, line: 2 }));
}

#[test]
fn include_of_another_users_home() {
    let r = resolve("Include ~root/.ssh/extra.conf\n", "web");
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::Undeterminable);
    assert_eq!(f.keyword.as_deref(), Some("include"));
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

/// The host `ssh` would use for a numeric name of this OS's C library, or `None` where this
/// crate has no rule for it (docs/design/ssh-backends.md "숫자 주소").
fn this_os(glibc: &str, bionic: &str, darwin: &str) -> Option<String> {
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        Some(glibc.to_string())
    } else if cfg!(target_os = "android") {
        Some(bionic.to_string())
    } else if cfg!(target_vendor = "apple") {
        Some(darwin.to_string())
    } else {
        None
    }
}

#[test]
fn a_numeric_host_follows_the_c_library_of_this_os() {
    let cases = [
        (
            "010.0.0.1",
            None,
            this_os("8.0.0.1", "10.0.0.1", "10.0.0.1"),
        ),
        ("127.1", None, this_os("127.0.0.1", "127.1", "127.0.0.1")),
        (
            "::ffff:192.0.2.1",
            Some("AddressFamily=inet"),
            this_os("192.0.2.1", "::ffff:192.0.2.1", "::ffff:192.0.2.1"),
        ),
        // Read alike by every library: no refusal on any OS.
        ("192.0.2.1", None, Some("192.0.2.1".to_string())),
    ];
    for (target, extra, want) in cases {
        let cli = extra.map(|o| option(o, "override")).into_iter().collect();
        let r = resolve_with("", target, cli);
        match want {
            Some(h) => {
                assert!(r.refusal.is_none(), "{target}: {:?}", r.refusal);
                assert_eq!(r.host, h, "{target}");
            }
            None => {
                let f = r.refusal.expect("a refusal");
                assert_eq!(f.kind, RefusalKind::Undeterminable, "{target}");
                assert_eq!(f.keyword.as_deref(), Some("hostname"));
                assert!(f.message.contains(target), "{}", f.message);
            }
        }
    }
}

#[test]
fn a_localnetwork_list_is_judged_whatever_the_match_gives() {
    // The other criterion is false, so only the validity of the list can stop `ssh`.
    let r = resolve(
        "Match host nomatch localnetwork 127.0/8\n    Port 2222\n",
        "example",
    );
    let want =
        if cfg!(all(target_os = "linux", target_env = "gnu")) || cfg!(target_vendor = "apple") {
            // A valid list: the block does not apply.
            None
        } else if cfg!(target_os = "android") {
            // bionic does not read 127.0 as an address: `Invalid network list`.
            Some(RefusalKind::ConfigError)
        } else {
            // No rule, and the libraries do not agree on whether the list is valid.
            Some(RefusalKind::Undeterminable)
        };
    assert_eq!(refusal_kind(&r), want, "{:?}", r.refusal);
    if want == Some(RefusalKind::Undeterminable) {
        let f = r.refusal.expect("a refusal");
        assert_eq!(f.keyword.as_deref(), Some("match localnetwork"));
        assert!(f.message.contains("bionic not an address"), "{}", f.message);
    }
    // A bad mask is known before the address is read: a list error on every OS.
    let r = resolve(
        "Match originalhost other localnetwork 127.1/abc\n",
        "example",
    );
    assert_eq!(
        refusal_kind(&r),
        Some(RefusalKind::ConfigError),
        "{:?}",
        r.refusal
    );
}

#[test]
fn a_localnetwork_refusal_names_the_c_library_readings() {
    let r = resolve("Match localnetwork 010.0.0.0/8\n    Port 2222\n", "example");
    let f = r.refusal.expect("a refusal");
    assert_eq!(f.kind, RefusalKind::Undeterminable);
    assert_eq!(f.keyword.as_deref(), Some("match localnetwork"));
    let readings = f
        .message
        .contains("glibc 8.0.0.0, bionic 10.0.0.0, Darwin 10.0.0.0");
    // Only an OS without a rule reads the list by all three libraries.
    let no_rule = !(cfg!(all(target_os = "linux", target_env = "gnu"))
        || cfg!(target_os = "android")
        || cfg!(target_vendor = "apple"));
    assert_eq!(readings, no_rule, "{}", f.message);
    // A list read alike says only that the interfaces are not known. Darwin gives
    // ff02:1:: like the others, though writing it would need an interface.
    for list in ["10.0.0.0/8", "ff02:1::/32"] {
        let r = resolve(
            &format!("Match localnetwork {list}\n    Port 2222\n"),
            "example",
        );
        let f = r.refusal.expect("a refusal");
        assert_eq!(f.kind, RefusalKind::Undeterminable);
        assert!(!f.message.contains("C libraries"), "{list}: {}", f.message);
        assert!(
            !f.message.contains("cannot tell the address"),
            "{list}: {}",
            f.message
        );
    }
}

#[test]
fn a_unicast_prefix_based_multicast_host_is_kept_on_every_os() {
    // Flags 0x30: not link-local multicast for Darwin, so no interface is read.
    let r = resolve("Host m\n    HostName ff32:40:fe80::1234\n", "m");
    assert!(r.refusal.is_none(), "{:?}", r.refusal);
    assert_eq!(r.host, "ff32:40:fe80::1234");
}

#[test]
fn a_scoped_link_local_list_entry_is_judged_with_the_address_given() {
    // Too long a mask, and host bits left after Darwin clears the second word: list errors
    // on every OS, though the other criterion is false.
    for list in ["fe80:1::/129", "fe80:1::1/16"] {
        let r = resolve(
            &format!("Match originalhost other localnetwork {list}\n"),
            "example",
        );
        assert_eq!(
            refusal_kind(&r),
            Some(RefusalKind::ConfigError),
            "{list}: {:?}",
            r.refusal
        );
    }
    // Valid for every library: the block does not apply and nothing is refused.
    let r = resolve("Match host nomatch localnetwork fe80:1::/32\n", "example");
    assert!(r.refusal.is_none(), "{:?}", r.refusal);
}

#[test]
fn key_files_keep_order_origin_and_expansion() {
    let dir = folder();
    let key = write(&dir, "key", "").display().to_string();
    let mut ctx = context(vec![
        CommandLineValue {
            value: CommandLine::Identity(key.clone()),
            label: "target".to_string(),
        },
        option("IdentityFile=/o/%r", "override"),
    ]);
    ctx.env
        .insert("TASTY_KEYS".to_string(), "/keys".to_string());
    let path = write(
        &dir,
        "config",
        "Host web\n  User alice\n  IdentityFile ~/.ssh/id_%h\n  IdentityFile NONE\n  \
         IdentityFile ${TASTY_KEYS}/k_%u\n  CertificateFile ~/c_%r\n  CertificateFile none\n  \
         IdentitiesOnly yes\n",
    );
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(path.clone()),
    })
    .expect("load")
    .resolve("web", &ctx);
    assert!(r.refusal.is_none(), "{:?}", r.refusal);
    let line = |n| Origin::File {
        path: path.clone(),
        line: n,
    };
    let cl = |l: &str| Origin::CommandLine {
        label: l.to_string(),
    };
    let ids: Vec<(&str, Option<&str>, ProvidedBy, Origin)> = r
        .identity_files
        .iter()
        .map(|k| {
            (
                k.written.as_str(),
                k.path.as_deref(),
                k.provided_by,
                k.origin.clone(),
            )
        })
        .collect();
    // `-i` first, then `-o`, then the file in order; `none` (any case) is left out.
    assert_eq!(
        ids,
        vec![
            (
                key.as_str(),
                Some(key.as_str()),
                ProvidedBy::IdentityOption,
                cl("target")
            ),
            (
                "/o/%r",
                Some("/o/alice"),
                ProvidedBy::UserConfig,
                cl("override")
            ),
            (
                "~/.ssh/id_%h",
                Some("/home/tester/.ssh/id_web"),
                ProvidedBy::UserConfig,
                line(3)
            ),
            (
                "${TASTY_KEYS}/k_%u",
                Some("/keys/k_tester"),
                ProvidedBy::UserConfig,
                line(5)
            ),
        ]
    );
    // `ssh -G` keeps `none`.
    assert_eq!(r.options.identity_files.len(), 5);
    // Certificates have no `none` skip (`ssh.c:2454`-`:2482`).
    let certs: Vec<(&str, Option<&str>, Origin)> = r
        .certificate_files
        .iter()
        .map(|k| (k.written.as_str(), k.path.as_deref(), k.origin.clone()))
        .collect();
    assert_eq!(
        certs,
        vec![
            ("~/c_%r", Some("/home/tester/c_alice"), line(6)),
            ("none", Some("none"), line(7)),
        ]
    );
    assert_eq!(r.options.identities_only, Some(Flag::Yes));
    assert_eq!(r.source_of("identitiesonly"), line(8));
}

#[test]
fn default_key_files_are_marked_default() {
    let r = resolve("Host web\n  IdentitiesOnly no\n", "web");
    assert!(!r.identity_files.is_empty());
    for k in &r.identity_files {
        assert_eq!(k.origin, Origin::Default, "{k:?}");
        assert_eq!(k.provided_by, ProvidedBy::NotUser, "{k:?}");
        let p = k.path.as_deref().expect("under the known home");
        assert!(p.starts_with("/home/tester/.ssh/id_"), "{p}");
    }
    assert!(r.certificate_files.is_empty());
    assert_eq!(r.options.identities_only, Some(Flag::No));
    // Without a home the default files cannot be named: they are kept without a path.
    let dir = folder();
    let path = write(&dir, "config", "");
    let mut ctx = context(Vec::new());
    ctx.home = None;
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(path),
    })
    .expect("load")
    .resolve("web", &ctx);
    assert!(r.identity_files.iter().all(|k| k.path.is_none()));
}

#[test]
fn an_unset_variable_in_a_key_file_refuses() {
    let r = resolve("IdentityFile /k/${TASTY_UNSET_FOR_TEST}\n", "web");
    let f = r.refusal.as_ref().expect("refused");
    assert_eq!(f.keyword.as_deref(), Some("identityfile"));
    assert!(r.identity_files.is_empty());
}

#[test]
fn a_refused_key_file_leaves_no_key_files() {
    // The first entry expands, the second does not: the lists are empty, not cut.
    let r = resolve(
        "Host web\n  IdentityFile /k/one\n  IdentityFile /k/${TASTY_UNSET_FOR_TEST}\n  \
         CertificateFile /c/one\n",
        "web",
    );
    let f = r.refusal.as_ref().expect("refused");
    assert_eq!(f.kind, RefusalKind::ConfigError);
    assert_eq!(
        refusal_place(&r),
        (Some("identityfile".to_string()), Some(3))
    );
    assert!(r.identity_files.is_empty(), "{:?}", r.identity_files);
    assert!(r.certificate_files.is_empty(), "{:?}", r.certificate_files);
    // `ssh -G` shows the values (`ssh.c:1612`-`:1615` comes before `ssh.c:1646`).
    assert_eq!(r.options.identity_files.len(), 2);
    let dump = r.dump_client_config().expect("a dump");
    assert!(
        dump.contains("identityfile /k/${TASTY_UNSET_FOR_TEST}"),
        "{dump}"
    );
}

#[test]
fn path_refusals_name_the_keyword_and_line() {
    for (keyword, line) in [
        ("IdentityFile", "identityfile"),
        ("CertificateFile", "certificatefile"),
        ("UserKnownHostsFile", "userknownhostsfile"),
        ("GlobalKnownHostsFile", "globalknownhostsfile"),
        ("ControlPath", "controlpath"),
        ("RevokedHostKeys", "revokedhostkeys"),
    ] {
        let r = resolve(
            &format!("Host web\n  User alice\n  {keyword} ~other_user/x\n"),
            "web",
        );
        assert_eq!(
            refusal_kind(&r),
            Some(RefusalKind::Undeterminable),
            "{keyword}"
        );
        assert_eq!(
            refusal_place(&r),
            (Some(line.to_string()), Some(3)),
            "{keyword}"
        );
    }
    let r = resolve(
        "Host web\n  UserKnownHostsFile /k/${TASTY_UNSET_FOR_TEST}\n",
        "web",
    );
    assert_eq!(refusal_kind(&r), Some(RefusalKind::ConfigError));
    assert_eq!(
        refusal_place(&r),
        (Some("userknownhostsfile".to_string()), Some(2))
    );
}

#[test]
fn a_tilde_from_a_variable_is_expanded_for_the_lookup() {
    let dir = folder();
    let path = write(
        &dir,
        "config",
        "Host web\n  UserKnownHostsFile ${KH_PATH}\n",
    );
    let mut ctx = context(Vec::new());
    ctx.env.insert("KH_PATH".to_string(), "~/kh".to_string());
    let r = ConfigSet::load(ConfigInputs {
        files: ConfigFiles::Only(path),
    })
    .expect("load")
    .resolve("web", &ctx);
    assert!(r.refusal.is_none(), "{:?}", r.refusal);
    // `ssh -G` shows the value after `${}` (`ssh.c:1535`-`:1546`); the lookup expands the
    // `~` it gave (`ssh.c:1791`-`:1794`).
    assert_eq!(r.options.user_hostfiles, vec!["~/kh".to_string()]);
    assert_eq!(r.known_hosts_files[0], PathBuf::from("/home/tester/kh"));
}

#[test]
fn known_hosts_files_and_query() {
    let r = resolve(
        "Host web\n  Port 2222\n  UserKnownHostsFile ~/kh %d/kh2\n  \
         GlobalKnownHostsFile ~/global /etc/g2\n",
        "web",
    );
    assert_eq!(
        r.known_hosts_files,
        vec![
            PathBuf::from("/home/tester/kh"),
            PathBuf::from("/home/tester/kh2"),
            PathBuf::from("/home/tester/global"),
            PathBuf::from("/etc/g2"),
        ]
    );
    assert_eq!(r.host_key_query().name, "[web]:2222");
    let alias = resolve("Host web\n  Port 2222\n  HostKeyAlias other\n", "web");
    assert_eq!(alias.host_key_query().name, "other");
    let plain = resolve("", "web");
    assert_eq!(plain.host_key_query().name, "web");
}
