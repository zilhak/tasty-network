//! Links and channels after handshake v2 (android #7, #7+): the ordered channel of an attach
//! link, bulk channels, request channels, frame reads and writes, closing.
//!
//! Every call that waits (reading a frame, a request call, opening a bulk channel, finishing)
//! blocks the calling thread; call it off the UI thread. Reading and writing may run on two
//! threads at once.
//!
//! An attach link lives while any of its objects lives: the [`AttachLink`], its ordered
//! [`FrameChannel`], or a [`BulkChannel`] under it with that bulk channel's [`FrameChannel`].
//! Each of them holds the core link, so dropping the `AttachLink` (Swift frees it at once in
//! `try net.openAttach(...).ordered()`, Kotlin when the collector runs) does not end the
//! channels the app still uses. The link ends when the last of them is freed, or earlier by
//! [`FrameChannel::finish`] or a `force_close`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tasty_net_frame::StreamTag;
use tasty_net_transport::{
    AttemptCancel, BulkChannel as CoreBulk, ChannelReader, ChannelWriter, EndReason, Finish, Link,
    LinkCloser, PeerInfo, RequestClient, RequestError,
};

use crate::error::{ChannelFailure, HandshakeFailure, ReadEnd, RequestFailure};
use crate::records::{ChannelTransport, LinkHolder, LocalSide};
use crate::secrets::Secrets;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The frame types of the stream protocol (`StreamTag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FrameTag {
    /// Terminal bytes or another stream payload.
    Data,
    /// A JSON control message.
    Control,
    /// A heartbeat. The channel sends heartbeats by itself and
    /// [`FrameChannel::read_frame`] never returns one: a received Ping only renews the read
    /// timeout. The app does not need to write one.
    Ping,
    /// The end of an attach.
    Detach,
    /// A mesh data chunk.
    MeshData,
}

impl From<StreamTag> for FrameTag {
    fn from(tag: StreamTag) -> Self {
        match tag {
            StreamTag::Data => Self::Data,
            StreamTag::Control => Self::Control,
            StreamTag::Ping => Self::Ping,
            StreamTag::Detach => Self::Detach,
            StreamTag::MeshData => Self::MeshData,
        }
    }
}

impl From<FrameTag> for StreamTag {
    fn from(tag: FrameTag) -> Self {
        match tag {
            FrameTag::Data => Self::Data,
            FrameTag::Control => Self::Control,
            FrameTag::Ping => Self::Ping,
            FrameTag::Detach => Self::Detach,
            FrameTag::MeshData => Self::MeshData,
        }
    }
}

/// One frame read from a channel. The library does not read the payload; its meaning is the
/// app's and tasty's.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Frame {
    /// The frame type.
    pub tag: FrameTag,
    /// The payload bytes.
    pub payload: Vec<u8>,
}

/// How [`FrameChannel::finish`] ended (`Finish`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FinishResult {
    /// The peer ended its side within the drain cap.
    ReaderEnded,
    /// The drain cap ran out and the link was closed both ways.
    DrainCapReached,
    /// The channel was finished or closed before.
    AlreadyFinished,
}

/// The peer of a link or channel as the handshake proved it (`PeerInfo`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PeerRecord {
    /// The peer's device ID.
    pub device_id: String,
    /// The peer's instance ID.
    pub instance_id: String,
    /// Which end holds the workspace (none on a bulk or request channel).
    pub holder: Option<LinkHolder>,
    /// The capability names the peer offered in its handshake (such as `net.request`).
    pub caps: Vec<String>,
    /// Which transport the channel came from.
    pub transport: ChannelTransport,
    /// Whether this device opened it.
    pub local_side: LocalSide,
    /// Whether this device holds the workspace on an attach link.
    pub local_holds: bool,
}

impl PeerRecord {
    /// The record with the texts the peer chose (its instance ID and capability names)
    /// scrubbed of this end's tokens. A peer that took the token from the dialer's first
    /// message could put it there and still prove its key.
    pub(crate) fn scrubbed(self, secrets: &Secrets) -> Self {
        Self {
            instance_id: secrets.scrub(&self.instance_id),
            caps: self.caps.iter().map(|c| secrets.scrub(c)).collect(),
            ..self
        }
    }
}

impl From<&PeerInfo> for PeerRecord {
    fn from(peer: &PeerInfo) -> Self {
        Self {
            device_id: peer.device_id().to_string(),
            instance_id: peer.instance_id().to_string(),
            holder: peer.holder().map(LinkHolder::from),
            caps: peer.caps().to_vec(),
            transport: peer.transport().into(),
            local_side: peer.local_side().into(),
            local_holds: peer.local_holds(),
        }
    }
}

/// Cancels a connection attempt from another thread: [`CancelHandle::cancel`] makes the
/// blocked open return `HandshakeFailure::Cancelled`. One handle is for one attempt.
#[derive(Debug, Default, uniffi::Object)]
pub struct CancelHandle {
    pub(crate) inner: AttemptCancel,
}

#[uniffi::export]
impl CancelHandle {
    /// A new handle.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Cancels the attempt this handle was given to.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether [`CancelHandle::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

/// The core attach link, shared by every object of the link. Dropping the last holder drops
/// the core link, which closes every channel of it that did not end normally.
pub(crate) struct LinkOwner(Mutex<Link>);

/// Shows nothing of the core link: its `Debug` carries the peer as the handshake gave it,
/// before the token scrub.
impl std::fmt::Debug for LinkOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkOwner").finish_non_exhaustive()
    }
}

