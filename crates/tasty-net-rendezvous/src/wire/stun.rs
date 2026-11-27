//! The STUN subset: Binding Request and Binding Success Response with XOR-MAPPED-ADDRESS
//! ([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §5, §14.2), and the punch probe
//! attribute `TASTY-PUNCH` (docs/design/rendezvous.md "펀칭 프로브").
//!
//! The server answers Binding Requests without authentication attributes. It drops a request
//! that carries `TASTY-PUNCH` (probes are for devices) or any comprehension-required
//! attribute (for example RFC 5780 CHANGE-REQUEST and RESPONSE-PORT): it answers only the
//! address the request came from.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::message::PunchToken;

/// STUN header length.
pub const STUN_HEADER_LEN: usize = 20;
/// The magic cookie.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// The upper 16 bits of the magic cookie (the XOR of the port).
const MAGIC_COOKIE_HIGH: u16 = 0x2112;
/// The transaction ID length.
pub const TRANSACTION_ID_LEN: usize = 12;
/// Binding Request message type.
pub const BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response message type.
pub const BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
/// XOR-MAPPED-ADDRESS attribute type.
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// TASTY-PUNCH attribute type (comprehension-optional, not registered).
pub const ATTR_TASTY_PUNCH: u16 = 0xC0A1;
/// The punch token length.
pub const PUNCH_TOKEN_LEN: usize = 16;
/// Attribute types below this are comprehension-required.
const COMPREHENSION_OPTIONAL_MIN: u16 = 0x8000;
/// Attribute header length (type and length).
const ATTR_HEADER_LEN: usize = 4;
/// Attributes are padded to a multiple of this.
const ATTR_ALIGN: usize = 4;
/// XOR-MAPPED-ADDRESS families.
const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;
/// The reserved first byte of an address value.
const ADDRESS_RESERVED: u8 = 0x00;
/// The two top bits of the message type are zero in STUN.
const TYPE_TOP_BITS: u16 = 0xC000;

/// A transaction ID.
pub type TransactionId = [u8; TRANSACTION_ID_LEN];

/// The class of a parsed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunClass {
    /// Binding Request.
    BindingRequest,
    /// Binding Success Response.
    BindingSuccess,
}

/// A parsed Binding Request or Binding Success Response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunMessage {
    /// Request or response.
    pub class: StunClass,
    /// The transaction ID.
    pub transaction_id: TransactionId,
    /// The XOR-MAPPED-ADDRESS value, decoded.
    pub mapped: Option<SocketAddr>,
    /// The TASTY-PUNCH token.
    pub punch_token: Option<PunchToken>,
    /// Whether an unknown comprehension-required attribute is present.
    pub unknown_required: bool,
}

/// Why a packet is not a message of this subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunError {
    /// Shorter than the header, or the length field does not match the packet.
    Length,
    /// Not STUN (top bits or magic cookie).
    NotStun,
    /// Another method or class.
    Unsupported,
    /// An attribute runs past the end or has a wrong value length.
    Attribute,
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            StunError::Length => "STUN length does not match the packet",
            StunError::NotStun => "packet is not STUN",
            StunError::Unsupported => "STUN message is not a Binding request or success response",
            StunError::Attribute => "STUN attribute is malformed",
        };
        f.write_str(text)
    }
}

impl std::error::Error for StunError {}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(size_of::<u8>()).and_then(|b| b.first().copied())
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(size_of::<u16>())
            .and_then(|b| b.try_into().ok())
            .map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(size_of::<u32>())
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
    }
}

/// Parse a packet.
pub fn parse(packet: &[u8]) -> Result<StunMessage, StunError> {
    let mut r = Reader(packet);
    let msg_type = r.u16().ok_or(StunError::Length)?;
    let length = usize::from(r.u16().ok_or(StunError::Length)?);
    let cookie = r.u32().ok_or(StunError::Length)?;
    let txid: TransactionId = r
        .take(TRANSACTION_ID_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or(StunError::Length)?;
    if msg_type & TYPE_TOP_BITS != 0 || cookie != MAGIC_COOKIE {
        return Err(StunError::NotStun);
    }
    if r.0.len() != length || !length.is_multiple_of(ATTR_ALIGN) {
        return Err(StunError::Length);
    }
    let class = match msg_type {
        BINDING_REQUEST => StunClass::BindingRequest,
        BINDING_SUCCESS_RESPONSE => StunClass::BindingSuccess,
        _ => return Err(StunError::Unsupported),
    };
    let mut out = StunMessage {
        class,
        transaction_id: txid,
        mapped: None,
        punch_token: None,
        unknown_required: false,
    };
    while !r.0.is_empty() {
        let attr_type = r.u16().ok_or(StunError::Attribute)?;
        let attr_len = usize::from(r.u16().ok_or(StunError::Attribute)?);
        let value = r.take(attr_len).ok_or(StunError::Attribute)?;
        r.take(attr_len.next_multiple_of(ATTR_ALIGN) - attr_len)
            .ok_or(StunError::Attribute)?;
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                out.mapped = Some(decode_xor_address(value, &txid)?);
            }
            ATTR_TASTY_PUNCH => {
                let token: [u8; PUNCH_TOKEN_LEN] =
                    value.try_into().map_err(|_| StunError::Attribute)?;
                out.punch_token = Some(PunchToken(token));
            }
            t if t < COMPREHENSION_OPTIONAL_MIN => out.unknown_required = true,
            _ => {}
        }
    }
    Ok(out)
}

