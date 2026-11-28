//! The rendezvous server of the two-process P2P tests: `tasty-net-rendezvous` in the test's
//! own process, on a free loopback port (docs/design/testing.md "P2P 시험").

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value};

use super::process::log;

/// How long the server's shutdown may take. The devices close their server connections
/// before they exit, so it ends at once; a device that was killed leaves its connection to
/// the server's idle timeout, which is longer.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(10);

/// A running server and the runtime it runs on. Dropping it shuts the server down, waiting at
/// most [`SHUTDOWN_LIMIT`].
pub struct Rendezvous {
    rt: tokio::runtime::Runtime,
    server: Option<Server>,
    /// The address the devices are given (`--server`).
    pub addr: SocketAddr,
    /// The key the devices pin (`--server-key`).
    pub public_key: DeviceId,
}

impl Rendezvous {
    /// A server with its primary and alternate ports on free loopback ports.
    pub fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("a runtime for the rendezvous server");
        let mut settings = ServerSettings::new();
        settings
            .set(
                "listen.primary",
                Value::Address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            )
            .expect("listen.primary");
        settings
            .set("listen.alternate_port", Value::Port(0))
            .expect("listen.alternate_port");
        let mut config =
            ServerConfig::new(DeviceKey::generate().expect("a key for the rendezvous server"));
        config.settings = settings;
        let server = rt
            .block_on(Server::start(config))
            .expect("the rendezvous server starts");
        let (addr, public_key) = (server.addrs().primary, server.public_key());
        log(&format!("rendezvous server on {addr}"));
        Self {
            rt,
            server: Some(server),
            addr,
            public_key,
        }
    }

    /// Shuts the server down; whether that ended within [`SHUTDOWN_LIMIT`].
    fn shut(&mut self) -> bool {
        let Some(server) = self.server.take() else {
            return true;
        };
        self.rt
            .block_on(async { tokio::time::timeout(SHUTDOWN_LIMIT, server.shutdown()).await })
            .is_ok()
    }

    /// Shuts the server down and checks that it ended in time: every device closed its
    /// connection to it.
    pub fn stop(mut self) {
        assert!(
            self.shut(),
            "the rendezvous server did not shut down within {SHUTDOWN_LIMIT:?}"
        );
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        if !self.shut() {
            log(&format!(
                "the rendezvous server did not shut down within {SHUTDOWN_LIMIT:?}"
            ));
        }
    }
}
