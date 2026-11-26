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
use crate::lock::lock;
use crate::reject::TooOldEvidence;
use crate::transport::{
    ConnectError, Connection, DialAuth, Endpoint, EndpointHandle, Transport, TransportKind,
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

type Published = Arc<Mutex<HashMap<TunnelKey, Weak<SshTunnel>>>>;

/// The SSH forward transport.
///
/// [`Transport::connect`] reads the peer's connection record over SSH (port, token, instance
/// ID), reuses the published tunnel of that record or starts a new `ssh -L`, and opens a
/// loopback TCP connection through it (`TCP_NODELAY` on). The tunnel lives as long as the
/// channels through it: every channel holds a reference and the last one reaps the `ssh`
/// child. A new tunnel belongs to the attempt that started it until that attempt's first
/// handshake succeeds; only then is it shared. When to connect again is the consumer's
/// decision ([`tasty_net_ssh::Backoff`], [`crate::HandshakeError::is_retryable`]); each new
/// attempt discovers the record again.
pub struct SshTransport {
    launcher: SshLauncher,
    discovery: PortDiscoveryConfig,
    mode: PortMode,
    published: Published,
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
        }
    }

    /// How many shared tunnels are alive now.
    pub fn live_tunnels(&self) -> usize {
        lock(&self.published)
            .values()
            .filter(|t| t.upgrade().is_some_and(|t| t.is_alive()))
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
    fn shared_tunnel(&self, key: &TunnelKey) -> Option<Arc<SshTunnel>> {
        let mut published = lock(&self.published);
        published.retain(|_, t| t.strong_count() > 0);
        published
            .get(key)
            .and_then(Weak::upgrade)
            .filter(|t| t.is_alive())
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
    tunnel: &Arc<SshTunnel>,
    cancel: &AttemptCancel,
) -> Result<RawChannel, ConnectError> {
    let verdict = tunnel.listener_verdict();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, tunnel.local_port()));
    let connected = TcpStream::connect_timeout(&addr, LOCAL_CONNECT_TIMEOUT);
    if cancel.is_cancelled() {
        return Err(ConnectError::Cancelled);
    }
    let stream = connected.map_err(ConnectError::Io)?;
    let channel = RawChannel::from_tcp_as(stream, TransportKind::Ssh).map_err(ConnectError::Io)?;
    Ok(channel
        .with_verdict(verdict)
        .keep_alive(Arc::clone(tunnel) as Arc<dyn std::any::Any + Send + Sync>))
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
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let record = self.discover(target, cancel)?;
        let key = TunnelKey::new(target, &record);
        let (tunnel, owned) = match self.shared_tunnel(&key) {
            Some(tunnel) => (tunnel, None),
            None => {
                if cancel.is_cancelled() {
                    return Err(ConnectError::Cancelled);
                }
                let established = SshTunnel::establish(&self.launcher, target, record.port);
                if cancel.is_cancelled() {
                    if let Ok(tunnel) = established {
                        OwnedTunnel(Arc::new(tunnel)).cancel();
                    }
                    return Err(ConnectError::Cancelled);
                }
                let tunnel = Arc::new(established.map_err(ConnectError::Tunnel)?);
                // A cancel from here on finds the tunnel registered, or is refused by the
                // registration, which reaps it.
                let registration = cancel
                    .register(Arc::new(OwnedTunnel(Arc::clone(&tunnel))))
                    .map_err(|_| ConnectError::Cancelled)?;
                (tunnel, Some(registration))
            }
        };
        let channel = open_through(&tunnel, cancel)?;
        Ok(Connection {
            channel,
            endpoint: Endpoint::new(Arc::new(SshEndpoint {
                tunnel,
                key,
                token: record.token,
                published: Arc::clone(&self.published),
                owned: Mutex::new(owned),
            })),
        })
    }
}

/// The endpoint one discovery and tunnel reached.
struct SshEndpoint {
    tunnel: Arc<SshTunnel>,
    key: TunnelKey,
    token: tasty_net_identity::ConnectionToken,
    published: Published,
    /// The attempt's hold on a new tunnel, released when the tunnel is published.
    owned: Mutex<Option<Registration>>,
}

impl EndpointHandle for SshEndpoint {
    fn connect_more(&self, cancel: &AttemptCancel) -> Result<RawChannel, ConnectError> {
        if cancel.is_cancelled() {
            return Err(ConnectError::Cancelled);
        }
        if !self.tunnel.is_alive() {
            return Err(ConnectError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "the SSH tunnel is gone",
            )));
        }
        open_through(&self.tunnel, cancel)
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
        lock(&self.published).insert(self.key.clone(), Arc::downgrade(&self.tunnel));
        Ok(())
    }
}
