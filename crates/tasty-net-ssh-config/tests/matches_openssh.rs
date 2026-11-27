//! `ssh -G` comparison (docs/design/ssh-backends.md "시험 목록", "비교 시험의 기준 클라이언트").
//!
//! Every folder under `tests/fixtures/` has a `config` and a `cases` file. Each line of `cases`
//! is the arguments of one `ssh -F <folder>/config -G <arguments>` call (`-o`, `-p`, `-l`, `-N`,
//! then the destination), optionally followed by `=>` and what is expected:
//!
//! - nothing: the reference client succeeds, prints the same text as
//!   [`Resolution::dump_client_config`], and the resolution is not refused;
//! - `also-refused <kind>`: the same, and this crate refuses the resolution with that kind
//!   (`security_not_implemented` or `undeterminable`, a refusal OpenSSH does not make);
//! - `refused <kind> <place> [<keyword>]`: the reference client exits with 255 and its error
//!   output names the place, and this crate refuses with that kind at that place (and keyword).
//!   The place is `<file>:<line>` with the file relative to the fixture folder,
//!   `command-line` for a `-o` value, or `-` for a stop after reading (`ssh.c`, the
//!   `ssh -G` output); with `-` the error output must name the keyword instead.
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
    CommandLine, CommandLineValue, ConfigFiles, ConfigInputs, ConfigSet, Origin, RefusalKind,
    Resolution, ResolveContext,
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

/// Copy the fixtures to a new folder and clear group and other write on the copies: OpenSSH
/// refuses an included file with them (`CHECKPERM`), and a checkout made under umask 002 has
/// them. The checkout itself is not changed.
fn copy_fixtures(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("fixture copy folder");
    for entry in std::fs::read_dir(from).expect("fixture folder") {
        let path = entry.expect("fixture entry").path();
        let target = to.join(path.file_name().expect("fixture name"));
        if path.is_dir() {
            copy_fixtures(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("copy a fixture");
        }
        restrict_mode(&target);
    }
}

#[cfg(unix)]
fn restrict_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    const GROUP_OTHER_WRITE: u32 = 0o022;
    let mut perm = std::fs::metadata(path)
        .expect("fixture metadata")
        .permissions();
    perm.set_mode(perm.mode() & !GROUP_OTHER_WRITE);
    std::fs::set_permissions(path, perm).expect("fixture mode");
}

#[cfg(not(unix))]
fn restrict_mode(_: &Path) {}

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

/// What a case expects (see the file comment).
enum Expect {
    Same,
    AlsoRefused(RefusalKind),
    Refused {
        kind: RefusalKind,
        place: Place,
        keyword: Option<String>,
    },
}

enum Place {
    File { file: String, line: usize },
    CommandLine,
    AfterReading,
}

fn kind_of(name: &str) -> Result<RefusalKind, String> {
    Ok(match name {
        "config_error" => RefusalKind::ConfigError,
        "unknown_keyword" => RefusalKind::UnknownKeyword,
        "security_not_implemented" => RefusalKind::SecurityNotImplemented,
        "refuse_connection" => RefusalKind::RefuseConnection,
        "undeterminable" => RefusalKind::Undeterminable,
        other => return Err(format!("unknown refusal kind {other:?}")),
    })
}

fn parse_expect(text: &str) -> Result<Expect, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        ["also-refused", kind] => {
            let kind = kind_of(kind)?;
            if !matches!(
                kind,
                RefusalKind::SecurityNotImplemented | RefusalKind::Undeterminable
            ) {
                return Err(format!(
                    "also-refused takes a refusal OpenSSH does not make, not {kind:?}"
                ));
            }
            Ok(Expect::AlsoRefused(kind))
        }
        ["refused", kind, place, rest @ ..] if rest.len() <= 1 => {
            let place = if *place == "command-line" {
                Place::CommandLine
            } else if *place == "-" {
                if rest.is_empty() {
                    return Err("a refusal after reading names its keyword".to_string());
                }
                Place::AfterReading
            } else {
                let (file, line) = place
                    .rsplit_once(':')
                    .ok_or_else(|| format!("bad place {place:?}"))?;
                Place::File {
                    file: file.to_string(),
                    line: line.parse().map_err(|_| format!("bad line in {place:?}"))?,
                }
            };
            Ok(Expect::Refused {
                kind: kind_of(kind)?,
                place,
                keyword: rest.first().map(|k| k.to_string()),
            })
        }
        _ => Err(format!("bad expectation {text:?}")),
    }
}

/// The exit code OpenSSH uses for a configuration error (`exit(255)`, `fatal`).
const SSH_FAILURE: i32 = 255;

