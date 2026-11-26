//! The request channel: `{id, method, params}` requests and `{id, result}` or `{id, error}`
//! responses in Control frames after handshake v2 (docs/design/transport.md "요청 채널").
//! This crate defines no method; the accepting consumer keeps the allow list.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use tasty_net_frame::{ReadFailure, StreamTag};

use crate::channel::{ChannelReader, ChannelWriter, ReadError};
use crate::end::LinkCloser;
use crate::link::PeerInfo;
use crate::lock::lock;

/// The accepting consumer's request handling.
pub trait RequestHandler: Send + Sync {
    /// Answers one request: `Ok` becomes `{id, result}`, `Err` becomes `{id, error}`. A
    /// method the consumer does not allow should be an `Err`.
    fn handle(&self, peer: &PeerInfo, method: &str, params: Value) -> Result<Value, Value>;
}

/// Why a call gave no result.
#[derive(Debug)]
#[non_exhaustive]
pub enum RequestError {
    /// The peer answered with `{id, error}`; this is the `error` value.
    Remote(Value),
    /// No answer came within the call's timeout. The channel stays usable.
    TimedOut,
    /// The channel ended before the answer.
    Closed(ReadFailure),
    /// The request could not be written.
    Io(io::Error),
    /// The peer's answer had neither `result` nor `error`, or both.
    Protocol(&'static str),
    /// The peer's answer was longer than one frame can carry; the peer answered with
    /// [`RESPONSE_TOO_LARGE`] instead.
    ResponseTooLarge,
}

/// The `error.code` a [`RequestServer`] answers with when its answer does not fit in one
/// frame ([`tasty_net_frame::MAX_FRAME_LEN`]). The client reports it as
/// [`RequestError::ResponseTooLarge`].
pub const RESPONSE_TOO_LARGE: &str = "net.response_too_large";

/// Why [`RequestServer::serve`] stopped before the peer closed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServeError {
    /// Reading the next request failed.
    Read(ReadError),
    /// Writing an answer failed.
    Write(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "reading a request failed: {e}"),
            Self::Write(_) => f.write_str("writing an answer failed"),
        }
    }
}

impl Error for ServeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Write(e) => Some(e),
        }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote(_) => f.write_str("the peer answered the request with an error"),
            Self::TimedOut => f.write_str("the request got no answer in time"),
            Self::Closed(failure) => write!(f, "the request channel ended ({failure})"),
            Self::Io(_) => f.write_str("the request could not be written"),
            Self::Protocol(what) => write!(f, "the peer's answer is not a valid answer: {what}"),
            Self::ResponseTooLarge => f.write_str("the peer's answer did not fit in one frame"),
        }
    }
}

impl Error for RequestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

type Answer = Result<Value, RequestError>;

#[derive(Default)]
struct Pending {
    waiting: HashMap<u64, Sender<Answer>>,
    /// Set once the reader ended; later calls fail at once.
    ended: Option<ReadFailure>,
}

/// The dialer side of a request channel. Calls may come from several threads; a reader
/// thread routes each answer to its call by `id`. Dropping the client closes its sending
/// direction, waits up to the drain cap for the peer's last answers, and joins the reader
/// thread.
pub struct RequestClient {
    writer: Mutex<ChannelWriter>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    reader: Option<JoinHandle<()>>,
    closer: LinkCloser,
    peer: PeerInfo,
}

impl fmt::Debug for RequestClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestClient")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl RequestClient {
    pub(crate) fn start(
        reader: ChannelReader,
        writer: ChannelWriter,
        closer: LinkCloser,
        peer: PeerInfo,
    ) -> io::Result<Self> {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let routes = Arc::clone(&pending);
        let reader = std::thread::Builder::new()
            .name("tasty-net-request".into())
            .spawn(move || route_answers(reader, &routes))?;
        Ok(Self {
            writer: Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            reader: Some(reader),
            closer,
            peer,
        })
    }

    /// Sends `method` with `params` and waits up to `timeout` for its answer.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Answer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = lock(&self.pending);
            if let Some(failure) = pending.ended {
                return Err(RequestError::Closed(failure));
            }
            pending.waiting.insert(id, tx);
        }
        let request = json!({ "id": id, "method": method, "params": params });
        let written =
            lock(&self.writer).write_frame(StreamTag::Control, request.to_string().as_bytes());
        if let Err(e) = written {
            lock(&self.pending).waiting.remove(&id);
            return Err(RequestError::Io(e));
        }
        match rx.recv_timeout(timeout) {
            Ok(answer) => answer,
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.pending).waiting.remove(&id);
                Err(RequestError::TimedOut)
            }
            Err(RecvTimeoutError::Disconnected) => Err(RequestError::Closed(
                lock(&self.pending).ended.unwrap_or(ReadFailure::PeerClosed),
            )),
        }
    }

    /// The peer.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// The closer of the channel.
    pub fn closer(&self) -> LinkCloser {
        self.closer.clone()
    }
}

