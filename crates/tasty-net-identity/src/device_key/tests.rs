use std::fs;

use super::*;
use crate::{
    UnsafeReason,
    secret_file::{self, fault},
};

/// The key file parser with the default limit.
fn parse(bytes: &[u8]) -> Result<[u8; 32], CorruptKind> {
    super::parse(bytes, IdentitySettings::default().max_secret_bytes())
}

fn temps_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect()
}

/// Every temporary file left in `dir` is empty: no secret byte was written before the failure.
fn assert_temps_empty(dir: &Path) {
    for name in temps_in(dir) {
        let len = fs::metadata(dir.join(&name)).unwrap().len();
        assert_eq!(len, 0, "{name} holds {len} bytes");
    }
}

#[test]
fn same_dir_same_id_other_dir_other_id() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = DeviceKey::load_or_create(a.path()).unwrap();
    let again = DeviceKey::load_or_create(a.path()).unwrap();
    let other = DeviceKey::load_or_create(b.path()).unwrap();
    assert_eq!(first.id(), again.id());
    assert_eq!(first.to_seed(), again.to_seed());
    assert_ne!(first.id(), other.id());
    assert!(temps_in(a.path()).is_empty());
}

#[test]
fn creates_missing_nested_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("a").join("b");
    let key = DeviceKey::load_or_create(&dir).unwrap();
    assert_eq!(DeviceKey::load_or_create(&dir).unwrap().id(), key.id());
}

#[test]
fn file_format_is_header_and_seed_hex() {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::load_or_create(dir.path()).unwrap();
    let text = fs::read_to_string(dir.path().join(DEVICE_KEY_FILE)).unwrap();
    assert_eq!(
        text,
        format!("tasty-net-device-key-v1\n{}\n", hex::encode(&key.to_seed()))
    );
}

#[test]
fn final_line_break_is_optional() {
    let seed = [7u8; 32];
    let mut text = b"tasty-net-device-key-v1\n".to_vec();
    text.extend_from_slice(hex::encode(&seed).as_bytes());
    assert_eq!(parse(&text), Ok(seed));
    text.push(b'\n');
    assert_eq!(parse(&text), Ok(seed));
    text.push(b'\n');
    assert_eq!(parse(&text), Err(CorruptKind::TrailingData));
}

#[test]
fn parse_reports_the_kind() {
    let seed_hex = hex::encode(&[0xabu8; 32]);
    assert_eq!(parse(b""), Err(CorruptKind::Header));
    assert_eq!(
        parse(format!("tasty-net-device-key-v2\n{seed_hex}\n").as_bytes()),
        Err(CorruptKind::Header)
    );
    assert_eq!(
        parse(format!("tasty-net-device-key-v1\n{}\n", seed_hex.to_uppercase()).as_bytes()),
        Err(CorruptKind::Seed)
    );
    assert_eq!(
        parse(format!("tasty-net-device-key-v1\r\n{seed_hex}\n").as_bytes()),
        Err(CorruptKind::Header)
    );
    assert_eq!(parse(&vec![b'a'; 5000]), Err(CorruptKind::TooLarge));
}

#[test]
fn truncated_file_is_corrupt_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    DeviceKey::load_or_create(dir.path()).unwrap();
    let path = dir.path().join(DEVICE_KEY_FILE);
    let full = fs::read(&path).unwrap();
    let truncated = &full[..full.len() - 10];
    // Rewrite in place so the file keeps its private permissions.
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| io::Write::write_all(&mut f, truncated))
        .unwrap();
    let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
    assert!(
        matches!(
            error,
            IdentityError::Corrupt {
                kind: CorruptKind::Seed,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        truncated,
        "the file is not replaced"
    );
}

#[test]
fn eight_threads_on_an_empty_dir_get_one_id() {
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(8);
        let ids: Vec<DeviceId> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        DeviceKey::load_or_create(dir.path()).unwrap().id()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        assert!(temps_in(dir.path()).is_empty());
    }
}

#[test]
fn protection_failure_writes_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let error = {
        let _fail = fault::fail_protect();
        DeviceKey::load_or_create(dir.path()).unwrap_err()
    };
    assert!(matches!(error, IdentityError::Protect { .. }), "{error:?}");
    assert!(!dir.path().join(DEVICE_KEY_FILE).exists());
    assert_temps_empty(dir.path());
    assert!(temps_in(dir.path()).is_empty());
    DeviceKey::load_or_create(dir.path()).unwrap();
}

