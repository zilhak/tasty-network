//! A test-only cdylib in the shape of an app's library: its own UniFFI namespace and export,
//! and `tasty-net-ffi` linked in as an rlib. `scripts/check-ffi-bindgen.sh` generates the
//! bindings of both from the one library (docs/design/mobile.md "rlib 링크 계약").
#![deny(unsafe_code)]

// An rlib nothing refers to is not linked, and its exports would be missing from the
// library; an app keeps it with the same line.
use tasty_net_ffi as _;

uniffi::setup_scaffolding!("tasty_link_test");

/// The export of this crate, next to those of `tasty-net-ffi`.
#[uniffi::export]
pub fn link_test_device_id_len(device_id: String) -> u64 {
    device_id.len() as u64
}
