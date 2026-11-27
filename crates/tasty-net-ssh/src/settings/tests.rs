use super::*;
use crate::{OptionSource, SshLauncher, SshTarget, push_common_opts};

/// The rows of the first table after the heading `heading` of a design document, as
/// (key, cells by column name).
fn table_rows(doc: &str, heading: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/design")
        .join(doc);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut lines = text
        .lines()
        .skip_while(|l| !(l.starts_with('#') && l.trim_start_matches('#').trim() == heading));
    assert!(lines.next().is_some(), "{doc}: no heading {heading}");
    let mut table = lines
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'));
    let cells = |l: &str| -> Vec<String> {
        l.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_owned())
            .collect()
    };
    let header = cells(table.next().expect("table header"));
    table.next(); // the |---| line
    table
        .map(|l| {
            let row = cells(l);
            let key = row[0].trim_matches('`').to_owned();
            (key, header.iter().cloned().zip(row).collect())
        })
        .collect()
}

/// The comparable form of a design cell: the text before a parenthesis, without backticks and
/// without the spaces that group digits.
fn normalize(cell: &str) -> String {
    let head = cell.split('(').next().unwrap_or(cell).replace('`', "");
    let chars: Vec<char> = head.trim().chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        let between_digits = *c == ' '
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if !between_digits {
            out.push(*c);
        }
    }
    out
}

#[test]
fn defaults_match_design_tables() {
    let rows = table_rows("transport.md", "SSH 크레이트 설정값");
    let doc_keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    let code_keys: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(code_keys, doc_keys, "the keys and their order");
    for ((key, cells), s) in rows.iter().zip(DEFAULTS) {
        assert_eq!(
            normalize(&s.default.to_string()),
            normalize(&cells["기본"]),
            "{key}: default"
        );
        assert_eq!(s.range_text(), normalize(&cells["범위"]), "{key}: range");
    }
}

fn secs(n: u64) -> Value {
    Value::Duration(Duration::from_secs(n))
}

