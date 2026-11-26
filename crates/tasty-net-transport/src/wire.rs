//! The bytes of handshake v2: message 1 (one JSON line), the challenge, hello, proof and ACK
//! (JSON in Control frames), and the signed transcript. This file and its tests are the
//! canonical byte form (docs/design/handshake.md).
//!
//! The reply frames are told apart by their top-level key: a frame with `ok` is an ACK (the
//! v1 ACK has the same shape), `challenge`, `hello` and `proof` wrap their fields in an object
//! under that key.

use std::str::FromStr;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tasty_net_identity::{ConnectionToken, DeviceId, Signature};

use crate::hex;
use crate::reject::RejectReason;
use crate::role::Holder;

/// The `proto` value of handshake v2.
pub const PROTO: u32 = 2;
/// The method of message 1.
pub const STREAM_OPEN_METHOD: &str = "stream.open";
/// Longest message 1 line this crate accepts, line break included. Longer lines are
/// [`RejectReason::Malformed`]. It is far below the tasty request line limit.
pub const MAX_HANDSHAKE_LINE: usize = 64 * 1024;
/// The fixed `error` of a [`RejectReason::LegacyProtocol`] refusal. A v1 client shows it as
/// `stream.open rejected: <error>`.
pub const LEGACY_PROTOCOL_ERROR: &str =
    "unsupported stream proto 1 (server speaks 2); update Tasty on the connecting machine";
/// The domain separation string at the start of the signed transcript.
const TRANSCRIPT_DOMAIN: &str = "tasty-net handshake v2";

/// What a channel is opened for (message 1 `purpose`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// The ordered channel of an attach link.
    Attach,
    /// A bulk channel under an attach link.
    Bulk,
    /// A request channel.
    Request,
}

impl Purpose {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Bulk => "bulk",
            Self::Request => "request",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "attach" => Some(Self::Attach),
            "bulk" => Some(Self::Bulk),
            "request" => Some(Self::Request),
            _ => None,
        }
    }
}

/// Whether `name` is a valid capability name: 1 to 64 bytes, starting with an ASCII
/// lowercase letter or digit, then lowercase letters, digits, `.`, `_` and `-`.
pub fn is_valid_capability(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    bytes.len() <= 64
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && rest
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// Checks a capability list: every name valid, no name twice.
pub(crate) fn valid_caps(caps: &[String]) -> bool {
    caps.iter().all(|c| is_valid_capability(c))
        && caps.iter().enumerate().all(|(i, c)| !caps[..i].contains(c))
}

/// Which path the channel came over.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Path {
    /// SSH (token, nonces, signatures).
    Ssh,
    /// P2P: the device is the connection's remote ID; `scope` names the QUIC connection.
    P2p { remote_id: DeviceId, scope: u64 },
}

/// Message 1 as the dialer writes it.
pub(crate) struct OpenOut<'a> {
    pub purpose: Purpose,
    pub token: Option<&'a ConnectionToken>,
    pub device_id: Option<DeviceId>,
    pub instance_id: &'a str,
    pub nonce: Option<[u8; 32]>,
    pub holder: Option<Holder>,
    pub caps: Option<&'a [String]>,
    pub ext: Option<&'a str>,
    pub bulk_credential: Option<[u8; 32]>,
}

impl OpenOut<'_> {
    /// The message 1 line, line break included.
    pub(crate) fn line(&self) -> Vec<u8> {
        let mut params = Map::new();
        params.insert("proto".into(), json!(PROTO));
        params.insert("purpose".into(), json!(self.purpose.as_str()));
        if let Some(token) = self.token {
            params.insert("token".into(), json!(token.expose_hex()));
        }
        if let Some(id) = self.device_id {
            params.insert("device_id".into(), json!(id.to_string()));
        }
        params.insert("instance_id".into(), json!(self.instance_id));
        if let Some(nonce) = self.nonce {
            params.insert("nonce".into(), json!(hex::encode(&nonce)));
        }
        if let Some(holder) = self.holder {
            params.insert("holder".into(), json!(holder.as_str()));
        }
        if let Some(caps) = self.caps {
            params.insert("caps".into(), json!(caps));
        }
        if let Some(ext) = self.ext {
            params.insert("ext".into(), json!(ext));
        }
        if let Some(cred) = self.bulk_credential {
            params.insert("bulk_credential".into(), json!(hex::encode(&cred)));
        }
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": STREAM_OPEN_METHOD,
            "params": Value::Object(params),
        });
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        line
    }
}