/// A framed channel: the ordered channel of an attach link or a bulk channel.
#[derive(Debug, uniffi::Object)]
pub struct FrameChannel {
    reader: Mutex<Option<ChannelReader>>,
    writer: Mutex<Option<ChannelWriter>>,
    closer: LinkCloser,
    /// Keeps the attach link alive while this channel is used.
    _owner: Arc<LinkOwner>,
}

impl FrameChannel {
    fn new(
        reader: ChannelReader,
        writer: ChannelWriter,
        closer: LinkCloser,
        owner: Arc<LinkOwner>,
    ) -> Self {
        Self {
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(Some(writer)),
            closer,
            _owner: owner,
        }
    }
}

#[uniffi::export]
impl FrameChannel {
    /// Waits for the next frame that is not a Ping (the core `ChannelReader::read_frame`
    /// skips Pings; they only renew the read timeout). After one failure every later read
    /// returns the same failure.
    pub fn read_frame(&self) -> Result<Frame, ChannelFailure> {
        let mut reader = lock(&self.reader);
        let reader = reader.as_mut().ok_or(ChannelFailure::AlreadyClosed)?;
        match reader.read_frame() {
            Ok(frame) => Ok(Frame {
                tag: frame.tag.into(),
                payload: frame.payload,
            }),
            Err(e) => Err((&e).into()),
        }
    }

    /// Writes one frame and flushes.
    pub fn write_frame(&self, tag: FrameTag, payload: Vec<u8>) -> Result<(), ChannelFailure> {
        let writer = lock(&self.writer);
        let writer = writer.as_ref().ok_or(ChannelFailure::AlreadyClosed)?;
        writer
            .write_frame(tag.into(), &payload)
            .map_err(|e| ChannelFailure::Write {
                detail: e.to_string(),
            })
    }

    /// Closes the sending direction after the frames written so far. Later writes fail.
    pub fn half_close(&self) -> Result<(), ChannelFailure> {
        let mut writer = lock(&self.writer);
        let writer = writer.as_mut().ok_or(ChannelFailure::AlreadyClosed)?;
        writer.half_close().map_err(|e| ChannelFailure::Write {
            detail: e.to_string(),
        })
    }

    /// Marks the reading half as ended without waiting for the peer, after the app read a
    /// frame that ends the channel for it (a `Detach`, or a `Control` it reads as the end) and
    /// reads nothing more (the core `ChannelReader::finish`). Later reads fail with
    /// `AlreadyClosed`. Call it before [`FrameChannel::finish`] so that the normal end does
    /// not wait out the drain cap for a reader that stopped. A read blocked on another thread
    /// holds the reading half; this call then waits for that read to return.
    pub fn finish_reading(&self) {
        if let Some(mut reader) = lock(&self.reader).take() {
            reader.finish();
        }
    }

    /// The normal end: closes the sending direction and waits up to the drain cap for the
    /// peer to end its side (the reading thread sees the end first, or
    /// [`FrameChannel::finish_reading`] marked it).
    pub fn finish(&self) -> FinishResult {
        let Some(mut writer) = lock(&self.writer).take() else {
            return FinishResult::AlreadyFinished;
        };
        match writer.finish() {
            Finish::ReaderEnded => FinishResult::ReaderEnded,
            Finish::DrainCapReached { .. } => FinishResult::DrainCapReached,
        }
    }

