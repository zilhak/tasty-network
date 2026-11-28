//! A russh server inside the test process for the embedded backend's tests. It listens on a
//! free loopback port, lets in one client key and records what the client sent, so a test can
//! see the channel requests of the embedded backend from the other end.

use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{
    Auth, ChannelOpenHandle, Config, Handler, Msg, PublicKeyOffer, RunningServerHandle, Server,
    Session, VerifiedPublicKey,
};
use russh::{Channel, ChannelId, ChannelOpenFailure};

/// How the server answers an `exec` request.
#[derive(Clone, Copy)]
pub(crate) enum ExecReply {
    /// Accepts it and never writes or closes.
    Silent,
    /// Accepts it and writes this many bytes of stdout, then nothing more.
    Flood(usize),
    /// Confirms the `session` channel open only after this long, then stays silent.
    LateOpen(Duration),
    /// Never answers the `session` channel open (neither confirms nor refuses it).
    NeverOpen,
}

/// How the server answers a `direct-tcpip` open.
#[derive(Clone, Copy)]
pub(crate) enum DirectReply {
    /// Refuses it with this reason code (RFC 4254 5.1: 1 to 4).
    Refuse(u32),
    /// Answers by the originator port (the client's local connection) modulo 4: 0 opens the
    /// channel and closes it at once, 1 refuses with code 1, 2 with code 3, 3 with code 4.
    ByPort,
    /// Opens the channel and closes it at once.
    Open,
}

impl DirectReply {
    /// The refusal code for the originator port `port` ([`Self::ByPort`]); `None` opens.
    pub(crate) fn by_port(port: u32) -> Option<u32> {
        [None, Some(1), Some(3), Some(4)][usize::try_from(port % 4).unwrap_or_default()]
    }
}

/// Which authentication lets the client in.
#[derive(Clone, Copy)]
pub(crate) enum AuthRule {
    /// The client key.
    Key,
    /// This password first (a partial success that asks for `publickey` next), then the
    /// client key. A key offered before the password is refused.
    PasswordThenKey(&'static str),
}

/// What the server saw, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Exec(ChannelId),
    Close(ChannelId),
    /// A public key offered or signed with, by its algorithm name.
    Key(String),
    /// A certificate offered.
    Cert,
    /// A password.
    Password,
}

pub(crate) struct TestServer {
    runtime: Option<tokio::runtime::Runtime>,
    handle: RunningServerHandle,
    port: u16,
    events: Arc<Mutex<Vec<Event>>>,
    opened: Arc<Mutex<Vec<ChannelId>>>,
    host_key: PublicKey,
}

/// The client key the server lets in.
pub(crate) fn client_key() -> PrivateKey {
    key(7)
}

fn key(seed: u8) -> PrivateKey {
    let pair = Ed25519Keypair::from_seed(&[seed; 32]);
    PrivateKey::new(KeypairData::Ed25519(pair), "tasty-net-ssh-test").expect("test key")
}

impl TestServer {
    /// Starts the server and writes the client key it lets in to `client_key` (mode 0600).
    pub(crate) fn start(client_key: &Path, exec: ExecReply, direct: DirectReply) -> Self {
        Self::start_with(client_key, exec, direct, AuthRule::Key)
    }

