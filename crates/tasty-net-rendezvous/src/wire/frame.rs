//! Coordination frames: a 4-byte big-endian unsigned length and that many bytes of one UTF-8
//! JSON object (docs/design/rendezvous.md "프레임").
//!
//! The reader compares the declared length with the limit as soon as the 4 length bytes are in
//! and before it reads or allocates the body. The frame deadline starts at the first byte of the
//! frame. The limit and the deadline are settings of the side that reads.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The length prefix size.
pub const FRAME_HEADER_LEN: usize = 4;

/// Why a frame could not be read or written. Every case closes the connection as `malformed`.
#[derive(Debug)]
pub enum FrameError {
    /// The declared length is 0.
    ZeroLength,
    /// The declared length is over the limit.
    TooLong {
        /// The declared length.
        declared: u64,
        /// The limit.
        limit: u32,
    },
    /// The stream ended inside a frame.
    Truncated,
    /// The frame did not end within the frame deadline after its first byte.
    Timeout,
    /// The stream failed.
    Io(std::io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::ZeroLength => write!(f, "frame length is zero"),
            FrameError::TooLong { declared, limit } => {
                write!(f, "frame length {declared} is over the limit {limit}")
            }
            FrameError::Truncated => write!(f, "stream ended inside a frame"),
            FrameError::Timeout => write!(f, "frame did not end within the frame deadline"),
            FrameError::Io(e) => write!(f, "frame stream failed: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Check a declared length against the limit.
pub fn check_length(declared: u32, limit: u32) -> Result<usize, FrameError> {
    if declared == 0 {
        return Err(FrameError::ZeroLength);
    }
    if declared > limit {
        return Err(FrameError::TooLong {
            declared: u64::from(declared),
            limit,
        });
    }
    usize::try_from(declared).map_err(|_| FrameError::TooLong {
        declared: u64::from(declared),
        limit,
    })
}

/// One frame for `body`. A body that is empty or over `limit` is refused.
pub fn encode_frame(body: &[u8], limit: u32) -> Result<Vec<u8>, FrameError> {
    let declared = u32::try_from(body.len()).map_err(|_| FrameError::TooLong {
        declared: u64::try_from(body.len()).unwrap_or(u64::MAX),
        limit,
    })?;
    check_length(declared, limit)?;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    out.extend_from_slice(&declared.to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Write one frame for `body`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
    limit: u32,
) -> Result<(), FrameError> {
    let frame = encode_frame(body, limit)?;
    writer.write_all(&frame).await.map_err(FrameError::Io)
}

/// Read one frame. `Ok(None)` is a clean end of the stream before the first byte of a frame.
///
/// The deadline `frame_timeout` starts when the first byte of the frame arrives. The declared
/// length is checked against `limit` before the body is read or a buffer for it is allocated.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: u32,
    frame_timeout: Duration,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0; FRAME_HEADER_LEN];
    let first = reader.read(&mut header).await.map_err(FrameError::Io)?;
    if first == 0 {
        return Ok(None);
    }
    let rest = async {
        read_exact_or_truncated(reader, &mut header[first..]).await?;
        let len = check_length(u32::from_be_bytes(header), limit)?;
        let mut body = vec![0; len];
        read_exact_or_truncated(reader, &mut body).await?;
        Ok(body)
    };
    match tokio::time::timeout(frame_timeout, rest).await {
        Ok(body) => body.map(Some),
        Err(_) => Err(FrameError::Timeout),
    }
}

async fn read_exact_or_truncated<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<(), FrameError> {
    match reader.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(e) => Err(FrameError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::ReadBuf;

    use super::*;

    const LIMIT: u32 = 16 * 1024;
    const DEADLINE: Duration = Duration::from_secs(5);

    /// A reader that hands out the given chunks one read at a time, then either ends or stays
    /// pending forever. It records how many bytes were read.
    struct Chunks {
        chunks: Vec<Vec<u8>>,
        end: bool,
        read: usize,
    }

    impl AsyncRead for Chunks {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.chunks.is_empty() {
                return if self.end {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                };
            }
            let chunk = self.chunks.remove(0);
            let n = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..n]);
            if n < chunk.len() {
                self.chunks.insert(0, chunk[n..].to_vec());
            }
            self.read += n;
            Poll::Ready(Ok(()))
        }
    }

    fn reader(chunks: Vec<Vec<u8>>, end: bool) -> Chunks {
        Chunks {
            chunks,
            end,
            read: 0,
        }
    }

