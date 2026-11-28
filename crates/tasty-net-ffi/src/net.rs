//! The bindings' entry object [`TastyNet`] and its event stream (android #8, #9, #7, #7+).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey, PeerRef};
use tasty_net_p2p::{P2pConfig, P2pEndpoint, PairingList, RendezvousServer};
use tasty_net_transport::{
    AttemptCancel, ConnectOptions, ConnectionEvent, ConnectionTable, ConnectionTableConfig,
    DialConfig, Holder, P2pTransport, Transport, dial_handshake, dial_request,
};

use crate::channel::{AttachLink, CancelHandle, RequestChannel};
use crate::error::{HandshakeFailure, SetupFailure};
use crate::records::{ConnectionRecord, LinkHolder, NetEvent, SettingEntry};
use crate::secrets::Secrets;
use crate::settings::{FfiSettings, show};
use crate::store::{AppStore, PeerKeyStore, PeerStoreError, reregister_with};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The P2P path: the one rendezvous server and the paired devices.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct P2pSetup {
    /// The server's `ip:port`.
    pub server_addr: String,
    /// The server's public key in device ID form.
    pub server_key: String,
    /// The device IDs of the devices paired with this one.
    pub paired_devices: Vec<String>,
}

/// One time setting of this crate (docs/design/mobile.md "FFI 설정값").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DurationSetting {
    /// The key.
    pub key: String,
    /// The value.
    pub value: Duration,
}

/// What [`TastyNet::new`] takes.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TastyNetConfig {
    /// The directory of the device key, in the app's private storage. The key is made there on
    /// first use and read afterwards.
    pub key_dir: String,
    /// The app's opaque instance ID, sent in handshakes. Must not be empty.
    pub instance_id: String,
    /// The capability names this end offers in handshakes.
    pub caps: Vec<String>,
    /// The P2P path; none for no P2P.
    pub p2p: Option<P2pSetup>,
    /// Settings of this crate; keys not given take their default.
    pub settings: Vec<DurationSetting>,
}

/// How an attach link is opened.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AttachOptions {
    /// Which end holds the workspace: `Dialer` is the forward direction.
    pub holder: LinkHolder,
    /// The domain `ext` (JSON text) for the accepting end's domain check.
    pub ext: Option<String>,
    /// The display name of the peer in the connection list.
    pub name: Option<String>,
    /// The peer's name in the app's [`PeerKeyStore`]; the device ID when none.
    pub peer_name: Option<String>,
}

/// How a request channel is opened.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RequestOptions {
    /// The display name of the peer in the connection list.
    pub name: Option<String>,
    /// The peer's name in the app's [`PeerKeyStore`]; the device ID when none.
    pub peer_name: Option<String>,
}

/// The events of the connection table, pulled by the app on its own thread.
#[derive(Debug, uniffi::Object)]
pub struct EventStream {
    table: ConnectionTable,
    queue: Mutex<VecDeque<ConnectionEvent>>,
    poll: Duration,
    secrets: Secrets,
}

impl EventStream {
    pub(crate) fn new(table: ConnectionTable, poll: Duration, secrets: Secrets) -> Self {
        Self {
            table,
            queue: Mutex::new(VecDeque::new()),
            poll,
            secrets,
        }
    }
}

#[uniffi::export]
impl EventStream {
    /// Waits up to `timeout` for the next event; none when none came. Blocks the calling
    /// thread. After [`NetEvent::Lagged`] read the connection list again. The wait is counted
    /// from the start of the call, so any `timeout` keeps its bound, even one too large for a
    /// point in time.
    pub fn next(&self, timeout: Duration) -> Option<NetEvent> {
        self.next_with(timeout, &mut SystemClock(Instant::now()))
    }
}

impl EventStream {
    /// [`EventStream::next`] on `clock`: the time left is `timeout` less the time `clock`
    /// says has passed since the call started.
    pub(crate) fn next_with(&self, timeout: Duration, clock: &mut dyn Clock) -> Option<NetEvent> {
        loop {
            {
                let mut queue = lock(&self.queue);
                if queue.is_empty() {
                    queue.extend(self.table.events());
                }
                if let Some(event) = queue.pop_front() {
                    return Some(NetEvent::from(&event).scrubbed(&self.secrets));
                }
            }
            let left = timeout.saturating_sub(clock.elapsed());
            if left.is_zero() {
                return None;
            }
            clock.sleep(left.min(self.poll));
        }
    }
}

/// The time source of [`EventStream::next`]; the tests give one that does not wait.
pub(crate) trait Clock {
    /// The time since the call started.
    fn elapsed(&self) -> Duration;
    /// Waits `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// The system's clock, from the start of one call.
struct SystemClock(Instant);

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// What dials: the device key, the handshake settings, the app's trust store and the tokens
/// the dialer showed (scrubbed out of the peer's answers).
pub(crate) struct Dialer {
    pub(crate) config: DialConfig,
    pub(crate) store: Option<Arc<dyn PeerKeyStore>>,
    pub(crate) secrets: Secrets,
}

impl Dialer {
    fn config_for(&self, peer_name: &str) -> DialConfig {
        match &self.store {
            Some(store) => self.config.clone().with_peer_store(
                Arc::new(AppStore(Arc::clone(store))),
                PeerRef::new(peer_name),
            ),
            None => self.config.clone(),
        }
    }

