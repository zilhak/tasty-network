//! Lowercase hex, the only text form of keys, signatures and tokens on the wire and in files.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Decode exactly `2 * N` lowercase hex digits. Uppercase and any other length are refused.
pub(crate) fn decode<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(text.chunks_exact(2)) {
        *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_strictness() {
        let bytes = [0x00, 0x7f, 0xa5, 0xff];
        let text = encode(&bytes);
        assert_eq!(text, "007fa5ff");
        assert_eq!(decode::<4>(text.as_bytes()), Some(bytes));
        assert_eq!(decode::<4>(b"007FA5FF"), None);
        assert_eq!(decode::<4>(b"007fa5f"), None);
        assert_eq!(decode::<4>(b"007fa5ff00"), None);
        assert_eq!(decode::<4>(b"007fa5fg"), None);
    }
}
