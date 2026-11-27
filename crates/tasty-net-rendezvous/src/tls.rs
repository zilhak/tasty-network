//! RFC 7250 raw public key TLS 1.3 with device keys (docs/design/p2p.md "인증",
//! docs/design/rendezvous.md "기기 인증").
//!
//! The "certificate" a side sends is the SubjectPublicKeyInfo DER of its ed25519 key. A
//! received SPKI is accepted only when it is exactly the 12-byte ed25519 prefix and 32 key
//! bytes; those 32 bytes are the device ID. The TLS 1.3 CertificateVerify, checked with
//! [`rustls::crypto::verify_tls13_signature_with_raw_key`], proves the key. Session
//! resumption, tickets and 0-RTT are off so that every connection proves its key again. The
//! crypto provider is ring. The server name (SNI) is a fixed value and is not verified.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
    UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tasty_net_identity::{DeviceId, DeviceKey};

/// The ed25519 SubjectPublicKeyInfo prefix (RFC 8410): SEQUENCE, AlgorithmIdentifier with OID
/// 1.3.101.112, BIT STRING of 33 bytes with 0 unused bits.
pub const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The ed25519 PKCS#8 v1 prefix (RFC 8410 §7 without the public key): SEQUENCE, version 0,
/// AlgorithmIdentifier, OCTET STRING holding an OCTET STRING of the 32-byte seed.
const ED25519_PKCS8_V1_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The server name sent in SNI. It is not used to verify anything.
pub const SERVER_NAME: &str = "tasty-p2p";

/// The ed25519 key length.
const ED25519_KEY_LEN: usize = 32;

/// Why a TLS configuration could not be made or a key could not be read.
#[derive(Debug)]
pub enum TlsError {
    /// rustls refused the configuration or the key.
    Rustls(rustls::Error),
    /// The SPKI is not an ed25519 key of the exact form.
    NotEd25519Spki,
    /// The 32 bytes are not a valid ed25519 point.
    InvalidKey,
    /// The key is a point of small order: a signature with it does not show that the peer
    /// holds a private key ([`is_weak_key`]).
    WeakKey,
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::Rustls(e) => write!(f, "TLS configuration failed: {e}"),
            TlsError::NotEd25519Spki => write!(f, "public key is not an ed25519 SPKI"),
            TlsError::InvalidKey => write!(f, "public key is not a valid ed25519 key"),
            TlsError::WeakKey => write!(f, "public key is a weak (small order) ed25519 key"),
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TlsError::Rustls(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rustls::Error> for TlsError {
    fn from(e: rustls::Error) -> Self {
        TlsError::Rustls(e)
    }
}

/// The SPKI DER of a device ID.
pub fn spki_der(id: &DeviceId) -> Vec<u8> {
    let mut out = ED25519_SPKI_PREFIX.to_vec();
    out.extend_from_slice(id.as_bytes());
    out
}

/// The device ID in an SPKI. Only the exact ed25519 form of a key that is not weak is
/// accepted.
pub fn device_id_from_spki(spki: &[u8]) -> Result<DeviceId, TlsError> {
    let key = spki
        .strip_prefix(&ED25519_SPKI_PREFIX[..])
        .ok_or(TlsError::NotEd25519Spki)?;
    let key: &[u8; ED25519_KEY_LEN] = key.try_into().map_err(|_| TlsError::NotEd25519Spki)?;
    let id = DeviceId::from_bytes(key).map_err(|_| TlsError::InvalidKey)?;
    if is_weak_key(&id) {
        return Err(TlsError::WeakKey);
    }
    Ok(id)
}

/// Whether `id` is a weak ed25519 public key: a point of small order (`ed25519-dalek`
/// `VerifyingKey::is_weak`). For such a key a CertificateVerify signature can hold without
/// a private key, so neither TLS nor an allowlist takes it. A key that does not decode counts
/// as weak.
pub fn is_weak_key(id: &DeviceId) -> bool {
    ed25519_dalek::VerifyingKey::from_bytes(id.as_bytes())
        .ok()
        .is_none_or(|k| k.is_weak())
}

fn pkcs8_v1_der(key: &DeviceKey) -> PrivatePkcs8KeyDer<'static> {
    let mut der = ED25519_PKCS8_V1_PREFIX.to_vec();
    der.extend_from_slice(&key.to_seed());
    PrivatePkcs8KeyDer::from(der)
}

/// The ring crypto provider this crate uses on both sides.
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The key and "certificate" (SPKI) a side presents.
pub fn certified_key(
    key: &DeviceKey,
    provider: &CryptoProvider,
) -> Result<Arc<CertifiedKey>, TlsError> {
    let signing = provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(pkcs8_v1_der(key)))?;
    let spki = CertificateDer::from(spki_der(&key.id()));
    Ok(Arc::new(CertifiedKey::new(vec![spki], signing)))
}

fn ed25519_only() -> Vec<SignatureScheme> {
    vec![SignatureScheme::ED25519]
}

fn verify_raw_signature(
    algorithms: &WebPkiSupportedAlgorithms,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    if dss.scheme != SignatureScheme::ED25519 {
        return Err(rustls::Error::PeerMisbehaved(
            rustls::PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme,
        ));
    }
    rustls::crypto::verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(cert.as_ref()),
        dss,
        algorithms,
    )
}