/// Message 1 after the checks of steps 1 to 3.
#[derive(Debug)]
pub(crate) struct OpenIn {
    pub purpose: Purpose,
    pub device_id: Option<DeviceId>,
    pub instance_id: String,
    pub nonce: Option<[u8; 32]>,
    pub holder: Option<Holder>,
    pub caps: Vec<String>,
    pub ext: Option<String>,
    pub bulk_credential: Option<[u8; 32]>,
}

/// Step 1: the line is a JSON object with object `params`, and `proto` is 2. The line may
/// come with or without its line break; either way it is measured with the break, so it
/// holds at most [`MAX_HANDSHAKE_LINE`] - 1 bytes before it.
pub(crate) fn parse_open_line(line: &[u8]) -> Result<Map<String, Value>, RejectReason> {
    let content = line.strip_suffix(b"\n").unwrap_or(line);
    if content.len() + 1 > MAX_HANDSHAKE_LINE {
        return Err(RejectReason::Malformed);
    }
    let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(line) else {
        return Err(RejectReason::Malformed);
    };
    if message.get("method").and_then(Value::as_str) != Some(STREAM_OPEN_METHOD) {
        return Err(RejectReason::Malformed);
    }
    let Some(Value::Object(params)) = message.remove("params") else {
        return Err(RejectReason::Malformed);
    };
    // Missing, not an integer, negative or above u32: malformed.
    let proto = params
        .get("proto")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .ok_or(RejectReason::Malformed)?;
    match proto {
        PROTO => Ok(params),
        1 => Err(RejectReason::LegacyProtocol),
        _ => Err(RejectReason::UnsupportedProtocol),
    }
}

/// Step 2: the token, compared in constant time. Only a missing field is
/// [`RejectReason::TokenMissing`]; anything that is not the token (`null`, another type,
/// another format) is [`RejectReason::TokenInvalid`].
pub(crate) fn check_token(
    params: &Map<String, Value>,
    token: Option<&ConnectionToken>,
) -> Result<(), RejectReason> {
    let received = match params.get("token") {
        None => return Err(RejectReason::TokenMissing),
        Some(Value::String(s)) => s.as_bytes(),
        Some(_) => return Err(RejectReason::TokenInvalid),
    };
    match token {
        Some(token) if token.matches(received) => Ok(()),
        _ => Err(RejectReason::TokenInvalid),
    }
}

fn string_field<'a>(
    params: &'a Map<String, Value>,
    name: &str,
    required: bool,
) -> Result<Option<&'a str>, RejectReason> {
    match params.get(name) {
        None if !required => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        _ => Err(RejectReason::Malformed),
    }
}

fn hex_field(
    params: &Map<String, Value>,
    name: &str,
    required: bool,
) -> Result<Option<[u8; 32]>, RejectReason> {
    string_field(params, name, required)?
        .map(|s| hex::decode::<32>(s).ok_or(RejectReason::Malformed))
        .transpose()
}

fn caps_field(value: Option<&Value>) -> Result<Vec<String>, RejectReason> {
    let Some(Value::Array(items)) = value else {
        return Err(RejectReason::Malformed);
    };
    let caps = items
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or(RejectReason::Malformed)?;
    if !valid_caps(&caps) {
        return Err(RejectReason::Malformed);
    }
    Ok(caps)
}

