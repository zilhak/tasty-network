//! The SSH forward transport: port discovery over SSH, one shared `-L` tunnel per discovered
//! endpoint, and loopback TCP channels through it (docs/design/transport.md "채널 대응",
//! "터널 수명과 재연결").

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tasty_net_identity::{ConnectionRecord, ParsedRecord};
use tasty_net_ssh::{
    PortDiscoveryConfig, PortDiscoveryFailureKind, PortMode, SshLauncher, SshTarget, SshTunnel,
    discover_remote_port,
};

use crate::cancel::{AttemptCancel, Cancellable, Cancelled, Registration};
use crate::channel::{ChannelControl, RawChannel};
use crate::connection_table::{
    Cause, ConnectionInfo, ConnectionPeer, ConnectionState, ConnectionTable, EntryRef, JoinGuard,
    Joined, NewEntry, PathKind, Phase, Segment, Via,
};
use crate::lock::lock;
use crate::reject::TooOldEvidence;
use crate::role::{Holder, Side};
use crate::transport::{
    ConnectError, ConnectOptions, Connection, DialAuth, Endpoint, EndpointHandle, Transport,
    TransportKind,
};

/// Bound of one loopback connect to the tunnel's local port.
pub const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What makes two connections share a tunnel: the same target and the same discovery result.
/// A new port or instance ID after a restart gives a new key, so an old tunnel is not used for
/// new channels.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TunnelKey {
    destination: String,
    ssh_port: Option<u16>,
    identity_file: Option<String>,
    extra_options: Vec<String>,
    remote_port: u16,
    instance_id: String,
}

impl TunnelKey {
    fn new(target: &SshTarget, record: &ConnectionRecord) -> Self {
        Self {
            destination: target.destination.clone(),
            ssh_port: target.ssh_port,
            identity_file: target.identity_file.clone(),
            extra_options: target.extra_options.clone(),
            remote_port: record.port,
            instance_id: record.instance_id.clone(),
        }
    }
}

type Published = Arc<Mutex<HashMap<TunnelKey, Weak<TunnelHold>>>>;

/// A tunnel and its entry in the connection table. Every channel through the tunnel and the
/// endpoint hold it; when the last one goes, the entry ends (and the tunnel is reaped).
struct TunnelHold {
    tunnel: Arc<SshTunnel>,
    entry: Option<EntryRef>,
}

impl Drop for TunnelHold {
    fn drop(&mut self) {
        let Some(entry) = &self.entry else {
            return;
        };
        let alive = self.tunnel.is_alive();
        entry.end_by(|state, recorded| tunnel_end(alive, state, recorded));
    }
}

/// What the entry of a tunnel ends with, once nothing holds the tunnel (`alive` if it still
/// runs), on an entry in `state` whose last cause is `recorded`. Once the entry was connected,
/// a tunnel that exited names the end whatever was recorded: a link or channel closed here
/// after the tunnel died only cleaned up; a live one ends with the last link's end. A cause
/// recorded before the entry was connected is the attempt's own failure (such as a cancel that
/// reaped the tunnel) and stays, so the redialer reads it.
fn tunnel_end(
    alive: bool,
    state: ConnectionState,
    recorded: Option<&Cause>,
) -> crate::connection_table::EndBy {
    use crate::connection_table::EndBy;
    match (state, recorded) {
        (ConnectionState::Connected, _) if !alive => EndBy::Connection(Cause::TunnelExited),
        (ConnectionState::Connected, _) => EndBy::LastLink,
        (_, Some(cause)) => EndBy::Connection(cause.clone()),
        _ if !alive => EndBy::Connection(Cause::TunnelExited),
        _ => EndBy::Connection(Cause::Ended),
    }
}

/// The `ssh` segment: the profile's target as given.
fn ssh_target_text(target: &SshTarget) -> String {
    match target.ssh_port {
        Some(port) => format!("{}:{port}", target.destination),
        None => target.destination.clone(),
    }
}

fn loopback(port: u16) -> String {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port)).to_string()
}

