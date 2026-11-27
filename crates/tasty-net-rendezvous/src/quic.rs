//! quinn configurations from the settings (docs/design/p2p.md "의존성이 노출하는 값",
//! "고정값"). Every value comes from [`QuicParams`]; the fixed values are set here.

use std::sync::Arc;

use quinn::crypto::rustls::QuicServerConfig;
use quinn::{
    AckFrequencyConfig, EndpointConfig, MtuDiscoveryConfig, TransportConfig, ValidationTokenConfig,
    VarInt,
};
use quinn_proto::HashedConnectionIdGenerator;

use crate::settings::QuicParams;

/// QUIC version 1 (RFC 9000 §15), the only supported version (fixed `quic.supported_versions`).
pub const QUIC_VERSION_1: u32 = 0x0000_0001;

fn var_int(n: u64) -> VarInt {
    VarInt::from_u64(n).expect("the settings check the VarInt range")
}

/// The transport configuration.
pub fn transport_config(p: &QuicParams) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(var_int(p.max_bidi_streams))
        .max_concurrent_uni_streams(var_int(p.max_uni_streams))
        .max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(p.idle_timeout).expect("the settings check the range"),
        ))
        .keep_alive_interval(Some(p.keepalive_interval))
        .initial_rtt(p.initial_rtt)
        .packet_threshold(p.packet_threshold)
        .time_threshold(p.time_threshold)
        .persistent_congestion_threshold(p.persistent_congestion_threshold)
        .stream_receive_window(var_int(p.stream_receive_window))
        .receive_window(var_int(p.receive_window))
        .send_window(p.send_window)
        .initial_mtu(p.initial_mtu)
        .min_mtu(p.min_mtu)
        .crypto_buffer_size(p.crypto_buffer_size)
        .datagram_receive_buffer_size(p.datagram_receive_buffer_size)
        .datagram_send_buffer_size(p.datagram_send_buffer_size);
    let mut mtu = MtuDiscoveryConfig::default();
    mtu.interval(p.mtu_interval)
        .upper_bound(p.mtu_upper_bound)
        .black_hole_cooldown(p.mtu_black_hole_cooldown)
        .minimum_change(p.mtu_minimum_change);
    t.mtu_discovery_config(Some(mtu));
    if p.ack_frequency {
        let mut ack = AckFrequencyConfig::default();
        ack.ack_eliciting_threshold(var_int(p.ack_eliciting_threshold))
            .max_ack_delay(p.max_ack_delay)
            .reordering_threshold(var_int(p.reordering_threshold));
        t.ack_frequency_config(Some(ack));
    } else {
        t.ack_frequency_config(None);
    }
    let mut cubic = quinn::congestion::CubicConfig::default();
    cubic.initial_window(p.cubic_initial_window);
    t.congestion_controller_factory(Arc::new(cubic));
    t
}

/// The endpoint configuration, with the fixed values (grease off, QUIC v1 only).
pub fn endpoint_config(p: &QuicParams) -> EndpointConfig {
    let mut e = EndpointConfig::default();
    e.max_udp_payload_size(p.max_udp_payload_size)
        .expect("the settings check the range");
    e.min_reset_interval(p.min_reset_interval)
        .grease_quic_bit(false)
        .supported_versions(vec![QUIC_VERSION_1]);
    let lifetime = p.cid_lifetime;
    e.cid_generator(move || {
        let mut g = HashedConnectionIdGenerator::new();
        if let Some(d) = lifetime {
            g.set_lifetime(d);
        }
        Box::new(g)
    });
    e
}

/// The server configuration over a rustls server configuration. No preferred address
/// (fixed `quic.preferred_address`).
pub fn server_config(p: &QuicParams, crypto: QuicServerConfig) -> quinn::ServerConfig {
    let mut s = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    s.transport_config(Arc::new(transport_config(p)));
    let mut token = ValidationTokenConfig::default();
    token
        .lifetime(p.validation_token_lifetime)
        .sent(p.validation_token_sent);
    s.validation_token_config(token)
        .retry_token_lifetime(p.retry_token_lifetime)
        .max_incoming(p.max_incoming)
        .incoming_buffer_size(p.incoming_buffer_size)
        .incoming_buffer_size_total(p.incoming_buffer_size_total);
    s
}