fn route_answers(mut reader: ChannelReader, pending: &Mutex<Pending>) {
    let failure = loop {
        match reader.read_frame() {
            Ok(frame) if frame.tag == StreamTag::Control => {
                let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) else {
                    tracing::debug!("request channel: an answer that is not JSON was ignored");
                    continue;
                };
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let answer = match (value.get("result"), value.get("error")) {
                    (None, Some(error))
                        if error.get("code").and_then(Value::as_str)
                            == Some(RESPONSE_TOO_LARGE) =>
                    {
                        Err(RequestError::ResponseTooLarge)
                    }
                    (None, Some(error)) => Err(RequestError::Remote(error.clone())),
                    (Some(result), None) => Ok(result.clone()),
                    (Some(_), Some(_)) => Err(RequestError::Protocol(
                        "an answer with both result and error",
                    )),
                    (None, None) => {
                        Err(RequestError::Protocol("an answer without result or error"))
                    }
                };
                if let Some(tx) = lock(pending).waiting.remove(&id) {
                    let _delivered = tx.send(answer).is_ok();
                }
            }
            Ok(frame) if frame.tag == StreamTag::Detach => break ReadFailure::PeerClosed,
            Ok(_) => {}
            Err(e) => break e.failure(),
        }
    };
    let mut pending = lock(pending);
    pending.ended = Some(failure);
    for (_, tx) in pending.waiting.drain() {
        let _delivered = tx.send(Err(RequestError::Closed(failure))).is_ok();
    }
}

impl Drop for RequestClient {
    fn drop(&mut self) {
        lock(&self.writer).finish();
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            tracing::warn!("the request reader thread panicked");
        }
    }
}

/// The accepting side of a request channel.
pub struct RequestServer {
    pub(crate) reader: ChannelReader,
    pub(crate) writer: ChannelWriter,
    pub(crate) closer: LinkCloser,
    pub(crate) peer: PeerInfo,
}

