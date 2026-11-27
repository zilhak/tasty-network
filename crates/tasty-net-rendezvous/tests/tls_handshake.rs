//! The first-stage check of docs/design/p2p.md "인증": quinn takes the rustls raw public key
//! configurations as they are, the handshake completes with the ring provider, both sides see
//! the other's device ID, and a wrong key or ALPN fails inside the handshake.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use tasty_net_identity::DeviceKey;
use tasty_net_rendezvous::tls::{self, TlsOptions};
use tasty_net_rendezvous::wire::ALPN_RENDEZVOUS;

const DEADLINE: Duration = Duration::from_secs(10);

fn server(key: &DeviceKey, alpn: &[u8]) -> quinn::Endpoint {
    let crypto = tls::server_config(key, alpn, TlsOptions::default()).unwrap();
    let config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto).unwrap()));
    quinn::Endpoint::server(config, SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap()
}

fn client() -> quinn::Endpoint {
    quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap()
}

fn client_config(key: &DeviceKey, expected: &DeviceKey, alpn: &[u8]) -> quinn::ClientConfig {
    let crypto = tls::client_config(key, expected.id(), alpn, TlsOptions::default()).unwrap();
    quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()))
}

#[tokio::test]
async fn raw_public_key_handshake_identifies_both_sides() {
    let server_key = DeviceKey::from_seed([1; 32]);
    let device_key = DeviceKey::from_seed([2; 32]);
    let server = server(&server_key, ALPN_RENDEZVOUS);
    let addr = server.local_addr().unwrap();

    let accept = tokio::spawn(async move {
        let incoming = server.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        let seen = tls::peer_device_id(&conn);
        (seen, conn, server)
    });

    let client = client();
    let conn = tokio::time::timeout(
        DEADLINE,
        client
            .connect_with(
                client_config(&device_key, &server_key, ALPN_RENDEZVOUS),
                addr,
                tls::SERVER_NAME,
            )
            .unwrap(),
    )
    .await
    .expect("handshake in time")
    .expect("handshake succeeds");
    assert_eq!(tls::peer_device_id(&conn), Some(server_key.id()));
    let (seen, server_conn, _server) = tokio::time::timeout(DEADLINE, accept)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen, Some(device_key.id()));
    assert_eq!(
        server_conn.handshake_data().and_then(|d| d
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .ok()
            .and_then(|d| d.protocol)),
        Some(ALPN_RENDEZVOUS.to_vec())
    );
}

#[tokio::test]
async fn wrong_server_key_fails_in_handshake() {
    let server_key = DeviceKey::from_seed([1; 32]);
    let other_key = DeviceKey::from_seed([3; 32]);
    let device_key = DeviceKey::from_seed([2; 32]);
    let server = server(&server_key, ALPN_RENDEZVOUS);
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            if let Ok(conn) = incoming.await {
                conn.closed().await;
            }
        }
    });
    let client = client();
    let got = tokio::time::timeout(
        DEADLINE,
        client
            .connect_with(
                client_config(&device_key, &other_key, ALPN_RENDEZVOUS),
                addr,
                tls::SERVER_NAME,
            )
            .unwrap(),
    )
    .await
    .expect("ends in time");
    let err = got.expect_err("the key does not match");
    assert!(
        matches!(err, quinn::ConnectionError::TransportError(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn wrong_alpn_fails_in_handshake() {
    let server_key = DeviceKey::from_seed([1; 32]);
    let device_key = DeviceKey::from_seed([2; 32]);
    let server = server(&server_key, ALPN_RENDEZVOUS);
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            if let Ok(conn) = incoming.await {
                conn.closed().await;
            }
        }
    });
    let client = client();
    let got = tokio::time::timeout(
        DEADLINE,
        client
            .connect_with(
                client_config(&device_key, &server_key, b"tasty-rendezvous/2"),
                addr,
                tls::SERVER_NAME,
            )
            .unwrap(),
    )
    .await
    .expect("ends in time");
    match got.expect_err("ALPN does not match") {
        // TLS alert no_application_protocol (120) as a QUIC crypto error 0x100 + 120.
        quinn::ConnectionError::ConnectionClosed(close) => {
            assert_eq!(u64::from(close.error_code), 0x100 + 120, "{close:?}");
        }
        other => panic!("expected a TLS alert close, got {other:?}"),
    }
}