/// A complete key file of the seed `[7; 32]`, as another creator would have published it.
const OTHER_KEY: &[u8] = b"tasty-net-device-key-v1\n\
0707070707070707070707070707070707070707070707070707070707070707\n";

#[test]
fn a_temp_file_that_cannot_be_removed_is_reported() {
    // The temporary name is left only when the publication lost to another creator (or was a
    // hard link), so the injected competitor makes it stay.
    let dir = tempfile::tempdir().unwrap();
    let error = {
        fault::publish_first(OTHER_KEY);
        let _fail = fault::fail_discard();
        DeviceKey::load_or_create(dir.path()).unwrap_err()
    };
    let IdentityError::Leftover {
        leftover, cause, ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(cause.is_none(), "the key itself is in place: {error:?}");
    assert!(leftover.exists());
    assert_eq!(temps_in(dir.path()).len(), 1);
    let key = DeviceKey::load_or_create(dir.path()).unwrap();
    assert_eq!(key.to_seed(), [7u8; 32]);
    let own_seed = parse(&fs::read(leftover).unwrap()).unwrap();
    let seed = hex::encode(&own_seed);
    assert!(!format!("{error} {error:?}").contains(&seed));
}

#[test]
fn a_key_published_meanwhile_is_kept_and_read() {
    let dir = tempfile::tempdir().unwrap();
    fault::publish_first(OTHER_KEY);
    let key = DeviceKey::load_or_create(dir.path()).unwrap();
    assert_eq!(key.to_seed(), [7u8; 32], "the existing key wins");
    assert_eq!(
        fs::read(dir.path().join(DEVICE_KEY_FILE)).unwrap(),
        OTHER_KEY
    );
    assert!(temps_in(dir.path()).is_empty());
}

#[test]
fn publication_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"first").unwrap();
    let temp = dir.path().join("temp");
    fs::write(&temp, b"second").unwrap();
    let error = secret_file::publish_new(&temp, &target).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error:?}");
    assert_eq!(fs::read(&target).unwrap(), b"first");
    assert_eq!(fs::read(&temp).unwrap(), b"second", "the new file stays");
}

#[test]
fn readers_never_see_a_partial_key() {
    let max_len = IdentitySettings::default().max_secret_bytes();
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        let done = std::sync::atomic::AtomicBool::new(false);
        let barrier = std::sync::Barrier::new(5);
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                barrier.wait();
                let mut seen = None;
                loop {
                    let finished = done.load(std::sync::atomic::Ordering::Acquire);
                    // Any file at the name is complete: never Corrupt, never another key.
                    match read(&path, max_len).unwrap() {
                        Some(key) if seen.is_none() => seen = Some(key.id()),
                        Some(key) => assert_eq!(Some(key.id()), seen),
                        None => {}
                    }
                    if finished {
                        return seen;
                    }
                }
            });
            let creators: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        DeviceKey::load_or_create(dir.path()).unwrap().id()
                    })
                })
                .collect();
            // Stop and join the reader before a creator's panic is passed on, so that a failed
            // creation fails the test instead of leaving the reader waiting.
            let joined: Vec<_> = creators.into_iter().map(|h| h.join()).collect();
            done.store(true, std::sync::atomic::Ordering::Release);
            let seen = reader.join();
            let ids: Vec<DeviceId> = joined
                .into_iter()
                .map(|r| r.unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .collect();
            let seen = seen.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
            assert_eq!(seen, Some(ids[0]));
        });
    }
}

