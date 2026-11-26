//! The umask is process-wide, so this test has its own binary and is its only test.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use tasty_net_identity::{ConnectionRecord, ConnectionToken, DEVICE_KEY_FILE, DeviceKey};

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
fn new_secret_directories_are_0700_under_a_umask_that_removes_owner_bits() {
    let root = tempfile::tempdir().unwrap();
    let key_dir = root.path().join("keys");
    let record_dir = root.path().join("home");
    {
        // 0700 & !0277 = 0500: without the explicit chmod the owner could not write.
        let _mask = Umask::set(0o277);
        DeviceKey::load_or_create(&key_dir).unwrap();
        let record = ConnectionRecord {
            port: 1,
            token: ConnectionToken::issue().unwrap(),
            instance_id: "i".to_owned(),
        };
        // Only the publication is checked here: under this mask the lock file is created 0400,
        // which retire cannot reopen for writing (reported, outside this fix).
        let _publication = record.publish(&record_dir.join("record")).unwrap();
        assert_eq!(mode(&record_dir), 0o700);
        assert_eq!(mode(&record_dir.join("record")), 0o600);
    }
    assert_eq!(mode(&key_dir), 0o700);
    assert_eq!(mode(&key_dir.join(DEVICE_KEY_FILE)), 0o600);
}