    /// [`Self::start`] with the authentication `rule`.
    pub(crate) fn start_with(
        client_key: &Path,
        exec: ExecReply,
        direct: DirectReply,
        rule: AuthRule,
    ) -> Self {
        let client = self::client_key();
        let text = client.to_openssh(LineEnding::LF).expect("encode");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        std::io::Write::write_all(
            &mut options.open(client_key).expect("key file"),
            text.as_bytes(),
        )
        .expect("write key");
        let host = key(9);
        let host_key = host.public_key().clone();
        let config = Arc::new(Config {
            keys: vec![host],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Config::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let events = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let mut server = Handle {
            opened: Arc::clone(&opened),
            allowed: Arc::new(client.public_key().clone()),
            exec,
            direct,
            rule,
            password_ok: false,
            events: Arc::clone(&events),
        };
        let (tx, rx) = mpsc::channel();
        runtime.spawn(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind");
            let port = socket.local_addr().expect("addr").port();
            let running = server.run_on_socket(config, &socket);
            let _sent = tx.send((running.handle(), port)).is_ok();
            let _ended = running.await.is_ok();
        });
        let (handle, port) = rx.recv_timeout(Duration::from_secs(10)).expect("started");
        Self {
            runtime: Some(runtime),
            handle,
            port,
            events,
            opened,
            host_key,
        }
    }

    /// The `known_hosts` line of the server for `127.0.0.1` on its port.
    pub(crate) fn known_hosts_line(&self) -> String {
        format!(
            "[127.0.0.1]:{} {}",
            self.port,
            self.host_key.to_openssh().expect("public key")
        )
    }

    /// The configuration that points `h.example` at this server with only `key` and the
    /// known_hosts file `known`.
    pub(crate) fn config(&self, key: &Path, known: &Path) -> String {
        format!(
            "Host *\n  HostName 127.0.0.1\n  Port {}\n  User u\n  IdentityFile \"{}\"\n  \
             IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile \"{}\"\n  \
             StrictHostKeyChecking yes\n  ConnectTimeout 10\n",
            self.port,
            key.display(),
            known.display()
        )
    }

    /// Starts an SSH agent on the Unix socket `socket` that holds `keys`.
    #[cfg(unix)]
    pub(crate) fn agent(&self, socket: &Path, keys: &[PrivateKey]) {
        use russh::keys::agent::client::AgentClient;
        let runtime = self.runtime.as_ref().expect("runtime");
        let listener = runtime
            .block_on(async { tokio::net::UnixListener::bind(socket) })
            .expect("agent socket");
        let connections = futures::stream::unfold(listener, |listener| async move {
            let next = listener.accept().await.map(|(stream, _)| stream);
            Some((next, listener))
        });
        runtime.spawn(async move {
            let _served = russh::keys::agent::server::serve(Box::pin(connections), ())
                .await
                .is_ok();
        });
        runtime.block_on(async {
            let mut client = AgentClient::connect_uds(socket).await.expect("agent");
            for key in keys {
                client.add_identity(key, &[]).await.expect("add key");
            }
        });
    }

    /// The `session` channels the server confirmed.
    pub(crate) fn opened(&self) -> Vec<ChannelId> {
        self.opened.lock().expect("opened").clone()
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.events.lock().expect("events").clone()
    }

    /// Waits up to `limit` for `want` to hold on the events.
    pub(crate) fn wait_for(&self, limit: Duration, want: impl Fn(&[Event]) -> bool) -> bool {
        let end = Instant::now() + limit;
        loop {
            if want(&self.events()) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown("test over".into());
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

#[derive(Clone)]
struct Handle {
    allowed: Arc<PublicKey>,
    exec: ExecReply,
    direct: DirectReply,
    rule: AuthRule,
    /// The password of [`AuthRule::PasswordThenKey`] came on this connection.
    password_ok: bool,
    events: Arc<Mutex<Vec<Event>>>,
    /// The `session` channels the server confirmed, in order.
    opened: Arc<Mutex<Vec<ChannelId>>>,
}

impl Handle {
    fn judge(&self, key: &PublicKey) -> Auth {
        let in_order = match self.rule {
            AuthRule::Key => true,
            AuthRule::PasswordThenKey(_) => self.password_ok,
        };
        if in_order && key.key_data() == self.allowed.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        }
    }

    fn note(&self, event: Event) {
        self.events.lock().expect("events").push(event);
    }
}

impl Server for Handle {
    type Handler = Self;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl Handler for Handle {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        self.note(Event::Key(
            offer.public_key.algorithm().as_str().to_string(),
        ));
        Ok(self.judge(offer.public_key))
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        self.note(Event::Key(key.public_key.algorithm().as_str().to_string()));
        Ok(self.judge(key.public_key))
    }

    async fn auth_openssh_certificate(
        &mut self,
        _user: &str,
        _certificate: &russh::keys::Certificate,
    ) -> Result<Auth, Self::Error> {
        self.note(Event::Cert);
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.note(Event::Password);
        match self.rule {
            AuthRule::PasswordThenKey(want) if password == want => {
                self.password_ok = true;
                Ok(Auth::Reject {
                    proceed_with_methods: Some(russh::MethodSet::from(
                        &[russh::MethodKind::PublicKey][..],
                    )),
                    partial_success: true,
                })
            }
            _ => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let id = channel.id();
        let opened = Arc::clone(&self.opened);
        let late = match self.exec {
            ExecReply::LateOpen(after) => after,
            ExecReply::Silent | ExecReply::Flood(_) => Duration::ZERO,
            ExecReply::NeverOpen => {
                // The answer is held, not dropped, so russh sends no refusal either.
                tokio::spawn(async move {
                    let _held = (reply, channel);
                    std::future::pending::<()>().await;
                });
                return Ok(());
            }
        };
        // The confirmation goes from its own task, so the server reads the client meanwhile.
        tokio::spawn(async move {
            tokio::time::sleep(late).await;
            reply.accept().await;
            opened.lock().expect("opened").push(id);
            drop(channel);
        });
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.note(Event::Exec(channel));
        session.channel_success(channel)?;
        if let ExecReply::Flood(n) = self.exec {
            let handle = session.handle();
            tokio::spawn(async move {
                let _sent = handle.data(channel, vec![b'x'; n]).await.is_ok();
            });
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.note(Event::Close(channel));
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let code = match self.direct {
            DirectReply::Refuse(code) => code,
            DirectReply::Open => {
                reply.accept().await;
                let _closed = channel.close().await.is_ok();
                return Ok(());
            }
            DirectReply::ByPort => match DirectReply::by_port(originator_port) {
                Some(code) => code,
                None => {
                    reply.accept().await;
                    let _closed = channel.close().await.is_ok();
                    return Ok(());
                }
            },
        };
        let reason = match code {
            1 => ChannelOpenFailure::AdministrativelyProhibited,
            2 => ChannelOpenFailure::ConnectFailed,
            3 => ChannelOpenFailure::UnknownChannelType,
            _ => ChannelOpenFailure::ResourceShortage,
        };
        reply.reject(reason).await;
        Ok(())
    }
}
