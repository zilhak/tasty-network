//! Splits mesh bytes into [`StreamTag::MeshData`](crate::StreamTag::MeshData) chunks and
//! reassembles them. The mesh format itself is opaque here.
//!
//! The chunk header is big-endian `surface_id: u32, frame_id: u64, chunk_index: u32,
//! chunk_count: u32, total_len: u32, generation: u64, frame_seq: u64, full_textures: u8`.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// Fixed wire size of the chunk header in bytes.
pub const MESH_CHUNK_HEADER_LEN: usize = 4 + 8 + 4 + 4 + 4 + 8 + 8 + 1;

/// Data bytes per chunk, chosen so that header plus data fit in
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN).
pub const MESH_CHUNK_MAX_PAYLOAD: usize = (crate::MAX_FRAME_LEN as usize) - MESH_CHUNK_HEADER_LEN;

/// Header of one mesh chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshChunkMeta {
    /// Surface the mesh frame belongs to.
    pub surface_id: u32,
    /// Monotonic ID of the mesh frame the chunk belongs to. The sender increments it per
    /// frame. It is only the reassembly key, separate from `frame_seq` and `generation`.
    pub frame_id: u64,
    /// Position of this chunk, from 0 to `chunk_count - 1`.
    pub chunk_index: u32,
    /// Number of chunks of this frame, at least 1.
    pub chunk_count: u32,
    /// Length of the whole frame, the sum of all chunk data.
    pub total_len: u32,
    /// Mesh generation, passed through unchanged.
    pub generation: u64,
    /// Mesh frame sequence, passed through unchanged.
    pub frame_seq: u64,
    /// Whether the frame carries full textures, passed through unchanged.
    pub full_textures: bool,
}

/// Encodes one chunk as a [`StreamTag::MeshData`](crate::StreamTag::MeshData) payload.
pub fn encode_mesh_chunk(meta: &MeshChunkMeta, chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MESH_CHUNK_HEADER_LEN + chunk.len());
    out.extend_from_slice(&meta.surface_id.to_be_bytes());
    out.extend_from_slice(&meta.frame_id.to_be_bytes());
    out.extend_from_slice(&meta.chunk_index.to_be_bytes());
    out.extend_from_slice(&meta.chunk_count.to_be_bytes());
    out.extend_from_slice(&meta.total_len.to_be_bytes());
    out.extend_from_slice(&meta.generation.to_be_bytes());
    out.extend_from_slice(&meta.frame_seq.to_be_bytes());
    out.push(meta.full_textures as u8);
    out.extend_from_slice(chunk);
    out
}

/// Inverse of [`encode_mesh_chunk`]. Returns `None` when `buf` is shorter than
/// [`MESH_CHUNK_HEADER_LEN`].
pub fn decode_mesh_chunk(buf: &[u8]) -> Option<(MeshChunkMeta, &[u8])> {
    if buf.len() < MESH_CHUNK_HEADER_LEN {
        return None;
    }
    let surface_id = u32::from_be_bytes(buf[0..4].try_into().ok()?);
    let frame_id = u64::from_be_bytes(buf[4..12].try_into().ok()?);
    let chunk_index = u32::from_be_bytes(buf[12..16].try_into().ok()?);
    let chunk_count = u32::from_be_bytes(buf[16..20].try_into().ok()?);
    let total_len = u32::from_be_bytes(buf[20..24].try_into().ok()?);
    let generation = u64::from_be_bytes(buf[24..32].try_into().ok()?);
    let frame_seq = u64::from_be_bytes(buf[32..40].try_into().ok()?);
    let full_textures = buf[40] != 0;
    let meta = MeshChunkMeta {
        surface_id,
        frame_id,
        chunk_index,
        chunk_count,
        total_len,
        generation,
        frame_seq,
        full_textures,
    };
    Some((meta, &buf[MESH_CHUNK_HEADER_LEN..]))
}

