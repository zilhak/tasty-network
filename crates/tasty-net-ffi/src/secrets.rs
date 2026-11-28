//! The connection tokens this end showed to peers, kept out of every text a peer can fill
//! (docs/design/mobile.md "바인딩 API (1단계)", the token boundary).
//!
//! A dialer sends the token of the peer's record in its first handshake message. The peer
//! answers with texts of its own (the refusal's sentence, `ext` and reason code, the `ext` of
//! a successful answer, a request's `error` value), and the connection table copies the
//! reason into its causes. A peer that sends the token back would put it into errors, records
//! and events of the bindings. Every such text goes through [`Secrets::scrub`] first, which
//! replaces each token this end sent with [`Secrets::MASK`]: its hex form in any letter case,
//! and the token's bytes themselves when they are valid UTF-8 (a peer can decode the hex and
//! send the bytes as text).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tasty_net_transport::DialAuth;

/// The tokens one [`TastyNet`](crate::TastyNet) (or one test dialer) sent.
#[derive(Clone, Default)]
pub(crate) struct Secrets(Arc<Mutex<Known>>);

/// The texts of the tokens: hex forms in lowercase (matched in any letter case) and the
/// tokens' bytes as text where they are valid UTF-8 (matched exactly).
#[derive(Default)]
struct Known {
    hex: Vec<String>,
    raw: Vec<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secrets({} tokens)", self.lock().hex.len())
    }
}

/// The digits of the token's hex form, in their order.
const HEX_DIGITS: &str = "0123456789abcdef";

/// The bytes of a hex text, or none when it is not hex.
fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    let radix = u8::try_from(HEX_DIGITS.len()).ok()?;
    let mut out = Vec::new();
    let mut high = None;
    for c in hex.chars() {
        let digit = u8::try_from(HEX_DIGITS.find(c.to_ascii_lowercase())?).ok()?;
        match high.take() {
            None => high = Some(digit),
            Some(h) => out.push(h * radix + digit),
        }
    }
    high.is_none().then_some(out)
}

impl Secrets {
    /// What a token becomes in a scrubbed text.
    pub(crate) const MASK: &'static str = "[token]";

    fn lock(&self) -> MutexGuard<'_, Known> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remembers the token of `auth`, the credential a connection is about to send.
    pub(crate) fn remember(&self, auth: Option<&DialAuth>) {
        if let Some(DialAuth::Token(token)) = auth {
            let hex = token.expose_hex().to_ascii_lowercase();
            let raw = hex_bytes(&hex)
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|text| !text.is_empty());
            let mut known = self.lock();
            if !known.hex.contains(&hex) {
                known.hex.push(hex);
            }
            if let Some(raw) = raw
                && !known.raw.contains(&raw)
            {
                known.raw.push(raw);
            }
        }
    }

    /// `text` with every remembered token replaced by [`Secrets::MASK`].
    pub(crate) fn scrub(&self, text: &str) -> String {
        let known = self.lock();
        let mut out = text.to_string();
        for token in &known.hex {
            out = replace_ignoring_case(&out, token);
        }
        for raw in &known.raw {
            out = out.replace(raw.as_str(), Self::MASK);
        }
        out
    }

    /// [`Secrets::scrub`] on an optional text.
    pub(crate) fn scrub_opt(&self, text: Option<String>) -> Option<String> {
        text.map(|t| self.scrub(&t))
    }
}

/// `text` with every ASCII-case-insensitive occurrence of the lowercase `needle` masked.
fn replace_ignoring_case(text: &str, needle: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    // ASCII lowercasing keeps every byte offset, so a match in `lower` is a match in `text`
    // at the same offsets, on character boundaries (the needle is ASCII).
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let (mut rest, mut lower_rest) = (text, lower.as_str());
    while let Some(found) = lower_rest.find(needle) {
        out.push_str(&rest[..found]);
        out.push_str(Secrets::MASK);
        rest = &rest[found + needle.len()..];
        lower_rest = &lower_rest[found + needle.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn every_case_of_a_token_is_masked() {
        assert_eq!(
            replace_ignoring_case("a ABcd b abcd", "abcd"),
            "a [token] b [token]"
        );
        assert_eq!(replace_ignoring_case("é abcd", "abcd"), "é [token]");
        assert_eq!(replace_ignoring_case("none", "abcd"), "none");
    }

    #[test]
    fn the_token_bytes_as_text_are_masked_too() {
        assert_eq!(hex_bytes("5e41"), Some(b"^A".to_vec()));
        assert_eq!(hex_bytes("5E4"), None);
        assert_eq!(hex_bytes("zz"), None);
        let secrets = Secrets::default();
        let token = tasty_net_identity::ConnectionToken::from_str(&"5e".repeat(32)).expect("token");
        secrets.remember(Some(&DialAuth::Token(token)));
        let bytes = "^".repeat(32);
        assert_eq!(secrets.scrub(&format!("id {bytes}!")), "id [token]!");
        assert_eq!(secrets.scrub(&"5E".repeat(32)), Secrets::MASK);
        // Bytes that are not UTF-8 have no text form; their hex is still masked.
        let other = tasty_net_identity::ConnectionToken::from_str(&"c7".repeat(32)).expect("token");
        secrets.remember(Some(&DialAuth::Token(other)));
        assert_eq!(secrets.scrub(&"c7".repeat(32)), Secrets::MASK);
        assert_eq!(secrets.lock().raw.len(), 1);
    }
}