    #[tokio::test]
    async fn frame_length_bounds() {
        assert!(matches!(
            check_length(0, LIMIT),
            Err(FrameError::ZeroLength)
        ));
        assert_eq!(check_length(1, LIMIT).unwrap(), 1);
        assert_eq!(check_length(LIMIT, LIMIT).unwrap(), LIMIT as usize);
        assert!(matches!(
            check_length(LIMIT + 1, LIMIT),
            Err(FrameError::TooLong { declared, limit }) if declared == u64::from(LIMIT) + 1 && limit == LIMIT
        ));
        assert!(matches!(
            check_length(u32::MAX, u32::MAX - 1),
            Err(FrameError::TooLong { .. })
        ));
        assert_eq!(check_length(u32::MAX, u32::MAX).unwrap(), u32::MAX as usize);
        assert!(matches!(
            encode_frame(b"", LIMIT),
            Err(FrameError::ZeroLength)
        ));
        let big = vec![b'x'; LIMIT as usize + 1];
        assert!(matches!(
            encode_frame(&big, LIMIT),
            Err(FrameError::TooLong { .. })
        ));
        let exact = vec![b'x'; LIMIT as usize];
        let frame = encode_frame(&exact, LIMIT).unwrap();
        assert_eq!(&frame[..4], &LIMIT.to_be_bytes());
        let mut r = reader(vec![frame], true);
        assert_eq!(
            read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().unwrap(),
            exact
        );

        // A zero length on the wire is malformed.
        let mut r = reader(vec![vec![0, 0, 0, 0]], true);
        assert!(matches!(
            read_frame(&mut r, LIMIT, DEADLINE).await,
            Err(FrameError::ZeroLength)
        ));
    }

    #[tokio::test]
    async fn partial_and_coalesced_frames() {
        let a = encode_frame(br#"{"type":"a"}"#, LIMIT).unwrap();
        let b = encode_frame(br#"{"type":"bb"}"#, LIMIT).unwrap();

        // Byte by byte.
        let bytes: Vec<Vec<u8>> = a.iter().map(|b| vec![*b]).collect();
        let mut r = reader(bytes, true);
        assert_eq!(
            read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().unwrap(),
            br#"{"type":"a"}"#
        );
        assert!(read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().is_none());

        // Header split, then the rest of a and all of b in one read.
        let mut joined = a[2..].to_vec();
        joined.extend_from_slice(&b);
        let mut r = reader(vec![a[..2].to_vec(), joined], true);
        assert_eq!(
            read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().unwrap(),
            br#"{"type":"a"}"#
        );
        assert_eq!(
            read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().unwrap(),
            br#"{"type":"bb"}"#
        );
        assert!(read_frame(&mut r, LIMIT, DEADLINE).await.unwrap().is_none());

        // The stream ends inside the header and inside the body.
        let mut r = reader(vec![a[..3].to_vec()], true);
        assert!(matches!(
            read_frame(&mut r, LIMIT, DEADLINE).await,
            Err(FrameError::Truncated)
        ));
        let mut r = reader(vec![a[..a.len() - 1].to_vec()], true);
        assert!(matches!(
            read_frame(&mut r, LIMIT, DEADLINE).await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn frame_deadline_starts_at_first_byte() {
        // Nothing arrives: no deadline runs (the connection idle timeout covers this).
        let mut r = reader(Vec::new(), false);
        let waited = tokio::time::timeout(DEADLINE * 2, read_frame(&mut r, LIMIT, DEADLINE)).await;
        assert!(waited.is_err());

        // One byte arrives and the rest never does: the frame deadline ends it.
        let mut r = reader(vec![vec![0]], false);
        assert!(matches!(
            read_frame(&mut r, LIMIT, DEADLINE).await,
            Err(FrameError::Timeout)
        ));
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_before_allocation() {
        // The header declares more than the limit and the body never comes. The reader must
        // refuse on the header alone: it does not wait for the body and reads nothing past
        // the 4 length bytes.
        let mut r = reader(vec![(LIMIT + 1).to_be_bytes().to_vec()], false);
        let got = tokio::time::timeout(DEADLINE, read_frame(&mut r, LIMIT, DEADLINE))
            .await
            .expect("must not wait for the body");
        assert!(matches!(got, Err(FrameError::TooLong { .. })));
        assert_eq!(r.read, FRAME_HEADER_LEN);

        // The largest u32 is refused the same way without trying to allocate 4 GiB.
        let mut r = reader(vec![u32::MAX.to_be_bytes().to_vec()], false);
        let got = tokio::time::timeout(DEADLINE, read_frame(&mut r, LIMIT, DEADLINE))
            .await
            .expect("must not wait for the body");
        assert!(matches!(got, Err(FrameError::TooLong { .. })));
    }
}