fn bad_spki() -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
}

/// The client-side verifier: the server's raw public key must be `expected`.
#[derive(Debug)]
pub struct ExpectedKeyVerifier {
    expected: DeviceId,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ExpectedKeyVerifier {
    /// A verifier that accepts only `expected`.
    pub fn new(expected: DeviceId, provider: &CryptoProvider) -> Self {
        ExpectedKeyVerifier {
            expected,
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for ExpectedKeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(bad_spki());
        }
        let got = device_id_from_spki(end_entity.as_ref()).map_err(|_| bad_spki())?;
        if got != self.expected {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_raw_signature(&self.algorithms, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ed25519_only()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The server-side verifier: any well-formed ed25519 raw public key whose CertificateVerify
/// checks. Which devices are let in is decided after the handshake (access policy, pairing).
#[derive(Debug)]
pub struct AnyDeviceVerifier {
    algorithms: WebPkiSupportedAlgorithms,
}

impl AnyDeviceVerifier {
    /// The verifier.
    pub fn new(provider: &CryptoProvider) -> Self {
        AnyDeviceVerifier {
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for AnyDeviceVerifier {
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(bad_spki());
        }
        device_id_from_spki(end_entity.as_ref()).map_err(|_| bad_spki())?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_raw_signature(&self.algorithms, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ed25519_only()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// TLS values a caller may set (docs/design/p2p.md "의존성이 노출하는 값").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlsOptions {
    /// `tls.max_fragment_size`.
    pub max_fragment_size: Option<usize>,
}

/// The client configuration: present `key`, accept only `expected_peer`, offer `alpn`.
pub fn client_config(
    key: &DeviceKey,
    expected_peer: DeviceId,
    alpn: &[u8],
    options: TlsOptions,
) -> Result<rustls::ClientConfig, TlsError> {
    let provider = crypto_provider();
    let certified = certified_key(key, &provider)?;
    let verifier = Arc::new(ExpectedKeyVerifier::new(expected_peer, &provider));
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(
            rustls::client::AlwaysResolvesClientRawPublicKeys::new(certified),
        ));
    config.alpn_protocols = vec![alpn.to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    config.max_fragment_size = options.max_fragment_size;
    Ok(config)
}

/// The server configuration: present `key`, require a client raw public key, offer `alpn`.
pub fn server_config(
    key: &DeviceKey,
    alpn: &[u8],
    options: TlsOptions,
) -> Result<rustls::ServerConfig, TlsError> {
    let provider = crypto_provider();
    let certified = certified_key(key, &provider)?;
    let verifier = Arc::new(AnyDeviceVerifier::new(&provider));
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(
            rustls::server::AlwaysResolvesServerRawPublicKeys::new(certified),
        ));
    config.alpn_protocols = vec![alpn.to_vec()];
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    config.max_fragment_size = options.max_fragment_size;
    Ok(config)
}

/// The device ID of the peer of an established QUIC connection: the one SPKI rustls saw.
pub fn peer_device_id(connection: &quinn::Connection) -> Option<DeviceId> {
    let identity = connection.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    match certs.as_slice() {
        [spki] => device_id_from_spki(spki.as_ref()).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The neutral point (y = 1), of order 1, and the point of order 2 (y = -1).
    pub(crate) fn small_order_ids() -> Vec<DeviceId> {
        let mut neutral = [0; 32];
        neutral[0] = 1;
        let mut order_two = [0xff; 32];
        order_two[0] = 0xec;
        order_two[31] = 0x7f;
        [neutral, order_two]
            .iter()
            .map(|b| DeviceId::from_bytes(b).expect("a valid point"))
            .collect()
    }

    #[test]
    fn small_order_keys_are_refused() {
        for id in small_order_ids() {
            assert!(is_weak_key(&id), "{id}");
            assert!(matches!(
                device_id_from_spki(&spki_der(&id)),
                Err(TlsError::WeakKey)
            ));
        }
        let normal = DeviceKey::from_seed([3; 32]).id();
        assert!(!is_weak_key(&normal));
        assert_eq!(device_id_from_spki(&spki_der(&normal)).unwrap(), normal);
    }

    #[test]
    fn spki_round_trip_and_strictness() {
        let key = DeviceKey::from_seed([3; 32]);
        let spki = spki_der(&key.id());
        assert_eq!(spki.len(), 44);
        assert_eq!(device_id_from_spki(&spki).unwrap(), key.id());
        let mut longer = spki.clone();
        longer.push(0);
        assert!(device_id_from_spki(&longer).is_err());
        assert!(device_id_from_spki(&spki[..43]).is_err());
        let mut other_oid = spki.clone();
        other_oid[8] = 0x71; // 1.3.101.113 is ed448
        assert!(device_id_from_spki(&other_oid).is_err());
    }

    #[test]
    fn ring_loads_the_pkcs8_v1_key() {
        let provider = crypto_provider();
        let key = DeviceKey::from_seed([9; 32]);
        let certified = certified_key(&key, &provider).unwrap();
        assert_eq!(
            certified.key.algorithm(),
            rustls::SignatureAlgorithm::ED25519
        );
        assert_eq!(certified.cert[0].as_ref(), spki_der(&key.id()).as_slice());
    }
}
