//! `ssh -G` comparison (docs/design/ssh-backends.md "시험 목록", "비교 시험의 기준 클라이언트").
//!
//! Every folder under `tests/fixtures/` has a `config` and a `cases` file. Each line of `cases`
//! is the arguments of one `ssh -F <folder>/config -G <arguments>` call (`-o`, `-p`, `-l`, then
//! the destination). The reference client must print the same text as
//! [`Resolution::dump_client_config`]; where it fails, the resolution must be refused.
//!
//! The reference client is the one `scripts/install-openssh-ref.sh` builds from the base
//! commit: `$TASTY_OPENSSH_REF_DIR/bin/ssh` or `<workspace>/target/tools/openssh-10.6p2/bin/ssh`,
//! never one from `PATH`. Its `ssh -V` must start with the base version. Without it the test
//! fails when `TASTY_OPENSSH_REF_REQUIRED` is not empty (CI) and is skipped with a message
//! otherwise. Windows does not compare.
//!
//! OpenSSH takes the local user name and home from the passwd entry, not from `USER` or
//! `HOME`, and the host name from `gethostname`. The test asks the reference client for them
//! (`~`, `%u`, `%l` and `%i` of a probe configuration) and gives the same values to the port.

use std::path::{Path, PathBuf};

use tasty_net_ssh_config::settings::OPENSSH_VERSION;
use tasty_net_ssh_config::{
    CommandLine, CommandLineValue, ConfigFiles, ConfigInputs, ConfigSet, RefusalKind, Resolution,
    ResolveContext,
};

/// The environment value the fixtures use for absolute `Include` paths.
const FIXTURE_DIR_VAR: &str = "TASTY_FIXTURE_DIR";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reference_client() -> PathBuf {
    match std::env::var_os("TASTY_OPENSSH_REF_DIR") {
        Some(dir) => PathBuf::from(dir).join("bin/ssh"),
        None => manifest_dir().join("../../target/tools/openssh-10.6p2/bin/ssh"),
    }
}

/// `ssh -F <config> -G <args>` of the reference client.
#[cfg(unix)]
fn reference(ssh: &Path, config: &Path, args: &[&str], fixture: &Path) -> std::process::Output {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(ssh);
    // ssh -G puts argv[0] into the ProxyCommand that ProxyJump makes; the port writes "ssh".
    cmd.arg0("ssh")
        .env_clear()
        .env(FIXTURE_DIR_VAR, fixture)
        .arg("-F")
        .arg(config)
        .arg("-G")
        .args(args);
    cmd.output().expect("run the reference client")
}

#[cfg(not(unix))]
fn reference(_: &Path, _: &Path, _: &[&str], _: &Path) -> std::process::Output {
    unreachable!("the comparison does not run on this OS")
}

/// The local values OpenSSH uses.
struct Local {
    user: String,
    home: String,
    hostname: String,
    uid: u32,
}