/// Linux, Android, macOS and iOS publish with a rename that refuses to replace and need no hard
/// link; the hard link is only the fallback where that rename is not offered.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod no_replace_rename {
    use super::*;

    /// The errors with which the rename says it is not offered here.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const UNSUPPORTED: [i32; 2] = [libc::ENOSYS, libc::EINVAL];
    #[cfg(target_vendor = "apple")]
    const UNSUPPORTED: [i32; 2] = [libc::ENOTSUP, libc::EINVAL];

    fn assert_publish_error(error: &IdentityError) {
        assert!(
            matches!(
                error,
                IdentityError::Io {
                    op: FileOp::Publish,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_key_is_created_where_hard_links_are_denied() {
        // The Android app sandbox answers link() with EACCES (SELinux).
        let dir = tempfile::tempdir().unwrap();
        let key = {
            let _deny = fault::fail_link(libc::EACCES);
            DeviceKey::load_or_create(dir.path()).unwrap()
        };
        let text = fs::read_to_string(dir.path().join(DEVICE_KEY_FILE)).unwrap();
        assert_eq!(
            text,
            format!("tasty-net-device-key-v1\n{}\n", hex::encode(&key.to_seed()))
        );
        assert!(temps_in(dir.path()).is_empty());
        assert_eq!(
            DeviceKey::load_or_create(dir.path()).unwrap().id(),
            key.id()
        );
    }

    #[test]
    fn eight_threads_without_hard_links_get_one_id() {
        for _ in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let barrier = std::sync::Barrier::new(8);
            let ids: Vec<DeviceId> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        s.spawn(|| {
                            let _deny = fault::fail_link(libc::EACCES);
                            barrier.wait();
                            DeviceKey::load_or_create(dir.path()).unwrap().id()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
            assert!(temps_in(dir.path()).is_empty());
        }
    }

    #[test]
    fn without_the_rename_the_key_is_hard_linked() {
        for code in UNSUPPORTED {
            let dir = tempfile::tempdir().unwrap();
            let key = {
                let _unsupported = fault::fail_rename(code);
                DeviceKey::load_or_create(dir.path()).unwrap()
            };
            assert!(temps_in(dir.path()).is_empty(), "{code}");
            assert_eq!(
                DeviceKey::load_or_create(dir.path()).unwrap().id(),
                key.id()
            );
        }
    }

    #[test]
    fn the_hard_link_fallback_never_replaces_either() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"first").unwrap();
        let temp = dir.path().join("temp");
        fs::write(&temp, b"second").unwrap();
        let _unsupported = fault::fail_rename(UNSUPPORTED[0]);
        let error = secret_file::publish_new(&temp, &target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error:?}");
        assert_eq!(fs::read(&target).unwrap(), b"first");
    }

    #[test]
    fn without_the_rename_and_hard_links_publication_fails_with_both_errors() {
        for code in UNSUPPORTED {
            let dir = tempfile::tempdir().unwrap();
            let error = {
                let _unsupported = fault::fail_rename(code);
                let _deny = fault::fail_link(libc::EACCES);
                DeviceKey::load_or_create(dir.path()).unwrap_err()
            };
            assert_publish_error(&error);
            let IdentityError::Io { source, .. } = &error else {
                unreachable!()
            };
            // The kind is the one of the link; the message names both steps and both errors.
            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied, "{source:?}");
            let message = source.to_string();
            for part in [
                "hard link".to_owned(),
                "rename".to_owned(),
                format!("os error {}", libc::EACCES),
                format!("os error {code}"),
            ] {
                assert!(message.contains(&part), "{part:?} missing in {message:?}");
            }
            let link = std::error::Error::source(source).unwrap();
            assert!(
                link.to_string()
                    .contains(&format!("os error {}", libc::EACCES)),
                "{link:?}"
            );
            assert!(!dir.path().join(DEVICE_KEY_FILE).exists());
            assert!(temps_in(dir.path()).is_empty());
        }
    }

    #[test]
    fn another_rename_error_is_returned_without_a_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let error = {
            let _denied = fault::fail_rename(libc::EACCES);
            DeviceKey::load_or_create(dir.path()).unwrap_err()
        };
        assert_publish_error(&error);
        assert!(!dir.path().join(DEVICE_KEY_FILE).exists(), "no fallback");
        assert!(temps_in(dir.path()).is_empty());
    }
}