impl fmt::Debug for RequestServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestServer")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl RequestServer {
    /// The peer.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// The closer of the channel.
    pub fn closer(&self) -> LinkCloser {
        self.closer.clone()
    }

    /// Answers requests in order until the peer closes its sending direction, then closes
    /// this end after the last answer. A request without a numeric `id` is ignored; one
    /// without a string `method` gets `{"code": "invalid_request"}`; an answer longer than
    /// one frame is replaced by `{"code": RESPONSE_TOO_LARGE}`. A failed read (other than
    /// the peer's close) or a failed answer write ends it with that error.
    pub fn serve(mut self, handler: &dyn RequestHandler) -> Result<(), ServeError> {
        loop {
            match self.reader.read_frame() {
                Ok(frame) if frame.tag == StreamTag::Control => {
                    let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) else {
                        continue;
                    };
                    let Some(id) = value.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let answer = match value.get("method").and_then(Value::as_str) {
                        Some(method) => {
                            let params = value.get("params").cloned().unwrap_or(Value::Null);
                            match handler.handle(&self.peer, method, params) {
                                Ok(result) => json!({ "id": id, "result": result }),
                                Err(error) => json!({ "id": id, "error": error }),
                            }
                        }
                        None => json!({ "id": id, "error": { "code": "invalid_request" } }),
                    };
                    match self
                        .writer
                        .write_frame(StreamTag::Control, answer.to_string().as_bytes())
                    {
                        Ok(()) => {}
                        // Refused before any byte went out: the answer is longer than a frame.
                        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                            let instead =
                                json!({ "id": id, "error": { "code": RESPONSE_TOO_LARGE } });
                            self.writer
                                .write_frame(StreamTag::Control, instead.to_string().as_bytes())
                                .map_err(ServeError::Write)?;
                        }
                        Err(e) => return Err(ServeError::Write(e)),
                    }
                }
                Ok(frame) if frame.tag == StreamTag::Detach => break,
                Ok(_) => {}
                Err(e) if e.failure() == ReadFailure::PeerClosed => break,
                Err(e) => return Err(ServeError::Read(e)),
            }
        }
        if let Err(error) = self.writer.half_close() {
            tracing::debug!(%error, "request channel: closing the sending direction failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelConfig, RawChannel};
    use crate::memory::memory_channel_pair;
    use crate::role::Side;
    use crate::transport::TransportKind;
    use tasty_net_frame::{read_frame, write_frame};
    use tasty_net_identity::DeviceKey;

    fn peer() -> PeerInfo {
        PeerInfo {
            device_id: DeviceKey::from_seed([1; 32]).id(),
            instance_id: "peer".into(),
            holder: None,
            caps: Vec::new(),
            transport: TransportKind::Memory,
            local_side: Side::Dialer,
        }
    }

    fn client_on(raw: RawChannel) -> RequestClient {
        let framed = raw.framed(&ChannelConfig::default()).expect("framed");
        RequestClient::start(framed.reader, framed.writer, framed.closer, peer()).expect("start")
    }

    /// Answers the first request on `raw` with `answer` (its `id` filled in).
    fn answer_once(mut raw: RawChannel, answer: Value) -> std::thread::JoinHandle<RawChannel> {
        std::thread::spawn(move || {
            let request = read_frame(&mut raw).expect("request");
            let request: Value = serde_json::from_slice(&request.payload).expect("json");
            let mut answer = answer;
            answer["id"] = request["id"].clone();
            write_frame(&mut raw, StreamTag::Control, answer.to_string().as_bytes())
                .expect("answer");
            raw
        })
    }

    #[test]
    fn an_answer_needs_exactly_one_of_result_and_error() {
        for (answer, both) in [
            (json!({}), false),
            (json!({"result": 1, "error": {"code": "x"}}), true),
        ] {
            let (a, b) = memory_channel_pair();
            let client = client_on(a);
            let peer = answer_once(b, answer);
            match client.call("m", Value::Null, Duration::from_secs(5)) {
                Err(RequestError::Protocol(what)) => {
                    assert_eq!(what.contains("both"), both, "{what}");
                }
                other => panic!("{other:?}"),
            }
            drop(peer.join().expect("peer"));
        }
    }

    struct Big;

    impl RequestHandler for Big {
        fn handle(&self, _peer: &PeerInfo, _method: &str, _params: Value) -> Result<Value, Value> {
            let len = usize::try_from(tasty_net_frame::MAX_FRAME_LEN).expect("usize");
            Ok(Value::String("x".repeat(len)))
        }
    }

    #[test]
    fn an_answer_longer_than_a_frame_is_told_to_the_caller() {
        let (a, b) = memory_channel_pair();
        let client = client_on(a);
        let framed = b.framed(&ChannelConfig::default()).expect("framed");
        let server = RequestServer {
            reader: framed.reader,
            writer: framed.writer,
            closer: framed.closer,
            peer: peer(),
        };
        let serving = std::thread::spawn(move || server.serve(&Big));
        assert!(matches!(
            client.call("m", Value::Null, Duration::from_secs(5)),
            Err(RequestError::ResponseTooLarge)
        ));
        drop(client);
        serving.join().expect("server").expect("served to the end");
    }

    #[test]
    fn a_failed_answer_write_ends_serve_with_its_error() {
        let (mut a, b) = memory_channel_pair();
        let framed = b.framed(&ChannelConfig::default()).expect("framed");
        let server = RequestServer {
            reader: framed.reader,
            writer: framed.writer,
            closer: framed.closer,
            peer: peer(),
        };
        let request = json!({"id": 1, "method": "m", "params": null});
        write_frame(&mut a, StreamTag::Control, request.to_string().as_bytes()).expect("request");
        // The peer goes away entirely: the request is buffered, the answer has nowhere to go.
        drop(a);
        assert!(matches!(server.serve(&Big), Err(ServeError::Write(_))));
    }
}