/// The SSH forward transport.
///
/// [`Transport::connect`] reads the peer's connection record over SSH (port, token, instance
/// ID), reuses the published tunnel of that record or starts a new `ssh -L`, and opens a
/// loopback TCP connection through it (`TCP_NODELAY` on). The tunnel lives as long as the
/// channels through it: every channel holds a reference and the last one reaps the `ssh`
/// child. A new tunnel belongs to the attempt that started it until that attempt's first
/// handshake succeeds; only then is it shared. When to connect again is the consumer's
/// decision ([`tasty_net_ssh::Backoff`], [`crate::HandshakeError::is_retryable`],
/// [`crate::Redialer`]); each new attempt discovers the record again.
///
/// With a [`ConnectionTable`] ([`SshTransport::with_connections`]) every tunnel is one entry
/// (`ssh_forward` or `ssh_role_reversed`), from port discovery until its last channel ends.
pub struct SshTransport {
    launcher: SshLauncher,
    discovery: PortDiscoveryConfig,
    mode: PortMode,
    published: Published,
    connections: Option<ConnectionTable>,
}

impl fmt::Debug for SshTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTransport")
            .field("launcher", &self.launcher)
            .field("mode", &self.mode)
            .field("published", &lock(&self.published).len())
            .finish_non_exhaustive()
    }
}

impl SshTransport {
    /// A transport that starts `ssh` with `launcher` and reads the record as `discovery`
    /// and `mode` say.
    pub fn new(launcher: SshLauncher, discovery: PortDiscoveryConfig, mode: PortMode) -> Self {
        Self {
            launcher,
            discovery,
            mode,
            published: Arc::new(Mutex::new(HashMap::new())),
            connections: None,
        }
    }

    /// Reports the tunnels of this transport in `table`.
    pub fn with_connections(mut self, table: ConnectionTable) -> Self {
        self.connections = Some(table);
        self
    }

    /// The table this transport reports in.
    pub fn connections(&self) -> Option<&ConnectionTable> {
        self.connections.as_ref()
    }

    /// How many shared tunnels are alive now.
    pub fn live_tunnels(&self) -> usize {
        lock(&self.published)
            .values()
            .filter(|t| t.upgrade().is_some_and(|t| t.tunnel.is_alive()))
            .count()
    }

    fn discover(
        &self,
        target: &SshTarget,
        cancel: &AttemptCancel,
    ) -> Result<ConnectionRecord, ConnectError> {
        let _scope = cancel.ssh_cancel().scope();
        let parsed = discover_remote_port(
            &self.launcher,
            target,
            self.mode,
            &self.discovery,
            ConnectionRecord::parse,
        )
        .map_err(|e| {
            if e.kind() == PortDiscoveryFailureKind::Cancelled || cancel.is_cancelled() {
                ConnectError::Cancelled
            } else {
                ConnectError::Discovery(e)
            }
        })?;
        match parsed {
            ParsedRecord::Record(record) => Ok(record),
            // No token: none of the v2 purposes can be opened, so nothing is connected.
            ParsedRecord::LegacyPort(_) => Err(ConnectError::PeerTooOld {
                evidence: TooOldEvidence::PortOnlyDiscovery,
            }),
        }
    }

    /// The published tunnel for `key`, if it still runs.
    fn shared_tunnel(&self, key: &TunnelKey) -> Option<Arc<TunnelHold>> {
        let mut published = lock(&self.published);
        published.retain(|_, t| t.strong_count() > 0);
        published
            .get(key)
            .and_then(Weak::upgrade)
            .filter(|t| t.tunnel.is_alive())
    }