/// Splits mesh bytes into encoded chunk payloads of at most [`MESH_CHUNK_MAX_PAYLOAD`]
/// data bytes each.
///
/// Empty input still yields one empty chunk. The caller sends the chunks in order, one
/// [`StreamTag::MeshData`](crate::StreamTag::MeshData) frame each. `bytes` must be shorter
/// than 4 GiB, since `total_len` is a `u32`.
pub fn split_mesh_frame(
    surface_id: u32,
    frame_id: u64,
    generation: u64,
    frame_seq: u64,
    full_textures: bool,
    bytes: &[u8],
) -> Vec<Vec<u8>> {
    let total_len = bytes.len() as u32;
    let chunk_count = bytes.len().div_ceil(MESH_CHUNK_MAX_PAYLOAD).max(1) as u32;
    (0..chunk_count)
        .map(|i| {
            let start = i as usize * MESH_CHUNK_MAX_PAYLOAD;
            let end = (start + MESH_CHUNK_MAX_PAYLOAD).min(bytes.len());
            let meta = MeshChunkMeta {
                surface_id,
                frame_id,
                chunk_index: i,
                chunk_count,
                total_len,
                generation,
                frame_seq,
                full_textures,
            };
            encode_mesh_chunk(&meta, &bytes[start..end])
        })
        .collect()
}

/// Why [`MeshFrameAssembler::push_chunk`] rejected a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshAssembleError {
    /// The header is truncated or inconsistent: `chunk_count` is 0, `chunk_index` is out
    /// of range, or any header field other than `chunk_index` (`chunk_count`,
    /// `total_len`, `generation`, `frame_seq`, `full_textures`) disagrees with earlier
    /// chunks of the same frame.
    Malformed,
    /// The lengths do not add up: `total_len` exceeds what `chunk_count` chunks can
    /// carry (rejected at once to avoid a large allocation), a chunk carries more than
    /// [`MESH_CHUNK_MAX_PAYLOAD`] bytes, or the received data of a frame is longer than
    /// `total_len` or, once every chunk arrived, differs from it.
    LengthMismatch,
}

impl fmt::Display for MeshAssembleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the mesh chunk header is malformed",
            Self::LengthMismatch => "the mesh chunk lengths do not match the frame length",
        })
    }
}

impl Error for MeshAssembleError {}

/// Reassembly state of a frame whose chunks have not all arrived yet.
struct InFlight {
    chunk_count: u32,
    total_len: u32,
    generation: u64,
    frame_seq: u64,
    full_textures: bool,
    received: HashMap<u32, Vec<u8>>,
    /// Sum of the data lengths in `received`, kept in `u64` so it cannot wrap.
    received_len: u64,
}

/// Reassembles chunks per `(surface_id, frame_id)`, so chunks of different surfaces and
/// frames may interleave. Within a frame the chunks may arrive in any order.
#[derive(Default)]
pub struct MeshFrameAssembler {
    inflight: HashMap<(u32, u64), InFlight>,
}

