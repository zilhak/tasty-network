//! Coordination messages (docs/design/rendezvous.md "메시지 표").
//!
//! The top-level key `type` selects the message. An unknown `type` is an error (the reader
//! closes as `malformed`); unknown fields are dropped while parsing and are neither kept nor
//! forwarded. Addresses are `"203.0.113.5:3478"` / `"[2001:db8::1]:3478"` strings and times
//! are integer milliseconds (`*_ms`).
//!
//! Address lists a device sends stay strings here: an entry that does not parse is not a format
//! error but a dropped candidate (docs/design/rendezvous.md "후보 주소 규칙").

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tasty_net_identity::DeviceId;

use super::candidate::CandidateKind;

/// A message from a device to the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceMessage {
    /// Registration. It must arrive within `hello_timeout` after the connection.
    Hello(Hello),
    /// The NAT judgement and changed host addresses.
    NatReport(NatReport),
    /// A punch request towards `peer`.
    PunchRequest(PunchRequest),
    /// The answer of the receiving side to a `punch_offer`.
    PunchAnswer(PunchAnswer),
    /// The end of a punch on this device.
    PunchResult(PunchResult),
}

/// A message from the server to a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The device is registered.
    Welcome(Welcome),
    /// The server saw the remote address of the coordination connection change.
    ReflexiveChanged(ReflexiveChanged),
    /// A punch request is refused.
    PunchRefused(PunchRefused),
    /// Tells the receiving side about a request.
    PunchOffer(PunchOffer),
    /// Start punching.
    PunchGo(PunchGo),
    /// A punch in progress is cancelled.
    PunchCancel(PunchCancel),
}

/// Why a message could not be parsed or written.
#[derive(Debug)]
pub struct MessageError(ErrorKind);

#[derive(Debug)]
enum ErrorKind {
    Json(serde_json::Error),
    /// The JSON parsed but breaks a rule of the message table.
    Shape(&'static str),
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ErrorKind::Json(e) => write!(f, "coordination message is malformed: {e}"),
            ErrorKind::Shape(rule) => write!(f, "coordination message is malformed: {rule}"),
        }
    }
}

impl std::error::Error for MessageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            ErrorKind::Json(e) => Some(e),
            ErrorKind::Shape(_) => None,
        }
    }
}

fn json(e: serde_json::Error) -> MessageError {
    MessageError(ErrorKind::Json(e))
}

fn shape(rule: &'static str) -> MessageError {
    MessageError(ErrorKind::Shape(rule))
}

/// A frame body is one JSON object. serde would also read an array as the tag and the fields
/// in order; that is not a message. No object in it, at any depth, has a key twice: a
/// `serde_json::Value` keeps only the last value of a repeated key, so the check reads the
/// body itself first.
fn parse_object<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, MessageError> {
    if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(shape("the top level is not a JSON object"));
    }
    serde_json::from_slice::<NoRepeatedKeys>(body).map_err(json)?;
    serde_json::from_slice(body).map_err(json)
}

/// Reads any JSON value and fails on an object that has a key twice.
struct NoRepeatedKeys;

impl<'de> Deserialize<'de> for NoRepeatedKeys {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NoRepeatedKeysVisitor)
    }
}

struct NoRepeatedKeysVisitor;

impl<'de> serde::de::Visitor<'de> for NoRepeatedKeysVisitor {
    type Value = NoRepeatedKeys;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoRepeatedKeys)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<NoRepeatedKeys>()?.is_some() {}
        Ok(NoRepeatedKeys)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(A::Error::custom("an object has a key twice"));
            }
            map.next_value::<NoRepeatedKeys>()?;
        }
        Ok(NoRepeatedKeys)
    }
}

/// A list whose entries are JSON objects. serde would also read an array entry as the fields
/// in order; that is not an entry of the message table.
fn objects<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    Vec::<serde_json::Map<String, serde_json::Value>>::deserialize(d)?
        .into_iter()
        .map(|m| T::deserialize(serde_json::Value::Object(m)).map_err(D::Error::custom))
        .collect()
}

/// A field the table requires that may be `null`: the key must be there.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}