    /// The entry of a new attempt: the redialer's, or a new one (`reconnecting` when
    /// `options` names the previous entry).
    fn start_entry(&self, target: &SshTarget, options: &ConnectOptions) -> Option<EntryRef> {
        let segments = vec![Segment {
            name: "ssh",
            local: None,
            remote: Some(ssh_target_text(target)),
        }];
        let via = Via::Ssh {
            target: ssh_target_text(target),
            remote_port: None,
        };
        if let Some(pending) = &options.pending {
            pending.update(|info| {
                info.segments = segments;
                info.via = via;
            });
            if let Some(holder) = options.holder {
                pending.choose_path(holder);
            }
            pending.phase(Phase::PortDiscovery);
            return Some(pending.clone());
        }
        let table = self.connections.as_ref()?;
        let path = match options.holder {
            Some(Holder::Acceptor) => PathKind::SshRoleReversed,
            _ => PathKind::SshForward,
        };
        let state = match options.previous {
            Some(_) => ConnectionState::Reconnecting,
            None => ConnectionState::Connecting,
        };
        Some(table.open(NewEntry {
            previous: options.previous,
            parent: None,
            peer: ConnectionPeer {
                name: options.name.clone(),
                ..ConnectionPeer::default()
            },
            path,
            local_side: Side::Dialer,
            segments,
            via,
            state,
            phase: Some(Phase::PortDiscovery),
            held: false,
            path_fixed: options.holder.is_some(),
        }))
    }

    /// Discovers the record, reuses or starts the tunnel, and opens the channel.
    fn connect_on(
        &self,
        target: &SshTarget,
        cancel: &AttemptCancel,
        options: &ConnectOptions,
        entry: Option<&EntryRef>,
    ) -> Result<Connection, ConnectError> {
        let record = self.discover(target, cancel)?;
        if let Some(entry) = entry {
            entry.update(|info| {
                if let Via::Ssh { remote_port, .. } = &mut info.via {
                    *remote_port = Some(record.port);
                }
                // Not yet verified: the handshake confirms it.
                info.peer.instance_id = Some(record.instance_id.clone());
                info.peer.instance_verified = false;
                info.segments.push(Segment {
                    name: "remote_listener",
                    local: None,
                    remote: Some(loopback(record.port)),
                });
            });
        }
        let key = TunnelKey::new(target, &record);
        let mut joined = None;
        let (hold, owned) = match self.shared_tunnel(&key) {
            Some(hold) => {
                if let Some(entry) = entry {
                    match (
                        &hold.entry,
                        options.previous.is_some() || options.pending.is_some(),
                    ) {
                        // A reconnection stays visible: once its handshake on the shared
                        // tunnel succeeds it ends naming the entry it joined; until then it
                        // stays `reconnecting`.
                        (Some(shared), true) => {
                            joined = Some(Joined::new(entry.clone(), shared.id()));
                        }
                        _ => entry.remove(),
                    }
                }
                (hold, None)
            }
            None => {
                if cancel.is_cancelled() {
                    return Err(ConnectError::Cancelled);
                }
                if let Some(entry) = entry {
                    entry.phase(Phase::Tunnel);
                }
                let established = SshTunnel::establish(&self.launcher, target, record.port);
                if cancel.is_cancelled() {
                    if let Ok(tunnel) = established {
                        OwnedTunnel(Arc::new(tunnel)).cancel();
                    }
                    return Err(ConnectError::Cancelled);
                }
                let tunnel = Arc::new(established.map_err(ConnectError::Tunnel)?);
                if let Some(entry) = entry {
                    entry.update(|info| {
                        info.segments.insert(
                            0,
                            Segment {
                                name: LOCAL_FORWARD,
                                local: Some(loopback(tunnel.local_port())),
                                remote: None,
                            },
                        );
                    });
                    entry.phase(Phase::Handshake);
                }
                let hold = Arc::new(TunnelHold {
                    tunnel: Arc::clone(&tunnel),
                    entry: entry.cloned(),
                });
                // A cancel from here on finds the tunnel registered, or is refused by the
                // registration, which reaps it.
                let registration = cancel
                    .register(Arc::new(OwnedTunnel(tunnel)))
                    .map_err(|_| ConnectError::Cancelled)?;
                (hold, Some(registration))
            }
        };
        let channel = open_through(&hold, cancel).inspect_err(|error| {
            // Noted before the hold drops and ends the entry with it.
            if let Some(entry) = entry {
                entry.note_cause(Cause::of_connect(error));
            }
        })?;
        let channel = match &joined {
            Some(joined) => {
                // The channel on the shared tunnel is open: the reconnection is in its
                // handshake now, as a new tunnel's is.
                joined.pending.phase(Phase::Handshake);
                channel.with_joined(joined.clone())
            }
            None => channel,
        };
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(SshEndpoint {
                hold,
                key,
                token: record.token,
                published: Arc::clone(&self.published),
                owned: Mutex::new(owned),
                _joined: joined.map(JoinGuard),
            })),
        })
    }
}