    /// Opens a channel to `target` and runs handshake v2 for an attach link.
    pub(crate) fn attach<T: Transport + ?Sized>(
        &self,
        transport: &T,
        target: &T::Target,
        peer_name: &str,
        options: &AttachOptions,
        cancel: &AttemptCancel,
    ) -> Result<AttachLink, HandshakeFailure> {
        let holder = Holder::from(options.holder);
        let mut connect = ConnectOptions::new().with_holder(holder);
        if let Some(name) = &options.name {
            connect = connect.with_name(name.clone());
        }
        let conn = transport
            .connect_with(target, cancel, &connect)
            .map_err(|e| HandshakeFailure::from(&e).scrubbed(&self.secrets))?;
        self.secrets.remember(conn.endpoint.auth().as_ref());
        let config = self.config_for(peer_name);
        let link = dial_handshake(conn, &config, holder, options.ext.as_deref(), cancel)
            .map_err(|e| HandshakeFailure::from(e).scrubbed(&self.secrets))?;
        AttachLink::new(link, self.secrets.clone())
    }

    /// Opens a channel to `target` and runs handshake v2 for a request channel.
    pub(crate) fn request<T: Transport + ?Sized>(
        &self,
        transport: &T,
        target: &T::Target,
        peer_name: &str,
        options: &RequestOptions,
        cancel: &AttemptCancel,
    ) -> Result<RequestChannel, HandshakeFailure> {
        let mut connect = ConnectOptions::new();
        if let Some(name) = &options.name {
            connect = connect.with_name(name.clone());
        }
        let conn = transport
            .connect_with(target, cancel, &connect)
            .map_err(|e| HandshakeFailure::from(&e).scrubbed(&self.secrets))?;
        self.secrets.remember(conn.endpoint.auth().as_ref());
        let config = self.config_for(peer_name);
        let client = dial_request(conn, &config, cancel)
            .map_err(|e| HandshakeFailure::from(e).scrubbed(&self.secrets))?;
        Ok(RequestChannel::new(client, self.secrets.clone()))
    }
}

/// The bindings' entry: one per app process. It owns the device key, the connection table,
/// the P2P endpoint (when set up) and its runtime; it sets up no process-wide state.
#[derive(uniffi::Object)]
pub struct TastyNet {
    device_id: DeviceId,
    dialer: Dialer,
    table: ConnectionTable,
    p2p: Option<P2pTransport>,
    events: Arc<EventStream>,
    settings: FfiSettings,
}

impl std::fmt::Debug for TastyNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TastyNet")
            .field("device_id", &self.device_id)
            .field("p2p", &self.p2p.is_some())
            .finish_non_exhaustive()
    }
}

fn device_id(text: &str) -> Result<DeviceId, String> {
    DeviceId::from_str(text).map_err(|e| e.to_string())
}

fn bind_p2p(
    key: &DeviceKey,
    key_dir: PathBuf,
    setup: &P2pSetup,
) -> Result<P2pEndpoint, SetupFailure> {
    let addr =
        SocketAddr::from_str(&setup.server_addr).map_err(|e| SetupFailure::InvalidAddress {
            detail: e.to_string(),
        })?;
    let public_key =
        device_id(&setup.server_key).map_err(|detail| SetupFailure::InvalidDeviceId { detail })?;
    let pairing = PairingList::new();
    for paired in &setup.paired_devices {
        pairing.add(device_id(paired).map_err(|detail| SetupFailure::InvalidDeviceId { detail })?);
    }
    let mut config = P2pConfig::new(key.clone(), Arc::new(pairing));
    config.enabled = true;
    config.key_dir = Some(key_dir);
    config.server = Some(RendezvousServer { addr, public_key });
    P2pEndpoint::bind(config).map_err(|e| SetupFailure::from(&e))
}

impl TastyNet {
    fn p2p_target(&self, peer: &str) -> Result<(&P2pTransport, DeviceId), HandshakeFailure> {
        let transport = self.p2p.as_ref().ok_or(HandshakeFailure::NoPath)?;
        let id = device_id(peer).map_err(|detail| HandshakeFailure::InvalidDeviceId { detail })?;
        Ok((transport, id))
    }
}