/// The XOR key of an address: the magic cookie followed by the transaction ID.
fn xor_key(txid: &TransactionId) -> impl Iterator<Item = u8> + '_ {
    MAGIC_COOKIE
        .to_be_bytes()
        .into_iter()
        .chain(txid.iter().copied())
}

fn decode_xor_address(value: &[u8], txid: &TransactionId) -> Result<SocketAddr, StunError> {
    let mut r = Reader(value);
    r.u8().ok_or(StunError::Attribute)?;
    let family = r.u8().ok_or(StunError::Attribute)?;
    let port = r.u16().ok_or(StunError::Attribute)? ^ MAGIC_COOKIE_HIGH;
    let ip = match family {
        FAMILY_V4 => {
            let raw: [u8; size_of::<Ipv4Addr>()] = r
                .take(size_of::<Ipv4Addr>())
                .and_then(|b| b.try_into().ok())
                .ok_or(StunError::Attribute)?;
            IpAddr::V4(Ipv4Addr::from(xor_bytes(raw, txid)))
        }
        FAMILY_V6 => {
            let raw: [u8; size_of::<Ipv6Addr>()] = r
                .take(size_of::<Ipv6Addr>())
                .and_then(|b| b.try_into().ok())
                .ok_or(StunError::Attribute)?;
            IpAddr::V6(Ipv6Addr::from(xor_bytes(raw, txid)))
        }
        _ => return Err(StunError::Attribute),
    };
    if !r.0.is_empty() {
        return Err(StunError::Attribute);
    }
    Ok(SocketAddr::new(ip, port))
}

fn xor_bytes<const N: usize>(mut raw: [u8; N], txid: &TransactionId) -> [u8; N] {
    for (b, k) in raw.iter_mut().zip(xor_key(txid)) {
        *b ^= k;
    }
    raw
}

fn encode(msg_type: u16, txid: &TransactionId, attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let body_len: usize = attrs
        .iter()
        .map(|(_, v)| ATTR_HEADER_LEN + v.len().next_multiple_of(ATTR_ALIGN))
        .sum();
    let mut out = Vec::with_capacity(STUN_HEADER_LEN + body_len);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(
        &u16::try_from(body_len)
            .expect("the subset's attributes are short")
            .to_be_bytes(),
    );
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    for (t, v) in attrs {
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(
            &u16::try_from(v.len())
                .expect("the subset's attributes are short")
                .to_be_bytes(),
        );
        out.extend_from_slice(v);
        out.resize(
            out.len() + v.len().next_multiple_of(ATTR_ALIGN) - v.len(),
            ADDRESS_RESERVED,
        );
    }
    out
}

fn encode_xor_address(addr: SocketAddr, txid: &TransactionId) -> Vec<u8> {
    let ip = addr.ip().to_canonical();
    let mut v = vec![ADDRESS_RESERVED];
    match ip {
        IpAddr::V4(v4) => {
            v.push(FAMILY_V4);
            v.extend_from_slice(&(addr.port() ^ MAGIC_COOKIE_HIGH).to_be_bytes());
            v.extend_from_slice(&xor_bytes(v4.octets(), txid));
        }
        IpAddr::V6(v6) => {
            v.push(FAMILY_V6);
            v.extend_from_slice(&(addr.port() ^ MAGIC_COOKIE_HIGH).to_be_bytes());
            v.extend_from_slice(&xor_bytes(v6.octets(), txid));
        }
    }
    v
}

/// A Binding Request. With `token` it is a punch probe.
pub fn binding_request(txid: &TransactionId, token: Option<&PunchToken>) -> Vec<u8> {
    let attrs: Vec<(u16, Vec<u8>)> = token
        .map(|t| (ATTR_TASTY_PUNCH, t.0.to_vec()))
        .into_iter()
        .collect();
    encode(BINDING_REQUEST, txid, &attrs)
}