/// A tunnel the attempt owns until it is published: cancelling the attempt reaps it.
struct OwnedTunnel(Arc<SshTunnel>);

impl Cancellable for OwnedTunnel {
    fn cancel(&self) {
        if let Err(error) = self.0.terminate_and_reap() {
            tracing::debug!(%error, "reaping a cancelled SSH tunnel failed");
        }
    }
}

/// Opens one loopback TCP channel through `tunnel`. The listener verdict is taken right
/// before connecting, so it judges only this channel. A cancel that arrived by the time the
/// connect returns wins over its result: the socket is closed and `Cancelled` returned. A
/// cancel after that reaches the channel when the handshake registers it.
fn open_through(
    hold: &Arc<TunnelHold>,
    cancel: &AttemptCancel,
) -> Result<RawChannel, ConnectError> {
    let tunnel = &hold.tunnel;
    let verdict = tunnel.listener_verdict();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, tunnel.local_port()));
    let connected = TcpStream::connect_timeout(&addr, LOCAL_CONNECT_TIMEOUT);
    if cancel.is_cancelled() {
        return Err(ConnectError::Cancelled);
    }
    let stream = connected.map_err(ConnectError::Io)?;
    let local = stream.local_addr();
    let channel = RawChannel::from_tcp_as(stream, TransportKind::Ssh).map_err(ConnectError::Io)?;
    let channel = channel.with_verdict(verdict);
    let channel = match &hold.entry {
        Some(entry) => {
            let channel = match local {
                Ok(local) => {
                    let listed = ForwardedChannel::open(entry, local);
                    channel.wrap_control(|inner| Arc::new(ForwardedControl { inner, listed }))
                }
                Err(error) => {
                    tracing::debug!(%error, "no local address for a channel through the tunnel");
                    channel
                }
            };
            entry.channel_opened(&channel.control());
            channel.with_entry(entry.clone())
        }
        None => channel,
    };
    let held = ChannelOnTunnel {
        hold: Arc::clone(hold),
        control: Arc::downgrade(&channel.control()),
    };
    Ok(channel.keep_alive(Arc::new(held) as Arc<dyn std::any::Any + Send + Sync>))
}

/// One channel's hold on its tunnel, dropped with the channel's last half: the tunnel's entry
/// learns which channel gave the tunnel up last ([`EntryRef::channel_released`]), so a live
/// tunnel's entry takes the cause of that channel's link.
struct ChannelOnTunnel {
    hold: Arc<TunnelHold>,
    control: Weak<dyn ChannelControl>,
}

impl Drop for ChannelOnTunnel {
    fn drop(&mut self) {
        if let Some(entry) = &self.hold.entry {
            entry.channel_released(&self.control);
        }
    }
}

/// The name of the segment between the local forwarding end and the channels opened on it.
const LOCAL_FORWARD: &str = "local_forward";

/// A channel open on the tunnel's forwarding end: its local address is listed on the
/// entry's `local_forward` segment until its socket is closed both ways or the channel goes.
struct ForwardedChannel {
    entry: EntryRef,
    addr: Mutex<Option<String>>,
}

impl ForwardedChannel {
    fn open(entry: &EntryRef, local: SocketAddr) -> Self {
        let addr = local.to_string();
        entry.update(|info| forwarded(info, |list| list.push(addr.clone())));
        Self {
            entry: entry.clone(),
            addr: Mutex::new(Some(addr)),
        }
    }

    /// Takes the address off the list (once).
    fn unlist(&self) {
        let Some(addr) = lock(&self.addr).take() else {
            return;
        };
        self.entry.update(|info| {
            forwarded(info, |list| {
                if let Some(at) = list.iter().position(|a| *a == addr) {
                    list.remove(at);
                }
            });
        });
    }
}

