//! Lowercase hex, the text form of nonces, credentials and signatures on the wire.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Exactly `2 * N` lowercase hex digits. Uppercase and any other length are refused.
pub(crate) fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
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

/// 32 bytes from the OS random source (nonces and bulk credentials).
pub(crate) fn random_32() -> Result<[u8; 32], std::io::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes)
}

/// A uniform random number in [0, 1) from the OS random source (the redialer's jitter).
pub(crate) fn random_unit() -> Result<f64, std::io::Error> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    // The top 53 bits, the precision of an f64, over 2^53.
    let bits = u64::from_le_bytes(bytes) >> (u64::BITS - f64::MANTISSA_DIGITS);
    Ok(bits as f64 / (1u64 << f64::MANTISSA_DIGITS) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_uppercase_and_other_lengths() {
        let bytes = [0x00, 0x7f, 0xab, 0xff];
        assert_eq!(encode(&bytes), "007fabff");
        assert_eq!(decode::<4>("007fabff"), Some(bytes));
        assert_eq!(decode::<4>("007FABFF"), None);
        assert_eq!(decode::<4>("007fab"), None);
        assert_eq!(decode::<4>("007fabzz"), None);
    }
}