#[uniffi::export]
impl TastyNet {
    /// Makes or reads the device key in `config.key_dir`, opens the P2P endpoint when
    /// `config.p2p` is given, and takes the app's trust store (none: no TOFU check). Blocks.
    #[uniffi::constructor]
    pub fn new(
        config: TastyNetConfig,
        peer_store: Option<Arc<dyn PeerKeyStore>>,
    ) -> Result<Arc<Self>, SetupFailure> {
        let mut settings = FfiSettings::default();
        for s in &config.settings {
            settings
                .set_duration(&s.key, s.value)
                .map_err(|e| SetupFailure::InvalidSetting {
                    detail: e.to_string(),
                })?;
        }
        // The handshake wire format takes any non-empty `instance_id`; the core `DialConfig`
        // does not check it, so a peer would refuse every handshake of an empty one.
        if config.instance_id.is_empty() {
            return Err(SetupFailure::InvalidInstanceId);
        }
        let key_dir = PathBuf::from(&config.key_dir);
        let key = DeviceKey::load_or_create(&key_dir).map_err(|e| SetupFailure::from(&e))?;
        let dial = DialConfig::new(key.clone(), config.instance_id.clone())
            .with_caps(config.caps.clone())
            .map_err(|_| SetupFailure::InvalidCapabilities)?;
        let table = ConnectionTable::new(ConnectionTableConfig::new());
        let p2p = match &config.p2p {
            Some(setup) => Some(
                P2pTransport::new(bind_p2p(&key, key_dir, setup)?).with_connections(table.clone()),
            ),
            None => None,
        };
        let secrets = Secrets::default();
        let events = Arc::new(EventStream::new(
            table.clone(),
            settings.poll_interval(),
            secrets.clone(),
        ));
        Ok(Arc::new(Self {
            device_id: key.id(),
            dialer: Dialer {
                config: dial,
                store: peer_store,
                secrets,
            },
            table,
            p2p,
            events,
            settings,
        }))
    }

    /// This device's ID. The secret key never leaves the library.
    pub fn device_id(&self) -> String {
        self.device_id.to_string()
    }

    /// A snapshot of every connection, oldest first.
    pub fn connections(&self) -> Vec<ConnectionRecord> {
        self.table
            .connections()
            .iter()
            .map(|info| ConnectionRecord::from(info).scrubbed(&self.dialer.secrets))
            .collect()
    }

    /// A snapshot of connection `id`, if the table still has it.
    pub fn connection(&self, id: u64) -> Option<ConnectionRecord> {
        self.table
            .connections()
            .iter()
            .find(|info| info.id.get() == id)
            .map(|info| ConnectionRecord::from(info).scrubbed(&self.dialer.secrets))
    }

    /// The event stream of the connection table. Every call returns the same stream; events
    /// read through it are gone for every caller.
    pub fn events(&self) -> Arc<EventStream> {
        Arc::clone(&self.events)
    }

    /// Every setting in effect with its source: this crate's, the connection table's and the
    /// P2P endpoint's.
    pub fn settings(&self) -> Vec<SettingEntry> {
        let own = self
            .settings
            .entries()
            .into_iter()
            .map(|(key, value, source)| SettingEntry {
                key: key.to_string(),
                value: show(value),
                source: source.to_string(),
            });
        let table = self.table.settings();
        let p2p = self
            .p2p
            .iter()
            .flat_map(|t| t.endpoint().settings())
            .map(|entry| SettingEntry {
                key: entry.key.to_string(),
                value: entry.value.to_string(),
                source: entry.source.to_string(),
            });
        own.chain(table.iter().map(SettingEntry::from))
            .chain(p2p)
            .collect()
    }

    /// Opens an attach link to the device `peer_device_id` over P2P and runs handshake v2.
    /// Blocks until the link is up or the attempt failed; `cancel` ends it from another thread.
    pub fn open_attach(
        &self,
        peer_device_id: String,
        options: AttachOptions,
        cancel: Option<Arc<CancelHandle>>,
    ) -> Result<Arc<AttachLink>, HandshakeFailure> {
        let (transport, target) = self.p2p_target(&peer_device_id)?;
        let peer_name = options.peer_name.clone().unwrap_or(peer_device_id);
        let own = AttemptCancel::new();
        let cancel = cancel.as_ref().map_or(&own, |c| &c.inner);
        self.dialer
            .attach(transport, &target, &peer_name, &options, cancel)
            .map(Arc::new)
    }

    /// Opens a request channel to the device `peer_device_id` over P2P. Blocks like
    /// [`TastyNet::open_attach`].
    pub fn open_request(
        &self,
        peer_device_id: String,
        options: RequestOptions,
        cancel: Option<Arc<CancelHandle>>,
    ) -> Result<Arc<RequestChannel>, HandshakeFailure> {
        let (transport, target) = self.p2p_target(&peer_device_id)?;
        let peer_name = options.peer_name.clone().unwrap_or(peer_device_id);
        let own = AttemptCancel::new();
        let cancel = cancel.as_ref().map_or(&own, |c| &c.inner);
        self.dialer
            .request(transport, &target, &peer_name, &options, cancel)
            .map(Arc::new)
    }

    /// The user's re-registration of `peer` with `device_id` after a key change
    /// (`HandshakeFailure::PeerKeyMismatch`), through the store's `replace`.
    pub fn reregister_peer(&self, peer: String, device_id: String) -> Result<(), PeerStoreError> {
        let store = self
            .dialer
            .store
            .as_ref()
            .ok_or_else(|| PeerStoreError::Failed {
                reason: "no peer key store was given".to_string(),
            })?;
        reregister_with(store, &peer, &device_id)
    }

    /// Closes the P2P endpoint; links over it end. Opening needs a new [`TastyNet`].
    pub fn shutdown(&self) {
        if let Some(p2p) = &self.p2p {
            p2p.endpoint().close();
        }
    }
}
