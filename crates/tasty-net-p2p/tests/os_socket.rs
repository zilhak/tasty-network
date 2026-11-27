//! Two endpoints on OS UDP sockets over loopback in one process, with a rendezvous server on
//! OS sockets too: what only a real socket shows (docs/design/testing.md "크레이트 단독").

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::{
    P2pCloseReason, P2pConfig, P2pConnectError, P2pConnection, P2pEndpoint, PairingList,
    RendezvousServer,
};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value};

/// A rendezvous server on loopback OS sockets.
struct Rendezvous {
    rt: tokio::runtime::Runtime,
    server: Option<Server>,
    info: RendezvousServer,
}

impl Rendezvous {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut settings = ServerSettings::new();
        settings
            .set(
                "listen.primary",
                Value::Address(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            )
            .unwrap();
        settings
            .set("listen.alternate_port", Value::Port(0))
            .unwrap();
        let mut config = ServerConfig::new(DeviceKey::from_seed([200; 32]));
        config.settings = settings;
        let server = rt.block_on(Server::start(config)).expect("server");
        let info = RendezvousServer {
            addr: server.addrs().primary,
            public_key: server.public_key(),
        };
        Rendezvous {
            rt,
            server: Some(server),
            info,
        }
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            self.rt.block_on(s.shutdown());
        }
    }
}

fn endpoint(seed: u8, server: RendezvousServer, peer: DeviceId) -> P2pEndpoint {
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(DeviceKey::from_seed([seed; 32]), pairing);
    config.enabled = true;
    config.server = Some(server);
    P2pEndpoint::bind(config).expect("bind")
}

/// Dial until the peer is registered, then take its side of the connection.
fn connect(a: &P2pEndpoint, b: &P2pEndpoint) -> (P2pConnection, P2pConnection) {
    let end = Instant::now() + Duration::from_secs(30);
    let taken = std::thread::scope(|s| {
        let accepted = s.spawn(|| b.accept_timeout(Some(Duration::from_secs(30))));
        let dialed = loop {
            match a.connect(b.id()) {
                Ok(c) => break c,
                Err(e)
                    if Instant::now() < end
                        && matches!(
                            e.connect_error(),
                            Some(
                                P2pConnectError::PeerNotFound
                                    | P2pConnectError::RendezvousUnavailable { .. }
                            )
                        ) =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("connect: {e:?}"),
            }
        };
        (dialed, accepted.join().unwrap())
    });
    (taken.0, taken.1.expect("accept"))
}

/// The accepting side reports the local IP its connection was taken on, from the destination
/// of the datagrams (Linux packet info). The endpoint is bound to the wildcard address, so the
/// bound address alone does not tell it.
#[test]
fn the_acceptor_reports_the_local_ip_of_its_connection() {
    let server = Rendezvous::start();
    let a_id = DeviceKey::from_seed([1; 32]).id();
    let b_id = DeviceKey::from_seed([2; 32]).id();
    let a = endpoint(1, server.info, b_id);
    let b = endpoint(2, server.info, a_id);
    let (ca, cb) = connect(&a, &b);
    let info = cb.info().expect("info");
    if cfg!(target_os = "linux") {
        assert_eq!(info.local_ip, Some(IpAddr::from(Ipv4Addr::LOCALHOST)));
    } else {
        assert!(info.local_ip.is_none_or(|ip| ip.is_loopback()), "{info:?}");
    }
    ca.close(P2pCloseReason::Closed);
    a.close();
    b.close();
}

/// `close` gives the OS socket up while the endpoint handle lives: its address can be bound
/// again (docs/design/p2p.md "상태 기계", `Off`: no socket).
#[test]
fn close_gives_the_os_socket_up() {
    let server = Rendezvous::start();
    let ep = endpoint(3, server.info, DeviceKey::from_seed([4; 32]).id());
    let addr = ep.local_addr().expect("bound");
    // While the endpoint is on, the address is taken.
    assert!(std::net::UdpSocket::bind(addr).is_err());
    let handle = ep.clone();
    ep.close();
    assert_eq!(handle.local_addr(), None);
    std::net::UdpSocket::bind(addr).expect("the address is free again");
}