/// `Ok(true)`: the same output; `Ok(false)`: refused as expected; `Err`: the difference.
fn check_case(
    expect: &Expect,
    out: &std::process::Output,
    stderr: &str,
    r: &Resolution,
    fixture: &Path,
) -> Result<bool, String> {
    let last = stderr.lines().last().unwrap_or("");
    match expect {
        Expect::Same | Expect::AlsoRefused(_) => {
            if !out.status.success() {
                return Err(format!("ssh fails ({last}) but the case expects success"));
            }
            let want_kind = match expect {
                Expect::AlsoRefused(k) => Some(*k),
                _ => None,
            };
            let got_kind = r.refusal.as_ref().map(|f| f.kind);
            if got_kind != want_kind {
                return Err(format!(
                    "port refusal {:?} ({}), the case expects {want_kind:?}",
                    got_kind,
                    r.refusal
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                ));
            }
            let want = String::from_utf8_lossy(&out.stdout).into_owned();
            match r.dump_client_config() {
                Ok(got) if got == want => Ok(true),
                Ok(got) => Err(first_difference(&want, &got)),
                Err(e) => Err(format!("dump failed: {e}")),
            }
        }
        Expect::Refused {
            kind,
            place,
            keyword,
        } => {
            if out.status.code() != Some(SSH_FAILURE) {
                return Err(format!(
                    "ssh exits with {:?} ({last}), the case expects {SSH_FAILURE}",
                    out.status.code()
                ));
            }
            let (want_origin, mark) = match place {
                Place::File { file, line } => {
                    let path = fixture.join(file);
                    let mark = format!("{} line {line}:", path.display());
                    (Some(Origin::File { path, line: *line }), mark)
                }
                Place::CommandLine => (
                    Some(Origin::CommandLine {
                        label: "command-line".to_string(),
                    }),
                    "command-line line 0:".to_string(),
                ),
                Place::AfterReading => (None, keyword.clone().unwrap_or_default()),
            };
            if !stderr
                .to_ascii_lowercase()
                .contains(&mark.to_ascii_lowercase())
            {
                return Err(format!("ssh error output does not name {mark:?}: {stderr}"));
            }
            let Some(f) = &r.refusal else {
                return Err("ssh refuses, the port does not".to_string());
            };
            let origin_ok = want_origin.is_none() || f.origin == want_origin;
            if f.kind != *kind || !origin_ok {
                return Err(format!(
                    "port refusal {:?} at {:?} ({f}), the case expects {kind:?} at {want_origin:?}",
                    f.kind, f.origin
                ));
            }
            if keyword.is_some() && f.keyword != *keyword {
                return Err(format!(
                    "port refusal keyword {:?}, the case expects {keyword:?}",
                    f.keyword
                ));
            }
            Ok(false)
        }
    }
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

    let source = manifest_dir().join("tests/fixtures");
    let root = std::env::temp_dir().join(format!(
        "tasty-net-ssh-config-fixtures-{}",
        std::process::id()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("remove an old fixture copy");
    }
    copy_fixtures(&source, &root);
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("tests/fixtures")
        .map(|e| e.expect("fixture entry").path())
        .filter(|p| p.join("cases").exists())
        .collect();
    fixtures.sort();

    let (mut cases, mut same, mut refused_alike) = (0usize, 0usize, 0usize);
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
            let (call, expect) = match case.split_once("=>") {
                Some((c, e)) => (c.trim(), parse_expect(e)),
                None => (case, Ok(Expect::Same)),
            };
            let label = format!("{name}: ssh -G {case}");
            let expect = match expect {
                Ok(e) => e,
                Err(e) => {
                    failures.push(format!("{label}: {e}"));
                    continue;
                }
            };
            let args: Vec<&str> = call.split_whitespace().collect();
            let out = reference(&ssh, &fixture.join("config"), &args, &fixture);
            let (ctx, target) = context(&args, &fixture, &local);
            let r: Resolution = set.resolve(&target, &ctx);
            // OpenSSH writes `<file> line <n>:`, and `<file>: line <n>:` for an unknown keyword.
            let stderr = String::from_utf8_lossy(&out.stderr).replace(": line ", " line ");
            match check_case(&expect, &out, &stderr, &r, &fixture) {
                Ok(true) => same += 1,
                Ok(false) => {
                    refused_alike += 1;
                    println!(
                        "{label}: both refuse (ssh: {}; port: {})",
                        stderr.lines().last().unwrap_or(""),
                        r.refusal
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    );
                }
                Err(e) => failures.push(format!("{label}: {e}")),
            }
        }
    }
    println!(
        "ssh -G comparison: {} fixtures, {cases} cases, {same} same output, {refused_alike} refused alike, {} differences",
        fixtures.len(),
        failures.len()
    );
    for f in &failures {
        println!("  {f}");
    }
    std::fs::remove_dir_all(&root).expect("remove the fixture copy");
    assert!(cases > 0, "no comparison case under {}", source.display());
    assert!(failures.is_empty(), "{} differences", failures.len());
}