impl MeshFrameAssembler {
    /// Creates an assembler with no frame in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk payload.
    ///
    /// Returns `Ok(Some((meta, bytes)))` with the whole frame when this chunk completes
    /// it (`meta.chunk_index` is then 0), and `Ok(None)` while chunks are missing.
    ///
    /// # Errors
    ///
    /// A [`MeshAssembleError`] for a corrupt or hostile header. The caller drops the
    /// chunk; when the error comes from a mismatch with earlier chunks, the progress of
    /// that frame is dropped too.
    ///
    /// The received data length of a frame is checked on every chunk. If resending a
    /// chunk index makes the sum exceed `total_len` even once, the frame is dropped with
    /// [`MeshAssembleError::LengthMismatch`]. tasty checks the sum only after every index
    /// has arrived; this crate checks earlier so that the data an incomplete frame that
    /// passed the length checks holds never exceeds `total_len` plus one chunk (when the
    /// frame is assembled, the assembly buffer comes on top of that).
    pub fn push_chunk(
        &mut self,
        buf: &[u8],
    ) -> Result<Option<(MeshChunkMeta, Vec<u8>)>, MeshAssembleError> {
        let (meta, payload) = decode_mesh_chunk(buf).ok_or(MeshAssembleError::Malformed)?;
        let index_out_of_range = meta.chunk_count == 0 || meta.chunk_index >= meta.chunk_count;
        let key = (meta.surface_id, meta.frame_id);
        if let Some(slot) = self.inflight.get(&key) {
            // Every chunk of a frame carries the same header apart from chunk_index. A
            // chunk whose header disagrees (or a reused frame_id) is treated as
            // corruption, so the result never depends on which chunk arrived first. This
            // comes before the index range and length bound checks, so such a chunk
            // always drops the frame. A chunk that agrees but whose index is out of
            // range is corrupt as well and drops the frame too.
            if slot.chunk_count != meta.chunk_count
                || slot.total_len != meta.total_len
                || slot.generation != meta.generation
                || slot.frame_seq != meta.frame_seq
                || slot.full_textures != meta.full_textures
                || index_out_of_range
            {
                self.inflight.remove(&key);
                return Err(MeshAssembleError::Malformed);
            }
        } else {
            // A chunk that would start a new frame is checked before any state exists
            // for it: its index must be in range, and total_len must be reachable by
            // chunk_count full chunks. A frame in progress already passed these checks
            // with the same header.
            if index_out_of_range {
                return Err(MeshAssembleError::Malformed);
            }
            let max_possible = u64::from(meta.chunk_count) * (MESH_CHUNK_MAX_PAYLOAD as u64);
            if u64::from(meta.total_len) > max_possible {
                return Err(MeshAssembleError::LengthMismatch);
            }
        }

        let slot = self.inflight.entry(key).or_insert_with(|| InFlight {
            chunk_count: meta.chunk_count,
            total_len: meta.total_len,
            generation: meta.generation,
            frame_seq: meta.frame_seq,
            full_textures: meta.full_textures,
            received: HashMap::new(),
            received_len: 0,
        });
        if payload.len() > MESH_CHUNK_MAX_PAYLOAD {
            self.inflight.remove(&key);
            return Err(MeshAssembleError::LengthMismatch);
        }
        if let Some(old) = slot.received.insert(meta.chunk_index, payload.to_vec()) {
            slot.received_len -= old.len() as u64;
        }
        slot.received_len += payload.len() as u64;
        // More data than the header announced can never complete correctly.
        if slot.received_len > u64::from(slot.total_len) {
            self.inflight.remove(&key);
            return Err(MeshAssembleError::LengthMismatch);
        }

        if slot.received.len() as u64 == u64::from(slot.chunk_count) {
            let slot = self.inflight.remove(&key).expect("just inserted above");
            // Check the received length before allocating, so a header with a large
            // total_len and small chunks cannot cause an allocation of that size.
            if slot.received_len != u64::from(slot.total_len) {
                return Err(MeshAssembleError::LengthMismatch);
            }
            let mut assembled = Vec::with_capacity(slot.total_len as usize);
            for i in 0..slot.chunk_count {
                let Some(part) = slot.received.get(&i) else {
                    // A hole in the indices (a duplicate overwrote a slot): corruption.
                    return Err(MeshAssembleError::Malformed);
                };
                assembled.extend_from_slice(part);
            }
            let out_meta = MeshChunkMeta {
                surface_id: meta.surface_id,
                frame_id: meta.frame_id,
                chunk_index: 0,
                chunk_count: slot.chunk_count,
                total_len: slot.total_len,
                generation: slot.generation,
                frame_seq: slot.frame_seq,
                full_textures: slot.full_textures,
            };
            return Ok(Some((out_meta, assembled)));
        }
        Ok(None)
    }