#[test]
fn a_failure_whose_cleanup_fails_keeps_the_first_error() {
    let dir = tempfile::tempdir().unwrap();
    let error = {
        let _protect = fault::fail_protect();
        let _discard = fault::fail_discard();
        DeviceKey::load_or_create(dir.path()).unwrap_err()
    };
    assert!(
        matches!(
            &error,
            IdentityError::Leftover { cause: Some(cause), .. }
                if matches!(**cause, IdentityError::Protect { .. })
        ),
        "{error:?}"
    );
    assert!(std::error::Error::source(&error).is_some());
    // The kept temporary file shows that the protection failed before any byte was written.
    let IdentityError::Leftover { leftover, .. } = &error else {
        unreachable!()
    };
    assert_eq!(fs::metadata(leftover).unwrap().len(), 0);
    assert_eq!(temps_in(dir.path()).len(), 1);
    assert_temps_empty(dir.path());
}

#[test]
fn a_random_source_failure_is_an_error_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _fail = super::random_fault::fail_random();
    let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(error, IdentityError::Random(_)), "{error:?}");
    assert!(std::error::Error::source(&error).is_some());
    assert!(!dir.path().join(DEVICE_KEY_FILE).exists());
    assert!(temps_in(dir.path()).is_empty());
    assert!(DeviceKey::generate().is_err());
    assert!(crate::ConnectionToken::issue().is_err());
}

/// Check that `seed` gives the device ID `id` and the signature `signature` of `message`, in
/// both directions through the text forms.
fn assert_vector(seed: &str, id: &str, message: &[u8], signature: &str) {
    let key = DeviceKey::from_seed(hex::decode::<32>(seed.as_bytes()).unwrap());
    assert_eq!(key.id().to_string(), id);
    assert_eq!(key.id(), id.parse::<DeviceId>().unwrap());
    assert_eq!(key.sign(message).to_string(), signature);
    assert!(
        id.parse::<DeviceId>()
            .unwrap()
            .verify(message, &signature.parse().unwrap())
    );
}

/// The device ID and signature of a fixed seed. The expected values were taken from the
/// implementation of develop 132a3d6, so a change of the ed25519 implementation
/// that alters key files, device IDs or signatures fails here.
#[test]
fn a_fixed_seed_keeps_its_id_and_signature() {
    assert_vector(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        FIXED_ID,
        b"tasty-net fixed vector",
        "4780ca113a06f7f67815d44feda1eac6a036f4fce368744ec9e57d2b6d61f150\
         1c9e883874f22c6a99aa53e9aa5db6bc9dd08e1f754cc0d4ca1b2de2f0c72601",
    );
}

/// RFC 8032 section 7.1, TEST 1 (empty message).
#[test]
fn rfc8032_test_1() {
    assert_vector(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        b"",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    );
}

#[test]
fn sign_and_verify() {
    let key = DeviceKey::generate().unwrap();
    let other = DeviceKey::generate().unwrap();
    let payload = b"tasty-net handshake v2 payload".to_vec();
    let signature = key.sign(&payload);
    assert!(key.id().verify(&payload, &signature));

    let mut changed = payload.clone();
    changed[0] ^= 1;
    assert!(!key.id().verify(&changed, &signature));
    assert!(!other.id().verify(&payload, &signature));

    let mut bad = signature.to_bytes();
    bad[10] ^= 1;
    assert!(!key.id().verify(&payload, &Signature::from_bytes(bad)));
}

#[test]
fn seed_round_trip_keeps_the_id() {
    let key = DeviceKey::generate().unwrap();
    let again = DeviceKey::from_seed(key.to_seed());
    assert_eq!(key.id(), again.id());
    assert_eq!(again.to_seed(), key.to_seed());
}

/// The ID of the fixed seed in [`a_fixed_seed_keeps_its_id_and_signature`].
const FIXED_ID: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";

#[test]
fn device_id_text_is_strict_lowercase_hex() {
    let id = FIXED_ID.parse::<DeviceId>().unwrap();
    assert_eq!(id.to_string(), FIXED_ID);
    assert_eq!(DeviceId::from_bytes(id.as_bytes()).unwrap(), id);
    let refused = [
        // Uppercase hex of a valid key.
        FIXED_ID.to_ascii_uppercase(),
        // The z-base-32 text of the same key.
        "yqooxx9u3aemh8mo5wcqq16yufu6jitouq1o4za751dger1igghy".to_owned(),
        // 63 and 65 hex digits.
        FIXED_ID[..63].to_owned(),
        format!("{FIXED_ID}0"),
        String::new(),
    ];
    for text in refused {
        assert_eq!(
            text.parse::<DeviceId>(),
            Err(DeviceIdParseError::Format),
            "{text:?}"
        );
    }
    // y = 2 is not on the curve.
    let mut off_curve = [0u8; 32];
    off_curve[0] = 2;
    assert_eq!(
        DeviceId::from_bytes(&off_curve),
        Err(DeviceIdParseError::InvalidKey)
    );
    assert_eq!(
        hex::encode(&off_curve).parse::<DeviceId>(),
        Err(DeviceIdParseError::InvalidKey)
    );
}

