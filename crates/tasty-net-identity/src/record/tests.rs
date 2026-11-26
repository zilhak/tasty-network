use std::fs;

use super::*;
use crate::{
    FileOp,
    secret_file::{self, fault},
};

fn record(port: u16) -> ConnectionRecord {
    ConnectionRecord {
        port,
        token: ConnectionToken::issue().unwrap(),
        instance_id: "instance \"one\"".to_owned(),
    }
}

#[test]
fn line_round_trip() {
    let r = record(4242);
    let line = r.to_line();
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    assert_eq!(ConnectionRecord::parse(&line), Ok(ParsedRecord::Record(r)));
}

#[test]
fn legacy_port_and_unknown_fields() {
    assert_eq!(
        ConnectionRecord::parse("4242\n"),
        Ok(ParsedRecord::LegacyPort(4242))
    );
    assert_eq!(
        ConnectionRecord::parse("0").unwrap_err().problem,
        RecordProblem::BadPort
    );
    assert_eq!(
        ConnectionRecord::parse("70000").unwrap_err().problem,
        RecordProblem::BadPort
    );
    let token = ConnectionToken::issue().unwrap();
    let text = format!(
        r#"{{"future":1,"port":1,"token":"{}","instance_id":"i"}}"#,
        token.expose_hex()
    );
    assert!(matches!(
        ConnectionRecord::parse(&text),
        Ok(ParsedRecord::Record(ConnectionRecord { port: 1, .. }))
    ));
}

#[test]
fn errors_name_the_field_and_never_carry_the_text() {
    let token = ConnectionToken::issue().unwrap();
    let hex = token.expose_hex();
    let cases = [
        (
            format!(
                r#"{{"port":1,"token":"{}","instance_id":"i"}}"#,
                hex.to_uppercase()
            ),
            RecordProblem::BadToken,
        ),
        (
            format!(r#"{{"token":"{hex}","instance_id":"i"}}"#),
            RecordProblem::Missing(RecordField::Port),
        ),
        (
            format!(r#"{{"port":1,"instance_id":"i"}} {hex}"#),
            RecordProblem::NotRecord,
        ),
        (
            format!(r#"{{"port":"1","token":"{hex}","instance_id":"i"}}"#),
            RecordProblem::BadPort,
        ),
        (
            format!(r#"{{"port":1,"token":"{hex}","instance_id":""}}"#),
            RecordProblem::BadInstanceId,
        ),
        (
            format!(r#"{{"port":1,"token":"{hex}"}}"#),
            RecordProblem::Missing(RecordField::InstanceId),
        ),
        (format!("[\"{hex}\"]"), RecordProblem::NotRecord),
        (format!("port {hex}"), RecordProblem::NotRecord),
    ];
    for (text, problem) in cases {
        let error = ConnectionRecord::parse(&text).unwrap_err();
        assert_eq!(error.problem, problem, "{text}");
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(hex), "{shown}");
        assert!(!shown.to_lowercase().contains(hex), "{shown}");
    }
    assert_eq!(
        ConnectionRecord::parse(r#"{"port":1,"token":"abc","instance_id":"i"}"#)
            .unwrap_err()
            .to_string(),
        "token: not 64 lowercase hex"
    );
}

#[test]
fn debug_hides_the_token() {
    let r = record(1);
    assert!(!format!("{r:?}").contains(r.token.expose_hex()));
}

#[test]
fn publication_debug_hides_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let r = record(1);
    let publication = r.publish(&dir.path().join("record")).unwrap();
    let debug = format!("{publication:?} {publication:#?}");
    assert!(!debug.contains(r.token.expose_hex()), "{debug}");
    assert!(debug.contains("RecordPublication"));
    publication.retire().unwrap();
}

#[test]
fn publish_then_retire_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasty.record");
    let r = record(5000);
    let publication = r.publish(&path).unwrap();
    assert_eq!(publication.path(), path);
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(ConnectionRecord::parse(&text), Ok(ParsedRecord::Record(r)));
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
    assert!(!path.exists());
}

#[test]
fn two_issues_publish_different_tokens_with_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let key = crate::DeviceKey::load_or_create(dir.path()).unwrap();
    let path = dir.path().join("record");
    let first = record(1);
    let p1 = first.publish(&path).unwrap();
    let second = record(1);
    let p2 = second.publish(&path).unwrap();
    assert_ne!(first.token, second.token);
    assert_eq!(
        crate::DeviceKey::load_or_create(dir.path()).unwrap().id(),
        key.id()
    );
    assert_eq!(p1.retire().unwrap(), RetireOutcome::Replaced);
    assert_eq!(p2.retire().unwrap(), RetireOutcome::Removed);
}

#[test]
fn a_newer_instance_with_the_same_contents_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let r = record(1);
    let old = r.publish(&path).unwrap();
    // Same bytes, new generation: only the file identity tells them apart.
    let new = r.publish(&path).unwrap();
    assert_eq!(old.retire().unwrap(), RetireOutcome::Replaced);
    assert!(path.exists());
    assert_eq!(new.retire().unwrap(), RetireOutcome::Removed);
}

#[test]
fn retire_without_the_lock_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let publication = record(1).publish(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::remove_file(dir.path().join("record.lock")).unwrap();
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Missing);
}