    /// Closes the link both ways at once; a blocked read returns. (Not `close`: the
    /// generated Kotlin object already has `close()`, which frees the object.)
    pub fn force_close(&self) {
        self.closer.force_close(EndReason::Forced);
    }
}

/// An attach link: handshake v2 succeeded and the ordered channel is open. Its channels keep
/// the link alive on their own (see the module documentation).
#[derive(Debug, uniffi::Object)]
pub struct AttachLink {
    /// Held for opening bulk channels; the values below are copied out of it.
    link: Arc<LinkOwner>,
    closer: LinkCloser,
    ordered: Arc<FrameChannel>,
    peer: PeerRecord,
    link_id: String,
    client_id: Option<u32>,
    ext: Option<String>,
    secrets: Secrets,
}

impl AttachLink {
    /// The FFI link over `link`; the peer's texts are scrubbed of the tokens in `secrets`.
    pub(crate) fn new(mut link: Link, secrets: Secrets) -> Result<Self, HandshakeFailure> {
        let (reader, writer) = link.take_ordered().ok_or(HandshakeFailure::AlreadyClosed)?;
        let closer = link.closer();
        let peer = PeerRecord::from(link.peer()).scrubbed(&secrets);
        let link_id = link.link_id().to_hex();
        let client_id = link.client_id();
        let ext = secrets.scrub_opt(link.ext().map(str::to_string));
        let owner = Arc::new(LinkOwner(Mutex::new(link)));
        let ordered = Arc::new(FrameChannel::new(
            reader,
            writer,
            closer.clone(),
            Arc::clone(&owner),
        ));
        Ok(Self {
            peer,
            link_id,
            client_id,
            ext,
            link: owner,
            closer,
            ordered,
            secrets,
        })
    }
}

#[uniffi::export]
impl AttachLink {
    /// The ordered channel. Every call returns the same channel.
    pub fn ordered(&self) -> Arc<FrameChannel> {
        Arc::clone(&self.ordered)
    }

    /// The peer, with the capability names it offered.
    pub fn peer(&self) -> PeerRecord {
        self.peer.clone()
    }

    /// Whether the peer offered capability `name`.
    pub fn has_cap(&self, name: String) -> bool {
        self.peer.caps.contains(&name)
    }

    /// The link ID in hex, the same on both ends.
    pub fn link_id(&self) -> String {
        self.link_id.clone()
    }

    /// The number the accepting end gave the connection.
    pub fn client_id(&self) -> Option<u32> {
        self.client_id
    }

    /// The domain `ext` of the accepting end's answer.
    pub fn ext(&self) -> Option<String> {
        self.ext.clone()
    }

    /// Opens a bulk channel under this link (only the dialer of an attach link can). Opening
    /// two at once on one link runs them one after the other.
    pub fn open_bulk(&self, ext: Option<String>) -> Result<Arc<BulkChannel>, HandshakeFailure> {
        let bulk = lock(&self.link.0)
            .open_bulk(ext.as_deref())
            .map_err(|e| HandshakeFailure::from(e).scrubbed(&self.secrets))?;
        Ok(Arc::new(BulkChannel::new(
            bulk,
            Arc::clone(&self.link),
            &self.secrets,
        )))
    }

    /// Closes the link both ways at once.
    pub fn force_close(&self) {
        self.closer.force_close(EndReason::Forced);
    }
}

/// A bulk channel under an attach link. Its end is its own; the parent's end ends it too. It
/// keeps the parent link alive while it is used.
#[derive(Debug, uniffi::Object)]
pub struct BulkChannel {
    channel: Arc<FrameChannel>,
    peer: PeerRecord,
    ext: Option<String>,
}

impl BulkChannel {
    fn new(bulk: CoreBulk, parent: Arc<LinkOwner>, secrets: &Secrets) -> Self {
        let peer = PeerRecord::from(bulk.peer()).scrubbed(secrets);
        let ext = secrets.scrub_opt(bulk.ext().map(str::to_string));
        Self {
            channel: Arc::new(FrameChannel::new(
                bulk.reader,
                bulk.writer,
                bulk.closer,
                parent,
            )),
            peer,
            ext,
        }
    }
}

#[uniffi::export]
impl BulkChannel {
    /// The channel.
    pub fn channel(&self) -> Arc<FrameChannel> {
        Arc::clone(&self.channel)
    }

    /// The peer.
    pub fn peer(&self) -> PeerRecord {
        self.peer.clone()
    }

