//! Checks that the mesh assembler compares the received length with `total_len` before it
//! allocates the frame buffer.
//!
//! The global allocator below records the largest single allocation while a test window
//! is open. It is shared by everything in this test binary, so this file holds exactly one
//! test; a second test running in parallel would show up in the record.
//!
//! The `unsafe` here is only what implementing `GlobalAlloc` requires. This file is its own
//! crate, so the `#![forbid(unsafe_code)]` of the library does not apply to it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tasty_net_frame::{
    MESH_CHUNK_MAX_PAYLOAD, MeshAssembleError, MeshChunkMeta, MeshFrameAssembler, encode_mesh_chunk,
};

/// Delegates to [`System`] and records the largest requested size while `RECORDING` is set.
struct Recording;

static RECORDING: AtomicBool = AtomicBool::new(false);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn record(size: usize) {
    if RECORDING.load(Ordering::SeqCst) {
        LARGEST.fetch_max(size, Ordering::SeqCst);
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so the
// `GlobalAlloc` contract that `System` upholds holds here too. Recording touches only
// atomics and never allocates.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract for `layout`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` (through this allocator) with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: `ptr` was allocated by `System` with `layout`, and the caller upholds
        // `GlobalAlloc::realloc`'s contract for `new_size`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

/// Two one-byte chunks that announce the largest `total_len` two chunks can carry: the
/// frame completes, fails the length check, and never allocates `total_len` bytes.
#[test]
fn a_short_frame_with_a_large_total_len_is_rejected_before_allocating_it() {
    let total_len = 2 * MESH_CHUNK_MAX_PAYLOAD;
    let meta = |chunk_index| MeshChunkMeta {
        surface_id: 1,
        frame_id: 1,
        chunk_index,
        chunk_count: 2,
        total_len: total_len as u32,
        generation: 0,
        frame_seq: 0,
        full_textures: false,
    };
    let first = encode_mesh_chunk(&meta(0), b"a");
    let second = encode_mesh_chunk(&meta(1), b"b");
    let mut assembler = MeshFrameAssembler::new();

    LARGEST.store(0, Ordering::SeqCst);
    RECORDING.store(true, Ordering::SeqCst);
    let first_result = assembler.push_chunk(&first);
    let second_result = assembler.push_chunk(&second);
    RECORDING.store(false, Ordering::SeqCst);
    let largest = LARGEST.load(Ordering::SeqCst);

    assert_eq!(first_result, Ok(None));
    assert_eq!(second_result, Err(MeshAssembleError::LengthMismatch));
    assert!(largest > 0, "the window recorded no allocation at all");
    assert!(
        largest < total_len,
        "largest single allocation {largest} reached total_len {total_len}"
    );
}