#[test]
fn retire_after_removal_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let publication = record(1).publish(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Missing);
}

#[test]
fn protection_failure_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let error = {
        let _fail = fault::fail_protect();
        record(1).publish(&path).unwrap_err()
    };
    assert!(matches!(error, IdentityError::Protect { .. }), "{error:?}");
    assert!(!path.exists());
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_failed_cleanup_after_a_failed_publication_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let error = {
        let _protect = fault::fail_protect();
        let _discard = fault::fail_discard();
        record(1).publish(&path).unwrap_err()
    };
    let IdentityError::Leftover {
        leftover,
        cause: Some(cause),
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(
        matches!(**cause, IdentityError::Protect { .. }),
        "{error:?}"
    );
    assert!(leftover.exists());
    assert!(!path.exists());
}

fn assert_refused_and_kept(r: ConnectionRecord, problem: RecordProblem) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let kept = record(4242);
    let publication = kept.publish(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let error = r.publish(&path).unwrap_err();
    assert!(
        matches!(&error, IdentityError::InvalidRecord(e) if e.problem == problem),
        "{error:?}"
    );
    assert!(!format!("{error} {error:?}").contains(r.token.expose_hex()));
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "the existing record stays"
    );
    assert!(temps(dir.path()).is_empty());
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
}

fn temps(dir: &Path) -> Vec<std::ffi::OsString> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains(".tmp-"))
        .collect()
}

#[test]
fn records_a_reader_would_refuse_are_not_published() {
    assert_refused_and_kept(record(0), RecordProblem::BadPort);
    let mut empty = record(1);
    empty.instance_id.clear();
    assert_refused_and_kept(empty, RecordProblem::BadInstanceId);
    let mut long = record(1);
    long.instance_id = "x".repeat(secret_file::MAX_SECRET_LEN);
    assert_refused_and_kept(long, RecordProblem::TooLong);
}

#[test]
fn the_longest_accepted_record_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let mut r = record(65535);
    r.instance_id = String::new();
    let overhead = {
        let mut one = r.clone();
        one.instance_id = "x".to_owned();
        one.to_line().len() - 1
    };
    r.instance_id = "x".repeat(secret_file::MAX_SECRET_LEN - overhead);
    assert_eq!(r.to_line().len(), secret_file::MAX_SECRET_LEN);
    let publication = r.publish(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(ConnectionRecord::parse(&text), Ok(ParsedRecord::Record(r)));
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
}