    /// The domain `ext` of the accepting end's answer.
    pub fn ext(&self) -> Option<String> {
        self.ext.clone()
    }
}

/// A request channel: calls with JSON parameters and results (`RequestClient`). Calls may come
/// from several threads.
///
/// The request channel opens without looking at the peer's capabilities; check
/// [`RequestChannel::has_cap`] (`net.request`) before the first call.
#[derive(uniffi::Object)]
pub struct RequestChannel {
    client: Mutex<Option<Arc<RequestClient>>>,
    closer: LinkCloser,
    peer: PeerRecord,
    secrets: Secrets,
    /// Set by [`RequestChannel::finish`] before it closes the channel, so that a call it
    /// woke in the middle of writing returns `Closed` and not `Io`.
    finished: AtomicBool,
}

/// Shows the scrubbed peer, not the core client: its `Debug` carries the peer as the
/// handshake gave it.
impl std::fmt::Debug for RequestChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestChannel")
            .field("peer", &self.peer)
            .field("finished", &self.finished.load(Ordering::SeqCst))
            .field("closer", &self.closer)
            .finish_non_exhaustive()
    }
}

impl RequestChannel {
    /// The FFI channel over `client`; the peer's error values are scrubbed of the tokens in
    /// `secrets`.
    pub(crate) fn new(client: RequestClient, secrets: Secrets) -> Self {
        let peer = PeerRecord::from(client.peer()).scrubbed(&secrets);
        let closer = client.closer();
        Self {
            client: Mutex::new(Some(Arc::new(client))),
            closer,
            peer,
            secrets,
            finished: AtomicBool::new(false),
        }
    }

    /// The FFI failure of a call. After [`RequestChannel::finish`] a write that failed is the
    /// end of the channel: the call returns `Closed`, as a call woken while waiting for its
    /// answer does.
    fn failure(&self, error: &RequestError) -> RequestFailure {
        let failure = RequestFailure::from(error).scrubbed(&self.secrets);
        if self.finished.load(Ordering::SeqCst) && matches!(failure, RequestFailure::Io { .. }) {
            return RequestFailure::Closed {
                end: ReadEnd::Other,
            };
        }
        failure
    }

    /// How many calls hold the channel now.
    #[cfg(test)]
    pub(crate) fn calls_running(&self) -> usize {
        lock(&self.client)
            .as_ref()
            .map_or(0, |client| Arc::strong_count(client) - 1)
    }
}

#[uniffi::export]
impl RequestChannel {
    /// Sends `method` with `params_json` (JSON text) and waits up to `timeout` for the
    /// answer; returns the `result` value as JSON text.
    pub fn call(
        &self,
        method: String,
        params_json: String,
        timeout: Duration,
    ) -> Result<String, RequestFailure> {
        let params: serde_json::Value =
            serde_json::from_str(&params_json).map_err(|e| RequestFailure::InvalidParams {
                detail: e.to_string(),
            })?;
        let client = lock(&self.client)
            .clone()
            .ok_or(RequestFailure::AlreadyClosed)?;
        match client.call(&method, params, timeout) {
            Ok(value) => Ok(value.to_string()),
            Err(e) => Err(self.failure(&e)),
        }
    }

    /// The peer, with the capability names it offered in its handshake.
    pub fn peer(&self) -> PeerRecord {
        self.peer.clone()
    }

    /// The capability names the peer offered in its handshake.
    pub fn caps(&self) -> Vec<String> {
        self.peer.caps.clone()
    }

    /// Whether the peer offered capability `name`.
    pub fn has_cap(&self, name: String) -> bool {
        self.peer.caps.contains(&name)
    }

    /// Ends the request channel. Calls that start after it fail with `AlreadyClosed`.
    ///
    /// With no call running, the end is normal: the sending direction closes and this call
    /// waits up to the drain cap for the peer to end its side. With calls running on other
    /// threads, the channel is closed both ways at once: those calls return
    /// `RequestFailure::Closed` without waiting for their timeout, whether they were waiting
    /// for their answer or still writing their request (end `Other` then), and nothing more
    /// is sent on the channel after this call returns. Finishing again does nothing.
    pub fn finish(&self) {
        let Some(client) = lock(&self.client).take() else {
            return;
        };
        self.finished.store(true, Ordering::SeqCst);
        match Arc::try_unwrap(client) {
            Ok(client) => drop(client),
            Err(running) => {
                self.closer.force_close(EndReason::Forced);
                drop(running);
            }
        }
    }
}
