//! The connection token of one instance (docs/design/identity.md "토큰 비교").

use std::{fmt, str::FromStr};

use subtle::ConstantTimeEq;

use crate::{device_key::os_random_32, error::RandomError, hex};

/// The token of one running instance: 32 OS random bytes as 64 lowercase hex digits, the same
/// form as tasty's `SessionToken::generate`. A new value per instance start; it is published in
/// a [`crate::ConnectionRecord`] and never shared between instances.
///
/// `Debug` and `Display` hide the value. Use [`ConnectionToken::expose_hex`] to put it on the
/// wire or in the record, and [`ConnectionToken::matches`] to check a received token.
#[derive(Clone)]
pub struct ConnectionToken(String);

impl ConnectionToken {
    /// Issue a new token.
    pub fn issue() -> Result<Self, RandomError> {
        Ok(Self(hex::encode(&os_random_32()?)))
    }

    /// The 64 lowercase hex digits. Do not log them.
    pub fn expose_hex(&self) -> &str {
        &self.0
    }

    /// Compare a received token (its wire bytes) with this token in constant time. A different
    /// length is false at once; the same length compares every byte.
    pub fn matches(&self, received: &[u8]) -> bool {
        constant_time_eq(self.0.as_bytes(), received)
    }
}

/// Byte comparison whose time does not depend on where the inputs differ. Inputs of different
/// lengths are unequal at once.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

impl PartialEq for ConnectionToken {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other.0.as_bytes())
    }
}

impl Eq for ConnectionToken {}

impl fmt::Debug for ConnectionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionToken(<redacted>)")
    }
}

impl fmt::Display for ConnectionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl FromStr for ConnectionToken {
    type Err = TokenParseError;

    /// Only 64 lowercase hex digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode::<32>(s.as_bytes()).ok_or(TokenParseError)?;
        Ok(Self(s.to_owned()))
    }
}

/// The text is not 64 lowercase hex digits. It carries no part of the refused text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenParseError;

impl fmt::Display for TokenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("token is not 64 lowercase hex digits")
    }
}

impl std::error::Error for TokenParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_gives_64_lowercase_hex_and_new_values() {
        let a = ConnectionToken::issue().unwrap();
        let b = ConnectionToken::issue().unwrap();
        assert_eq!(a.expose_hex().len(), 64);
        assert!(
            a.expose_hex()
                .bytes()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(a, b);
        assert_eq!(a.expose_hex().parse::<ConnectionToken>().unwrap(), a);
    }

    #[test]
    fn comparison_same_one_byte_off_and_other_length() {
        let token = ConnectionToken::issue().unwrap();
        let same = token.expose_hex().as_bytes().to_vec();
        assert!(token.matches(&same));

        let mut one_off = same.clone();
        one_off[63] = if one_off[63] == b'0' { b'1' } else { b'0' };
        assert!(!token.matches(&one_off));
        let mut first_off = same.clone();
        first_off[0] = if first_off[0] == b'0' { b'1' } else { b'0' };
        assert!(!token.matches(&first_off));

        assert!(!token.matches(&same[..63]));
        let mut longer = same.clone();
        longer.push(b'0');
        assert!(!token.matches(&longer));
        assert!(!token.matches(b""));

        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"a", b"ab"));
    }

    #[test]
    fn parse_refuses_other_forms_and_formatting_hides_the_value() {
        let token = ConnectionToken::issue().unwrap();
        let hex = token.expose_hex();
        assert!("A".repeat(64).parse::<ConnectionToken>().is_err());
        assert!("a".repeat(64).parse::<ConnectionToken>().is_ok());
        assert!(hex[..62].parse::<ConnectionToken>().is_err());
        assert!(format!("{hex}0").parse::<ConnectionToken>().is_err());
        assert!(
            format!("{}g", &hex[..63])
                .parse::<ConnectionToken>()
                .is_err()
        );
        assert!(!format!("{token:?}").contains(hex));
        assert!(!format!("{token}").contains(hex));
    }
}
