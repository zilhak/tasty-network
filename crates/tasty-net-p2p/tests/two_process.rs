//! Two OS processes punch and talk over loopback UDP sockets (docs/design/testing.md "크레이트
//! 단독"). The test binary runs itself again as the second device; the rendezvous server runs
//! in the parent.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tasty_net_identity::{DeviceId, DeviceKey};
use tasty_net_p2p::{
    P2pCloseReason, P2pConfig, P2pConnectError, P2pEndpoint, PairingList, RendezvousServer,
};
use tasty_net_rendezvous::server::{Server, ServerConfig};
use tasty_net_rendezvous::settings::{ServerSettings, Value};
use tasty_net_rendezvous::wire::message::NatMapping;

const TEST: &str = "two_process_punch_and_echo";
const CHILD: &str = "TASTY_P2P_TWO_PROCESS_CHILD";
const SERVER: &str = "TASTY_P2P_TWO_PROCESS_SERVER";
const SERVER_KEY: &str = "TASTY_P2P_TWO_PROCESS_SERVER_KEY";
const PEER: &str = "TASTY_P2P_TWO_PROCESS_PEER";

/// Ends the child if the parent fails first.
struct Reap(std::process::Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _waited = self.0.wait();
    }
}

fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

fn endpoint(seed: u8, server: RendezvousServer, peer: DeviceId) -> P2pEndpoint {
    let pairing = Arc::new(PairingList::new());
    pairing.add(peer);
    let mut config = P2pConfig::new(key(seed), pairing);
    config.enabled = true;
    config.server = Some(server);
    P2pEndpoint::bind(config).expect("bind")
}

/// The second device: take one connection, echo one stream, wait for the peer to close.
fn child() {
    let server = RendezvousServer {
        addr: std::env::var(SERVER).unwrap().parse().unwrap(),
        public_key: std::env::var(SERVER_KEY).unwrap().parse().unwrap(),
    };
    let peer: DeviceId = std::env::var(PEER).unwrap().parse().unwrap();
    let ep = endpoint(2, server, peer);
    let conn = ep
        .accept_timeout(Some(Duration::from_secs(60)))
        .expect("accept");
    assert_eq!(conn.remote_device_id(), peer);
    let mut s = conn
        .accept_bi(Some(Duration::from_secs(30)))
        .expect("stream");
    let mut got = Vec::new();
    s.read_to_end(&mut got).unwrap();
    s.write_all(&got).unwrap();
    s.finish().unwrap();
    assert!(conn.wait_closed(Duration::from_secs(30)).is_ok());
    assert_eq!(conn.close_reason(), Some(P2pCloseReason::Closed));
    ep.close();
}

#[test]
fn two_process_punch_and_echo() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
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
    // The second server IP: 127.0.0.2 on Linux, where all of 127/8 is local
    // (docs/design/testing.md "모의 NAT").
    let second_ip = cfg!(target_os = "linux");
    if second_ip {
        settings
            .set(
                "listen.alternate_ip",
                Value::Ip(Ipv4Addr::new(127, 0, 0, 2).into()),
            )
            .unwrap();
    }
    let mut config = ServerConfig::new(key(200));
    config.settings = settings;
    let server = rt.block_on(Server::start(config)).expect("server");
    let info = RendezvousServer {
        addr: server.addrs().primary,
        public_key: server.public_key(),
    };
    let me = key(1).id();
    let peer = key(2).id();
    let mut child = Reap(
        Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture", "--test-threads", "1"])
            .env(CHILD, "1")
            .env(SERVER, info.addr.to_string())
            .env(SERVER_KEY, info.public_key.to_string())
            .env(PEER, me.to_string())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("child"),
    );
    let ep = endpoint(1, info, peer);
    // Without a NAT the server's addresses all see one mapping.
    let judged = Instant::now() + Duration::from_secs(30);
    while ep
        .status()
        .nat_answers
        .iter()
        .all(|a| a.reflexive.is_none())
        && Instant::now() < judged
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let want = if second_ip {
        NatMapping::Independent
    } else {
        NatMapping::IndependentPortOnly
    };
    assert_eq!(ep.status().nat, want, "{:?}", ep.status().nat_answers);
    // The child registers in its own time.
    let end = Instant::now() + Duration::from_secs(60);
    let conn = loop {
        match ep.connect(peer) {
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
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => panic!("connect: {e:?}"),
        }
    };
    assert_eq!(conn.remote_device_id(), peer);
    let mut s = conn.open_bi().unwrap();
    s.write_all(b"two processes").unwrap();
    s.finish().unwrap();
    let mut back = Vec::new();
    s.read_to_end(&mut back).unwrap();
    assert_eq!(back, b"two processes");
    let info = conn.info().unwrap();
    assert!(info.remote_candidate.is_some());
    conn.close(P2pCloseReason::Closed);
    let status = child.0.wait().expect("child ends");
    assert!(status.success(), "{status:?}");
    ep.close();
    rt.block_on(server.shutdown());
}