#[test]
fn device_ids_are_ordered_by_their_bytes() {
    let mut ids: Vec<DeviceId> = (0..8)
        .map(|_| DeviceKey::generate().unwrap().id())
        .collect();
    ids.sort();
    for pair in ids.windows(2) {
        assert!(pair[0].as_bytes() < pair[1].as_bytes());
        assert!(pair[0].to_string() < pair[1].to_string());
    }
}

#[test]
fn a_device_id_is_its_32_bytes() {
    assert_eq!(std::mem::size_of::<DeviceId>(), 32);
}

#[test]
fn signature_text_round_trip() {
    let signature = DeviceKey::generate().unwrap().sign(b"x");
    let text = signature.to_string();
    assert_eq!(text.len(), 128);
    assert_eq!(text.parse::<Signature>().unwrap(), signature);
    assert!(text[..127].parse::<Signature>().is_err());
}

#[test]
fn debug_hides_the_seed() {
    let key = DeviceKey::generate().unwrap();
    let debug = format!("{key:?}");
    assert!(!debug.contains(&hex::encode(&key.to_seed())));
    assert!(debug.contains(&key.id().to_string()));
}

#[test]
fn a_directory_at_the_key_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(DEVICE_KEY_FILE)).unwrap();
    let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
    assert!(
        matches!(
            error,
            IdentityError::UnsafeFile {
                reason: UnsafeReason::NotRegular,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_file_in_place_of_the_directory_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("keys");
    fs::write(&dir, "not a directory").unwrap();
    let error = DeviceKey::load_or_create(&dir).unwrap_err();
    assert!(
        matches!(
            error,
            IdentityError::UnsafeFile {
                reason: UnsafeReason::NotRegular,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(fs::read_to_string(&dir).unwrap(), "not a directory");
}

#[test]
fn a_trailing_separator_names_the_same_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("keys");
    let key = DeviceKey::load_or_create(&dir).unwrap();
    let mut with_separator = dir.into_os_string();
    with_separator.push(std::path::MAIN_SEPARATOR_STR);
    let again = DeviceKey::load_or_create(Path::new(&with_separator)).unwrap();
    assert_eq!(again.id(), key.id());
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn modes_are_0600_and_0700() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("keys");
        DeviceKey::load_or_create(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(DEVICE_KEY_FILE)), 0o600);
    }

    #[test]
    fn a_wider_existing_directory_is_narrowed() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        DeviceKey::load_or_create(dir.path()).unwrap();
        assert_eq!(mode(dir.path()), 0o700);
    }

    #[test]
    fn a_wide_key_file_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let key = DeviceKey::load_or_create(dir.path()).unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::WideMode { mode: 0o644 },
                    ..
                }
            ),
            "{error:?}"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(
            DeviceKey::load_or_create(dir.path()).unwrap().id(),
            key.id()
        );
    }

    #[test]
    fn a_symlinked_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        DeviceKey::load_or_create(elsewhere.path()).unwrap();
        symlink(
            elsewhere.path().join(DEVICE_KEY_FILE),
            dir.path().join(DEVICE_KEY_FILE),
        )
        .unwrap();
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
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
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        let error = DeviceKey::load_or_create(&link).unwrap_err();
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
    }

    fn assert_symlink_refused(dir: &Path) {
        let error = DeviceKey::load_or_create(dir).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::Symlink,
                    ..
                }
            ),
            "{}: {error:?}",
            dir.display()
        );
    }

    #[test]
    fn an_unreadable_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        DeviceKey::load_or_create(dir.path()).unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&path).is_ok() {
            // Running with privileges that bypass the mode; the case cannot be made here.
            return;
        }
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::CheckFailed,
                    source: Some(_),
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_dangling_directory_link_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        symlink(root.path().join("missing"), &link).unwrap();
        assert_symlink_refused(&link);
        assert!(
            !root.path().join("missing").exists(),
            "nothing was created through the link"
        );
    }

    #[test]
    fn a_directory_link_with_a_trailing_slash_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        let mut with_slash = link.into_os_string();
        with_slash.push("/");
        assert_symlink_refused(Path::new(&with_slash));
        assert!(!real.join(DEVICE_KEY_FILE).exists());
    }

    #[test]
    fn a_fifo_at_the_key_path_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::NotRegular,
                    ..
                }
            ),
            "{error:?}"
        );
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::secret_file::windows::testing::{Entry, read_acl, set_null_dacl};
    use windows_sys::Win32::{
        Storage::FileSystem::FILE_ALL_ACCESS, System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
    };

    fn assert_private(path: &Path) {
        let acl = read_acl(path);
        assert!(acl.owner_is_current_user, "{acl:?}");
        assert!(acl.dacl_present && !acl.dacl_null, "{acl:?}");
        assert!(acl.protected, "inheritance is blocked: {acl:?}");
        assert_eq!(
            acl.entries,
            vec![Entry {
                current_user: true,
                ace_type: ACCESS_ALLOWED_ACE_TYPE as u8,
                flags: 0,
                mask: FILE_ALL_ACCESS,
            }]
        );
    }

    #[test]
    fn key_file_acl_is_the_current_sid_only_and_protected() {
        let dir = tempfile::tempdir().unwrap();
        DeviceKey::load_or_create(dir.path()).unwrap();
        assert_private(&dir.path().join(DEVICE_KEY_FILE));
    }

    #[test]
    fn a_key_directory_deeper_than_max_path_works() {
        let root = tempfile::tempdir().unwrap();
        let mut dir = root.path().to_path_buf();
        while dir.as_os_str().len() <= 300 {
            dir.push("d".repeat(40));
        }
        let key = DeviceKey::load_or_create(&dir).unwrap();
        assert!(dir.join(DEVICE_KEY_FILE).as_os_str().len() > 260);
        assert_private(&dir.join(DEVICE_KEY_FILE));
        assert_eq!(DeviceKey::load_or_create(&dir).unwrap().id(), key.id());
    }

    #[test]
    fn a_security_descriptor_failure_is_a_protect_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = {
            let _fail = crate::secret_file::windows::fault::fail_descriptor();
            DeviceKey::load_or_create(dir.path()).unwrap_err()
        };
        assert!(matches!(error, IdentityError::Protect { .. }), "{error:?}");
        assert!(!dir.path().join(DEVICE_KEY_FILE).exists());
        assert!(temps_in(dir.path()).is_empty());
    }

    #[test]
    fn a_widened_acl_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        DeviceKey::load_or_create(dir.path()).unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        let output = std::process::Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(R)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::Dacl,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_null_dacl_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        DeviceKey::load_or_create(dir.path()).unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        set_null_dacl(&path);
        // The prepared file differs from a private one only by its NULL DACL: present,
        // protected, owned by the current user.
        let acl = read_acl(&path);
        assert!(acl.dacl_present && acl.dacl_null, "{acl:?}");
        assert!(acl.protected, "{acl:?}");
        assert!(acl.owner_is_current_user, "{acl:?}");
        assert!(acl.entries.is_empty(), "{acl:?}");
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(
                error,
                IdentityError::UnsafeFile {
                    reason: UnsafeReason::Dacl,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_junction_at_the_key_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_KEY_FILE);
        let output = std::process::Command::new("cmd.exe")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&path)
            .arg(target.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
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
    }

    #[test]
    fn a_plain_file_with_inherited_acl_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let seed_hex = hex::encode(&[3u8; 32]);
        fs::write(
            dir.path().join(DEVICE_KEY_FILE),
            format!("tasty-net-device-key-v1\n{seed_hex}\n"),
        )
        .unwrap();
        let error = DeviceKey::load_or_create(dir.path()).unwrap_err();
        assert!(
            matches!(error, IdentityError::UnsafeFile { .. }),
            "{error:?}"
        );
    }
}
