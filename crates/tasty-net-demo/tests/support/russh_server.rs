//! The test-only Rust SSH server, the fallback when OpenSSH sshd cannot serve
//! (docs/design/testing.md "Windows" 2).
//!
//! It runs on a tokio runtime inside the test process and listens on a free loopback port. It
//! lets in one public key (written as an OpenSSH private key file for the system `ssh`), and
//! takes two requests:
//! - `exec`: runs the command with the OS shell (`sh -c`, Windows `cmd /C`) as the test user
//!   and returns its stdout, stderr and exit code. Port discovery reads the record this way.
//! - `direct-tcpip` (`ssh -L`): connects to the requested address and carries bytes both
//!   ways. A refused connect is answered with the SSH "connect failed" open failure, as
//!   OpenSSH does.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{
    Auth, ChannelOpenHandle, Config, Handler, Msg, PublicKeyOffer, RunningServerHandle, Server,
    Session, VerifiedPublicKey,
};
use russh::{Channel, ChannelId, ChannelOpenFailure};
use tasty_net_identity::DeviceKey;

pub struct RusshServer {
    runtime: tokio::runtime::Runtime,
    handle: RunningServerHandle,
    port: u16,
}

/// 32 random bytes, from the identity crate's OS randomness.
fn seed() -> Result<[u8; 32], String> {
    DeviceKey::generate()
        .map(|k| k.to_seed())
        .map_err(|e| format!("random: {e}"))
}

fn ed25519_key() -> Result<PrivateKey, String> {
    let pair = Ed25519Keypair::from_seed(&seed()?);
    PrivateKey::new(KeypairData::Ed25519(pair), "tasty-net-test").map_err(|e| e.to_string())
}

/// Writes `key` as an OpenSSH private key file only this user can read (ssh refuses a key
/// file others can read).
fn write_private_key(key: &PrivateKey, path: &Path) -> Result<(), String> {
    let text = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("encoding the client key: {e}"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

impl RusshServer {
    /// Starts the server and writes the one client key it lets in to `client_key`.
    pub fn start(client_key: &Path) -> Result<Self, String> {
        let client = ed25519_key()?;
        write_private_key(&client, client_key)?;
        let config = Arc::new(Config {
            keys: vec![ed25519_key()?],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Config::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("russh-test-server")
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let mut server = TestServer {
            allowed: Arc::new(client.public_key().clone()),
        };
        runtime.spawn(async move {
            let socket = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(socket) => socket,
                Err(e) => {
                    let _sent = tx.send(Err(format!("bind: {e}"))).is_ok();
                    return;
                }
            };
            let port = match socket.local_addr() {
                Ok(addr) => addr.port(),
                Err(e) => {
                    let _sent = tx.send(Err(format!("local_addr: {e}"))).is_ok();
                    return;
                }
            };
            let running = server.run_on_socket(config, &socket);
            let _sent = tx.send(Ok((running.handle(), port))).is_ok();
            if let Err(e) = running.await {
                super::process::log(&format!("the russh test server ended: {e}"));
            }
        });
        let (handle, port) = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("the russh test server did not start: {e}"))??;
        Ok(Self {
            runtime,
            handle,
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Ends the server and its runtime.
    pub fn stop(self) {
        self.handle.shutdown("test over".into());
        self.runtime.shutdown_timeout(Duration::from_secs(5));
    }
}

#[derive(Clone)]
struct TestServer {
    allowed: Arc<PublicKey>,
}

impl Server for TestServer {
    type Handler = Self;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl TestServer {
    fn judge(&self, key: &PublicKey) -> Auth {
        if key.key_data() == self.allowed.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        }
    }
}

/// Runs `command` with the OS shell. Returns stdout, stderr and the exit code.
fn run_shell(command: &str) -> (Vec<u8>, Vec<u8>, u32) {
    #[cfg(windows)]
    let mut shell = {
        // The command line goes to cmd as the client sent it, as Windows OpenSSH runs
        // `cmd.exe /c <command>`; quoting it as one argument would change its quotes.
        let mut c = Command::new("cmd");
        std::os::windows::process::CommandExt::raw_arg(c.arg("/C"), command);
        c
    };
    #[cfg(not(windows))]
    let mut shell = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    match shell.stdin(std::process::Stdio::null()).output() {
        Ok(out) => {
            let code = out
                .status
                .code()
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(255);
            (out.stdout, out.stderr, code)
        }
        Err(e) => (Vec::new(), format!("{e}\n").into_bytes(), 127),
    }
}

impl Handler for TestServer {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(self.judge(offer.public_key))
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(self.judge(key.public_key))
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        session.channel_success(channel)?;
        let handle = session.handle();
        tokio::spawn(async move {
            let (stdout, stderr, code) = tokio::task::spawn_blocking(move || run_shell(&command))
                .await
                .unwrap_or_else(|e| (Vec::new(), format!("{e}\n").into_bytes(), 255));
            let _out = handle.data(channel, stdout).await.is_ok();
            let _err = handle.extended_data(channel, 1, stderr).await.is_ok();
            let _status = handle.exit_status_request(channel, code).await.is_ok();
            let _eof = handle.eof(channel).await.is_ok();
            let _closed = handle.close(channel).await.is_ok();
        });
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(port) = u16::try_from(port_to_connect) else {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        match tokio::net::TcpStream::connect((host_to_connect, port)).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _copied = tokio::io::copy_bidirectional(&mut stream, &mut tcp)
                        .await
                        .is_ok();
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}