    /// Drops every frame in progress for `surface_id`, so that on detach or reconnect a
    /// stale partial frame does not mix into the next session.
    pub fn forget_surface(&mut self, surface_id: u32) {
        self.inflight.retain(|(sid, _), _| *sid != surface_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_FRAME_LEN, StreamTag, read_frame, write_frame};
    use std::io::Cursor;

    #[test]
    fn chunk_header_roundtrip() {
        let meta = MeshChunkMeta {
            surface_id: 7,
            frame_id: 99,
            chunk_index: 1,
            chunk_count: 3,
            total_len: 12345,
            generation: 42,
            frame_seq: 8,
            full_textures: true,
        };
        let encoded = encode_mesh_chunk(&meta, b"hello");
        let (decoded, payload) = decode_mesh_chunk(&encoded).unwrap();
        assert_eq!(decoded, meta);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn decode_rejects_truncated_header() {
        assert!(decode_mesh_chunk(&[0u8; MESH_CHUNK_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn split_and_reassemble_large_frame() {
        let bytes: Vec<u8> = (0..(MAX_FRAME_LEN as usize * 2 + 777))
            .map(|i| (i % 256) as u8)
            .collect();
        let chunks = split_mesh_frame(3, 1, 10, 20, false, &bytes);
        assert!(
            chunks.len() > 1,
            "must actually split across multiple chunks"
        );

        // Round-trip through real frames to check the payload limit as well.
        let mut assembler = MeshFrameAssembler::new();
        let mut result = None;
        for chunk in &chunks {
            let mut buf = Vec::new();
            write_frame(&mut buf, StreamTag::MeshData, chunk).unwrap();
            let mut cur = Cursor::new(buf);
            let frame = read_frame(&mut cur).unwrap();
            assert_eq!(frame.tag, StreamTag::MeshData);
            if let Some(r) = assembler.push_chunk(&frame.payload).unwrap() {
                result = Some(r);
            }
        }
        let (meta, assembled) = result.expect("all chunks delivered — must assemble");
        assert_eq!(assembled, bytes);
        assert_eq!(meta.surface_id, 3);
        assert_eq!(meta.frame_id, 1);
        assert_eq!(meta.generation, 10);
        assert_eq!(meta.frame_seq, 20);
        assert!(!meta.full_textures);
    }

    #[test]
    fn split_empty_frame_yields_one_chunk() {
        let chunks = split_mesh_frame(1, 1, 0, 0, true, &[]);
        assert_eq!(chunks.len(), 1);
        let mut assembler = MeshFrameAssembler::new();
        let (meta, assembled) = assembler
            .push_chunk(&chunks[0])
            .unwrap()
            .expect("single chunk completes immediately");
        assert!(assembled.is_empty());
        assert!(meta.full_textures);
    }

    #[test]
    fn interleaved_surfaces_do_not_cross_contaminate() {
        let bytes_a = vec![0xAAu8; MESH_CHUNK_MAX_PAYLOAD + 10];
        let bytes_b = vec![0xBBu8; MESH_CHUNK_MAX_PAYLOAD + 10];
        let chunks_a = split_mesh_frame(1, 1, 0, 0, false, &bytes_a);
        let chunks_b = split_mesh_frame(2, 1, 0, 0, false, &bytes_b);
        assert_eq!(chunks_a.len(), 2);
        assert_eq!(chunks_b.len(), 2);

        let mut assembler = MeshFrameAssembler::new();
        assert!(assembler.push_chunk(&chunks_a[0]).unwrap().is_none());
        assert!(assembler.push_chunk(&chunks_b[0]).unwrap().is_none());
        let (meta_a, assembled_a) = assembler.push_chunk(&chunks_a[1]).unwrap().unwrap();
        let (meta_b, assembled_b) = assembler.push_chunk(&chunks_b[1]).unwrap().unwrap();
        assert_eq!(meta_a.surface_id, 1);
        assert_eq!(assembled_a, bytes_a);
        assert_eq!(meta_b.surface_id, 2);
        assert_eq!(assembled_b, bytes_b);
    }

    #[test]
    fn malicious_total_len_is_rejected() {
        let meta = MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index: 0,
            chunk_count: 1,
            total_len: u32::MAX,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let encoded = encode_mesh_chunk(&meta, b"short");
        let mut assembler = MeshFrameAssembler::new();
        let err = assembler.push_chunk(&encoded).unwrap_err();
        assert_eq!(err, MeshAssembleError::LengthMismatch);
    }

    #[test]
    fn total_len_not_matching_actual_assembled_is_rejected() {
        let meta = MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index: 0,
            chunk_count: 1,
            total_len: 999, // the data is 5 bytes
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let encoded = encode_mesh_chunk(&meta, b"short");
        let mut assembler = MeshFrameAssembler::new();
        let err = assembler.push_chunk(&encoded).unwrap_err();
        assert_eq!(err, MeshAssembleError::LengthMismatch);
    }

    #[test]
    fn forget_surface_drops_inflight_state() {
        let bytes = vec![0u8; MESH_CHUNK_MAX_PAYLOAD + 1];
        let chunks = split_mesh_frame(5, 1, 0, 0, false, &bytes);
        assert_eq!(chunks.len(), 2);
        let mut assembler = MeshFrameAssembler::new();
        assert!(assembler.push_chunk(&chunks[0]).unwrap().is_none());
        assembler.forget_surface(5);
        assert!(assembler.push_chunk(&chunks[1]).unwrap().is_none());
    }

    /// The assembler keys chunks by index, so the arrival order inside a frame does not
    /// matter.
    #[test]
    fn chunks_in_any_order_reassemble_to_the_original_bytes() {
        let bytes: Vec<u8> = (0..(MESH_CHUNK_MAX_PAYLOAD * 3 + 5))
            .map(|i| (i % 253) as u8)
            .collect();
        let chunks = split_mesh_frame(4, 9, 1, 2, true, &bytes);
        assert_eq!(chunks.len(), 4);
        for order in [[3usize, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            let mut assembler = MeshFrameAssembler::new();
            let mut result = None;
            for (n, &i) in order.iter().enumerate() {
                let out = assembler.push_chunk(&chunks[i]).unwrap();
                if n + 1 < order.len() {
                    assert!(out.is_none(), "{order:?}: complete before the last chunk");
                } else {
                    result = out;
                }
            }
            let (meta, assembled) = result.expect("the last chunk completes the frame");
            assert_eq!(assembled, bytes, "{order:?}");
            assert_eq!(meta.chunk_index, 0);
            assert_eq!(meta.chunk_count, 4);
            assert_eq!(meta.total_len as usize, bytes.len());
        }
    }

    #[test]
    fn a_full_chunk_fills_exactly_one_max_frame() {
        let bytes = vec![1u8; MESH_CHUNK_MAX_PAYLOAD];
        let chunks = split_mesh_frame(1, 1, 0, 0, false, &bytes);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), MAX_FRAME_LEN as usize);
        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::MeshData, &chunks[0]).unwrap();

        let chunks = split_mesh_frame(1, 2, 0, 0, false, &[1u8; MESH_CHUNK_MAX_PAYLOAD + 1]);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].len(), MESH_CHUNK_HEADER_LEN + 1);
    }

    /// Fixed bytes of the tasty 698aa49e03 chunk header.
    #[test]
    fn chunk_header_bytes_match_tasty_wire() {
        let meta = MeshChunkMeta {
            surface_id: 0x0102_0304,
            frame_id: 0x0506_0708_090A_0B0C,
            chunk_index: 1,
            chunk_count: 2,
            total_len: 0x0000_0100,
            generation: 3,
            frame_seq: 4,
            full_textures: true,
        };
        let expected: [u8; MESH_CHUNK_HEADER_LEN + 1] = [
            1, 2, 3, 4, // surface_id
            5, 6, 7, 8, 9, 10, 11, 12, // frame_id
            0, 0, 0, 1, // chunk_index
            0, 0, 0, 2, // chunk_count
            0, 0, 1, 0, // total_len
            0, 0, 0, 0, 0, 0, 0, 3, // generation
            0, 0, 0, 0, 0, 0, 0, 4,    // frame_seq
            1,    // full_textures
            0xEE, // data
        ];
        assert_eq!(MESH_CHUNK_HEADER_LEN, 41);
        assert_eq!(encode_mesh_chunk(&meta, &[0xEE]), expected);
    }

    #[test]
    fn zero_chunk_count_and_out_of_range_index_are_malformed() {
        let mut meta = MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index: 0,
            chunk_count: 0,
            total_len: 0,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let mut assembler = MeshFrameAssembler::new();
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta, &[])),
            Err(MeshAssembleError::Malformed)
        );
        meta.chunk_count = 2;
        meta.chunk_index = 2;
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta, &[])),
            Err(MeshAssembleError::Malformed)
        );
        assert_eq!(
            assembler.push_chunk(&[0u8; MESH_CHUNK_HEADER_LEN - 1]),
            Err(MeshAssembleError::Malformed)
        );
    }

    #[test]
    fn a_header_that_disagrees_with_earlier_chunks_drops_the_frame() {
        let bytes = vec![7u8; MESH_CHUNK_MAX_PAYLOAD + 1];
        let chunks = split_mesh_frame(1, 1, 0, 0, false, &bytes);
        let mut assembler = MeshFrameAssembler::new();
        assert!(assembler.push_chunk(&chunks[0]).unwrap().is_none());
        let (mut meta, data) = decode_mesh_chunk(&chunks[1]).unwrap();
        meta.chunk_count = 3;
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta, data)),
            Err(MeshAssembleError::Malformed)
        );
        // The frame was dropped: the real second chunk now starts a new frame.
        assert!(assembler.push_chunk(&chunks[1]).unwrap().is_none());
    }

    /// A chunk whose generation, frame_seq or full_textures differs from the earlier
    /// chunks of its frame is rejected, so the result cannot depend on arrival order.
    #[test]
    fn a_chunk_with_different_pass_through_fields_is_malformed() {
        type Change = fn(&mut MeshChunkMeta);
        let changes: [(&str, Change); 3] = [
            ("generation", |m| m.generation += 1),
            ("frame_seq", |m| m.frame_seq += 1),
            ("full_textures", |m| m.full_textures = !m.full_textures),
        ];
        let bytes = vec![3u8; MESH_CHUNK_MAX_PAYLOAD + 1];
        let chunks = split_mesh_frame(1, 1, 10, 20, false, &bytes);
        for (field, change) in changes {
            let mut assembler = MeshFrameAssembler::new();
            assert!(assembler.push_chunk(&chunks[0]).unwrap().is_none());
            let (mut meta, data) = decode_mesh_chunk(&chunks[1]).unwrap();
            change(&mut meta);
            assert_eq!(
                assembler.push_chunk(&encode_mesh_chunk(&meta, data)),
                Err(MeshAssembleError::Malformed),
                "{field}"
            );
            // The frame was dropped: the real second chunk now starts a new frame.
            assert!(
                assembler.push_chunk(&chunks[1]).unwrap().is_none(),
                "{field}"
            );
        }
    }

    /// Every chunk arrived but the data is shorter than total_len: the frame is rejected
    /// without building a total_len-sized buffer.
    #[test]
    fn a_short_frame_with_a_large_total_len_is_rejected_before_allocating() {
        // Two chunks may announce up to 2 * MESH_CHUNK_MAX_PAYLOAD bytes; send 2 bytes.
        let total_len = (2 * MESH_CHUNK_MAX_PAYLOAD) as u32;
        let meta = |chunk_index| MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index,
            chunk_count: 2,
            total_len,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let mut assembler = MeshFrameAssembler::new();
        assert!(
            assembler
                .push_chunk(&encode_mesh_chunk(&meta(0), b"a"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta(1), b"b")),
            Err(MeshAssembleError::LengthMismatch)
        );
        // The rejected frame left no state behind.
        assert!(assembler.inflight.is_empty());
    }

    #[test]
    fn more_data_than_total_len_is_rejected_as_soon_as_it_arrives() {
        let meta = |chunk_index| MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index,
            chunk_count: 3,
            total_len: 4,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let mut assembler = MeshFrameAssembler::new();
        assert!(
            assembler
                .push_chunk(&encode_mesh_chunk(&meta(0), b"abc"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta(1), b"de")),
            Err(MeshAssembleError::LengthMismatch)
        );
        assert!(assembler.inflight.is_empty());
    }

    /// A repeated chunk index replaces the earlier data; the length sum follows it.
    #[test]
    fn a_repeated_chunk_replaces_its_length() {
        let meta = |chunk_index| MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index,
            chunk_count: 2,
            total_len: 4,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let mut assembler = MeshFrameAssembler::new();
        assert!(
            assembler
                .push_chunk(&encode_mesh_chunk(&meta(0), b"xyz"))
                .unwrap()
                .is_none()
        );
        assert!(
            assembler
                .push_chunk(&encode_mesh_chunk(&meta(0), b"ab"))
                .unwrap()
                .is_none()
        );
        let (_, assembled) = assembler
            .push_chunk(&encode_mesh_chunk(&meta(1), b"cd"))
            .unwrap()
            .expect("both chunks arrived");
        assert_eq!(assembled, b"abcd");
    }

    /// A chunk of a frame in progress whose header disagrees is Malformed even when its
    /// total_len is also beyond what its chunk_count can carry, and the frame is dropped.
    #[test]
    fn a_disagreeing_header_beyond_the_length_bound_is_malformed() {
        let bytes = vec![5u8; MESH_CHUNK_MAX_PAYLOAD + 1];
        let chunks = split_mesh_frame(1, 1, 0, 0, false, &bytes);
        let (base, data) = decode_mesh_chunk(&chunks[1]).unwrap();
        type Change = fn(&mut MeshChunkMeta);
        let changes: [(&str, Change); 2] = [
            ("chunk_count", |m| {
                m.chunk_count = 1;
                m.chunk_index = 0;
            }),
            ("total_len", |m| m.total_len = u32::MAX),
        ];
        for (field, change) in changes {
            let mut meta = base;
            change(&mut meta);
            assert!(
                u64::from(meta.total_len)
                    > u64::from(meta.chunk_count) * MESH_CHUNK_MAX_PAYLOAD as u64,
                "{field}: the chunk must also be beyond the length bound"
            );
            let mut assembler = MeshFrameAssembler::new();
            assert!(assembler.push_chunk(&chunks[0]).unwrap().is_none());
            assert_eq!(
                assembler.push_chunk(&encode_mesh_chunk(&meta, data)),
                Err(MeshAssembleError::Malformed),
                "{field}"
            );
            assert!(
                assembler.inflight.is_empty(),
                "{field}: frame state is dropped"
            );
        }
    }

    /// A first chunk beyond the length bound is rejected without creating state.
    #[test]
    fn a_first_chunk_beyond_the_length_bound_leaves_no_state() {
        let meta = MeshChunkMeta {
            surface_id: 1,
            frame_id: 1,
            chunk_index: 0,
            chunk_count: 2,
            total_len: (2 * MESH_CHUNK_MAX_PAYLOAD + 1) as u32,
            generation: 0,
            frame_seq: 0,
            full_textures: false,
        };
        let mut assembler = MeshFrameAssembler::new();
        assert_eq!(
            assembler.push_chunk(&encode_mesh_chunk(&meta, b"x")),
            Err(MeshAssembleError::LengthMismatch)
        );
        assert!(assembler.inflight.is_empty());
    }

    /// A chunk of a frame in progress whose chunk_count differs is Malformed and drops the
    /// frame even when its index is also out of range for its own chunk_count.
    #[test]
    fn a_disagreeing_chunk_count_with_an_out_of_range_index_drops_the_frame() {
        let bytes = vec![6u8; MESH_CHUNK_MAX_PAYLOAD + 1];
        let chunks = split_mesh_frame(1, 1, 0, 0, false, &bytes);
        let (base, data) = decode_mesh_chunk(&chunks[1]).unwrap();
        type Change = fn(&mut MeshChunkMeta);
        let changes: [(&str, Change); 2] = [
            ("chunk_count=1, chunk_index=1", |m| {
                m.chunk_count = 1;
                m.chunk_index = 1;
            }),
            ("chunk_count=0", |m| m.chunk_count = 0),
        ];
        for (case, change) in changes {
            let mut meta = base;
            change(&mut meta);
            let mut assembler = MeshFrameAssembler::new();
            assert!(assembler.push_chunk(&chunks[0]).unwrap().is_none());
            assert_eq!(
                assembler.push_chunk(&encode_mesh_chunk(&meta, data)),
                Err(MeshAssembleError::Malformed),
                "{case}"
            );
            assert!(
                assembler.inflight.is_empty(),
                "{case}: frame state is dropped"
            );
            // The real index 1 now starts a new frame instead of completing the old one.
            assert!(
                assembler.push_chunk(&chunks[1]).unwrap().is_none(),
                "{case}"
            );
        }
    }

    #[test]
    fn assemble_errors_are_std_errors_with_english_text() {
        let err: Box<dyn Error> = Box::new(MeshAssembleError::LengthMismatch);
        assert_eq!(
            err.to_string(),
            "the mesh chunk lengths do not match the frame length"
        );
        assert_eq!(
            MeshAssembleError::Malformed.to_string(),
            "the mesh chunk header is malformed"
        );
    }
}