impl DeviceMessage {
    /// Parse a frame body. Besides the JSON shape, a `punch_result` must give a reason exactly
    /// when it is not `ok`, and a `punch_answer` exactly when it does not `accept`: a success or
    /// an acceptance has no `reason` key at all, not even `null`.
    pub fn parse(body: &[u8]) -> Result<Self, MessageError> {
        let value: serde_json::Value = parse_object(body)?;
        let has_reason = value.get("reason").is_some();
        let msg: DeviceMessage = serde_json::from_value(value).map_err(json)?;
        match &msg {
            DeviceMessage::PunchResult(r)
                if (r.ok && has_reason) || (!r.ok && r.reason.is_none()) =>
            {
                Err(shape("punch_result has a reason exactly when ok is false"))
            }
            DeviceMessage::PunchAnswer(a)
                if (a.accept && has_reason) || (!a.accept && a.reason.is_none()) =>
            {
                Err(shape(
                    "punch_answer has a reason exactly when accept is false",
                ))
            }
            _ => Ok(msg),
        }
    }

    /// The frame body (JSON) of this message.
    pub fn to_json(&self) -> Result<Vec<u8>, MessageError> {
        serde_json::to_vec(self).map_err(json)
    }
}

impl ServerMessage {
    /// Parse a frame body.
    pub fn parse(body: &[u8]) -> Result<Self, MessageError> {
        parse_object(body)
    }

    /// The frame body (JSON) of this message.
    pub fn to_json(&self) -> Result<Vec<u8>, MessageError> {
        serde_json::to_vec(self).map_err(json)
    }

    /// The `type` name of this message.
    pub fn type_name(&self) -> &'static str {
        match self {
            ServerMessage::Welcome(_) => "welcome",
            ServerMessage::ReflexiveChanged(_) => "reflexive_changed",
            ServerMessage::PunchRefused(_) => "punch_refused",
            ServerMessage::PunchOffer(_) => "punch_offer",
            ServerMessage::PunchGo(_) => "punch_go",
            ServerMessage::PunchCancel(_) => "punch_cancel",
        }
    }
}

/// `hello`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The coordination protocol number ([`super::PROTO`]).
    pub proto: u32,
    /// Host candidate addresses in the device's own priority order (required, may be empty).
    pub host_candidates: Vec<String>,
}

/// `nat_report`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatReport {
    /// The NAT mapping judgement.
    pub mapping: NatMapping,
    /// For each server STUN address the device asked, the reflexive address it got
    /// (required, may be empty).
    #[serde(deserialize_with = "objects")]
    pub stun_results: Vec<StunResult>,
    /// Changed host addresses. Absent keeps the addresses the server has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_candidates: Option<Vec<String>>,
}

/// One entry of `nat_report.stun_results`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StunResult {
    /// The server STUN address that was asked.
    pub server: String,
    /// The reflexive address in its answer, `null` when there was no answer (the key is
    /// required).
    #[serde(deserialize_with = "nullable")]
    pub reflexive: Option<String>,
}

/// `punch_request`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchRequest {
    /// The device's number for this request.
    pub request_id: u64,
    /// The device ID of the peer.
    #[serde(with = "device_id_text")]
    pub peer: DeviceId,
}

/// `punch_answer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchAnswer {
    /// The punch this answers.
    pub punch_id: u64,
    /// Whether the receiving side accepts.
    pub accept: bool,
    /// Why it refuses (only when `accept` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<AnswerReason>,
}

/// `punch_result`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchResult {
    /// The punch this reports.
    pub punch_id: u64,
    /// Whether the QUIC handshake finished.
    pub ok: bool,
    /// Why it failed (only when `ok` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ResultReason>,
}

/// `welcome`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The remote address of the coordination connection as the server sees it.
    pub reflexive: SocketAddr,
    /// The alternate STUN addresses (docs/design/rendezvous.md "광고 주소와 바인딩 주소").
    pub stun: Vec<SocketAddr>,
    /// The server values the device checks against its own settings.
    pub limits: Limits,
}

/// `welcome.limits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// `offer_timeout`.
    pub offer_timeout_ms: u64,
    /// `start_delay_max`.
    pub start_delay_max_ms: u64,
    /// `punch_window`.
    pub punch_window_ms: u64,
    /// `quic_handshake_timeout`.
    pub quic_handshake_ms: u64,
    /// The session TTL of the `go_sent` stage.
    pub punch_session_ttl_ms: u64,
    /// `max_host_candidates`.
    pub max_host_candidates: u32,
    /// `max_reported_reflexive`.
    pub max_reported_reflexive: u32,
    /// `max_message_bytes`.
    pub max_message_bytes: u32,
}