/// Step 3: the fields the purpose needs on this path, with their types and formats. Unknown
/// fields are ignored.
pub(crate) fn parse_fields(
    params: &Map<String, Value>,
    path: Path,
) -> Result<OpenIn, RejectReason> {
    let purpose = string_field(params, "purpose", true)?
        .and_then(Purpose::parse)
        .ok_or(RejectReason::Malformed)?;
    let instance_id = string_field(params, "instance_id", true)?
        .filter(|s| !s.is_empty())
        .ok_or(RejectReason::Malformed)?
        .to_string();
    let ext = string_field(params, "ext", false)?.map(str::to_string);
    let ssh = matches!(path, Path::Ssh);
    let open = match purpose {
        Purpose::Attach | Purpose::Request => {
            let device_id = string_field(params, "device_id", true)?
                .map(|s| DeviceId::from_str(s).map_err(|_| RejectReason::Malformed))
                .transpose()?;
            if let (Path::P2p { remote_id, .. }, Some(id)) = (path, device_id)
                && id != remote_id
            {
                return Err(RejectReason::Malformed);
            }
            let holder = if purpose == Purpose::Attach {
                Some(
                    string_field(params, "holder", true)?
                        .and_then(Holder::parse)
                        .ok_or(RejectReason::Malformed)?,
                )
            } else {
                None
            };
            OpenIn {
                purpose,
                device_id,
                instance_id,
                nonce: if ssh {
                    hex_field(params, "nonce", true)?
                } else {
                    None
                },
                holder,
                caps: caps_field(params.get("caps"))?,
                ext,
                bulk_credential: None,
            }
        }
        Purpose::Bulk => OpenIn {
            purpose,
            device_id: None,
            instance_id,
            nonce: None,
            holder: None,
            caps: Vec::new(),
            ext,
            bulk_credential: hex_field(params, "bulk_credential", true)?,
        },
    };
    Ok(open)
}

/// The acceptor's challenge (SSH) or hello (P2P).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Greeting {
    pub device_id: DeviceId,
    pub instance_id: String,
    pub caps: Vec<String>,
    /// SSH only.
    pub nonce: Option<[u8; 32]>,
    /// SSH only.
    pub sig: Option<Signature>,
}

impl Greeting {
    pub(crate) fn encode(&self, key: &str) -> Vec<u8> {
        let mut fields = Map::new();
        fields.insert("device_id".into(), json!(self.device_id.to_string()));
        fields.insert("instance_id".into(), json!(self.instance_id));
        fields.insert("caps".into(), json!(self.caps));
        if let Some(nonce) = self.nonce {
            fields.insert("nonce".into(), json!(hex::encode(&nonce)));
        }
        if let Some(sig) = self.sig {
            fields.insert("sig".into(), json!(sig.to_string()));
        }
        let mut outer = Map::new();
        outer.insert(key.into(), Value::Object(fields));
        Value::Object(outer).to_string().into_bytes()
    }

    fn parse(fields: &Map<String, Value>, ssh: bool) -> Option<Self> {
        let device_id = DeviceId::from_str(fields.get("device_id")?.as_str()?).ok()?;
        let instance_id = fields.get("instance_id")?.as_str()?.to_string();
        if instance_id.is_empty() {
            return None;
        }
        let caps = caps_field(fields.get("caps")).ok()?;
        let (nonce, sig) = if ssh {
            (
                Some(hex::decode::<32>(fields.get("nonce")?.as_str()?)?),
                Some(Signature::from_str(fields.get("sig")?.as_str()?).ok()?),
            )
        } else {
            (None, None)
        };
        Some(Self {
            device_id,
            instance_id,
            caps,
            nonce,
            sig,
        })
    }
}

/// Message 4.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ack {
    pub ok: bool,
    pub proto: u32,
    pub reason: Option<RejectReason>,
    pub error: Option<String>,
    pub client_id: Option<u32>,
    pub bulk_credential: Option<[u8; 32]>,
    pub ext: Option<String>,
}

impl Ack {
    pub(crate) fn refusal(reason: &RejectReason, ext: Option<String>) -> Self {
        Self {
            ok: false,
            proto: PROTO,
            reason: Some(reason.clone()),
            error: Some(reason.message().to_string()),
            ext,
            ..Self::default()
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut fields = Map::new();
        fields.insert("ok".into(), json!(self.ok));
        fields.insert("proto".into(), json!(self.proto));
        if let Some(reason) = &self.reason {
            fields.insert("reason".into(), json!(reason.as_str()));
        }
        if let Some(error) = &self.error {
            fields.insert("error".into(), json!(error));
        }
        if let Some(id) = self.client_id {
            fields.insert("client_id".into(), json!(id));
        }
        if let Some(cred) = self.bulk_credential {
            fields.insert("bulk_credential".into(), json!(hex::encode(&cred)));
        }
        if let Some(ext) = &self.ext {
            fields.insert("ext".into(), json!(ext));
        }
        Value::Object(fields).to_string().into_bytes()
    }

    fn parse(fields: &Map<String, Value>) -> Option<Self> {
        let ok = fields.get("ok")?.as_bool()?;
        // A missing `proto` reads as 0, which no refusal or success accepts.
        let proto = match fields.get("proto") {
            None => 0,
            Some(v) => u32::try_from(v.as_u64()?).ok()?,
        };
        let text = |name: &str| -> Option<Option<String>> {
            match fields.get(name) {
                None | Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s.clone())),
                Some(_) => None,
            }
        };
        let client_id = match fields.get("client_id") {
            None | Some(Value::Null) => None,
            Some(v) => Some(u32::try_from(v.as_u64()?).ok()?),
        };
        let bulk_credential = match text("bulk_credential")? {
            None => None,
            Some(s) => Some(hex::decode::<32>(&s)?),
        };
        Some(Self {
            ok,
            proto,
            reason: text("reason")?.map(|r| RejectReason::parse(&r)),
            error: text("error")?,
            client_id,
            bulk_credential,
            ext: text("ext")?,
        })
    }
}

