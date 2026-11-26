//! Byte layout of a bulk file chunk inside a [`StreamTag::Data`](crate::StreamTag::Data)
//! frame of a bulk channel.

/// Length of the bulk chunk sub-header `[transfer_id: u64 BE][seq: u32 BE]`.
pub const BULK_CHUNK_HEADER_LEN: usize = 12;

/// Encodes one bulk file chunk: the 12-byte sub-header followed by the raw file bytes
/// (no base64).
///
/// `seq` is for diagnostics and checks only. A bulk channel is ordered, so the receiver
/// does not reorder by it.
pub fn encode_bulk_chunk(transfer_id: u64, seq: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(BULK_CHUNK_HEADER_LEN + bytes.len());
    out.extend_from_slice(&transfer_id.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Inverse of [`encode_bulk_chunk`]: returns `(transfer_id, seq, file bytes)`, or `None`
/// when `buf` is shorter than [`BULK_CHUNK_HEADER_LEN`].
pub fn decode_bulk_chunk(buf: &[u8]) -> Option<(u64, u32, &[u8])> {
    if buf.len() < BULK_CHUNK_HEADER_LEN {
        return None;
    }
    let transfer_id = u64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    let seq = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    Some((transfer_id, seq, &buf[BULK_CHUNK_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_chunk_roundtrip() {
        let enc = encode_bulk_chunk(0x0102_0304_0506_0708, 42, b"payload");
        assert_eq!(&enc[..8], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&enc[8..12], &42u32.to_be_bytes());
        let (tid, seq, rest) = decode_bulk_chunk(&enc).unwrap();
        assert_eq!(tid, 0x0102_0304_0506_0708);
        assert_eq!(seq, 42);
        assert_eq!(rest, b"payload");
        let empty = encode_bulk_chunk(7, 0, b"");
        let (tid2, seq2, rest2) = decode_bulk_chunk(&empty).unwrap();
        assert_eq!(tid2, 7);
        assert_eq!(seq2, 0);
        assert!(rest2.is_empty());
    }

    #[test]
    fn bulk_chunk_bytes_match_tasty_wire() {
        assert_eq!(
            encode_bulk_chunk(1, 2, b"z"),
            [0u8, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, b'z']
        );
    }

    #[test]
    fn decode_bulk_chunk_rejects_truncated() {
        assert!(decode_bulk_chunk(&[]).is_none());
        assert!(decode_bulk_chunk(&[0u8; 11]).is_none());
        assert!(decode_bulk_chunk(&[0u8; 12]).is_some());
    }
}