/// `reflexive_changed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflexiveChanged {
    /// The new remote address.
    pub reflexive: SocketAddr,
}

/// `punch_refused`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchRefused {
    /// The refused request.
    pub request_id: u64,
    /// Why.
    pub reason: RefuseReason,
}

/// `punch_offer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchOffer {
    /// The server's number for the punch.
    pub punch_id: u64,
    /// The requesting device.
    #[serde(with = "device_id_text")]
    pub from: DeviceId,
}

/// `punch_go`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchGo {
    /// The server's number for the punch.
    pub punch_id: u64,
    /// The request number, only in the message to the requesting side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    /// The other device.
    #[serde(with = "device_id_text")]
    pub peer: DeviceId,
    /// `controlling` for the requesting side, `controlled` for the receiving side.
    pub role: Role,
    /// Which candidates are in the message.
    pub mode: PunchMode,
    /// The peer's candidates in priority order.
    #[serde(deserialize_with = "objects")]
    pub candidates: Vec<Candidate>,
    /// The peer's NAT mapping.
    pub peer_nat: NatMapping,
    /// The token this device sends.
    pub token_local: PunchToken,
    /// The token the peer sends.
    pub token_peer: PunchToken,
    /// The start delay.
    pub start_after_ms: u64,
    /// The probe window.
    pub window_ms: u64,
    /// The QUIC handshake deadline.
    pub handshake_ms: u64,
}

/// `punch_cancel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunchCancel {
    /// The cancelled punch.
    pub punch_id: u64,
    /// Why.
    pub reason: CancelReason,
}

/// One candidate of `punch_go.candidates`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Candidate {
    /// The address.
    pub addr: SocketAddr,
    /// Where the address came from.
    pub kind: CandidateKind,
}

/// The NAT mapping judgement (docs/design/p2p.md "NAT 판별").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NatMapping {
    /// Endpoint-independent mapping, judged with a second server IP.
    Independent,
    /// The two ports of one server IP got the same mapping (no second IP).
    IndependentPortOnly,
    /// The mapping depends on the destination.
    Dependent,
    /// Not judged or incomplete.
    #[default]
    Unknown,
}

/// The reasons of `punch_refused`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefuseReason {
    /// The peer is not registered or did not answer in time.
    NotFound,
    /// The peer refused (not paired).
    Declined,
    /// The peer's limits check against the server failed.
    PeerLimitsInvalid,
    /// No candidate path is possible for the NAT pair.
    NatUnsupported,
    /// A limit, or a punch of the same pair in the other direction.
    Busy,
    /// The device asked for itself.
    #[serde(rename = "self")]
    SelfPeer,
}

/// The reasons of a refusing `punch_answer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerReason {
    /// The requesting device is not in the pairing list.
    NotPaired,
    /// The receiving device's limits check against the server failed.
    LimitsInvalid,
}

/// The reasons of `punch_cancel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The other device's registration is gone.
    PeerGone,
    /// The session TTL passed.
    Expired,
    /// The same requesting device started a new session for this pair.
    Superseded,
    /// The other device reported `limits_invalid`.
    PeerLimitsInvalid,
}

/// The reasons of a failed `punch_result`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultReason {
    /// The values in `punch_go` were outside the limits.
    LimitsInvalid,
    /// The window or the handshake deadline passed.
    Timeout,
    /// The QUIC handshake failed.
    HandshakeFailed,
}

/// The punch role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The requesting side. It opens the QUIC connection.
    Controlling,
    /// The receiving side.
    Controlled,
}

/// Which candidates a punch uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PunchMode {
    /// Every kind.
    All,
    /// Host candidates only (both devices are `dependent`).
    HostOnly,
}

/// A punch token: 16 random bytes made by the server, sent as 32 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PunchToken(pub [u8; super::stun::PUNCH_TOKEN_LEN]);

impl PunchToken {
    /// A token from the OS random source.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; super::stun::PUNCH_TOKEN_LEN];
        getrandom::fill(&mut bytes)?;
        Ok(PunchToken(bytes))
    }
}

impl fmt::Debug for PunchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PunchToken({self})")
    }
}