/// A Control frame the dialer reads after message 1.
#[derive(Debug)]
pub(crate) enum Reply {
    Challenge(Greeting),
    Hello(Greeting),
    Ack(Ack),
}

pub(crate) fn parse_reply(payload: &[u8]) -> Option<Reply> {
    let Ok(Value::Object(top)) = serde_json::from_slice::<Value>(payload) else {
        return None;
    };
    if top.contains_key("ok") {
        return Ack::parse(&top).map(Reply::Ack);
    }
    if let Some(Value::Object(fields)) = top.get("challenge") {
        return Greeting::parse(fields, true).map(Reply::Challenge);
    }
    if let Some(Value::Object(fields)) = top.get("hello") {
        return Greeting::parse(fields, false).map(Reply::Hello);
    }
    None
}

pub(crate) fn encode_proof(sig: &Signature) -> Vec<u8> {
    json!({ "proof": { "sig": sig.to_string() } })
        .to_string()
        .into_bytes()
}

/// The proof's signature; `None` for anything that is not a proof.
pub(crate) fn parse_proof(payload: &[u8]) -> Option<Signature> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let sig = value.get("proof")?.get("sig")?.as_str()?;
    Signature::from_str(sig).ok()
}

/// Who signs the transcript.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Signer {
    Dialer,
    Acceptor,
}

