//! Both ends of handshake v2 (docs/design/handshake.md).
//!
//! [`accept_handshake`] runs the checks in the order of "검사 순서" and calls the consumer's
//! [`DomainPolicy`] only after all of them passed. The dialer side is [`dial_handshake`]
//! (attach), [`dial_request`] (request channel) and [`Link::open_bulk`].

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::time::{Duration, Instant};

use tasty_net_frame::{ReadFailure, StreamTag, read_frame, write_frame};
use tasty_net_identity::{
    ConnectionToken, DeviceId, DeviceKey, PeerKeyStore, PeerRef, TofuVerdict, check_peer,
};

use crate::bulk::{BulkParent, BulkRegistry};
use crate::cancel::{AttemptCancel, Registration};
use crate::channel::RawChannel;
use crate::domain::{AttachRequest, BulkRequest, DomainAccept, DomainPolicy};
use crate::end::{EndReason, GuardSlot, LinkCloser, shutdown_quietly};
use crate::heartbeat::HeartbeatPolicy;
use crate::hex::random_32;
use crate::link::{BulkChannel, BulkDial, Link, LinkTimeouts, PeerInfo, deadline_after};
use crate::reject::{HandshakeError, RejectReason, TooOldEvidence};
use crate::request::{RequestClient, RequestServer};
use crate::role::{Holder, Side};
use crate::transport::{Connection, DialAuth};
use crate::wire::{
    Ack, Greeting, MAX_HANDSHAKE_LINE, OpenIn, OpenOut, PROTO, Path, Purpose, Reply, Signer,
    Transcript, check_token, encode_proof, parse_fields, parse_open_line, parse_proof, parse_reply,
    valid_caps,
};

/// A capability list with an invalid or repeated name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidCapabilities;

impl fmt::Display for InvalidCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a capability name is invalid or repeated")
    }
}

impl std::error::Error for InvalidCapabilities {}

type SharedStore = Arc<dyn PeerKeyStore + Send + Sync>;

/// How this end checks the peer's device ID against a TOFU record.
#[derive(Clone)]
struct Tofu {
    store: SharedStore,
    peer: PeerRef,
}

impl Tofu {
    fn check(&self, id: &DeviceId) -> Result<(), HandshakeError> {
        match check_peer(self.store.as_ref(), &self.peer, id).map_err(HandshakeError::Store)? {
            TofuVerdict::Mismatch { expected, got } => {
                Err(HandshakeError::PeerKeyMismatch { expected, got })
            }
            TofuVerdict::New | TofuVerdict::Match => Ok(()),
        }
    }
}

/// The dialing end's identity and settings.
#[derive(Clone)]
pub struct DialConfig {
    key: DeviceKey,
    instance_id: String,
    caps: Vec<String>,
    timeouts: LinkTimeouts,
    heartbeat: HeartbeatPolicy,
    tofu: Option<Tofu>,
}

impl fmt::Debug for DialConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DialConfig")
            .field("device_id", &self.key.id())
            .field("instance_id", &self.instance_id)
            .field("caps", &self.caps)
            .field("timeouts", &self.timeouts)
            .field("heartbeat", &self.heartbeat)
            .field("tofu", &self.tofu.as_ref().map(|t| t.peer.clone()))
            .finish()
    }
}

impl DialConfig {
    /// A dialer with `key` and the consumer's opaque `instance_id`. The defaults: no
    /// capability, [`LinkTimeouts::default`], [`HeartbeatPolicy::Periodic`] (the tasty
    /// client's policy), no TOFU check.
    pub fn new(key: DeviceKey, instance_id: impl Into<String>) -> Self {
        Self {
            key,
            instance_id: instance_id.into(),
            caps: Vec::new(),
            timeouts: LinkTimeouts::default(),
            heartbeat: HeartbeatPolicy::Periodic,
            tofu: None,
        }
    }

    /// The capability names this end offers.
    pub fn with_caps(mut self, caps: Vec<String>) -> Result<Self, InvalidCapabilities> {
        if !valid_caps(&caps) {
            return Err(InvalidCapabilities);
        }
        self.caps = caps;
        Ok(self)
    }

    pub fn with_timeouts(mut self, timeouts: LinkTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn with_heartbeat(mut self, heartbeat: HeartbeatPolicy) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Checks the peer's device ID against `store` under the name `peer` (tasty: the remote
    /// profile name). The first sight is recorded; a different key ends the handshake with
    /// [`HandshakeError::PeerKeyMismatch`].
    pub fn with_peer_store(mut self, store: SharedStore, peer: PeerRef) -> Self {
        self.tofu = Some(Tofu { store, peer });
        self
    }

    /// The device ID of this end.
    pub fn device_id(&self) -> DeviceId {
        self.key.id()
    }
}

/// A role reversal (`holder = "acceptor"`) the peer proposed, handed to the consumer for
/// approval. Answer with [`RoleProposal::accept`] or [`RoleProposal::refuse`] from any thread;
/// dropping it refuses. An answer after `role_proposal_timeout` has no effect.
#[derive(Debug)]
pub struct RoleProposal {
    peer: PeerInfo,
    /// The answer and when it was given: the approval deadline is judged on that time, not on
    /// when the waiting handshake takes the answer.
    reply: Sender<(bool, Instant)>,
}

impl RoleProposal {
    /// The authenticated peer that proposes to make this end the holder.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    pub fn accept(self) {
        let _sent = self.reply.send((true, Instant::now())).is_ok();
    }

    pub fn refuse(self) {
        let _sent = self.reply.send((false, Instant::now())).is_ok();
    }
}

type ProposalHandler = Arc<dyn Fn(RoleProposal) + Send + Sync>;

/// A bound on handshakes running at once on one accepting end.
#[derive(Clone)]
struct Limit {
    active: Arc<AtomicUsize>,
    max: usize,
}

struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Limit {
    fn acquire(&self) -> Option<Permit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Permit(Arc::clone(&self.active)))
    }
}

/// Default bound on handshakes running at once on one accepting end.
pub const DEFAULT_MAX_HANDSHAKES: usize = 64;

