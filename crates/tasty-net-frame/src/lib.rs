//! Frame codec and stream conventions of the Tasty network protocol.
//!
//! Every frame on a stream is `[tag: u8][len: u32 BE][payload]`. The format is the one
//! tasty already speaks (`tasty:crates/tasty-ipc/src/stream.rs` at 698aa49e03), so a peer
//! built on this crate and an older tasty exchange the same bytes.
//!
//! This crate also owns the payload layouts that sit inside a frame without knowing the
//! attach domain: the surface mux prefix ([`encode_mux`]), bulk file chunks
//! ([`encode_bulk_chunk`]) and mesh chunks ([`split_mesh_frame`], [`MeshFrameAssembler`]).
//! The read failure classification ([`ReadFailure`]) and the heartbeat constants are here
//! because every transport uses the same ones.
#![forbid(unsafe_code)]

mod read_failure;

use std::io::{self, Read, Write};
use std::time::Duration;

pub use read_failure::ReadFailure;

/// Frame header length: a 1-byte tag and a 4-byte big-endian payload length.
pub const FRAME_HEADER_LEN: usize = 5;

/// Upper bound of a frame payload (1 MiB).
///
/// [`write_frame`] refuses a longer payload and [`read_frame`] refuses a longer length
/// field before allocating, so a corrupt or hostile header cannot make the reader
/// allocate an arbitrary buffer.
pub const MAX_FRAME_LEN: u32 = 1 << 20;

/// Interval between [`StreamTag::Ping`] heartbeats.
///
/// Whether a side sends a Ping only after this long without other traffic or on a fixed
/// period is the transport's policy. Any received frame counts as liveness.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Default read timeout of a stream: six heartbeats.
///
/// The slack keeps a brief stall or jitter from looking like a dead peer; a peer is
/// declared gone only after it missed several heartbeats in a row.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Frame type tag, the first byte of every frame.
///
/// The numeric values are part of the wire format and never change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum StreamTag {
    /// Byte payload such as PTY output or echoed input, optionally prefixed by
    /// [`encode_mux`] or [`encode_bulk_chunk`].
    Data = 0,
    /// UTF-8 JSON for the handshake and session control.
    Control = 1,
    /// Heartbeat with an empty payload. Receiving any frame, this one included, renews
    /// the reader's read timeout, so a receiver needs no other handling.
    Ping = 2,
    /// Graceful close signal with an empty payload, in either direction.
    Detach = 3,
    /// Opaque mesh bytes split into chunks ([`split_mesh_frame`]). Kept apart from
    /// [`StreamTag::Data`] so a reader never has to tell mesh chunks from terminal
    /// output.
    MeshData = 4,
}

impl StreamTag {
    /// Returns the tag for a wire byte, or `None` for a byte no tag uses.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Data),
            1 => Some(Self::Control),
            2 => Some(Self::Ping),
            3 => Some(Self::Detach),
            4 => Some(Self::MeshData),
            _ => None,
        }
    }
}

/// One frame: a tag and its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamFrame {
    /// Frame type.
    pub tag: StreamTag,
    /// Payload bytes, at most [`MAX_FRAME_LEN`] long.
    pub payload: Vec<u8>,
}

impl StreamFrame {
    /// Builds a frame from a tag and a payload.
    pub fn new(tag: StreamTag, payload: Vec<u8>) -> Self {
        Self { tag, payload }
    }
}

/// Writes one frame and flushes.
///
/// The header and the payload are gathered into one buffer and handed to a single
/// `write_all`, which together with `TCP_NODELAY` keeps a small frame from being split
/// into delayed segments. `write_all` may still retry partial writes, so this does not
/// promise a single system call.
///
/// # Errors
///
/// `InvalidInput` when the payload is longer than [`MAX_FRAME_LEN`]; nothing is written
/// then. Otherwise the error of the underlying writer.
pub fn write_frame<W: Write>(w: &mut W, tag: StreamTag, payload: &[u8]) -> io::Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame payload exceeds u32"))?;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload exceeds MAX_FRAME_LEN",
        ));
    }
    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    buf.push(tag as u8);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one frame.
