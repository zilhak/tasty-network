//! The umask is process-wide, so this test has its own binary and is its only test.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use tasty_net_identity::{
    ConnectionRecord, ConnectionToken, DEVICE_KEY_FILE, DeviceKey, RetireOutcome,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Restores the previous umask when dropped, also when an assertion fails.
struct Umask(libc::mode_t);

impl Umask {
    fn set(mask: libc::mode_t) -> Self {
        // SAFETY: umask has no preconditions; it only swaps the process file creation mask.
        Self(unsafe { libc::umask(mask) })
    }
}

impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: as above; this restores the mask saved by `set`.
        unsafe { libc::umask(self.0) };
    }
}

#[test]
fn new_secret_files_and_directories_are_private_under_a_umask_that_removes_owner_bits() {
    let root = tempfile::tempdir().unwrap();
    let key_dir = root.path().join("keys");
    let record_dir = root.path().join("home");
    let nested_dir = root.path().join("a").join("keys");
    {
        // 0700 & !0277 = 0500: without the explicit chmod the owner could not write.
        let _mask = Umask::set(0o277);
        DeviceKey::load_or_create(&key_dir).unwrap();
        // Two missing levels, created by eight threads at once: an intermediate directory that
        // another thread made (and has not set to 0700 yet) must not fail the others.
        let ids: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| DeviceKey::load_or_create(&nested_dir).unwrap().id()))
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let record = ConnectionRecord {
            port: 1,
            token: ConnectionToken::issue().unwrap(),
            instance_id: "i".to_owned(),
        };
        let publication = record.publish(&record_dir.join("record")).unwrap();
        assert_eq!(mode(&record_dir), 0o700);
        assert_eq!(mode(&record_dir.join("record")), 0o600);
        // 0600 & !0277 = 0400: without the explicit chmod retire could not reopen the lock file.
        assert_eq!(publication.retire().unwrap(), RetireOutcome::Removed);
        assert!(!record_dir.join("record").exists());
        assert_eq!(mode(&record_dir.join("record.lock")), 0o600);
    }
    assert_eq!(mode(&key_dir), 0o700);
    assert_eq!(mode(&key_dir.join(DEVICE_KEY_FILE)), 0o600);
    assert_eq!(mode(&root.path().join("a")), 0o700);
    assert_eq!(mode(&nested_dir), 0o700);
    assert_eq!(mode(&nested_dir.join(DEVICE_KEY_FILE)), 0o600);
}