/// The accepting end's identity, token and checks. One value serves every channel of the
/// end: it holds the bulk credentials the end issued.
#[derive(Clone)]
pub struct AcceptConfig {
    key: DeviceKey,
    instance_id: String,
    token: Option<ConnectionToken>,
    caps: Vec<String>,
    timeouts: LinkTimeouts,
    heartbeat: HeartbeatPolicy,
    domain: Arc<dyn DomainPolicy>,
    accept_forward: bool,
    proposals: Option<ProposalHandler>,
    tofu: Option<Tofu>,
    bulk: BulkRegistry,
    limit: Limit,
}

impl fmt::Debug for AcceptConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptConfig")
            .field("device_id", &self.key.id())
            .field("instance_id", &self.instance_id)
            .field("token", &self.token)
            .field("caps", &self.caps)
            .field("accept_forward", &self.accept_forward)
            .field("proposals", &self.proposals.is_some())
            .field("bulk", &self.bulk)
            .finish_non_exhaustive()
    }
}

impl AcceptConfig {
    /// An accepting end with `key`, its `instance_id`, the `token` SSH-path dialers must
    /// show (`None` refuses every SSH-path channel with `token_invalid`; P2P channels need no
    /// token) and the consumer's domain check. The defaults: no capability,
    /// [`LinkTimeouts::default`], [`HeartbeatPolicy::WhenIdle`] (the tasty server's policy),
    /// forward attach accepted, role reversal refused, no TOFU check,
    /// [`DEFAULT_MAX_HANDSHAKES`].
    pub fn new(
        key: DeviceKey,
        instance_id: impl Into<String>,
        token: Option<ConnectionToken>,
        domain: Arc<dyn DomainPolicy>,
    ) -> Self {
        Self {
            key,
            instance_id: instance_id.into(),
            token,
            caps: Vec::new(),
            timeouts: LinkTimeouts::default(),
            heartbeat: HeartbeatPolicy::WhenIdle,
            domain,
            accept_forward: true,
            proposals: None,
            tofu: None,
            bulk: BulkRegistry::new(),
            limit: Limit {
                active: Arc::new(AtomicUsize::new(0)),
                max: DEFAULT_MAX_HANDSHAKES,
            },
        }
    }

    /// The capability names this end offers.
    pub fn with_caps(mut self, caps: Vec<String>) -> Result<Self, InvalidCapabilities> {
        if !valid_caps(&caps) {
            return Err(InvalidCapabilities);
        }
        self.caps = caps;
        Ok(self)
    }

    pub fn with_timeouts(mut self, timeouts: LinkTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn with_heartbeat(mut self, heartbeat: HeartbeatPolicy) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Whether this end takes `holder = "dialer"` (it provides the workspace).
    pub fn with_forward(mut self, accept: bool) -> Self {
        self.accept_forward = accept;
        self
    }

    /// Hands every `holder = "acceptor"` proposal of an authenticated peer to `handler`.
    pub fn with_role_proposals(
        mut self,
        handler: impl Fn(RoleProposal) + Send + Sync + 'static,
    ) -> Self {
        self.proposals = Some(Arc::new(handler));
        self
    }

    /// Checks the peer's device ID against `store` under the name `peer`.
    pub fn with_peer_store(mut self, store: SharedStore, peer: PeerRef) -> Self {
        self.tofu = Some(Tofu { store, peer });
        self
    }

    /// The bound on handshakes running at once; above it a channel is refused with `busy`.
    pub fn with_max_handshakes(mut self, max: usize) -> Self {
        self.limit = Limit {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        };
        self
    }

    /// The bulk credentials this end issued.
    pub fn bulk_registry(&self) -> &BulkRegistry {
        &self.bulk
    }
}

/// What an accepted channel became.
#[derive(Debug)]
pub enum Accepted {
    /// An attach link (boxed: it is much larger than the other variants).
    Link(Box<Link>),
    /// A bulk channel under an attach link this end accepted.
    Bulk(BulkChannel),
    /// A request channel.
    Request(RequestServer),
}

/// Why [`read_first_line`] returned no line.
#[derive(Debug)]
pub enum FirstLineError {
    /// No line break within the length limit.
    TooLong,
    /// The deadline passed.
    TimedOut,
    /// The channel ended before a line break.
    Closed,
    /// Another I/O failure.
    Io(io::Error),
}

impl fmt::Display for FirstLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => f.write_str("the first line is longer than the limit"),
            Self::TimedOut => f.write_str("the first line did not arrive in time"),
            Self::Closed => f.write_str("the channel ended before the first line"),
            Self::Io(_) => f.write_str("reading the first line failed"),
        }
    }
}

impl std::error::Error for FirstLineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Reads the first line of a channel by `deadline` and returns it without its line break.
/// `max_len` counts the line break too, as [`MAX_HANDSHAKE_LINE`] does: a line of
/// `max_len - 1` bytes and its break pass, one byte more is [`FirstLineError::TooLong`]. It
/// reads one byte at a time, so nothing after the line break is consumed and
/// [`accept_handshake`] gets no buffered bytes from it.
pub fn read_first_line(
    channel: &mut RawChannel,
    max_len: usize,
    deadline: Instant,
) -> Result<Vec<u8>, FirstLineError> {
    let control = channel.control();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(FirstLineError::TimedOut);
        }
        control
            .set_read_timeout(Some(deadline - now))
            .map_err(FirstLineError::Io)?;
        match channel.read(&mut byte) {
            Ok(0) => return Err(FirstLineError::Closed),
            // The line break counts toward `max_len` too.
            Ok(_) if byte[0] == b'\n' => {
                if line.len() + 1 > max_len {
                    return Err(FirstLineError::TooLong);
                }
                return Ok(line);
            }
            Ok(_) => {
                // This byte plus the line break still to come must fit in `max_len`.
                if line.len() + 2 > max_len {
                    return Err(FirstLineError::TooLong);
                }
                line.push(byte[0]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(match ReadFailure::of_io(&e) {
                    ReadFailure::HeartbeatTimeout => FirstLineError::TimedOut,
                    ReadFailure::PeerClosed => FirstLineError::Closed,
                    _ => FirstLineError::Io(e),
                });
            }
        }
    }
}