///
/// After any error the reader may already have consumed part of a header or payload, so
/// the frame boundary is lost: do not read the same stream again. [`ReadFailure::of_io`]
/// classifies the error.
///
/// # Errors
///
/// `UnexpectedEof` when the stream ends inside a frame (or before one), `InvalidData` for
/// an unknown tag or a length above [`MAX_FRAME_LEN`] (rejected before allocating), and
/// otherwise the error of the underlying reader, such as a read timeout.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<StreamFrame> {
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    r.read_exact(&mut hdr)?;
    let tag = StreamTag::from_u8(hdr[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown stream tag"))?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_LEN",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(StreamFrame { tag, payload })
}

/// Prefixes `bytes` with a 4-byte big-endian surface ID.
///
/// Used for the Data frames of a connection that carries several surfaces. A single
/// surface connection does not use it.
pub fn encode_mux(surface_id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&surface_id.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Inverse of [`encode_mux`]. Returns `None` when `buf` is shorter than 4 bytes.
pub fn decode_mux(buf: &[u8]) -> Option<(u32, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    let sid = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    Some((sid, &buf[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        for (tag, payload) in [
            (StreamTag::Data, b"hello world".to_vec()),
            (StreamTag::Control, br#"{"ok":true}"#.to_vec()),
            (StreamTag::Detach, Vec::new()),
            (StreamTag::Ping, vec![0u8; 1000]),
            (StreamTag::MeshData, vec![9u8; 64]),
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, tag, &payload).unwrap();
            let mut cur = Cursor::new(buf);
            let frame = read_frame(&mut cur).unwrap();
            assert_eq!(frame.tag, tag);
            assert_eq!(frame.payload, payload);
        }
    }

    /// A writer that accepts every byte at once must see the header and payload in one
    /// write call.
    #[test]
    fn write_frame_emits_one_write_call() {
        struct CountingWriter {
            writes: usize,
            buf: Vec<u8>,
        }
        impl Write for CountingWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        for payload in [Vec::new(), b"hi".to_vec(), vec![7u8; 4096]] {
            let mut w = CountingWriter {
                writes: 0,
                buf: Vec::new(),
            };
            write_frame(&mut w, StreamTag::Data, &payload).unwrap();
            assert_eq!(
                w.writes,
                1,
                "a {}-byte payload was split into {} writes",
                payload.len(),
                w.writes
            );
            assert_eq!(w.buf.len(), FRAME_HEADER_LEN + payload.len());
        }
    }

    #[test]
    fn write_frame_flushes() {
        struct FlushFlag(bool);
        impl Write for FlushFlag {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0 = true;
                Ok(())
            }
        }
        let mut w = FlushFlag(false);
        write_frame(&mut w, StreamTag::Ping, &[]).unwrap();
        assert!(w.0);
    }

    #[test]
    fn heartbeat_timeout_has_jitter_margin_over_interval() {
        assert!(HEARTBEAT_TIMEOUT >= HEARTBEAT_INTERVAL * 2);
        assert_eq!(HEARTBEAT_TIMEOUT, HEARTBEAT_INTERVAL * 6);
    }

    #[test]
    fn ping_frame_has_empty_payload_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::Ping, &[]).unwrap();
        let mut cur = Cursor::new(buf);
        let frame = read_frame(&mut cur).unwrap();
        assert_eq!(frame.tag, StreamTag::Ping);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn tag_from_u8() {
        for tag in [
            StreamTag::Data,
            StreamTag::Control,
            StreamTag::Ping,
            StreamTag::Detach,
            StreamTag::MeshData,
        ] {
            assert_eq!(StreamTag::from_u8(tag as u8), Some(tag));
        }
        assert_eq!(StreamTag::from_u8(5), None);
        assert_eq!(StreamTag::from_u8(9), None);
        assert_eq!(StreamTag::from_u8(u8::MAX), None);
    }

    #[test]
    fn read_rejects_unknown_tag() {
        // tag=9 (unknown), len=0
        let mut cur = Cursor::new(vec![9u8, 0, 0, 0, 0]);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// 5 is the first byte after the last tag (MeshData = 4).
    #[test]
    fn tag_five_is_a_protocol_failure() {
        let err = read_frame(&mut [5u8, 0, 0, 0, 0].as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(ReadFailure::of_io(&err), ReadFailure::Protocol);
    }

    #[test]
    fn read_rejects_oversize_len() {
        // tag=0, len=MAX_FRAME_LEN+1: rejected before allocating.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&(MAX_FRAME_LEN + 1).to_be_bytes());
        let mut cur = Cursor::new(bytes);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_frame_is_protocol_failure() {
        let mut hdr = vec![0u8];
        hdr.extend_from_slice(&(MAX_FRAME_LEN + 1).to_be_bytes());
        let err = read_frame(&mut hdr.as_slice()).unwrap_err();
        assert_eq!(ReadFailure::of_io(&err), ReadFailure::Protocol);
    }

    #[test]
    fn read_eof_is_err() {
        let mut cur = Cursor::new(Vec::new());
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_header_is_peer_closed() {
        let err = read_frame(&mut [1u8, 0].as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(ReadFailure::of_io(&err), ReadFailure::PeerClosed);
    }

    #[test]
    fn truncated_payload_is_peer_closed() {
        let err = read_frame(&mut [0u8, 0, 0, 0, 4, b'a', b'b'].as_slice()).unwrap_err();
        assert_eq!(ReadFailure::of_io(&err), ReadFailure::PeerClosed);
    }

    #[test]
    fn write_rejects_oversize_payload() {
        let big = vec![0u8; (MAX_FRAME_LEN + 1) as usize];
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, StreamTag::Data, &big).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(buf.is_empty(), "nothing is written for a rejected payload");
    }

    #[test]
    fn payload_of_exactly_max_frame_len_roundtrips() {
        let payload: Vec<u8> = (0..MAX_FRAME_LEN as usize)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::Data, &payload).unwrap();
        assert_eq!(buf.len(), FRAME_HEADER_LEN + MAX_FRAME_LEN as usize);
        assert_eq!(&buf[..FRAME_HEADER_LEN], &[0u8, 0, 0x10, 0, 0]);
        let frame = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(frame.tag, StreamTag::Data);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn consecutive_frames_keep_their_boundaries() {
        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::Data, b"one").unwrap();
        write_frame(&mut buf, StreamTag::Ping, &[]).unwrap();
        write_frame(&mut buf, StreamTag::Control, b"{}").unwrap();
        let mut r = buf.as_slice();
        assert_eq!(
            read_frame(&mut r).unwrap(),
            StreamFrame::new(StreamTag::Data, b"one".to_vec())
        );
        assert_eq!(
            read_frame(&mut r).unwrap(),
            StreamFrame::new(StreamTag::Ping, Vec::new())
        );
        assert_eq!(
            read_frame(&mut r).unwrap(),
            StreamFrame::new(StreamTag::Control, b"{}".to_vec())
        );
        assert!(r.is_empty());
    }

    /// Fixed byte strings of the tasty 698aa49e03 wire format. If the encoding ever drifts
    /// from what tasty sends and expects, this test fails.
    #[test]
    fn frame_bytes_match_tasty_wire() {
        const PING: [u8; 5] = [2, 0, 0, 0, 0];
        const DETACH: [u8; 5] = [3, 0, 0, 0, 0];
        const DATA_HI: [u8; 7] = [0, 0, 0, 0, 2, b'h', b'i'];
        const CONTROL_OK: [u8; 16] = [
            1, 0, 0, 0, 11, b'{', b'"', b'o', b'k', b'"', b':', b't', b'r', b'u', b'e', b'}',
        ];
        const MESH_300: [u8; 5] = [4, 0, 0, 0x01, 0x2C];

        let cases: [(StreamTag, Vec<u8>, &[u8]); 4] = [
            (StreamTag::Ping, Vec::new(), &PING),
            (StreamTag::Detach, Vec::new(), &DETACH),
            (StreamTag::Data, b"hi".to_vec(), &DATA_HI),
            (StreamTag::Control, br#"{"ok":true}"#.to_vec(), &CONTROL_OK),
        ];
        for (tag, payload, expected) in cases {
            let mut buf = Vec::new();
            write_frame(&mut buf, tag, &payload).unwrap();
            assert_eq!(buf, expected, "{tag:?}");
            let frame = read_frame(&mut &expected[..]).unwrap();
            assert_eq!(frame, StreamFrame::new(tag, payload));
        }

        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::MeshData, &[0xAB; 300]).unwrap();
        assert_eq!(&buf[..FRAME_HEADER_LEN], &MESH_300);
        assert_eq!(buf.len(), FRAME_HEADER_LEN + 300);
    }

    #[test]
    fn ping_frame_bytes_match_tasty_wire() {
        let mut buf = Vec::new();
        write_frame(&mut buf, StreamTag::Ping, &[]).unwrap();
        assert_eq!(buf, [2u8, 0, 0, 0, 0]);
    }

    #[test]
    fn mux_roundtrip() {
        let enc = encode_mux(42, b"hello");
        assert_eq!(&enc[..4], &42u32.to_be_bytes());
        let (sid, rest) = decode_mux(&enc).unwrap();
        assert_eq!(sid, 42);
        assert_eq!(rest, b"hello");
        let empty = encode_mux(7, b"");
        let (sid2, rest2) = decode_mux(&empty).unwrap();
        assert_eq!(sid2, 7);
        assert!(rest2.is_empty());
    }

    #[test]
    fn mux_bytes_match_tasty_wire() {
        assert_eq!(encode_mux(0x0102_0304, b"x"), [1u8, 2, 3, 4, b'x']);
    }

    #[test]
    fn decode_mux_rejects_truncated() {
        assert!(decode_mux(&[0u8, 1, 2]).is_none());
        assert!(decode_mux(&[]).is_none());
        assert!(decode_mux(&[0u8; 4]).is_some());
    }
}