impl fmt::Display for PunchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A token string that is not 32 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchTokenParseError;

impl fmt::Display for PunchTokenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "punch token is not 32 lowercase hex digits")
    }
}

impl std::error::Error for PunchTokenParseError {}

impl FromStr for PunchToken {
    type Err = PunchTokenParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s.as_bytes();
        if digits.len() != super::stun::PUNCH_TOKEN_LEN * HEX_DIGITS_PER_BYTE {
            return Err(PunchTokenParseError);
        }
        let mut out = [0; super::stun::PUNCH_TOKEN_LEN];
        for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(HEX_DIGITS_PER_BYTE)) {
            let text = std::str::from_utf8(pair).map_err(|_| PunchTokenParseError)?;
            if !text
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(PunchTokenParseError);
            }
            *byte = u8::from_str_radix(text, HEX_RADIX).map_err(|_| PunchTokenParseError)?;
        }
        Ok(PunchToken(out))
    }
}

/// Hex text: two digits per byte, base 16.
const HEX_DIGITS_PER_BYTE: usize = 2;
const HEX_RADIX: u32 = 16;

impl Serialize for PunchToken {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PunchToken {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

mod device_id_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use tasty_net_identity::DeviceId;

    pub fn serialize<S: Serializer>(id: &DeviceId, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(id)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DeviceId, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    #[test]
    fn unknown_type_is_an_error_and_unknown_fields_are_dropped() {
        assert!(DeviceMessage::parse(br#"{"type":"relay","data":"x"}"#).is_err());
        assert!(DeviceMessage::parse(br#"{"proto":1}"#).is_err());
        let m = DeviceMessage::parse(
            br#"{"type":"hello","proto":1,"host_candidates":[],"payload":"abc"}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            DeviceMessage::Hello(Hello {
                proto: 1,
                host_candidates: Vec::new()
            })
        );
        // Re-encoding carries only the known fields.
        let json = String::from_utf8(m.to_json().unwrap()).unwrap();
        assert!(!json.contains("payload"), "{json}");
    }

    #[test]
    fn messages_round_trip() {
        let peer = DeviceKey::from_seed([7; 32]).id();
        let go = ServerMessage::PunchGo(PunchGo {
            punch_id: 9,
            request_id: Some(3),
            peer,
            role: Role::Controlling,
            mode: PunchMode::All,
            candidates: vec![Candidate {
                addr: "[2001:db8::1]:3478".parse().unwrap(),
                kind: CandidateKind::ReportedReflexive,
            }],
            peer_nat: NatMapping::IndependentPortOnly,
            token_local: PunchToken([1; 16]),
            token_peer: PunchToken([0xab; 16]),
            start_after_ms: 100,
            window_ms: 5000,
            handshake_ms: 5000,
        });
        let json = String::from_utf8(go.to_json().unwrap()).unwrap();
        assert!(json.contains(r#""type":"punch_go""#), "{json}");
        assert!(json.contains(r#""addr":"[2001:db8::1]:3478""#), "{json}");
        assert!(json.contains(r#""kind":"reported_reflexive""#), "{json}");
        assert!(json.contains(&"ab".repeat(16)), "{json}");
        assert!(json.contains(&peer.to_string()), "{json}");
        assert_eq!(ServerMessage::parse(json.as_bytes()).unwrap(), go);
        // A candidate written as an array (fields in order) is not an entry.
        let entry = r#"{"addr":"[2001:db8::1]:3478","kind":"reported_reflexive"}"#;
        assert!(json.contains(entry), "{json}");
        let as_array = json.replace(entry, r#"["[2001:db8::1]:3478","reported_reflexive"]"#);
        assert!(ServerMessage::parse(as_array.as_bytes()).is_err());
        // A candidate entry with a key twice.
        let twice = json.replace(
            entry,
            r#"{"addr":"[2001:db8::1]:3478","kind":"reported_reflexive","kind":"host"}"#,
        );
        assert!(ServerMessage::parse(twice.as_bytes()).is_err());

        let refused = ServerMessage::PunchRefused(PunchRefused {
            request_id: 1,
            reason: RefuseReason::SelfPeer,
        });
        let json = String::from_utf8(refused.to_json().unwrap()).unwrap();
        assert!(json.contains(r#""reason":"self""#), "{json}");
    }

    #[test]
    fn only_objects_with_the_required_fields_are_messages() {
        // An array reads as the tag and the fields in order to serde: not a message.
        for body in [
            &br#"["hello",1,[]]"#[..],
            br#" ["punch_request",1,"00"]"#,
            br#""hello""#,
            b"1",
        ] {
            assert!(DeviceMessage::parse(body).is_err(), "{body:?}");
        }
        assert!(ServerMessage::parse(br#"["punch_cancel",1,"expired"]"#).is_err());
        // A key twice, at the top or in a list entry, is malformed: the last value does not
        // win.
        for body in [
            &br#"{"type":"hello","proto":2,"host_candidates":[],"proto":1}"#[..],
            br#"{"type":"hello","type":"hello","proto":1,"host_candidates":[]}"#,
            br#"{"type":"nat_report","mapping":"unknown","stun_results":[{"server":"192.0.2.1:3478","reflexive":null,"server":"192.0.2.2:3478"}]}"#,
        ] {
            assert!(DeviceMessage::parse(body).is_err(), "{body:?}");
        }
        // List entries are objects too, and a nullable field the table requires has its key.
        for (body, ok) in [
            (
                &br#"{"type":"nat_report","mapping":"unknown","stun_results":[{"server":"192.0.2.1:3478","reflexive":null}]}"#[..],
                true,
            ),
            (
                br#"{"type":"nat_report","mapping":"unknown","stun_results":[{"server":"192.0.2.1:3478"}]}"#,
                false,
            ),
            (
                br#"{"type":"nat_report","mapping":"unknown","stun_results":[["192.0.2.1:3478",null]]}"#,
                false,
            ),
        ] {
            assert_eq!(DeviceMessage::parse(body).is_ok(), ok, "{body:?}");
        }
        // Fields the table requires.
        for body in [
            &br#"{"type":"hello","proto":1}"#[..],
            br#"{"type":"nat_report","mapping":"unknown"}"#,
            br#"{"type":"punch_answer","punch_id":1}"#,
            br#"{"type":"punch_result","punch_id":1}"#,
        ] {
            assert!(DeviceMessage::parse(body).is_err(), "{body:?}");
        }
        // nat_report.host_candidates may be left out: the server keeps what it has.
        let m = DeviceMessage::parse(
            br#" {"type":"nat_report","mapping":"unknown","stun_results":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            m,
            DeviceMessage::NatReport(NatReport {
                host_candidates: None,
                ..
            })
        ));
        // A reason exactly when the punch failed or the answer refused.
        for (body, ok) in [
            (
                &br#"{"type":"punch_result","punch_id":1,"ok":true,"reason":"limits_invalid"}"#[..],
                false,
            ),
            (br#"{"type":"punch_result","punch_id":1,"ok":false}"#, false),
            (
                br#"{"type":"punch_result","punch_id":1,"ok":true,"reason":null}"#,
                false,
            ),
            (br#"{"type":"punch_result","punch_id":1,"ok":true}"#, true),
            (
                br#"{"type":"punch_result","punch_id":1,"ok":false,"reason":"timeout"}"#,
                true,
            ),
            (
                br#"{"type":"punch_answer","punch_id":1,"accept":true,"reason":"not_paired"}"#,
                false,
            ),
            (
                br#"{"type":"punch_answer","punch_id":1,"accept":false}"#,
                false,
            ),
            (
                br#"{"type":"punch_answer","punch_id":1,"accept":true,"reason":null}"#,
                false,
            ),
            (
                br#"{"type":"punch_answer","punch_id":1,"accept":true}"#,
                true,
            ),
            (
                br#"{"type":"punch_answer","punch_id":1,"accept":false,"reason":"not_paired"}"#,
                true,
            ),
        ] {
            assert_eq!(DeviceMessage::parse(body).is_ok(), ok, "{body:?}");
        }
    }

    #[test]
    fn token_text_is_strict() {
        assert!("AB".repeat(16).parse::<PunchToken>().is_err());
        assert!("ab".repeat(15).parse::<PunchToken>().is_err());
        assert!("zz".repeat(16).parse::<PunchToken>().is_err());
        assert_eq!(
            "0f".repeat(16).parse::<PunchToken>().unwrap(),
            PunchToken([0x0f; 16])
        );
    }
}