/// A Binding Success Response carrying `mapped` as XOR-MAPPED-ADDRESS. With `token` it is the
/// answer to a punch probe.
pub fn binding_success(
    txid: &TransactionId,
    mapped: SocketAddr,
    token: Option<&PunchToken>,
) -> Vec<u8> {
    let mut attrs = vec![(ATTR_XOR_MAPPED_ADDRESS, encode_xor_address(mapped, txid))];
    if let Some(t) = token {
        attrs.push((ATTR_TASTY_PUNCH, t.0.to_vec()));
    }
    encode(BINDING_SUCCESS_RESPONSE, txid, &attrs)
}

/// A new random transaction ID.
pub fn random_transaction_id() -> Result<TransactionId, getrandom::Error> {
    let mut id = [0; TRANSACTION_ID_LEN];
    getrandom::fill(&mut id)?;
    Ok(id)
}

/// What the server does with a STUN packet on one of its ports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerStunAction {
    /// Send this response to the address the request came from.
    Answer(Vec<u8>),
    /// Drop: not a plain Binding Request.
    Drop,
}

/// The server's answer to a packet from `from`. Only a Binding Request without `TASTY-PUNCH`
/// and without comprehension-required attributes gets an answer, and the answer goes to
/// `from` only.
pub fn server_answer(packet: &[u8], from: SocketAddr) -> ServerStunAction {
    match parse(packet) {
        Ok(m)
            if m.class == StunClass::BindingRequest
                && m.punch_token.is_none()
                && !m.unknown_required =>
        {
            ServerStunAction::Answer(binding_success(&m.transaction_id, from, None))
        }
        _ => ServerStunAction::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: TransactionId = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn sizes_match_the_design() {
        assert_eq!(binding_request(&TXID, None).len(), 20);
        let v4 = binding_success(&TXID, "203.0.113.5:3478".parse().unwrap(), None);
        assert_eq!(v4.len(), 32);
        let v6 = binding_success(&TXID, "[2001:db8::1]:3478".parse().unwrap(), None);
        assert_eq!(v6.len(), 44);
    }

    #[test]
    fn round_trip_with_token() {
        let token = PunchToken([0x5a; 16]);
        let req = parse(&binding_request(&TXID, Some(&token))).unwrap();
        assert_eq!(req.class, StunClass::BindingRequest);
        assert_eq!(req.punch_token, Some(token));
        assert_eq!(req.transaction_id, TXID);
        for addr in ["198.51.100.7:65535", "[2001:db8::7]:1"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let resp = parse(&binding_success(&TXID, addr, Some(&token))).unwrap();
            assert_eq!(resp.class, StunClass::BindingSuccess);
            assert_eq!(resp.mapped, Some(addr));
            assert_eq!(resp.punch_token, Some(token));
        }
    }

    #[test]
    fn rfc5769_ipv4_response_vector() {
        // RFC 5769 §2.2 transaction ID and mapped address 192.0.2.1:32853: the XOR-MAPPED-
        // ADDRESS value bytes are 0x0001 0xa147 0xe112a643.
        let txid: TransactionId = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let resp = binding_success(&txid, "192.0.2.1:32853".parse().unwrap(), None);
        assert_eq!(
            &resp[20..32],
            &[
                0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43
            ]
        );
    }

    #[test]
    fn server_drops_probes_and_redirect_requests() {
        let from: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        match server_answer(&binding_request(&TXID, None), from) {
            ServerStunAction::Answer(resp) => {
                assert_eq!(parse(&resp).unwrap().mapped, Some(from));
            }
            other => panic!("{other:?}"),
        }
        let probe = binding_request(&TXID, Some(&PunchToken([1; 16])));
        assert_eq!(server_answer(&probe, from), ServerStunAction::Drop);
        // CHANGE-REQUEST (0x0003) is comprehension-required.
        let mut change = binding_request(&TXID, None);
        change.extend_from_slice(&[0x00, 0x03, 0x00, 0x04, 0, 0, 0, 0x06]);
        change[3] = 8;
        assert!(parse(&change).unwrap().unknown_required);
        assert_eq!(server_answer(&change, from), ServerStunAction::Drop);
        // A response is not answered.
        let resp = binding_success(&TXID, from, None);
        assert_eq!(server_answer(&resp, from), ServerStunAction::Drop);
        // Garbage.
        assert_eq!(server_answer(&[0, 1, 0], from), ServerStunAction::Drop);
        let mut bad_cookie = binding_request(&TXID, None);
        bad_cookie[4] ^= 1;
        assert_eq!(parse(&bad_cookie), Err(StunError::NotStun));
    }
}