/// Reads and writes the handshake messages of one channel against one deadline.
struct Io<'a> {
    ch: &'a mut RawChannel,
    deadline: Instant,
    cancel: &'a AttemptCancel,
}

impl Io<'_> {
    fn left(&self) -> Result<Duration, HandshakeError> {
        let now = Instant::now();
        if self.cancel.is_cancelled() {
            return Err(HandshakeError::Cancelled);
        }
        if now >= self.deadline {
            return Err(HandshakeError::TimedOut);
        }
        Ok(self.deadline - now)
    }

    /// What ended the channel when a read saw it end: the attempt's cancel first, then the
    /// code the P2P connection was closed with, and only then `otherwise` (EOF or the bytes
    /// the read got). The cancel and the close shut the channel too, so the read alone cannot
    /// tell them from the peer.
    fn ended_by(&self, otherwise: HandshakeError) -> HandshakeError {
        if self.cancel.is_cancelled() {
            return HandshakeError::Cancelled;
        }
        if let Some(reason) = self.ch.control().close_reason() {
            return HandshakeError::Closed(reason);
        }
        otherwise
    }

    fn fail(&self, error: io::Error) -> HandshakeError {
        if self.cancel.is_cancelled() {
            return HandshakeError::Cancelled;
        }
        if let Some(reason) = self.ch.control().close_reason() {
            return HandshakeError::Closed(reason);
        }
        match ReadFailure::of_io(&error) {
            ReadFailure::HeartbeatTimeout => HandshakeError::TimedOut,
            ReadFailure::PeerClosed => HandshakeError::PeerClosed,
            ReadFailure::Protocol => HandshakeError::Protocol("bytes that are not a frame"),
            ReadFailure::Other if error.kind() == io::ErrorKind::BrokenPipe => {
                HandshakeError::PeerClosed
            }
            ReadFailure::Other => HandshakeError::Io(error),
        }
    }

    /// Reads the next Control frame. The socket timeout is set to the time left before each
    /// frame, and a frame that completes after the deadline is not taken: the handshake ends
    /// with [`HandshakeError::TimedOut`].
    fn read_control(&mut self) -> Result<Vec<u8>, HandshakeError> {
        loop {
            let left = self.left()?;
            self.ch
                .control()
                .set_read_timeout(Some(left))
                .map_err(HandshakeError::Io)?;
            match read_frame(&mut *self.ch) {
                Ok(frame) if frame.tag == StreamTag::Control => {
                    self.left()?;
                    return Ok(frame.payload);
                }
                Ok(frame) if frame.tag == StreamTag::Ping => {}
                Ok(_) => return Err(HandshakeError::Protocol("a frame other than Control")),
                Err(e) => return Err(self.fail(e)),
            }
        }
    }

    fn write_control(&mut self, payload: &[u8]) -> Result<(), HandshakeError> {
        let left = self.left()?;
        self.ch
            .control()
            .set_write_timeout(Some(left))
            .map_err(HandshakeError::Io)?;
        write_frame(&mut *self.ch, StreamTag::Control, payload).map_err(|e| self.fail(e))?;
        // A frame whose partial writes ran past the deadline is not taken as sent in time: the
        // handshake ends with `TimedOut` (an accepting attach then revokes its credential).
        self.left().map(drop)
    }

    fn write_line(&mut self, line: &[u8]) -> Result<(), HandshakeError> {
        let left = self.left()?;
        self.ch
            .control()
            .set_write_timeout(Some(left))
            .map_err(HandshakeError::Io)?;
        self.ch
            .write_all(line)
            .and_then(|()| self.ch.flush())
            .map_err(|e| self.fail(e))
    }

    /// Sends a refusing ACK, ends the sending direction so the peer reads it, and waits a
    /// little for the peer to close, so unread bytes of the peer do not turn the close into
    /// a reset that drops the ACK. Returns the error the accepting end reports.
    fn refuse(
        &mut self,
        reason: RejectReason,
        ext: Option<String>,
        linger: Duration,
    ) -> HandshakeError {
        let ack = Ack::refusal(&reason, ext);
        if let Err(error) = self.write_control(&ack.encode()) {
            tracing::debug!(%error, %reason, "the refusing ACK could not be written");
        } else {
            linger_close(self.ch, linger);
        }
        HandshakeError::Refused(reason)
    }
}

