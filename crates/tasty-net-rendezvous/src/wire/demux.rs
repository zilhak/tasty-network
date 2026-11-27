//! First-byte classes of a UDP packet on a socket that carries STUN and QUIC
//! ([RFC 7983](https://www.rfc-editor.org/rfc/rfc7983) §7,
//! [RFC 9443](https://www.rfc-editor.org/rfc/rfc9443) §3). There is no TURN on these sockets,
//! so 64–79 is QUIC too.

use std::ops::RangeInclusive;

/// First bytes of STUN messages.
pub const STUN_FIRST_BYTES: RangeInclusive<u8> = 0..=3;
/// First bytes of QUIC short header packets (fixed bit set).
pub const QUIC_SHORT_FIRST_BYTES: RangeInclusive<u8> = 64..=127;
/// First bytes of QUIC long header packets (fixed bit set).
pub const QUIC_LONG_FIRST_BYTES: RangeInclusive<u8> = 192..=255;

/// Where a received packet goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketClass {
    /// A STUN message.
    Stun,
    /// A QUIC packet.
    Quic,
    /// Anything else. It is dropped and counted.
    Other,
}

/// The class of a packet by its first byte. An empty packet is [`PacketClass::Other`].
pub fn classify(packet: &[u8]) -> PacketClass {
    match packet.first() {
        Some(b) if STUN_FIRST_BYTES.contains(b) => PacketClass::Stun,
        Some(b) if QUIC_SHORT_FIRST_BYTES.contains(b) || QUIC_LONG_FIRST_BYTES.contains(b) => {
            PacketClass::Quic
        }
        _ => PacketClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_byte_boundaries() {
        let cases = [
            (0u8, PacketClass::Stun),
            (3, PacketClass::Stun),
            (4, PacketClass::Other),
            (63, PacketClass::Other),
            (64, PacketClass::Quic),
            (127, PacketClass::Quic),
            (128, PacketClass::Other),
            (191, PacketClass::Other),
            (192, PacketClass::Quic),
            (255, PacketClass::Quic),
        ];
        for (b, want) in cases {
            assert_eq!(classify(&[b, 0, 0]), want, "first byte {b}");
        }
        assert_eq!(classify(&[]), PacketClass::Other);
    }
}