impl Drop for ForwardedChannel {
    fn drop(&mut self) {
        self.unlist();
    }
}

/// The control of a channel through the forwarding end. Closing the socket both ways (a
/// forced close, the parent link's end, a drain cap that ran out) takes the channel off the
/// `local_forward` list at once, though the consumer may still hold its halves.
struct ForwardedControl {
    inner: Arc<dyn ChannelControl>,
    listed: ForwardedChannel,
}

impl ChannelControl for ForwardedControl {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn shutdown_write(&self) -> io::Result<()> {
        self.inner.shutdown_write()
    }

    fn shutdown_and_wake(&self) -> io::Result<()> {
        self.listed.unlist();
        self.inner.shutdown_and_wake()
    }
    fn close_reason(&self) -> Option<crate::reject::P2pCloseReason> {
        self.inner.close_reason()
    }
}

/// Changes the channel addresses on the `local_forward` segment of `info`: its remote side
/// lists them, joined with [`FORWARDED_SEPARATOR`], or is empty without channels.
fn forwarded(info: &mut ConnectionInfo, change: impl FnOnce(&mut Vec<String>)) {
    let Some(segment) = info.segments.iter_mut().find(|s| s.name == LOCAL_FORWARD) else {
        return;
    };
    let mut list: Vec<String> = segment
        .remote
        .as_deref()
        .map(|r| r.split(FORWARDED_SEPARATOR).map(String::from).collect())
        .unwrap_or_default();
    change(&mut list);
    segment.remote = (!list.is_empty()).then(|| list.join(FORWARDED_SEPARATOR));
}

/// Between two channel addresses on the `local_forward` segment.
pub const FORWARDED_SEPARATOR: &str = ", ";

impl Transport for SshTransport {
    type Target = SshTarget;

    /// Discovers the record, reuses or starts the tunnel, and opens the channel. Discovery
    /// stops on cancellation at once; starting a new tunnel blocks until it is ready or fails
    /// (a few seconds). A cancel that arrived meanwhile then wins whichever way the start
    /// went: a started tunnel is reaped and the call returns `Cancelled`.
    fn connect(
        &self,
        target: &SshTarget,
        cancel: &AttemptCancel,
    ) -> Result<Connection, ConnectError> {
        self.connect_with(target, cancel, &ConnectOptions::new())
    }

    /// [`Transport::connect`] whose table entry follows `options`: it reconnects
    /// `options`' previous entry, carries its display name, and takes its path from its
    /// holder (`ssh_role_reversed` for [`Holder::Acceptor`], else `ssh_forward`).
    fn connect_with(
        &self,
        target: &SshTarget,
        cancel: &AttemptCancel,
        options: &ConnectOptions,
    ) -> Result<Connection, ConnectError> {
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let entry = self.start_entry(target, options);
        let connected = self.connect_on(target, cancel, options, entry.as_ref());
        if let (Err(error), Some(entry)) = (&connected, &entry) {
            entry.end(Cause::of_connect(error));
        }
        connected
    }
}

/// The endpoint one discovery and tunnel reached.
struct SshEndpoint {
    hold: Arc<TunnelHold>,
    key: TunnelKey,
    token: tasty_net_identity::ConnectionToken,
    published: Published,
    /// The attempt's hold on a new tunnel, released when the tunnel is published.
    owned: Mutex<Option<Registration>>,
    /// A reconnection waiting on a shared tunnel, ended if the attempt is dropped unfinished.
    _joined: Option<JoinGuard>,
}