/// Ask the reference client for the passwd name and home, the host name and the uid.
fn local_values(ssh: &Path) -> Local {
    const MARK: &str = "@@";
    let dir =
        std::env::temp_dir().join(format!("tasty-net-ssh-config-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("probe folder");
    let config = dir.join("config");
    std::fs::write(
        &config,
        format!("UserKnownHostsFile ~/{MARK}%u{MARK}%l{MARK}%i\n"),
    )
    .expect("probe config");
    let out = reference(ssh, &config, &["probe"], &dir);
    assert!(
        out.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix("userknownhostsfile "))
        .expect("the probe line");
    let parts: Vec<&str> = value.split(MARK).collect();
    assert_eq!(parts.len(), 4, "probe value {value:?}");
    Local {
        home: parts[0].trim_end_matches('/').to_string(),
        user: parts[1].to_string(),
        hostname: parts[2].to_string(),
        uid: parts[3].parse().expect("the uid"),
    }
}

/// Clear group and other write on the fixture files: OpenSSH refuses an included file with
/// them (`CHECKPERM`), and a checkout made under umask 002 has them.
#[cfg(unix)]
fn restrict_modes(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    const GROUP_OTHER_WRITE: u32 = 0o022;
    for entry in std::fs::read_dir(dir).expect("fixture folder") {
        let path = entry.expect("fixture entry").path();
        if path.is_dir() {
            restrict_modes(&path);
        }
        let mut perm = std::fs::metadata(&path)
            .expect("fixture metadata")
            .permissions();
        perm.set_mode(perm.mode() & !GROUP_OTHER_WRITE);
        std::fs::set_permissions(&path, perm).expect("fixture mode");
    }
}

#[cfg(not(unix))]
fn restrict_modes(_: &Path) {}

fn context(args: &[&str], fixture: &Path, local: &Local) -> (ResolveContext, String) {
    let mut command_line = Vec::new();
    let mut it = args.iter();
    let mut target = String::new();
    while let Some(a) = it.next() {
        let value = match *a {
            "-o" => CommandLine::Option(it.next().expect("-o value").to_string()),
            "-p" => CommandLine::Port(it.next().expect("-p value").to_string()),
            "-l" => CommandLine::User(it.next().expect("-l value").to_string()),
            "-N" => CommandLine::NoCommand,
            t => {
                target = t.to_string();
                continue;
            }
        };
        command_line.push(CommandLineValue {
            value,
            label: "command-line".to_string(),
        });
    }
    let mut ctx = ResolveContext {
        command_line,
        local_user: Some(local.user.clone()),
        home: Some(local.home.clone()),
        local_uid: Some(local.uid),
        local_hostname: Some(local.hostname.clone()),
        ..ResolveContext::default()
    };
    ctx.env.insert(
        FIXTURE_DIR_VAR.to_string(),
        fixture.to_string_lossy().into_owned(),
    );
    (ctx, target)
}

fn first_difference(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    for i in 0..w.len().max(g.len()) {
        if w.get(i) != g.get(i) {
            return format!("line {}: ssh -G {:?}, port {:?}", i + 1, w.get(i), g.get(i));
        }
    }
    "no difference".to_string()
}

#[test]
fn matches_ssh_g() {
    if cfg!(windows) {
        println!("ssh -G comparison: not run on Windows");
        return;
    }
    let ssh = reference_client();
    if !ssh.exists() {
        let msg = format!(
            "ssh -G comparison: no reference client at {} (bash scripts/install-openssh-ref.sh)",
            ssh.display()
        );
        if std::env::var_os("TASTY_OPENSSH_REF_REQUIRED").is_some_and(|v| !v.is_empty()) {
            panic!("{msg}");
        }
        println!("{msg}; skipped");
        return;
    }
    let version = std::process::Command::new(&ssh)
        .arg("-V")
        .output()
        .expect("run ssh -V");
    let line = String::from_utf8_lossy(&version.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    println!("reference client: {line}");
    assert!(
        line.starts_with(OPENSSH_VERSION),
        "reference client version differs: want {OPENSSH_VERSION}, got {line:?}"
    );
    let local = local_values(&ssh);

    let root = manifest_dir().join("tests/fixtures");
    restrict_modes(&root);
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("tests/fixtures")
        .map(|e| e.expect("fixture entry").path())
        .filter(|p| p.join("cases").exists())
        .collect();
    fixtures.sort();

    let (mut cases, mut same, mut both_refused) = (0usize, 0usize, 0usize);
    let mut failures = Vec::new();
    for fixture in &fixtures {
        let fixture = fixture.canonicalize().expect("fixture path");
        let set = ConfigSet::load(ConfigInputs {
            files: ConfigFiles::Only(fixture.join("config")),
        })
        .expect("load the fixture");
        let name = fixture
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let list = std::fs::read_to_string(fixture.join("cases")).expect("cases");
        for case in list
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            cases += 1;
            let args: Vec<&str> = case.split_whitespace().collect();
            let out = reference(&ssh, &fixture.join("config"), &args, &fixture);
            let (ctx, target) = context(&args, &fixture, &local);
            let r: Resolution = set.resolve(&target, &ctx);
            let label = format!("{name}: ssh -G {case}");
            // Only the errors-* fixtures are meant to fail.
            let should_fail = name.starts_with("errors-");
            if out.status.success() == should_fail {
                failures.push(format!(
                    "{label}: ssh {} but the fixture expects {}: {}",
                    if should_fail { "succeeds" } else { "fails" },
                    if should_fail { "a failure" } else { "success" },
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .last()
                        .unwrap_or("")
                ));
                continue;
            }
            if out.status.success() {
                let want = String::from_utf8_lossy(&out.stdout).into_owned();
                let config_refusal = r.refusal.as_ref().filter(|f| {
                    matches!(
                        f.kind,
                        RefusalKind::ConfigError
                            | RefusalKind::UnknownKeyword
                            | RefusalKind::RefuseConnection
                    )
                });
                if let Some(f) = config_refusal {
                    failures.push(format!("{label}: ssh accepts, port refuses: {f}"));
                    continue;
                }
                match r.dump_client_config() {
                    Ok(got) if got == want => same += 1,
                    Ok(got) => failures.push(format!("{label}: {}", first_difference(&want, &got))),
                    Err(e) => failures.push(format!("{label}: dump failed: {e}")),
                }
            } else {
                let err = String::from_utf8_lossy(&out.stderr);
                let err = err.lines().last().unwrap_or("").to_string();
                match &r.refusal {
                    Some(f)
                        if matches!(
                            f.kind,
                            RefusalKind::ConfigError
                                | RefusalKind::UnknownKeyword
                                | RefusalKind::RefuseConnection
                        ) =>
                    {
                        both_refused += 1;
                        println!("{label}: both refuse (ssh: {err}; port: {f})");
                    }
                    other => {
                        failures.push(format!("{label}: ssh fails ({err}), port gives {other:?}"))
                    }
                }
            }
        }
    }
    println!(
        "ssh -G comparison: {} fixtures, {cases} cases, {same} same output, {both_refused} refused by both, {} differences",
        fixtures.len(),
        failures.len()
    );
    for f in &failures {
        println!("  {f}");
    }
    assert!(cases > 0, "no comparison case under {}", root.display());
    assert!(failures.is_empty(), "{} differences", failures.len());
}