fn ms(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

#[test]
fn settings_list_matches_query() {
    let mut settings = SshSettings::new();
    settings.set("ssh.connect_timeout", secs(20)).unwrap();
    // The same value as the default given explicitly is still `configured`.
    settings.set("backoff.multiplier", Value::Count(2)).unwrap();
    let launcher = SshLauncher::new("ssh").with_settings(settings.clone());
    let entries = launcher.settings().entries();
    let keys: Vec<&str> = entries.iter().map(|e| e.key).collect();
    let listed: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(keys, listed);
    for e in &entries {
        let want = match e.key {
            "ssh.connect_timeout" | "backoff.multiplier" => Source::Configured,
            _ => Source::Default,
        };
        assert_eq!(e.source, want, "{e}");
        assert_eq!(launcher.settings().get(e.key), Some(*e));
    }
    assert_eq!(
        launcher
            .settings()
            .get("ssh.connect_timeout")
            .unwrap()
            .to_string(),
        "ssh.connect_timeout = 20 s (configured)"
    );
    assert_eq!(
        launcher
            .settings()
            .get("discovery.output_limit")
            .unwrap()
            .to_string(),
        "discovery.output_limit = 64 KiB (default)"
    );
    // The values the crate runs with are the values the query shows.
    let s = launcher.settings();
    assert_eq!(s.connect_timeout(), Duration::from_secs(20));
    assert_eq!(s.server_alive_interval(), Duration::from_secs(15));
    assert_eq!(s.server_alive_count_max(), 3);
    assert_eq!(s.step_timeout(), crate::PORT_DISCOVERY_STEP_TIMEOUT);
    assert_eq!(s.total_timeout(), crate::PORT_DISCOVERY_TOTAL_TIMEOUT);
    assert_eq!(s.output_limit(), 64 * 1024);
    assert_eq!(s.stderr_limit(), 16 * 1024);
    assert_eq!(s.stderr_detail_limit(), 512);
    assert_eq!(s.overflow_exit_grace(), Duration::from_millis(100));
    assert_eq!(s.stderr_grace(), Duration::from_millis(500));
    assert_eq!(s.poll_min(), Duration::from_millis(5));
    assert_eq!(s.poll_max(), Duration::from_millis(50));
    assert_eq!(s.ready_timeout(), Duration::from_secs(5));
    assert_eq!(s.ready_poll(), Duration::from_millis(50));
    assert_eq!(s.stderr_drain_limit(), Duration::from_millis(500));
    assert_eq!(s.stderr_drain_poll(), Duration::from_millis(10));
    assert_eq!(s.stderr_max_line(), 1024);
    assert_eq!(s.verdict_grace(), Duration::from_millis(500));
    assert_eq!(s.verdict_poll(), Duration::from_millis(20));
    assert_eq!(s.backoff_initial(), Duration::from_millis(500));
    assert_eq!(s.backoff_multiplier(), 2);
    assert_eq!(s.backoff_max_delay(), Duration::from_secs(30));
    assert_eq!(
        SshSettings::default().connect_timeout(),
        crate::SSH_CONNECT_TIMEOUT
    );
    // Unknown keys, wrong types and values out of range are refused and change nothing.
    let before = settings.clone();
    assert_eq!(
        settings.set("connect_timeout", secs(1)).unwrap_err(),
        SettingsError::UnknownKey("connect_timeout".to_owned())
    );
    for (key, value) in [
        ("ssh.server_alive_count_max", secs(1)),
        ("discovery.output_limit", Value::Count(1024)),
        // An ssh option takes whole seconds.
        ("ssh.server_alive_interval", ms(1500)),
    ] {
        assert!(
            matches!(
                settings.set(key, value),
                Err(SettingsError::WrongType { .. })
            ),
            "{key} = {value}"
        );
    }
    for (key, value) in [
        ("ssh.connect_timeout", secs(0)),
        ("ssh.server_alive_count_max", Value::Count(101)),
        ("discovery.total_timeout", secs(3601)),
        ("discovery.output_limit", Value::Bytes(1023)),
        ("backoff.multiplier", Value::Count(0)),
    ] {
        assert!(
            matches!(
                settings.set(key, value),
                Err(SettingsError::OutOfRange { .. })
            ),
            "{key} = {value}"
        );
    }
    assert_eq!(settings, before);
    // Both ends of a range are accepted.
    for s in DEFAULTS {
        let mut one = SshSettings::new();
        one.set(s.key, s.min).unwrap();
        one.set(s.key, s.max).unwrap();
    }
}

fn args(settings: &SshSettings, target: &SshTarget) -> Vec<String> {
    let mut args = Vec::new();
    push_common_opts(&mut args, target, false, &[], settings);
    args
}

/// The ssh options come from the settings, and the launcher's command carries them.
#[test]
fn the_ssh_options_come_from_the_settings() {
    let target = SshTarget::parse("host");
    let defaults = args(&SshSettings::default(), &target);
    for want in [
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "ConnectTimeout=10",
    ] {
        assert!(defaults.iter().any(|a| a == want), "{want}: {defaults:?}");
    }
    let mut settings = SshSettings::new();
    settings
        .set("ssh.server_alive_interval", secs(5))
        .unwrap()
        .set("ssh.server_alive_count_max", Value::Count(7))
        .unwrap()
        .set("ssh.connect_timeout", secs(42))
        .unwrap();
    let launcher = SshLauncher::new("ssh").with_settings(settings.clone());
    let cmd = launcher.command(&target, &[], &[], &[]);
    let from_launcher: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let built = args(&settings, &target);
    assert_eq!(from_launcher[..built.len()], built[..]);
    for want in [
        "ServerAliveInterval=5",
        "ServerAliveCountMax=7",
        "ConnectTimeout=42",
    ] {
        assert!(built.iter().any(|a| a == want), "{want}: {built:?}");
    }
    assert!(!built.iter().any(|a| a == "ServerAliveInterval=15"));
    // ConnectTimeout stays last, after the target's options.
    assert_eq!(built.last().map(String::as_str), Some("ConnectTimeout=42"));
}

/// The applied options say which value ssh uses: the settings' ServerAlive values win over the
/// target's (they come first), and the target's ConnectTimeout wins over the setting.
#[test]
fn applied_options_show_which_value_wins() {
    let mut settings = SshSettings::new();
    settings.set("ssh.server_alive_interval", secs(5)).unwrap();
    let launcher = SshLauncher::new("ssh").with_settings(settings);
    let shown = |target: &SshTarget| -> Vec<(String, String, String)> {
        launcher
            .applied_options(target)
            .iter()
            .map(|o| (o.option.to_owned(), o.value.clone(), o.source.to_string()))
            .collect()
    };
    let t = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
    let plain = SshTarget::parse("host");
    assert_eq!(
        shown(&plain),
        [
            t("ServerAliveInterval", "5", "configured"),
            t("ServerAliveCountMax", "3", "default"),
            t("ConnectTimeout", "10", "default"),
        ]
    );
    let mut own = SshTarget::parse("host");
    own.extra_options = vec![
        "ServerAliveInterval=99".into(),
        "connecttimeout=60".into(),
        "ConnectTimeout=70".into(),
    ];
    assert_eq!(
        shown(&own),
        [
            t("ServerAliveInterval", "5", "configured"),
            t("ServerAliveCountMax", "3", "default"),
            t("ConnectTimeout", "60", "target"),
        ]
    );
    // The command agrees: the setting's ServerAliveInterval and the target's first
    // ConnectTimeout are the first of their names.
    let built = args(launcher.settings(), &own);
    let first = |name: &str| {
        built
            .iter()
            .find(|a| a.to_ascii_lowercase().starts_with(&format!("{name}=")))
            .cloned()
    };
    assert_eq!(
        first("serveraliveinterval").as_deref(),
        Some("ServerAliveInterval=5")
    );
    assert_eq!(
        first("connecttimeout").as_deref(),
        Some("connecttimeout=60")
    );
    let applied = launcher.applied_options(&own);
    assert_eq!(applied[2].key, "ssh.connect_timeout");
    assert_eq!(applied[2].source, OptionSource::Target);
    assert_eq!(
        applied[2].to_string(),
        "ssh.connect_timeout (ConnectTimeout) = 60 (target)"
    );
}

/// The target's ConnectTimeout is read in the OpenSSH time format. `none` leaves it to the
/// launcher (with the launcher's source), and a value that cannot be read is shown as written
/// while discovery classifies with the launcher value.
#[test]
fn the_target_connect_timeout_is_read_like_openssh() {
    use crate::{connect_timeout_for, parse_openssh_time};
    let mut configured = SshSettings::new();
    configured.set("ssh.connect_timeout", secs(20)).unwrap();
    let applied = |settings: &SshSettings, options: &[&str]| {
        let mut target = SshTarget::parse("host");
        target.extra_options = options.iter().map(|o| (*o).to_owned()).collect();
        let launcher = SshLauncher::new("ssh").with_settings(settings.clone());
        let connect = launcher
            .applied_options(&target)
            .into_iter()
            .find(|o| o.option == "ConnectTimeout")
            .expect("ConnectTimeout is applied");
        (
            connect.value,
            connect.source.to_string(),
            connect_timeout_for(settings, &target),
        )
    };
    let d = Duration::from_secs;
    let defaults = SshSettings::default();
    for (options, value, source, bound) in [
        (&["ConnectTimeout=1m"][..], "1m", "target", d(60)),
        (&["ConnectTimeout 1m30s"][..], "1m30s", "target", d(90)),
        (&["connecttimeout=90"][..], "90", "target", d(90)),
        (&["ConnectTimeout=1H"][..], "1H", "target", d(3600)),
        (&["ConnectTimeout=none"][..], "10", "default", d(10)),
        (
            &["ConnectTimeout=none", "ConnectTimeout=5"][..],
            "5",
            "target",
            d(5),
        ),
        (&["ConnectTimeout=ten"][..], "ten", "target", d(10)),
        (&["ConnectTimeout=0"][..], "0", "target", Duration::MAX),
        // Each unit at most once; a fraction is not read (the integer format).
        (&["ConnectTimeout=1s2s"][..], "1s2s", "target", d(10)),
        (&["ConnectTimeout=1.5"][..], "1.5", "target", d(10)),
        (&["ConnectTimeout=1m30.5s"][..], "1m30.5s", "target", d(10)),
        // Quotes come off as ssh splits the argument, before none and the number are told.
        (&["ConnectTimeout=\"none\""][..], "10", "default", d(10)),
        (
            &["ConnectTimeout=\"none\"", "ConnectTimeout=60"][..],
            "60",
            "target",
            d(60),
        ),
        (&["ConnectTimeout='60'"][..], "'60'", "target", d(60)),
        (&["ConnectTimeout=\"60"][..], "\"60", "target", d(10)),
        (&["ConnectTimeout=1m 30s"][..], "1m 30s", "target", d(10)),
        // The first value that is not none is the one ssh takes, readable or not.
        (
            &["ConnectTimeout=5", "ConnectTimeout=60"][..],
            "5",
            "target",
            d(5),
        ),
        (
            &["ConnectTimeout=ten", "ConnectTimeout=60"][..],
            "ten",
            "target",
            d(10),
        ),
    ] {
        assert_eq!(
            applied(&defaults, options),
            (value.to_owned(), source.to_owned(), bound),
            "{options:?}"
        );
    }
    assert_eq!(
        applied(&configured, &["ConnectTimeout=none"]),
        ("20".to_owned(), "configured".to_owned(), d(20))
    );
    assert_eq!(
        applied(&configured, &["ConnectTimeout=1x"]),
        ("1x".to_owned(), "target".to_owned(), d(20))
    );
    // The format itself.
    assert_eq!(parse_openssh_time("1w2d3h4m5s"), Some(d(788_645)));
    assert_eq!(parse_openssh_time("1m30"), Some(d(90)));
    for refused in [
        "",
        "m",
        "1.5",
        "1m30.5s",
        "-1",
        "1 m",
        "99999999999",
        "1s2s",
        "1s2",
        "1m1M",
    ] {
        assert_eq!(parse_openssh_time(refused), None, "{refused:?}");
    }
}