impl EndpointHandle for SshEndpoint {
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        if !self.hold.tunnel.is_alive() {
            return Err(ConnectError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "the SSH tunnel is gone",
            )));
        }
        open_through(&self.hold, cancel)
    }

    fn auth(&self) -> Option<DialAuth> {
        Some(DialAuth::Token(self.token.clone()))
    }

    fn publish(&self) -> Result<(), Cancelled> {
        let Some(owned) = lock(&self.owned).take() else {
            return Ok(());
        };
        // A cancel that came first closes the tunnel; it is not shared.
        owned.release()?;
        lock(&self.published).insert(self.key.clone(), Arc::downgrade(&self.hold));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tasty_net_identity::DeviceKey;

    use super::*;
    use crate::connection_table::ConnectionEvent;
    use crate::end::{EndReason, LinkCloser};
    use crate::link::{LinkTimeouts, PeerInfo};
    use crate::link_id::LinkId;
    use crate::memory::memory_channel_pair;
    use crate::role::Holder;
    use crate::transport::TransportKind;

    /// A tunnel's entry, as [`SshTransport`] opens it.
    fn tunnel_entry(table: &ConnectionTable) -> EntryRef {
        table.open(NewEntry {
            previous: None,
            parent: None,
            peer: ConnectionPeer::default(),
            path: PathKind::SshForward,
            local_side: Side::Dialer,
            segments: Vec::new(),
            via: Via::Ssh {
                target: "peer".into(),
                remote_port: Some(1),
            },
            state: ConnectionState::Connecting,
            phase: Some(Phase::PortDiscovery),
            held: false,
            path_fixed: true,
        })
    }

    /// A link numbered `n` stands on the tunnel's `entry` through a channel of the tunnel, as
    /// [`open_through`] and the dialing handshake leave it: its closer and its channel's
    /// control, whose hold on the tunnel the test gives up ([`release`]).
    fn link(entry: &EntryRef, n: u8) -> (LinkCloser, Arc<dyn ChannelControl>) {
        let (channel, _peer) = memory_channel_pair();
        let control = channel.control();
        entry.channel_opened(&control);
        let closer = LinkCloser::new(Arc::clone(&control));
        let peer = PeerInfo {
            device_id: DeviceKey::from_seed([1; 32]).id(),
            instance_id: "peer".into(),
            holder: Some(Holder::Dialer),
            caps: Vec::new(),
            transport: TransportKind::Ssh,
            local_side: Side::Dialer,
        };
        let link_id = LinkId::of_credential(&[n; 32]);
        entry.link_up(&peer, link_id, &closer, &LinkTimeouts::default());
        entry.note_end_of(&closer, link_id);
        (closer, control)
    }

    /// The channel of `control` gives up its hold on the tunnel ([`ChannelOnTunnel`]).
    fn release(entry: &EntryRef, control: &Arc<dyn ChannelControl>) {
        entry.channel_released(&Arc::downgrade(control));
    }

    /// The entry's last cause, the cause of the transition that ended it and the event of
    /// that transition (exactly one).
    fn ended(table: &ConnectionTable, entry: &EntryRef) -> [Option<Cause>; 3] {
        let info = table
            .connection(entry.id())
            .expect("the ended entry is kept");
        assert_eq!(info.state, ConnectionState::Disconnected, "{info:#?}");
        let history = info.history.last().and_then(|h| h.cause.clone());
        let events: Vec<_> = table
            .events()
            .into_iter()
            .filter_map(|event| match event {
                ConnectionEvent::Transition { entry: at, .. }
                    if at.state == ConnectionState::Disconnected =>
                {
                    Some(at.cause)
                }
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 1, "{events:?}");
        [
            info.last_cause,
            history,
            events.into_iter().next().flatten(),
        ]
    }

    /// The last hold of the tunnel goes ([`TunnelHold`]'s drop), `alive` if it still runs.
    fn tunnel_gone(entry: &EntryRef, alive: bool) {
        entry.end_by(|state, recorded| tunnel_end(alive, state, recorded));
    }

    /// Two links one after another on one shared tunnel: the first is closed here, then the
    /// second is lost on another thread (a cloned closer) while its channel gives up the last
    /// hold of the tunnel (`alive` if the tunnel still runs), the second link's end hook
    /// before the tunnel's end or after it (`hook_first`).
    fn after_two_links(alive: bool, hook_first: bool) -> [Option<Cause>; 3] {
        let table = ConnectionTable::default();
        let entry = tunnel_entry(&table);
        let (first, first_channel) = link(&entry, 7);
        first.force_close(EndReason::Forced);
        release(&entry, &first_channel);
        let (second, second_channel) = link(&entry, 8);
        if hook_first {
            second.force_close(EndReason::Lost);
            release(&entry, &second_channel);
            tunnel_gone(&entry, alive);
        } else {
            release(&entry, &second_channel);
            tunnel_gone(&entry, alive);
            second.force_close(EndReason::Lost);
        }
        ended(&table, &entry)
    }

    /// A live tunnel's entry ends with the cause of the link whose channel gave up the last
    /// hold (`lost`), not the first link's (`closed_here`) it still showed; a tunnel that
    /// exited names the end (`tunnel_exited`). Both whichever of that link's end hook and the
    /// tunnel's end comes first, in the entry, its history and its events alike.
    #[test]
    fn a_shared_tunnel_ends_with_its_last_link_cause_whichever_comes_first() {
        for (alive, want) in [(true, Cause::Lost), (false, Cause::TunnelExited)] {
            for hook_first in [true, false] {
                let want = Some(want.clone());
                assert_eq!(
                    after_two_links(alive, hook_first),
                    [want.clone(), want.clone(), want],
                    "alive {alive}, hook first {hook_first}"
                );
            }
        }
    }

    /// Two links open at once on one live tunnel: the first ends normally while the consumer
    /// keeps its channel, the second is then lost and its channel goes, and the first link's
    /// channel gives up the last hold. The entry ends with the first link's `ended`, the link
    /// that gave the tunnel up last, not the second one that ended last.
    #[test]
    fn the_link_that_gives_up_the_tunnel_last_names_the_end() {
        let table = ConnectionTable::default();
        let entry = tunnel_entry(&table);
        let (first, first_channel) = link(&entry, 7);
        let (second, second_channel) = link(&entry, 8);
        first.end_normally();
        second.force_close(EndReason::Lost);
        release(&entry, &second_channel);
        assert_eq!(
            table.connection(entry.id()).expect("the entry").state,
            ConnectionState::Connected
        );
        release(&entry, &first_channel);
        tunnel_gone(&entry, true);
        let want = Some(Cause::Ended);
        assert_eq!(ended(&table, &entry), [want.clone(), want.clone(), want]);
    }

    /// A consumer's bulk guard whose drop waits for the test, telling it when the drop began.
    struct HeldGuard {
        began: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Drop for HeldGuard {
        fn drop(&mut self) {
            // A test that went no longer waits for either.
            let _began = self.began.send(());
            let _released = self.release.recv();
        }
    }

    /// Two links and a request channel of no link on one live tunnel, a bulk guard under the
    /// first link. The first is closed here and its channel goes while its end hooks wait
    /// behind the guard; the second is lost (its end noted) and its channel goes; the request
    /// channel gives up the last hold and the tunnel's end comes. A request channel is no
    /// link: the entry ends with the cause of the link whose channel gave up its hold last,
    /// the second's `lost`.
    #[test]
    fn a_request_channel_that_gives_up_the_tunnel_last_leaves_the_last_link_cause() {
        let table = ConnectionTable::default();
        let entry = tunnel_entry(&table);
        let (first, first_channel) = link(&entry, 7);
        let (second, second_channel) = link(&entry, 8);
        let (request, _peer) = memory_channel_pair();
        let request = request.control();
        entry.channel_opened(&request);
        let (began, began_rx) = std::sync::mpsc::channel();
        let (let_go, release_rx) = std::sync::mpsc::channel();
        let guard: Arc<crate::end::GuardSlot> = Arc::new(Mutex::new(Some(Box::new(HeldGuard {
            began,
            release: release_rx,
        }))));
        first.adopt_guard(&guard);
        let closing = {
            let first = first.clone();
            std::thread::spawn(move || {
                let _guard = guard;
                first.force_close(EndReason::Forced);
            })
        };
        began_rx.recv().expect("the guard's drop began");
        release(&entry, &first_channel);
        second.force_close(EndReason::Lost);
        release(&entry, &second_channel);
        release(&entry, &request);
        tunnel_gone(&entry, true);
        let_go.send(()).expect("the guard waits");
        closing.join().expect("the first link's close");
        let want = Some(Cause::Lost);
        assert_eq!(ended(&table, &entry), [want.clone(), want.clone(), want]);
    }
}