#[test]
fn a_failure_after_the_replacement_removes_the_new_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let r = record(1);
    let error = {
        let _fail = fault::fail_after_replace();
        r.publish(&path).unwrap_err()
    };
    assert!(
        matches!(
            error,
            IdentityError::Io {
                op: FileOp::Read,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        !path.exists(),
        "no record without a publication handle stays"
    );
    assert!(temps(dir.path()).is_empty());
}

#[test]
fn a_failed_removal_after_the_replacement_names_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let error = {
        let _fail = fault::fail_after_replace();
        let _discard = fault::fail_discard();
        record(1).publish(&path).unwrap_err()
    };
    let IdentityError::Leftover {
        leftover,
        cause: Some(_),
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(leftover.file_name(), path.file_name());
}

#[test]
fn a_lock_file_name_is_not_a_record_path() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["record.lock", "record.LOCK", ".lock"] {
        let path = dir.path().join(name);
        let error = record(1).publish(&path).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::InvalidRecordPath {
                    problem: RecordPathProblem::LockSuffix,
                    ..
                }
            ),
            "{name}: {error:?}"
        );
        assert!(!path.exists());
    }
    let error = record(1).publish(Path::new("/")).unwrap_err();
    assert!(
        matches!(
            error,
            IdentityError::InvalidRecordPath {
                problem: RecordPathProblem::NoFileName,
                ..
            }
        ),
        "{error:?}"
    );

    // A record next to another record's lock file keeps working.
    let path = dir.path().join("record");
    let publication = record(1).publish(&path).unwrap();
    assert!(dir.path().join("record.lock").exists());
    let mut forged = record(2).publish(&dir.path().join("other")).unwrap();
    forged.target = dir.path().join("record.lock");
    forged.path = forged.target.clone();
    let error = forged.retire().unwrap_err();
    assert!(
        matches!(error, IdentityError::InvalidRecordPath { .. }),
        "{error:?}"
    );
    assert!(
        dir.path().join("record.lock").exists(),
        "retire touched no lock file"
    );
    assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
}

#[test]
fn concurrent_publishers_leave_one_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("record");
    let records: Vec<ConnectionRecord> = (1..=8).map(record).collect();
    let publications: Vec<RecordPublication> = std::thread::scope(|s| {
        let handles: Vec<_> = records
            .iter()
            .map(|r| s.spawn(|| r.publish(&path).unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let text = fs::read_to_string(&path).unwrap();
    let ParsedRecord::Record(left) = ConnectionRecord::parse(&text).unwrap() else {
        panic!("not a record");
    };
    assert!(records.contains(&left));
    let outcomes: Vec<RetireOutcome> = publications
        .into_iter()
        .map(|p| p.retire().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == RetireOutcome::Removed)
            .count(),
        1,
        "{outcomes:?}"
    );
    assert!(!path.exists());
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn record_is_0600_in_a_0700_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("home");
        let path = dir.join("record");
        let publication = record(1).publish(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        publication.retire().unwrap();
    }

    /// Unix only: a Windows directory cannot be removed while the publication keeps the
    /// record open.
    #[test]
    fn retire_after_the_directory_is_gone_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("home");
        let publication = record(1).publish(&dir.join("record")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(publication.retire().unwrap(), RetireOutcome::Missing);
        assert!(!dir.exists(), "retire creates nothing");
    }

    #[test]
    fn a_symlink_at_the_record_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        fs::write(&other, "keep").unwrap();
        let path = dir.path().join("record");
        symlink(&other, &path).unwrap();
        let error = record(1).publish(&path).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::Symlink,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(fs::read_to_string(&other).unwrap(), "keep");
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::secret_file::windows::testing::read_acl;

    #[test]
    fn record_acl_is_the_current_sid_only_and_protected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record");
        let publication = record(1).publish(&path).unwrap();
        let acl = read_acl(&path);
        assert!(
            acl.owner_is_current_user && acl.protected && !acl.dacl_null,
            "{acl:?}"
        );
        assert_eq!(acl.entries.len(), 1, "{acl:?}");
        assert!(
            acl.entries[0].current_user && acl.entries[0].flags == 0,
            "{acl:?}"
        );
        publication.retire().unwrap();
    }
}