/// Ends the sending direction, then reads and drops the peer's bytes until its EOF, at most
/// `limit` long.
fn linger_close(ch: &mut RawChannel, limit: Duration) {
    let control = ch.control();
    if let Err(error) = control.shutdown_write() {
        tracing::debug!(%error, "ending the sending direction failed");
        return;
    }
    let Some(deadline) = Instant::now().checked_add(limit) else {
        return;
    };
    let mut sink = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline || control.set_read_timeout(Some(deadline - now)).is_err() {
            return;
        }
        match ch.read(&mut sink) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// A channel that closed during the handshake is explained by its SSH listener verdict.
fn explain(ch: &RawChannel, error: HandshakeError) -> HandshakeError {
    match error {
        HandshakeError::PeerClosed => {
            let dropped =
                tasty_net_ssh::connection_closed_error("the channel closed during the handshake");
            ch.listener_absent(&dropped).map_or(
                HandshakeError::PeerClosed,
                HandshakeError::RemoteListenerAbsent,
            )
        }
        HandshakeError::Io(e) => match ch.listener_absent(&e) {
            Some(absent) => HandshakeError::RemoteListenerAbsent(absent),
            None => HandshakeError::Io(e),
        },
        other => other,
    }
}

/// Message 1 as bytes, refused here when it is longer than [`MAX_HANDSHAKE_LINE`] with its
/// line break (the peer would refuse it as `malformed`).
fn checked_line(open: &OpenOut<'_>) -> Result<Vec<u8>, HandshakeError> {
    let line = open.line();
    if line.len() > MAX_HANDSHAKE_LINE {
        return Err(HandshakeError::MessageTooLong);
    }
    Ok(line)
}

/// The dialer's view after message 1.
struct Dialed {
    peer: PeerInfo,
    ack: Ack,
}

/// Runs the dialing side of an attach or request handshake on `conn` (message 1 to the
/// ACK). The channel is registered with `cancel` until the handshake ends. A success hands
/// the channel on by releasing that registration in one order with the cancel: a cancel that
/// came first (and closed the socket) makes the handshake `Cancelled`, so a closed socket
/// never comes back as a success, also on a shared tunnel that has nothing to publish.
fn dial_open(
    ch: &mut RawChannel,
    auth: &DialAuth,
    config: &DialConfig,
    purpose: Purpose,
    holder: Option<Holder>,
    ext: Option<&str>,
    cancel: &AttemptCancel,
) -> Result<Dialed, HandshakeError> {
    let registration = cancel
        .register_channel(ch.control())
        .map_err(|_| HandshakeError::Cancelled)?;
    let dialed = dial_open_registered(ch, auth, config, purpose, holder, ext, cancel)?;
    registration
        .release()
        .map_err(|_| HandshakeError::Cancelled)?;
    Ok(dialed)
}

fn dial_open_registered(
    ch: &mut RawChannel,
    auth: &DialAuth,
    config: &DialConfig,
    purpose: Purpose,
    holder: Option<Holder>,
    ext: Option<&str>,
    cancel: &AttemptCancel,
) -> Result<Dialed, HandshakeError> {
    config.timeouts.check()?;
    let mut wait = config.timeouts.handshake_timeout;
    if holder == Some(Holder::Acceptor) {
        wait = wait
            .checked_add(config.timeouts.role_proposal_timeout)
            .ok_or(HandshakeError::InvalidTimeouts)?;
    }
    let kind = ch.kind();
    let mut io = Io {
        ch,
        deadline: deadline_after(wait)?,
        cancel,
    };
    let (token, nonce) = match auth {
        DialAuth::Token(token) => (Some(token), Some(random_32().map_err(HandshakeError::Io)?)),
        DialAuth::P2p { .. } => (None, None),
    };
    let open = OpenOut {
        purpose,
        token,
        device_id: Some(config.key.id()),
        instance_id: &config.instance_id,
        nonce,
        holder,
        caps: Some(&config.caps),
        ext,
        bulk_credential: None,
    };
    io.write_line(&checked_line(&open)?)?;
    let first = parse_reply(&io.read_control()?).ok_or(HandshakeError::Protocol(
        "a reply that is not a handshake message",
    ))?;
    let (greeting, ack) = match (auth, first) {
        (_, Reply::Ack(ack)) => return Err(refusal_of(ack)),
        (DialAuth::Token(_), Reply::Challenge(g)) => {
            let (Some(their_nonce), Some(sig), Some(our_nonce)) = (g.nonce, g.sig, nonce) else {
                return Err(HandshakeError::Protocol(
                    "a challenge without nonce or signature",
                ));
            };
            let transcript = Transcript {
                purpose,
                holder,
                dialer_id: config.key.id(),
                acceptor_id: g.device_id,
                dialer_instance: &config.instance_id,
                acceptor_instance: &g.instance_id,
                dialer_nonce: our_nonce,
                acceptor_nonce: their_nonce,
                dialer_caps: &config.caps,
                acceptor_caps: &g.caps,
                ext,
            };
            if !g
                .device_id
                .verify(&transcript.digest(Signer::Acceptor), &sig)
            {
                return Err(HandshakeError::SignatureInvalid);
            }
            if g.instance_id == config.instance_id {
                return Err(HandshakeError::SelfAttach);
            }
            if let Some(tofu) = &config.tofu {
                tofu.check(&g.device_id)?;
            }
            let proof = config.key.sign(&transcript.digest(Signer::Dialer));
            io.write_control(&encode_proof(&proof))?;
            (g, read_ack(&mut io)?)
        }
        (DialAuth::P2p { remote_id }, Reply::Hello(g)) => {
            let ack = read_ack(&mut io)?;
            if g.device_id != *remote_id {
                return Err(HandshakeError::PeerMismatch);
            }
            if g.instance_id == config.instance_id {
                return Err(HandshakeError::SelfAttach);
            }
            if let Some(tofu) = &config.tofu {
                tofu.check(&g.device_id)?;
            }
            (g, ack)
        }
        _ => return Err(HandshakeError::Protocol("a reply of the other path")),
    };
    Ok(Dialed {
        peer: PeerInfo {
            device_id: greeting.device_id,
            instance_id: greeting.instance_id,
            holder,
            caps: greeting.caps,
            transport: kind,
            local_side: Side::Dialer,
        },
        ack,
    })
}

/// Reads message 4; a refusal becomes its error.
fn read_ack(io: &mut Io<'_>) -> Result<Ack, HandshakeError> {
    match parse_reply(&io.read_control()?) {
        Some(Reply::Ack(ack)) if ack.ok => {
            if ack.proto != PROTO {
                return Err(HandshakeError::Protocol("a success ACK of another proto"));
            }
            Ok(ack)
        }
        Some(Reply::Ack(ack)) => Err(refusal_of(ack)),
        _ => Err(HandshakeError::Protocol("a reply that is not an ACK")),
    }
}

/// The error for an ACK that came where it ends the handshake. A refusing ACK carries
/// `proto` and `error` (v1 too); a v2 refusal carries `reason` as well
/// (docs/design/handshake.md "구버전 처리", "필드").
fn refusal_of(ack: Ack) -> HandshakeError {
    if ack.ok {
        return HandshakeError::Protocol("a success ACK before the challenge");
    }
    if ack.proto == 0 {
        return HandshakeError::Protocol("a refusing ACK without proto");
    }
    let Some(error) = ack.error else {
        return HandshakeError::Protocol("a refusing ACK without error");
    };
    if ack.proto == 1 {
        return HandshakeError::PeerTooOld {
            evidence: TooOldEvidence::V1Ack,
        };
    }
    let Some(reason) = ack.reason else {
        return HandshakeError::Protocol("a v2 refusing ACK without reason");
    };
    HandshakeError::Rejected {
        reason,
        error,
        ext: ack.ext,
    }
}

/// Opens an attach link: handshake v2 on the ordered channel of `conn` with `holder`
/// ([`Holder::Dialer`] is the forward direction) and the consumer's `ext`. The ordered
/// channel's timeouts and heartbeat start right after the success ACK.
pub fn dial_handshake(
    conn: Connection,
    config: &DialConfig,
    holder: Holder,
    ext: Option<&str>,
    cancel: &AttemptCancel,
) -> Result<Link, HandshakeError> {
    let Connection {
        mut channel,
        endpoint,
    } = conn;
    let auth = endpoint.auth().ok_or(HandshakeError::NoCredential)?;
    let dialed = dial_open(
        &mut channel,
        &auth,
        config,
        Purpose::Attach,
        Some(holder),
        ext,
        cancel,
    )
    .map_err(|e| explain(&channel, e))?;
    let credential = dialed.ack.bulk_credential.ok_or(HandshakeError::Protocol(
        "an attach ACK without a bulk credential",
    ))?;
    endpoint.publish().map_err(|_| HandshakeError::Cancelled)?;
    let control = channel.control();
    let closer = LinkCloser::new(Arc::clone(&control));
    let framed = channel
        .framed_under(&config.timeouts.channel(config.heartbeat), closer.clone())
        .map_err(HandshakeError::Io)?;
    let normal = closer.clone();
    framed.on_normal_end(Box::new(move || normal.end_normally()));
    let ordered_shared = framed.shared();
    Ok(Link {
        ordered_shared,
        ordered: Some((framed.reader, framed.writer)),
        closer,
        control,
        peer: dialed.peer,
        client_id: dialed.ack.client_id,
        ext: dialed.ack.ext,
        bulk: Some(BulkDial {
            endpoint,
            credential,
            config: config.clone(),
            auth,
        }),
        _guard: None,
    })
}

/// Opens a request channel on `conn`. It needs no attach link.
pub fn dial_request(
    conn: Connection,
    config: &DialConfig,
    cancel: &AttemptCancel,
) -> Result<RequestClient, HandshakeError> {
    let Connection {
        mut channel,
        endpoint,
    } = conn;
    let auth = endpoint.auth().ok_or(HandshakeError::NoCredential)?;
    let dialed = dial_open(
        &mut channel,
        &auth,
        config,
        Purpose::Request,
        None,
        None,
        cancel,
    )
    .map_err(|e| explain(&channel, e))?;
    endpoint.publish().map_err(|_| HandshakeError::Cancelled)?;
    let framed = channel
        .framed(&config.timeouts.channel(config.heartbeat))
        .map_err(HandshakeError::Io)?;
    RequestClient::start(framed.reader, framed.writer, framed.closer, dialed.peer)
        .map_err(HandshakeError::Io)
}

/// The dialing side of a bulk channel under `link` ([`Link::open_bulk`]).
pub(crate) fn dial_bulk(link: &Link, ext: Option<&str>) -> Result<BulkChannel, HandshakeError> {
    let bulk = link.bulk.as_ref().ok_or(HandshakeError::NotBulkCapable)?;
    let cancel = AttemptCancel::new();
    let mut ch = bulk
        .endpoint
        .connect_more(&cancel)
        .map_err(HandshakeError::Connect)?;
    // Adopted before the handshake: a parent that ends meanwhile closes this channel too.
    if !link.closer.adopt(&ch.control()) {
        shutdown_quietly(ch.control().as_ref());
        return Err(HandshakeError::ParentEnded);
    }
    let _registration = cancel
        .register_channel(ch.control())
        .map_err(|_| HandshakeError::Cancelled)?;
    let config = &bulk.config;
    let mut io = Io {
        ch: &mut ch,
        deadline: deadline_after(config.timeouts.handshake_timeout)?,
        cancel: &cancel,
    };
    let token = match &bulk.auth {
        DialAuth::Token(token) => Some(token),
        DialAuth::P2p { .. } => None,
    };
    let open = OpenOut {
        purpose: Purpose::Bulk,
        token,
        device_id: None,
        instance_id: &config.instance_id,
        nonce: None,
        holder: None,
        caps: None,
        ext,
        bulk_credential: Some(bulk.credential),
    };
    let ack = checked_line(&open)
        .and_then(|line| io.write_line(&line))
        .and_then(|()| read_ack(&mut io));
    let ack = ack.map_err(|e| explain(&ch, e))?;
    let framed = ch
        .framed(&config.timeouts.channel(config.heartbeat))
        .map_err(HandshakeError::Io)?;
    Ok(BulkChannel {
        reader: framed.reader,
        writer: framed.writer,
        closer: framed.closer,
        peer: link.peer.clone(),
        ext: ack.ext,
        _guard: Arc::new(std::sync::Mutex::new(None)),
    })
}

/// Runs the accepting side of handshake v2 on a channel whose first line the consumer read
/// (the SSH path: a loopback socket the consumer accepted and found to start with
/// `stream.open`). `buffered` are bytes the consumer's line reader already took after the
/// line; they are read before the channel. The channel is registered with `cancel` until the
/// handshake ends.
///
/// The checks run in this order, and a failure sends a refusing ACK (except where noted) and
/// returns [`HandshakeError::Refused`]: the `proto` and format of message 1, the token
/// (constant time; no challenge before it passes), the fields of the purpose, the dialer's
/// signature (after the challenge), self attach, TOFU (when configured), the role (attach),
/// the bulk credential (bulk), and last the consumer's [`DomainPolicy`].
pub fn accept_handshake(
    channel: RawChannel,
    first_line: &[u8],
    buffered: Vec<u8>,
    config: &AcceptConfig,
    cancel: &AttemptCancel,
) -> Result<Accepted, HandshakeError> {
    let mut ch = channel.with_buffered(buffered);
    let registration = cancel
        .register_channel(ch.control())
        .map_err(|_| HandshakeError::Cancelled)?;
    config.timeouts.check()?;
    let deadline = deadline_after(config.timeouts.handshake_timeout)?;
    let permit = take_permit(&mut ch, deadline, config, cancel)?;
    accept_on(
        ch,
        first_line,
        Path::Ssh,
        config,
        Started {
            cancel,
            deadline,
            _registration: registration,
            _permit: permit,
        },
    )
}

/// What an accepting handshake holds from its start: the attempt, the deadline, the channel's
/// cancel registration and the slot of the concurrent-handshake limit.
struct Started<'a> {
    cancel: &'a AttemptCancel,
    deadline: Instant,
    _registration: Registration,
    _permit: Permit,
}

