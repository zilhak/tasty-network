//! The crate root denies `unsafe_code`, and only an Android module may allow it
//! (docs/design/crates.md "unsafe 규칙", director 2937). With the root `deny`, an `unsafe`
//! block elsewhere does not compile, so no allowance outside that module means no `unsafe`
//! outside it.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `path` (under `src`) is the Android module: `src/android.rs` or `src/android/…`.
fn is_android_module(src: &Path, path: &Path) -> bool {
    path.strip_prefix(src)
        .ok()
        .and_then(|p| p.components().next())
        .is_some_and(|c| {
            let c = c.as_os_str().to_string_lossy();
            c == "android" || c == "android.rs"
        })
}

#[test]
fn unsafe_code_is_denied_and_allowed_only_in_the_android_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lib = std::fs::read_to_string(src.join("lib.rs")).expect("lib.rs");
    assert!(
        lib.lines().any(|l| l.trim() == "#![deny(unsafe_code)]"),
        "the crate root denies unsafe_code"
    );
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 1, "{files:?}");
    let mut found = Vec::new();
    for file in files.iter().filter(|f| !is_android_module(&src, f)) {
        let text = std::fs::read_to_string(file).expect("read");
        for (no, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            if [
                "allow(unsafe_code",
                "expect(unsafe_code",
                "warn(unsafe_code",
            ]
            .iter()
            .any(|a| compact.contains(a))
            {
                found.push(format!("{}:{}: {line}", file.display(), no + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "unsafe_code allowed outside the Android module: {found:#?}"
    );
}