/// The fields both ends sign on the SSH path (docs/design/handshake.md "흐름 (SSH 경로)").
pub(crate) struct Transcript<'a> {
    pub purpose: Purpose,
    pub holder: Option<Holder>,
    pub dialer_id: DeviceId,
    pub acceptor_id: DeviceId,
    pub dialer_instance: &'a str,
    pub acceptor_instance: &'a str,
    pub dialer_nonce: [u8; 32],
    pub acceptor_nonce: [u8; 32],
    pub dialer_caps: &'a [String],
    pub acceptor_caps: &'a [String],
    pub ext: Option<&'a str>,
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_caps(out: &mut Vec<u8>, caps: &[String]) {
    let mut sorted: Vec<&[u8]> = caps.iter().map(|c| c.as_bytes()).collect();
    sorted.sort_unstable();
    let count = u32::try_from(sorted.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for name in sorted {
        put(out, name);
    }
}

impl Transcript<'_> {
    /// The SHA-256 of the transcript as `signer` signs it.
    pub(crate) fn digest(&self, signer: Signer) -> [u8; 32] {
        let mut out = Vec::new();
        put(&mut out, TRANSCRIPT_DOMAIN.as_bytes());
        put(
            &mut out,
            match signer {
                Signer::Dialer => b"dialer",
                Signer::Acceptor => b"acceptor",
            },
        );
        put(&mut out, &PROTO.to_be_bytes());
        put(&mut out, self.purpose.as_str().as_bytes());
        put(
            &mut out,
            self.holder.map(Holder::as_str).unwrap_or("").as_bytes(),
        );
        put(&mut out, self.dialer_id.as_bytes());
        put(&mut out, self.acceptor_id.as_bytes());
        put(&mut out, self.dialer_instance.as_bytes());
        put(&mut out, self.acceptor_instance.as_bytes());
        put(&mut out, &self.dialer_nonce);
        put(&mut out, &self.acceptor_nonce);
        put_caps(&mut out, self.dialer_caps);
        put_caps(&mut out, self.acceptor_caps);
        put(&mut out, self.ext.unwrap_or("").as_bytes());
        Sha256::digest(&out).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasty_net_identity::DeviceKey;

    fn token() -> ConnectionToken {
        ConnectionToken::from_str(&"ab".repeat(32)).expect("token")
    }

    fn key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    #[test]
    fn the_v1_line_of_tasty_is_legacy_not_malformed() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"stream.open","params":{"proto":1,"target_workspace":1}}"#;
        assert_eq!(
            parse_open_line(line).err(),
            Some(RejectReason::LegacyProtocol)
        );
    }

    #[test]
    fn proto_values_are_sorted_into_malformed_and_unsupported() {
        let with = |proto: &str| {
            format!(r#"{{"method":"stream.open","params":{{"proto":{proto}}}}}"#).into_bytes()
        };
        for bad in ["\"2\"", "-1", "2.5", "4294967296", "null", "true"] {
            assert_eq!(
                parse_open_line(&with(bad)).err(),
                Some(RejectReason::Malformed),
                "{bad}"
            );
        }
        assert_eq!(
            parse_open_line(br#"{"method":"stream.open","params":{}}"#).err(),
            Some(RejectReason::Malformed),
            "no proto"
        );
        for other in ["0", "3", "4294967295"] {
            assert_eq!(
                parse_open_line(&with(other)).err(),
                Some(RejectReason::UnsupportedProtocol),
                "{other}"
            );
        }
        assert!(parse_open_line(&with("2")).is_ok());
        assert_eq!(
            parse_open_line(b"not json").err(),
            Some(RejectReason::Malformed)
        );
        assert_eq!(
            parse_open_line(br#"{"method":"workspace.list","params":{"proto":2}}"#).err(),
            Some(RejectReason::Malformed)
        );
    }

    #[test]
    fn a_line_over_the_limit_is_malformed() {
        let mut line = br#"{"method":"stream.open","params":{"proto":2,"ext":""#.to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_HANDSHAKE_LINE));
        line.extend_from_slice(br#""}}"#);
        assert_eq!(parse_open_line(&line).err(), Some(RejectReason::Malformed));
    }

    /// A message 1 of exactly `len` bytes before its line break.
    fn line_of(len: usize) -> Vec<u8> {
        let head = br#"{"method":"stream.open","params":{"proto":2,"ext":""#;
        let tail = br#""}}"#;
        let mut line = head.to_vec();
        line.extend(std::iter::repeat_n(b'a', len - head.len() - tail.len()));
        line.extend_from_slice(tail);
        assert_eq!(line.len(), len);
        line
    }

    #[test]
    fn the_line_limit_counts_the_line_break() {
        let fits = line_of(MAX_HANDSHAKE_LINE - 1);
        assert!(parse_open_line(&fits).is_ok());
        let mut fits_with_break = fits.clone();
        fits_with_break.push(b'\n');
        assert!(parse_open_line(&fits_with_break).is_ok());
        let over = line_of(MAX_HANDSHAKE_LINE);
        assert_eq!(parse_open_line(&over).err(), Some(RejectReason::Malformed));
        let mut over_with_break = over;
        over_with_break.push(b'\n');
        assert_eq!(
            parse_open_line(&over_with_break).err(),
            Some(RejectReason::Malformed)
        );
    }

    #[test]
    fn token_checks_tell_missing_from_invalid() {
        let t = token();
        let params = |v: Value| -> Map<String, Value> {
            let mut m = Map::new();
            m.insert("token".into(), v);
            m
        };
        assert_eq!(
            check_token(&Map::new(), Some(&t)),
            Err(RejectReason::TokenMissing)
        );
        assert_eq!(
            check_token(&params(Value::Null), Some(&t)),
            Err(RejectReason::TokenInvalid),
            "null is a value that is not the token"
        );
        assert_eq!(
            check_token(&params(json!("cd".repeat(32))), Some(&t)),
            Err(RejectReason::TokenInvalid)
        );
        assert_eq!(
            check_token(&params(json!("short")), Some(&t)),
            Err(RejectReason::TokenInvalid)
        );
        assert_eq!(
            check_token(&params(json!(7)), Some(&t)),
            Err(RejectReason::TokenInvalid)
        );
        assert_eq!(
            check_token(&params(json!(t.expose_hex())), None),
            Err(RejectReason::TokenInvalid),
            "an acceptor without a token takes no SSH channel"
        );
        assert_eq!(
            check_token(&params(json!(t.expose_hex())), Some(&t)),
            Ok(())
        );
    }

    fn attach_out<'a>(caps: &'a [String], t: &'a ConnectionToken) -> OpenOut<'a> {
        OpenOut {
            purpose: Purpose::Attach,
            token: Some(t),
            device_id: Some(key(1).id()),
            instance_id: "dialer-1",
            nonce: Some([7; 32]),
            holder: Some(Holder::Dialer),
            caps: Some(caps),
            ext: Some(r#"{"target_workspace":1}"#),
            bulk_credential: None,
        }
    }

    #[test]
    fn message_one_round_trips_through_the_checks() {
        let t = token();
        let caps = vec!["net.request".to_string()];
        let line = attach_out(&caps, &t).line();
        assert_eq!(line.last(), Some(&b'\n'));
        let params = parse_open_line(&line).expect("step 1");
        check_token(&params, Some(&t)).expect("step 2");
        let open = parse_fields(&params, Path::Ssh).expect("step 3");
        assert_eq!(open.purpose, Purpose::Attach);
        assert_eq!(open.device_id, Some(key(1).id()));
        assert_eq!(open.instance_id, "dialer-1");
        assert_eq!(open.nonce, Some([7; 32]));
        assert_eq!(open.holder, Some(Holder::Dialer));
        assert_eq!(open.caps, caps);
        assert_eq!(open.ext.as_deref(), Some(r#"{"target_workspace":1}"#));
    }

    #[test]
    fn a_field_of_the_wrong_type_is_malformed() {
        let t = token();
        let caps = vec!["net.request".to_string()];
        let line = attach_out(&caps, &t).line();
        let base = parse_open_line(&line).expect("step 1");
        let cases: [(&str, Value); 9] = [
            ("purpose", json!("watch")),
            ("instance_id", json!(5)),
            ("instance_id", json!("")),
            ("device_id", json!("not-a-key")),
            ("nonce", json!("00")),
            ("holder", json!("both")),
            ("caps", json!("net.request")),
            ("caps", json!(["Net"])),
            ("ext", json!({"target": 1})),
        ];
        for (name, value) in cases {
            let mut params = base.clone();
            params.insert(name.into(), value.clone());
            assert_eq!(
                parse_fields(&params, Path::Ssh).err(),
                Some(RejectReason::Malformed),
                "{name} = {value}"
            );
        }
        let mut twice = base.clone();
        twice.insert("caps".into(), json!(["net.request", "net.request"]));
        assert_eq!(
            parse_fields(&twice, Path::Ssh).err(),
            Some(RejectReason::Malformed)
        );
        let mut missing = base;
        missing.remove("holder");
        assert_eq!(
            parse_fields(&missing, Path::Ssh).err(),
            Some(RejectReason::Malformed)
        );
    }

    #[test]
    fn a_p2p_device_id_must_be_the_remote_id() {
        let t = token();
        let caps = vec![];
        let line = attach_out(&caps, &t).line();
        let params = parse_open_line(&line).expect("step 1");
        let other = Path::P2p {
            remote_id: key(2).id(),
            scope: 0,
        };
        assert_eq!(
            parse_fields(&params, other).err(),
            Some(RejectReason::Malformed)
        );
        let same = Path::P2p {
            remote_id: key(1).id(),
            scope: 0,
        };
        let open = parse_fields(&params, same).expect("same device");
        assert_eq!(open.nonce, None, "P2P takes no nonce");
    }

    #[test]
    fn capability_names_follow_the_grammar() {
        for good in [
            "net.request",
            "net.topology",
            "a",
            "0-x_y.z",
            &"a".repeat(64),
        ] {
            assert!(is_valid_capability(good), "{good}");
        }
        for bad in [
            "",
            "Net",
            ".net",
            "-x",
            "net request",
            "net\n",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_capability(bad), "{bad:?}");
        }
    }

    #[test]
    fn replies_are_told_apart_by_their_key() {
        let g = Greeting {
            device_id: key(3).id(),
            instance_id: "acc".into(),
            caps: vec!["net.request".into()],
            nonce: Some([1; 32]),
            sig: Some(key(3).sign(b"x")),
        };
        match parse_reply(&g.encode("challenge")) {
            Some(Reply::Challenge(back)) => assert_eq!(back, g),
            other => panic!("{other:?}"),
        }
        let hello = Greeting {
            nonce: None,
            sig: None,
            ..g.clone()
        };
        match parse_reply(&hello.encode("hello")) {
            Some(Reply::Hello(back)) => assert_eq!(back, hello),
            other => panic!("{other:?}"),
        }
        let ack = Ack {
            ok: true,
            proto: PROTO,
            client_id: Some(9),
            bulk_credential: Some([4; 32]),
            ext: Some("{}".into()),
            ..Ack::default()
        };
        match parse_reply(&ack.encode()) {
            Some(Reply::Ack(back)) => assert_eq!(back, ack),
            other => panic!("{other:?}"),
        }
        // The v1 refusal of tasty 698aa49e03.
        let v1 =
            br#"{"ok":false,"proto":1,"error":"unsupported stream proto 2 (server speaks 1)"}"#;
        match parse_reply(v1) {
            Some(Reply::Ack(a)) => {
                assert!(!a.ok);
                assert_eq!(a.proto, 1);
                assert_eq!(a.reason, None);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_reply(b"{}").is_none());
        let sig = key(1).sign(b"y");
        assert_eq!(parse_proof(&encode_proof(&sig)), Some(sig));
        assert_eq!(parse_proof(b"{\"proof\":{}}"), None);
    }

    #[test]
    fn a_refusal_ack_carries_ok_proto_reason_and_error() {
        let ack = Ack::refusal(&RejectReason::LegacyProtocol, None);
        let value: Value = serde_json::from_slice(&ack.encode()).expect("json");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["proto"], json!(2));
        assert_eq!(value["reason"], json!("legacy_protocol"));
        assert_eq!(value["error"], json!(LEGACY_PROTOCOL_ERROR));
    }

    fn transcript<'a>(caps_d: &'a [String], caps_a: &'a [String]) -> Transcript<'a> {
        Transcript {
            purpose: Purpose::Attach,
            holder: Some(Holder::Dialer),
            dialer_id: key(1).id(),
            acceptor_id: key(2).id(),
            dialer_instance: "d",
            acceptor_instance: "a",
            dialer_nonce: [1; 32],
            acceptor_nonce: [2; 32],
            dialer_caps: caps_d,
            acceptor_caps: caps_a,
            ext: Some(r#"{"a":1}"#),
        }
    }

    #[test]
    fn the_transcript_binds_signer_fields_and_sorts_caps() {
        let ab = vec!["b".to_string(), "a".to_string()];
        let ba = vec!["a".to_string(), "b".to_string()];
        let none: Vec<String> = vec![];
        let t = transcript(&ab, &none);
        assert_ne!(t.digest(Signer::Dialer), t.digest(Signer::Acceptor));
        assert_eq!(
            t.digest(Signer::Dialer),
            transcript(&ba, &none).digest(Signer::Dialer),
            "caps are sorted before hashing"
        );
        // Moving a name from one list to the other changes the digest.
        let a = vec!["a".to_string()];
        let b = vec!["b".to_string()];
        assert_ne!(
            transcript(&a, &b).digest(Signer::Dialer),
            transcript(&ab, &none).digest(Signer::Dialer)
        );
        let mut other_ext = transcript(&ab, &none);
        other_ext.ext = Some(r#"{"a": 1}"#);
        assert_ne!(other_ext.digest(Signer::Dialer), t.digest(Signer::Dialer));
        let mut other_holder = transcript(&ab, &none);
        other_holder.holder = Some(Holder::Acceptor);
        assert_ne!(
            other_holder.digest(Signer::Dialer),
            t.digest(Signer::Dialer)
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A known answer: the digest of fixed fields, computed apart from this code (Python
    /// `hashlib` over the layout handshake.md gives, with the caps list as a count and
    /// length-prefixed names, not wrapped again in the list's total length).
    #[test]
    fn the_transcript_digest_matches_a_known_answer() {
        assert_eq!(
            hex(key(1).id().as_bytes()),
            "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c",
            "the input device ID"
        );
        let ab = vec!["b".to_string(), "a".to_string()];
        let none: Vec<String> = vec![];
        assert_eq!(
            hex(&transcript(&ab, &none).digest(Signer::Dialer)),
            "688a0c7bf24b1213237c7219649b33924aa002f59b692432a6b4bc25a76fc266"
        );
    }
}