/// Takes a slot of the concurrent-handshake limit, or refuses the channel with `busy` at once.
fn take_permit(
    ch: &mut RawChannel,
    deadline: Instant,
    config: &AcceptConfig,
    cancel: &AttemptCancel,
) -> Result<Permit, HandshakeError> {
    match config.limit.acquire() {
        Some(permit) => Ok(permit),
        None => {
            let mut io = Io {
                ch,
                deadline,
                cancel,
            };
            Err(io.refuse(RejectReason::Busy, None, config.timeouts.drain_cap))
        }
    }
}

/// The accepting side of handshake v2 on a P2P stream: this crate reads the first line
/// (within [`MAX_HANDSHAKE_LINE`] and `handshake_timeout`). `remote_id` is the QUIC
/// connection's proved remote ID; `scope` names that connection, so a bulk channel is taken
/// only under an attach link of the same connection.
pub fn accept_p2p_handshake(
    mut channel: RawChannel,
    remote_id: DeviceId,
    scope: u64,
    config: &AcceptConfig,
    cancel: &AttemptCancel,
) -> Result<Accepted, HandshakeError> {
    config.timeouts.check()?;
    let deadline = deadline_after(config.timeouts.handshake_timeout)?;
    // The first-line read is under the cancel and the concurrent-handshake limit too.
    let registration = cancel
        .register_channel(channel.control())
        .map_err(|_| HandshakeError::Cancelled)?;
    let permit = take_permit(&mut channel, deadline, config, cancel)?;
    let line = match read_first_line(&mut channel, MAX_HANDSHAKE_LINE, deadline) {
        Ok(line) => line,
        Err(FirstLineError::TooLong) => {
            let mut io = Io {
                ch: &mut channel,
                deadline,
                cancel,
            };
            return Err(io.refuse(RejectReason::Malformed, None, config.timeouts.drain_cap));
        }
        Err(error) => return Err(first_line_failure(&channel, cancel, error)),
    };
    accept_on(
        channel,
        &line,
        Path::P2p { remote_id, scope },
        config,
        Started {
            cancel,
            deadline,
            _registration: registration,
            _permit: permit,
        },
    )
}

