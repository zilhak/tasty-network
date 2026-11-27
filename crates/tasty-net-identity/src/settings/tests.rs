use super::*;
use crate::{ConnectionRecord, ConnectionToken, CorruptKind, DeviceKey, IdentityError};
use crate::{RecordProblem, RetireOutcome};

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
    let rows = table_rows("identity.md", "설정값");
    let doc_keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    let code_keys: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(code_keys, doc_keys, "the keys and their order");
    for ((key, cells), s) in rows.iter().zip(DEFAULTS) {
        assert_eq!(
            normalize(&s.default.to_string()),
            normalize(&cells["기본"]),
            "{key}: default"
        );
        assert_eq!(
            format!("{} ~ {}", s.min, s.max),
            normalize(&cells["범위"]),
            "{key}: range"
        );
    }
}

#[test]
fn settings_list_matches_query() {
    let mut settings = IdentitySettings::new();
    settings
        .set("new_parent_wait", Value::Duration(Duration::from_millis(3)))
        .unwrap();
    // The same value as the default given explicitly is still `configured`.
    settings
        .set("max_secret_bytes", Value::Bytes(4 * KIB))
        .unwrap();
    let entries = settings.entries();
    let keys: Vec<&str> = entries.iter().map(|e| e.key).collect();
    let listed: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(keys, listed);
    for e in &entries {
        let want = match e.key {
            "new_parent_wait" | "max_secret_bytes" => Source::Configured,
            _ => Source::Default,
        };
        assert_eq!(e.source, want, "{e}");
        assert_eq!(settings.get(e.key), Some(*e));
    }
    assert_eq!(
        settings.get("new_parent_wait").unwrap().to_string(),
        "new_parent_wait = 3 ms (configured)"
    );
    assert_eq!(
        settings.get("new_parent_retries").unwrap().to_string(),
        "new_parent_retries = 50 (default)"
    );
    // The values the crate runs with are the values the query shows.
    assert_eq!(settings.new_parent_wait(), Duration::from_millis(3));
    assert_eq!(settings.new_parent_retries(), 50);
    assert_eq!(settings.max_secret_bytes(), 4096);
    // Unknown keys, wrong types and values out of range are refused and change nothing.
    let before = settings.clone();
    assert_eq!(
        settings.set("retries", Value::Count(1)).unwrap_err(),
        SettingsError::UnknownKey("retries".to_owned())
    );
    assert!(matches!(
        settings.set("new_parent_retries", Value::Duration(Duration::ZERO)),
        Err(SettingsError::WrongType { .. })
    ));
    for (key, value) in [
        ("new_parent_retries", Value::Count(1001)),
        (
            "new_parent_wait",
            Value::Duration(Duration::from_millis(1001)),
        ),
        ("max_secret_bytes", Value::Bytes(KIB - 1)),
        ("max_secret_bytes", Value::Bytes(64 * KIB + 1)),
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
        let mut one = IdentitySettings::new();
        one.set(s.key, s.min).unwrap();
        one.set(s.key, s.max).unwrap();
        assert_eq!(one.get(s.key).unwrap().value, s.max);
    }
}

/// A key file of `len` bytes: a valid key followed by line breaks.
fn padded_key_file(dir: &std::path::Path, len: usize) {
    let path = dir.join(crate::DEVICE_KEY_FILE);
    let mut text = std::fs::read(&path).unwrap();
    text.resize(len, b'\n');
    std::fs::write(&path, text).unwrap();
}

#[test]
fn the_secret_limit_applies_to_key_files_and_records() {
    let mut small = IdentitySettings::new();
    small.set("max_secret_bytes", Value::Bytes(KIB)).unwrap();
    let limit = small.max_secret_bytes();

    // Key file: longer than the configured limit is too large, also when the default would
    // read it (and find the trailing data).
    let dir = tempfile::tempdir().unwrap();
    DeviceKey::load_or_create_with(dir.path(), &small).unwrap();
    padded_key_file(dir.path(), limit + 1);
    let corrupt =
        |settings: &IdentitySettings| match DeviceKey::load_or_create_with(dir.path(), settings) {
            Err(IdentityError::Corrupt { kind, .. }) => kind,
            other => panic!("{other:?}"),
        };
    assert_eq!(corrupt(&small), CorruptKind::TooLarge);
    assert_eq!(
        corrupt(&IdentitySettings::default()),
        CorruptKind::TrailingData
    );

    // Record: a line one byte over the configured limit is refused before anything is
    // written; the default takes it.
    let record = |len: usize| {
        let mut r = ConnectionRecord {
            port: 1,
            token: ConnectionToken::issue().unwrap(),
            instance_id: "x".to_owned(),
        };
        let overhead = r.to_line().len() - 1;
        r.instance_id = "x".repeat(len - overhead);
        assert_eq!(r.to_line().len(), len);
        r
    };
    let path = dir.path().join("record");
    match record(limit + 1).publish_with(&path, &small) {
        Err(IdentityError::InvalidRecord(e)) => assert_eq!(e.problem, RecordProblem::TooLong),
        other => panic!("{other:?}"),
    }
    assert!(!path.exists());
    let publication = record(limit + 1).publish(&path).unwrap();
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);

    // The publication reads its file back with its own limit, at publication and at retire.
    let mut large = IdentitySettings::new();
    large
        .set("max_secret_bytes", Value::Bytes(8 * KIB))
        .unwrap();
    let long = record(large.max_secret_bytes());
    let publication = long.publish_with(&path, &large).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap().len(),
        large.max_secret_bytes()
    );
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
}

#[cfg(unix)]
#[test]
fn the_retry_settings_apply_to_a_new_directory() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    // A parent of the current user that is not writable yet: creating below it is EACCES,
    // which is retried as for a parent another thread has not set to 0700 yet.
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::create_dir(parent.join("probe")).is_ok() {
        // Running with privileges that bypass the mode; the case cannot be made here.
        return;
    }
    let dir = parent.join("keys");
    let run = |retries: u64, wait: Duration| {
        let mut s = IdentitySettings::new();
        s.set("new_parent_retries", Value::Count(retries)).unwrap();
        s.set("new_parent_wait", Value::Duration(wait)).unwrap();
        let start = Instant::now();
        let error = DeviceKey::load_or_create_with(&dir, &s).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::Io {
                    op: crate::FileOp::CreateDir,
                    ..
                }
            ),
            "{error:?}"
        );
        start.elapsed()
    };
    // Every retry waits: three of 40 ms take at least 120 ms.
    let waited = run(3, Duration::from_millis(40));
    assert!(waited >= Duration::from_millis(120), "{waited:?}");
    // No retry, no wait: far less than the one wait of 1 s that a retry would take.
    let direct = run(0, Duration::from_millis(1000));
    assert!(direct < Duration::from_millis(1000), "{direct:?}");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
}
