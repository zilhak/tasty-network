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
use crate::channel::RawChannel;
use crate::connection_table::{
    Cause, ConnectionPeer, ConnectionState, ConnectionTable, EntryRef, Joined, NewEntry, PathKind,
    Phase, Segment, Via,
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
        // The cause the entry keeps (the last link's end, or why the attempt failed), unless
        // the tunnel itself exited.
        let cause = if self.tunnel.is_alive() {
            entry
                .status()
                .and_then(|(_, cause)| cause)
                .unwrap_or(Cause::Ended)
        } else {
            Cause::TunnelExited
        };
        entry.end(cause);
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
                info.peer.instance_id = Some(record.instance_id.clone());
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
                            joined = Some(Joined {
                                pending: entry.clone(),
                                into: shared.id(),
                            });
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
                                name: "local_forward",
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
            Some(joined) => channel.with_joined(joined.clone()),
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
    let channel = RawChannel::from_tcp_as(stream, TransportKind::Ssh).map_err(ConnectError::Io)?;
    let channel = channel
        .with_verdict(verdict)
        .keep_alive(Arc::clone(hold) as Arc<dyn std::any::Any + Send + Sync>);
    Ok(match &hold.entry {
        Some(entry) => channel.with_entry(entry.clone()),
        None => channel,
    })
}

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

/// Ends a reconnection entry whose attempt on a shared tunnel was dropped without a
/// handshake result (a redialer's entry only keeps the cause).
struct JoinGuard(Joined);

impl Drop for JoinGuard {
    fn drop(&mut self) {
        self.0.pending.end(Cause::Ended);
    }
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