/// Why reading the first line of a P2P stream failed: the attempt's cancel, then the code the
/// peer closed the connection with, then the read error itself.
fn first_line_failure(
    channel: &RawChannel,
    cancel: &AttemptCancel,
    error: FirstLineError,
) -> HandshakeError {
    if cancel.is_cancelled() {
        return HandshakeError::Cancelled;
    }
    if let Some(reason) = channel.control().close_reason() {
        return HandshakeError::Closed(reason);
    }
    match error {
        FirstLineError::TimedOut => HandshakeError::TimedOut,
        FirstLineError::Closed => HandshakeError::PeerClosed,
        FirstLineError::Io(e) => HandshakeError::Io(e),
        FirstLineError::TooLong => HandshakeError::Protocol("a first line over the limit"),
    }
}

/// What the role check decided.
enum RoleDecision {
    Accepted,
    Refused,
}

fn accept_on(
    mut ch: RawChannel,
    line: &[u8],
    path: Path,
    config: &AcceptConfig,
    started: Started<'_>,
) -> Result<Accepted, HandshakeError> {
    let cancel = started.cancel;
    let linger = config.timeouts.drain_cap;
    let kind = ch.kind();
    let control = ch.control();
    let mut io = Io {
        ch: &mut ch,
        deadline: started.deadline,
        cancel,
    };
    // 1. proto and format.
    let params = match parse_open_line(line) {
        Ok(params) => params,
        Err(reason) => return Err(io.refuse(reason, None, linger)),
    };
    // 2. token (SSH path).
    if matches!(path, Path::Ssh)
        && let Err(reason) = check_token(&params, config.token.as_ref())
    {
        return Err(io.refuse(reason, None, linger));
    }
    // 3. fields of the purpose.
    let open = match parse_fields(&params, path) {
        Ok(open) => open,
        Err(reason) => return Err(io.refuse(reason, None, linger)),
    };
    if open.purpose == Purpose::Bulk {
        let (parent, guard, ext) = accept_bulk(&mut io, open, path, config)?;
        let framed = ch
            .framed(&config.timeouts.channel(config.heartbeat))
            .map_err(HandshakeError::Io)?;
        return Ok(Accepted::Bulk(BulkChannel {
            reader: framed.reader,
            writer: framed.writer,
            closer: framed.closer,
            peer: parent.peer,
            ext,
            _guard: guard,
        }));
    }
    // 4. signature (SSH path).
    let device_id = match path {
        Path::Ssh => match challenge(&mut io, &open, config)? {
            Some(id) => id,
            None => return Err(io.refuse(RejectReason::SignatureInvalid, None, linger)),
        },
        Path::P2p { remote_id, .. } => remote_id,
    };
    // 5. self attach.
    if open.instance_id == config.instance_id {
        return Err(io.refuse(RejectReason::SelfAttach, None, linger));
    }
    // 6. TOFU.
    if let Some(tofu) = &config.tofu {
        match tofu.check(&device_id) {
            Ok(()) => {}
            Err(HandshakeError::PeerKeyMismatch { .. }) => {
                return Err(io.refuse(RejectReason::PeerKeyMismatch, None, linger));
            }
            Err(other) => return Err(other),
        }
    }
    let peer = PeerInfo {
        device_id,
        instance_id: open.instance_id.clone(),
        holder: open.holder,
        caps: open.caps.clone(),
        transport: kind,
        local_side: Side::Acceptor,
    };
    let greeting = Greeting {
        device_id: config.key.id(),
        instance_id: config.instance_id.clone(),
        caps: config.caps.clone(),
        nonce: None,
        sig: None,
    };
    let p2p_scope = match path {
        Path::P2p { scope, .. } => Some(scope),
        Path::Ssh => None,
    };
    if open.purpose == Purpose::Request {
        // 9. domain, only while the handshake is still in time.
        io.left()?;
        if let Err(reject) = config.domain.accept_request(&peer) {
            return Err(io.refuse(RejectReason::Domain, Some(reject.ext()), linger));
        }
        if p2p_scope.is_some() {
            io.write_control(&greeting.encode("hello"))?;
        }
        let ack = Ack {
            ok: true,
            proto: PROTO,
            ..Ack::default()
        };
        io.write_control(&ack.encode())?;
        let framed = ch
            .framed(&config.timeouts.channel(config.heartbeat))
            .map_err(HandshakeError::Io)?;
        return Ok(Accepted::Request(RequestServer {
            reader: framed.reader,
            writer: framed.writer,
            closer: framed.closer,
            peer,
        }));
    }
    // 7. role.
    let holder = open
        .holder
        .ok_or(HandshakeError::Protocol("an attach without holder"))?;
    let decision = match holder {
        Holder::Dialer if config.accept_forward => RoleDecision::Accepted,
        Holder::Dialer => RoleDecision::Refused,
        Holder::Acceptor => match &config.proposals {
            None => RoleDecision::Refused,
            Some(handler) => {
                let started = Instant::now();
                let decision = await_proposal(
                    &mut io,
                    handler,
                    &peer,
                    config.timeouts.role_proposal_timeout,
                )?;
                // The wait is not part of the handshake timeout.
                io.deadline = io
                    .deadline
                    .checked_add(started.elapsed())
                    .ok_or(HandshakeError::InvalidTimeouts)?;
                decision
            }
        },
    };
    if matches!(decision, RoleDecision::Refused) {
        return Err(io.refuse(RejectReason::RoleRefused, None, linger));
    }
    // 9. domain (8 is the bulk credential, checked in `accept_bulk`), only while the
    // handshake is still in time.
    io.left()?;
    let request = AttachRequest {
        peer: &peer,
        ext: open.ext.as_deref(),
    };
    let accept: DomainAccept = match config.domain.accept_attach(&request) {
        Ok(accept) => accept,
        Err(reject) => return Err(io.refuse(RejectReason::Domain, Some(reject.ext()), linger)),
    };
    let closer = LinkCloser::new(Arc::clone(&control));
    let credential = config
        .bulk
        .issue(BulkParent {
            closer: closer.clone(),
            grant: Arc::from(accept.grant.as_slice()),
            peer: peer.clone(),
            scope: p2p_scope,
        })
        .map_err(HandshakeError::Io)?;
    let written = (|| {
        if p2p_scope.is_some() {
            io.write_control(&greeting.encode("hello"))?;
        }
        let ack = Ack {
            ok: true,
            proto: PROTO,
            client_id: accept.client_id,
            bulk_credential: Some(credential),
            ext: accept.ext.clone(),
            ..Ack::default()
        };
        io.write_control(&ack.encode())
    })();
    if let Err(e) = written {
        // No link stands (the ACK failed, or its bytes went out but too late): the credential
        // is revoked so no bulk finds it any more, and the parent's closer ends, so a bulk
        // channel another thread opened under the credential meanwhile is woken and its
        // guard dropped, and a bulk that read the credential before the revoke is refused
        // when it adopts. The consumer's guard drops with `accept`.
        config.bulk.revoke(&credential);
        closer.force_close(EndReason::Forced);
        return Err(e);
    }
    let registry = config.bulk.clone();
    closer.on_end(Box::new(move || registry.revoke(&credential)));
    let framed = match ch.framed_under(&config.timeouts.channel(config.heartbeat), closer.clone()) {
        Ok(framed) => framed,
        Err(e) => {
            // The ACK is out but no link stands: end it so the credential is revoked and the
            // socket closes (the dialer sees the channel close).
            closer.force_close(EndReason::Forced);
            return Err(HandshakeError::Io(e));
        }
    };
    let normal = closer.clone();
    framed.on_normal_end(Box::new(move || normal.end_normally()));
    let ordered_shared = framed.shared();
    Ok(Accepted::Link(Box::new(Link {
        ordered_shared,
        ordered: Some((framed.reader, framed.writer)),
        closer,
        control,
        peer,
        client_id: accept.client_id,
        ext: open.ext,
        bulk: None,
        _guard: accept.guard,
    })))
}

/// Sends the challenge and checks the proof. `Ok(None)` means the signature did not verify.
fn challenge(
    io: &mut Io<'_>,
    open: &OpenIn,
    config: &AcceptConfig,
) -> Result<Option<DeviceId>, HandshakeError> {
    let (Some(dialer_id), Some(dialer_nonce)) = (open.device_id, open.nonce) else {
        return Err(HandshakeError::Protocol(
            "message 1 without device ID or nonce",
        ));
    };
    let nonce = random_32().map_err(HandshakeError::Io)?;
    let transcript = Transcript {
        purpose: open.purpose,
        holder: open.holder,
        dialer_id,
        acceptor_id: config.key.id(),
        dialer_instance: &open.instance_id,
        acceptor_instance: &config.instance_id,
        dialer_nonce,
        acceptor_nonce: nonce,
        dialer_caps: &open.caps,
        acceptor_caps: &config.caps,
        ext: open.ext.as_deref(),
    };
    let greeting = Greeting {
        device_id: config.key.id(),
        instance_id: config.instance_id.clone(),
        caps: config.caps.clone(),
        nonce: Some(nonce),
        sig: Some(config.key.sign(&transcript.digest(Signer::Acceptor))),
    };
    io.write_control(&greeting.encode("challenge"))?;
    let proof = io.read_control()?;
    let verified = parse_proof(&proof)
        .is_some_and(|sig| dialer_id.verify(&transcript.digest(Signer::Dialer), &sig));
    Ok(verified.then_some(dialer_id))
}

/// How long one look at the channel waits while a role proposal is pending.
const PROPOSAL_POLL: Duration = Duration::from_millis(50);

/// Hands the proposal to the consumer and waits for its answer or the proposal timeout,
/// measured from the call of the consumer's handler (docs/design/handshake.md "역할 반전 승인의
/// 기한"). While it waits it watches the channel: the dialer sends nothing until the ACK, so
/// EOF means the peer closed and any byte breaks the protocol. A cancel or a closed peer ends
/// the wait with that error, and an approval counts only if both are still clear after it
/// came, so the domain callback never runs for a handshake that already lost.
fn await_proposal(
    io: &mut Io<'_>,
    handler: &ProposalHandler,
    peer: &PeerInfo,
    limit: Duration,
) -> Result<RoleDecision, HandshakeError> {
    let deadline = deadline_after(limit)?;
    let (tx, rx) = mpsc::channel();
    handler(RoleProposal {
        peer: peer.clone(),
        reply: tx,
    });
    loop {
        if io.cancel.is_cancelled() {
            return Err(HandshakeError::Cancelled);
        }
        match rx.try_recv() {
            Ok((true, given)) => {
                // An approval given after the approval deadline is too late: it is refused
                // like no answer. One given in time counts even when the wait takes it only
                // after its last channel watch ran past the deadline.
                if given >= deadline {
                    return Ok(RoleDecision::Refused);
                }
                peer_still_there(io, Duration::from_millis(1))?;
                if io.cancel.is_cancelled() {
                    return Err(HandshakeError::Cancelled);
                }
                return Ok(RoleDecision::Accepted);
            }
            Ok((false, _)) | Err(TryRecvError::Disconnected) => {
                return Ok(RoleDecision::Refused);
            }
            Err(TryRecvError::Empty) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(RoleDecision::Refused);
        }
        peer_still_there(io, (deadline - now).min(PROPOSAL_POLL))?;
    }
}

/// Waits up to `wait` for a byte from the dialer, which must send nothing before the ACK:
/// a timeout is the good case. EOF and a byte are told apart in the order of [`Io::fail`]:
/// the cancel, then the P2P close code, then [`HandshakeError::PeerClosed`] or a protocol
/// error.
fn peer_still_there(io: &mut Io<'_>, wait: Duration) -> Result<(), HandshakeError> {
    io.ch
        .control()
        .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
        .map_err(HandshakeError::Io)?;
    let mut byte = [0u8; 1];
    match io.ch.read(&mut byte) {
        Ok(0) => Err(io.ended_by(HandshakeError::PeerClosed)),
        Ok(_) => Err(io.ended_by(HandshakeError::Protocol("bytes before the ACK"))),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
        Err(e) if ReadFailure::of_io(&e) == ReadFailure::HeartbeatTimeout => Ok(()),
        Err(e) => Err(io.fail(e)),
    }
}

/// Checks 8 and 9 of a bulk channel and writes its success ACK. Returns the parent, the
/// consumer's guard (already held by the parent) and the request `ext`.
fn accept_bulk(
    io: &mut Io<'_>,
    open: OpenIn,
    path: Path,
    config: &AcceptConfig,
) -> Result<(BulkParent, Arc<GuardSlot>, Option<String>), HandshakeError> {
    let linger = config.timeouts.drain_cap;
    let credential = open.bulk_credential.ok_or(HandshakeError::Protocol(
        "a bulk channel without credential",
    ))?;
    // 8. the credential names a live parent of the same path (and P2P connection).
    let parent = config.bulk.find(&credential).filter(|parent| match path {
        Path::Ssh => parent.scope.is_none(),
        Path::P2p { remote_id, scope } => {
            parent.scope == Some(scope) && parent.peer.device_id == remote_id
        }
    });
    let Some(parent) = parent else {
        return Err(io.refuse(RejectReason::BulkCredentialInvalid, None, linger));
    };
    // Registering as the parent's child is the point that decides against the parent's end.
    if !parent.closer.adopt(&io.ch.control()) {
        return Err(io.refuse(RejectReason::BulkCredentialInvalid, None, linger));
    }
    // 9. domain, only while the handshake is still in time.
    io.left()?;
    let request = BulkRequest {
        parent: &parent.peer,
        grant: &parent.grant,
        ext: open.ext.as_deref(),
    };
    let accept = match config.domain.accept_bulk(&request) {
        Ok(accept) => accept,
        Err(reject) => return Err(io.refuse(RejectReason::Domain, Some(reject.ext()), linger)),
    };
    // The consumer's guard goes to the parent before the ACK: the parent's end drops it
    // whether it comes before or after the ACK. If the ACK fails, the slot drops here and the
    // guard with it.
    let guard: Arc<GuardSlot> = Arc::new(std::sync::Mutex::new(accept.guard));
    parent.closer.adopt_guard(&guard);
    let ack = Ack {
        ok: true,
        proto: PROTO,
        client_id: accept.client_id,
        ext: accept.ext,
        ..Ack::default()
    };
    io.write_control(&ack.encode())?;
    Ok((parent, guard, open.ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn refusal(value: Value) -> HandshakeError {
        match parse_reply(value.to_string().as_bytes()) {
            Some(Reply::Ack(ack)) => refusal_of(ack),
            other => panic!("not an ACK: {other:?}"),
        }
    }

    #[test]
    fn a_refusing_ack_needs_proto_error_and_in_v2_reason() {
        assert!(matches!(
            refusal(json!({"ok": false, "error": "no", "reason": "busy"})),
            HandshakeError::Protocol(_)
        ));
        assert!(matches!(
            refusal(json!({"ok": false, "proto": 2, "reason": "busy"})),
            HandshakeError::Protocol(_)
        ));
        assert!(matches!(
            refusal(json!({"ok": false, "proto": 2, "error": "no"})),
            HandshakeError::Protocol(_)
        ));
        assert!(matches!(
            refusal(json!({"ok": false, "proto": 1})),
            HandshakeError::Protocol(_)
        ));
        assert!(matches!(
            refusal(json!({"ok": false, "proto": 1, "error": "old"})),
            HandshakeError::PeerTooOld {
                evidence: TooOldEvidence::V1Ack
            }
        ));
        match refusal(json!({"ok": false, "proto": 2, "error": "busy", "reason": "busy"})) {
            HandshakeError::Rejected { reason, error, .. } => {
                assert_eq!(reason, RejectReason::Busy);
                assert_eq!(error, "busy");
            }
            other => panic!("{other:?}"),
        }
    }
}
