//! Handshake v2 on the memory transport: the check order, the refusals, roles, bulk
//! credentials, request channels and cancellation, without sockets.

use std::io::{BufRead, BufReader, Read, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tasty_net_frame::{ReadFailure, StreamTag, read_frame, write_frame};
use tasty_net_identity::{
    ConnectionToken, DeviceKey, MemoryPeerKeyStore, PeerKeyStore, PeerRef, PinOutcome,
};
use tasty_net_transport::{
    AcceptConfig, Accepted, AttachRequest, AttemptCancel, BulkRequest, ChannelConfig, DialAuth,
    DialConfig, DomainAccept, DomainPolicy, DomainReject, FirstLineError, HandshakeError, Holder,
    LEGACY_PROTOCOL_ERROR, Link, LinkTimeouts, Listener, MAX_HANDSHAKE_LINE, MemoryListener,
    MemoryTransport, PeerInfo, RawChannel, RejectReason, RequestError, RequestHandler, Side,
    TooOldEvidence, Transport, TransportKind, accept_handshake, accept_p2p_handshake,
    dial_handshake, dial_request, domain_code, memory_channel_pair, memory_transport,
    read_first_line,
};

fn key(n: u8) -> DeviceKey {
    DeviceKey::from_seed([n; 32])
}

fn token(c: char) -> ConnectionToken {
    ConnectionToken::from_str(&c.to_string().repeat(64)).expect("token")
}

fn timeouts() -> LinkTimeouts {
    LinkTimeouts {
        read_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_secs(5),
        handshake_timeout: Duration::from_secs(5),
        role_proposal_timeout: Duration::from_millis(500),
        drain_cap: Duration::from_millis(300),
    }
}

/// A domain check that counts its calls, refuses `target` together with `target_workspace`,
/// and takes a bulk channel only while the grant equals the current generation.
#[derive(Default)]
struct TestPolicy {
    attach_calls: AtomicUsize,
    bulk_calls: AtomicUsize,
    request_calls: AtomicUsize,
    generation: AtomicU64,
    guards_dropped: Arc<AtomicUsize>,
}

struct Guard(Arc<AtomicUsize>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl DomainPolicy for TestPolicy {
    fn accept_attach(&self, request: &AttachRequest<'_>) -> Result<DomainAccept, DomainReject> {
        self.attach_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(ext) = request.ext() {
            let value: Value =
                serde_json::from_str(ext).map_err(|_| DomainReject::new("bad_ext"))?;
            let targets = ["target", "target_workspace", "bulk_workspace"]
                .iter()
                .filter(|k| value.get(**k).is_some())
                .count();
            if targets > 1 {
                return Err(DomainReject::new("conflicting_targets"));
            }
        }
        Ok(DomainAccept {
            ext: Some(r#"{"descriptor":"next"}"#.into()),
            client_id: Some(7),
            grant: self
                .generation
                .load(Ordering::SeqCst)
                .to_be_bytes()
                .to_vec(),
            guard: Some(Box::new(Guard(Arc::clone(&self.guards_dropped)))),
        })
    }

    fn accept_bulk(&self, request: &BulkRequest<'_>) -> Result<DomainAccept, DomainReject> {
        self.bulk_calls.fetch_add(1, Ordering::SeqCst);
        let now = self.generation.load(Ordering::SeqCst).to_be_bytes();
        if request.grant() != now {
            return Err(DomainReject::new("grant_changed"));
        }
        Ok(DomainAccept::default())
    }

    fn accept_request(&self, _peer: &PeerInfo) -> Result<(), DomainReject> {
        self.request_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Server {
    config: AcceptConfig,
    policy: Arc<TestPolicy>,
}

fn server(token: Option<ConnectionToken>) -> Server {
    let policy = Arc::new(TestPolicy::default());
    let config = AcceptConfig::new(key(2), "acceptor-1", token, policy.clone())
        .with_timeouts(timeouts())
        .with_caps(vec!["net.request".into()])
        .expect("caps");
    Server { config, policy }
}

fn client(n: u8, instance: &str) -> DialConfig {
    DialConfig::new(key(n), instance).with_timeouts(timeouts())
}

/// Accepts one channel on `listener` and runs the SSH-path accepting handshake on it.
fn accept_one(
    listener: Arc<MemoryListener>,
    config: AcceptConfig,
) -> JoinHandle<Result<Accepted, HandshakeError>> {
    std::thread::spawn(move || {
        let mut channel = listener.accept().expect("accept");
        let line = read_first_line(
            &mut channel,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("first line");
        accept_handshake(channel, &line, Vec::new(), &config, &AttemptCancel::new())
    })
}

/// Accepts `n` channels in a row and returns their results in order.
fn accept_many(
    listener: Arc<MemoryListener>,
    config: AcceptConfig,
    n: usize,
) -> JoinHandle<Vec<Result<Accepted, HandshakeError>>> {
    std::thread::spawn(move || {
        (0..n)
            .map(|_| {
                let mut channel = listener.accept().expect("accept");
                let line = read_first_line(
                    &mut channel,
                    MAX_HANDSHAKE_LINE,
                    Instant::now() + Duration::from_secs(5),
                )
                .expect("first line");
                accept_handshake(channel, &line, Vec::new(), &config, &AttemptCancel::new())
            })
            .collect()
    })
}

fn pair(token_for_dialer: Option<ConnectionToken>) -> (MemoryTransport, Arc<MemoryListener>) {
    let (transport, listener) = memory_transport();
    let transport = match token_for_dialer {
        Some(t) => transport.with_auth(DialAuth::Token(t)),
        None => transport,
    };
    (transport, Arc::new(listener))
}

fn link_of(accepted: Result<Accepted, HandshakeError>) -> Link {
    match accepted {
        Ok(Accepted::Link(link)) => *link,
        other => panic!("expected a link: {other:?}"),
    }
}

fn refused_reason(result: Result<Link, HandshakeError>) -> (RejectReason, Option<String>) {
    match result {
        Err(HandshakeError::Rejected { reason, ext, .. }) => (reason, ext),
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[test]
fn a_right_token_gives_a_forward_link_and_one_domain_call() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let mut dialer = dial_handshake(
        conn,
        &client(1, "dialer-1"),
        Holder::Dialer,
        Some(r#"{"target_workspace":1}"#),
        &AttemptCancel::new(),
    )
    .expect("dial");
    let mut accepted = link_of(acceptor.join().expect("acceptor"));
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 1);

    // Both ends see the forward roles and the proved peer.
    assert_eq!(dialer.peer().device_id(), key(2).id());
    assert_eq!(dialer.peer().holder(), Some(Holder::Dialer));
    assert!(dialer.peer().local_holds());
    assert!(dialer.peer().has_cap("net.request"));
    assert_eq!(dialer.peer().transport(), TransportKind::Memory);
    assert_eq!(accepted.peer().device_id(), key(1).id());
    assert_eq!(accepted.peer().instance_id(), "dialer-1");
    assert_eq!(accepted.peer().local_side(), Side::Acceptor);
    assert!(!accepted.peer().local_holds());
    let roles = accepted.peer().roles().expect("attach roles");
    assert_eq!(roles.provider, Side::Acceptor);
    assert_eq!(dialer.client_id(), Some(7));
    assert_eq!(dialer.ext(), Some(r#"{"descriptor":"next"}"#));
    assert_eq!(accepted.ext(), Some(r#"{"target_workspace":1}"#));

    // Frames flow both ways on the ordered channel.
    let (mut d_reader, d_writer) = dialer.take_ordered().expect("ordered");
    assert!(dialer.take_ordered().is_none(), "only once");
    let (mut a_reader, a_writer) = accepted.take_ordered().expect("ordered");
    a_writer
        .write_frame(StreamTag::Control, b"descriptor")
        .expect("write");
    assert_eq!(d_reader.read_frame().expect("read").payload, b"descriptor");
    d_writer
        .write_frame(StreamTag::Data, b"keys")
        .expect("write");
    assert_eq!(a_reader.read_frame().expect("read").payload, b"keys");
    drop((d_reader, d_writer, a_reader, a_writer));
    assert_eq!(s.policy.guards_dropped.load(Ordering::SeqCst), 0);
    drop(accepted);
    assert_eq!(
        s.policy.guards_dropped.load(Ordering::SeqCst),
        1,
        "the registration guard lives as long as the link"
    );
}

#[test]
fn a_missing_and_a_wrong_token_are_refused_with_different_reasons_before_the_domain() {
    for (dialer_token, want) in [
        (None, RejectReason::TokenMissing),
        (Some(token('b')), RejectReason::TokenInvalid),
    ] {
        let s = server(Some(token('a')));
        let (transport, listener) = pair(None);
        let acceptor = accept_one(listener, s.config.clone());
        // A dialer without a token writes message 1 by hand: the endpoint has no token.
        let mut conn = transport
            .connect(&(), &AttemptCancel::new())
            .expect("connect");
        let mut params = json!({
            "proto": 2, "purpose": "attach", "device_id": key(1).id().to_string(),
            "instance_id": "d", "nonce": "00".repeat(32), "holder": "dialer", "caps": [],
        });
        if let Some(t) = &dialer_token {
            params["token"] = json!(t.expose_hex());
        }
        let ack = raw_open(&mut conn.channel, params);
        assert_eq!(ack["ok"], json!(false));
        assert_eq!(ack["reason"], json!(want.as_str()));
        assert!(ack.get("challenge").is_none());
        assert!(matches!(
            acceptor.join().expect("acceptor"),
            Err(HandshakeError::Refused(r)) if r == want
        ));
        assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
    }
}

/// Writes message 1 with `params` and returns the first reply.
fn raw_open(channel: &mut RawChannel, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "stream.open", "params": params});
    channel
        .write_all(format!("{line}\n").as_bytes())
        .expect("write line");
    read_control(channel)
}

fn read_control(channel: &mut RawChannel) -> Value {
    let frame = read_frame(channel).expect("reply frame");
    assert_eq!(frame.tag, StreamTag::Control);
    serde_json::from_slice(&frame.payload).expect("json")
}

#[test]
fn the_tasty_v1_line_is_refused_as_legacy_before_the_domain() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    conn.channel
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"stream.open\",\"params\":{\"proto\":1,\"target_workspace\":1}}\n",
        )
        .expect("write");
    let ack = read_control(&mut conn.channel);
    assert_eq!(ack["ok"], json!(false));
    assert_eq!(ack["proto"], json!(2));
    assert_eq!(ack["reason"], json!("legacy_protocol"));
    assert_eq!(ack["error"], json!(LEGACY_PROTOCOL_ERROR));
    // The refusing end closed its sending direction after the ACK.
    assert!(read_frame(&mut conn.channel).is_err());
    drop(conn);
    assert!(matches!(
        acceptor.join().expect("acceptor"),
        Err(HandshakeError::Refused(RejectReason::LegacyProtocol))
    ));
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_field_of_the_wrong_type_is_malformed_not_a_version_mismatch() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let ack = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "attach", "token": token('a').expose_hex(),
            "device_id": 5, "instance_id": "d", "nonce": "00".repeat(32), "holder": "dialer",
            "caps": [],
        }),
    );
    assert_eq!(ack["reason"], json!("malformed"));
    drop(conn);
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn another_proto_is_unsupported() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let ack = raw_open(&mut conn.channel, json!({"proto": 3}));
    assert_eq!(ack["reason"], json!("unsupported_protocol"));
    drop(conn);
    assert!(acceptor.join().expect("acceptor").is_err());
}

#[test]
fn a_proof_signed_by_another_key_is_refused_before_the_domain() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let challenge = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "attach", "token": token('a').expose_hex(),
            "device_id": key(1).id().to_string(), "instance_id": "d",
            "nonce": "11".repeat(32), "holder": "dialer", "caps": [],
        }),
    );
    assert!(challenge.get("challenge").is_some(), "{challenge}");
    let forged = key(9).sign(b"anything");
    let proof = json!({"proof": {"sig": forged.to_string()}}).to_string();
    write_frame(&mut conn.channel, StreamTag::Control, proof.as_bytes()).expect("proof");
    let ack = read_control(&mut conn.channel);
    assert_eq!(ack["reason"], json!("signature_invalid"));
    drop(conn);
    assert!(matches!(
        acceptor.join().expect("acceptor"),
        Err(HandshakeError::Refused(RejectReason::SignatureInvalid))
    ));
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_dialer_refuses_a_challenge_signed_by_another_key() {
    let (transport, listener) = pair(Some(token('a')));
    let fake = std::thread::spawn(move || {
        let mut channel = listener.accept().expect("accept");
        let _line = read_first_line(
            &mut channel,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("line");
        let challenge = json!({"challenge": {
            "device_id": key(2).id().to_string(), "instance_id": "acc", "caps": [],
            "nonce": "22".repeat(32), "sig": key(9).sign(b"x").to_string(),
        }})
        .to_string();
        write_frame(&mut channel, StreamTag::Control, challenge.as_bytes()).expect("write");
        // The dialer closes without a proof.
        read_frame(&mut channel).map(|f| f.tag)
    });
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert!(
        matches!(result, Err(HandshakeError::SignatureInvalid)),
        "{result:?}"
    );
    let after = fake.join().expect("fake");
    assert!(after.is_err(), "no proof was sent: {after:?}");
}

#[test]
fn a_dialer_reports_a_v1_server_as_too_old() {
    let (transport, listener) = pair(Some(token('a')));
    let fake = std::thread::spawn(move || {
        let mut channel = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(&mut channel)
            .read_line(&mut line)
            .expect("line");
        let ack =
            br#"{"ok":false,"proto":1,"error":"unsupported stream proto 2 (server speaks 1)"}"#;
        write_frame(&mut channel, StreamTag::Control, ack).expect("ack");
        line
    });
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert!(
        matches!(
            result,
            Err(HandshakeError::PeerTooOld {
                evidence: TooOldEvidence::V1Ack
            })
        ),
        "{result:?}"
    );
    assert!(!result.as_ref().expect_err("error").is_retryable());
    let line = fake.join().expect("fake");
    let sent: Value = serde_json::from_str(&line).expect("json");
    assert_eq!(sent["params"]["proto"], json!(2));
}

#[test]
fn conflicting_targets_are_a_domain_refusal_and_the_channel_closes() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        Some(r#"{"target":3,"target_workspace":1}"#),
        &AttemptCancel::new(),
    );
    let (reason, ext) = refused_reason(result);
    assert_eq!(reason, RejectReason::Domain);
    assert_eq!(
        domain_code(ext.as_deref().expect("ext")).as_deref(),
        Some("conflicting_targets")
    );
    assert!(matches!(
        acceptor.join().expect("acceptor"),
        Err(HandshakeError::Refused(RejectReason::Domain))
    ));
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 1);
}

/// A TOFU store whose record of `peer` is already another device.
fn pinned_store(peer: &str, n: u8) -> Arc<dyn PeerKeyStore + Send + Sync> {
    let store = MemoryPeerKeyStore::new();
    assert_eq!(
        store
            .pin_if_absent(&PeerRef::new(peer), &key(n).id())
            .expect("pin"),
        PinOutcome::Inserted
    );
    Arc::new(store)
}

#[test]
fn a_tofu_mismatch_stops_the_dialer_before_the_domain() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let config = client(1, "d").with_peer_store(pinned_store("remote", 8), PeerRef::new("remote"));
    let result = dial_handshake(conn, &config, Holder::Dialer, None, &AttemptCancel::new());
    match result {
        Err(HandshakeError::PeerKeyMismatch { expected, got }) => {
            assert_eq!(expected, key(8).id());
            assert_eq!(got, key(2).id());
        }
        other => panic!("{other:?}"),
    }
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_tofu_mismatch_on_the_accepting_end_is_refused_before_the_domain() {
    let mut s = server(Some(token('a')));
    s.config = s
        .config
        .with_peer_store(pinned_store("dialer", 8), PeerRef::new("dialer"));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert_eq!(refused_reason(result).0, RejectReason::PeerKeyMismatch);
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn the_same_instance_is_refused_as_self_attach() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    // Same device key and instance as the acceptor.
    let config = DialConfig::new(key(2), "acceptor-1").with_timeouts(timeouts());
    let result = dial_handshake(conn, &config, Holder::Dialer, None, &AttemptCancel::new());
    assert!(
        matches!(result, Err(HandshakeError::SelfAttach)),
        "{result:?}"
    );
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_role_the_acceptor_does_not_take_is_refused() {
    // Forward refused.
    let mut s = server(Some(token('a')));
    s.config = s.config.with_forward(false);
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert_eq!(refused_reason(result).0, RejectReason::RoleRefused);
    assert!(acceptor.join().expect("acceptor").is_err());

    // A reversal without a proposal handler.
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Acceptor,
        None,
        &AttemptCancel::new(),
    );
    assert_eq!(refused_reason(result).0, RejectReason::RoleRefused);
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_reversal_waits_for_the_consumer_and_follows_its_answer() {
    for (answer, accepted) in [(Some(true), true), (Some(false), false), (None, false)] {
        let mut s = server(Some(token('a')));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        // The answering threads, joined at the end of the round.
        let answers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let started = Arc::clone(&answers);
        s.config = s.config.with_role_proposals(move |proposal| {
            record
                .lock()
                .expect("lock")
                .push(proposal.peer().device_id());
            let answering = match answer {
                Some(true) => std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    proposal.accept();
                }),
                Some(false) => std::thread::spawn(move || proposal.refuse()),
                // No answer: the proposal timeout (500 ms) refuses.
                None => std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(1));
                    drop(proposal);
                }),
            };
            started.lock().expect("lock").push(answering);
        });
        let (transport, listener) = pair(Some(token('a')));
        let acceptor = accept_one(listener, s.config.clone());
        let conn = transport
            .connect(&(), &AttemptCancel::new())
            .expect("connect");
        let result = dial_handshake(
            conn,
            &client(1, "d"),
            Holder::Acceptor,
            None,
            &AttemptCancel::new(),
        );
        assert_eq!(*seen.lock().expect("lock"), vec![key(1).id()], "{answer:?}");
        if accepted {
            let dialer = result.expect("accepted reversal");
            let acc = link_of(acceptor.join().expect("acceptor"));
            assert!(!dialer.peer().local_holds());
            assert!(acc.peer().local_holds());
            assert_eq!(acc.peer().roles().expect("roles").provider, Side::Dialer);
            assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(
                refused_reason(result).0,
                RejectReason::RoleRefused,
                "{answer:?}"
            );
            assert!(acceptor.join().expect("acceptor").is_err());
            assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
        }
        let answering = std::mem::take(&mut *answers.lock().expect("lock"));
        assert_eq!(answering.len(), 1);
        for thread in answering {
            thread.join().expect("answering thread");
        }
    }
}

#[test]
fn a_bulk_channel_needs_the_live_parent_and_its_grant() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_many(listener.clone(), s.config.clone(), 2);
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let dialer = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    )
    .expect("dial");

    // The parent's credential opens a bulk channel, and bytes flow on it.
    let mut bulk = dialer.open_bulk(Some(r#"{"bulk":1}"#)).expect("bulk");
    let mut results = acceptor.join().expect("acceptor").into_iter();
    let parent = link_of(results.next().expect("parent"));
    let mut accepted_bulk = match results.next().expect("bulk") {
        Ok(Accepted::Bulk(b)) => b,
        other => panic!("{other:?}"),
    };
    assert_eq!(accepted_bulk.peer().device_id(), key(1).id());
    assert_eq!(accepted_bulk.ext(), Some(r#"{"bulk":1}"#));
    bulk.writer
        .write_frame(StreamTag::Data, b"chunk")
        .expect("write");
    assert_eq!(
        accepted_bulk.reader.read_frame().expect("read").payload,
        b"chunk"
    );
    assert_eq!(s.policy.bulk_calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.config.bulk_registry().live(), 1);

    // A changed grant: the domain refuses, the parent stays.
    s.policy.generation.fetch_add(1, Ordering::SeqCst);
    let acceptor = accept_one(listener.clone(), s.config.clone());
    let refused = dialer.open_bulk(None);
    assert!(matches!(
        refused,
        Err(HandshakeError::Rejected {
            reason: RejectReason::Domain,
            ..
        })
    ));
    assert!(acceptor.join().expect("acceptor").is_err());
    s.policy.generation.fetch_sub(1, Ordering::SeqCst);

    // The parent ends: the open bulk channel closes on both ends, the credential is gone.
    drop(parent);
    assert_eq!(s.config.bulk_registry().live(), 0);
    assert_eq!(
        accepted_bulk
            .reader
            .read_frame()
            .expect_err("closed")
            .failure(),
        ReadFailure::PeerClosed
    );
    assert_eq!(
        bulk.reader.read_frame().expect_err("closed").failure(),
        ReadFailure::PeerClosed
    );
    let acceptor = accept_one(listener, s.config.clone());
    let late = dialer.open_bulk(None);
    assert!(matches!(
        late,
        Err(HandshakeError::Rejected {
            reason: RejectReason::BulkCredentialInvalid,
            ..
        })
    ));
    assert!(acceptor.join().expect("acceptor").is_err());
}

#[test]
fn a_credential_the_acceptor_did_not_issue_is_refused() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let ack = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "bulk", "token": token('a').expose_hex(),
            "instance_id": "d", "bulk_credential": "ab".repeat(32),
        }),
    );
    assert_eq!(ack["reason"], json!("bulk_credential_invalid"));
    drop(conn);
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.bulk_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn closing_the_parent_on_the_dialer_also_closes_its_bulk_channels() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_many(listener, s.config.clone(), 2);
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let dialer = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    )
    .expect("dial");
    let bulk = dialer.open_bulk(None).expect("bulk");
    let results = acceptor.join().expect("acceptor");
    let mut bulk_reader = bulk.reader;
    dialer
        .closer()
        .force_close(tasty_net_transport::EndReason::Forced);
    assert_eq!(
        bulk_reader.read_frame().expect_err("closed").failure(),
        ReadFailure::PeerClosed
    );
    drop(results);
}

#[test]
fn too_many_handshakes_at_once_is_busy_and_retryable() {
    let mut s = server(Some(token('a')));
    s.config = s.config.with_max_handshakes(0);
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    let error = result.expect_err("busy");
    assert!(error.is_retryable(), "{error:?}");
    assert!(matches!(
        error,
        HandshakeError::Rejected {
            reason: RejectReason::Busy,
            ..
        }
    ));
    assert!(acceptor.join().expect("acceptor").is_err());
}

#[test]
fn cancelling_a_waiting_dialer_returns_promptly() {
    let (transport, listener) = pair(Some(token('a')));
    // Nobody answers: the dialer waits for the challenge.
    let held = std::thread::spawn(move || listener.accept().expect("accept"));
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let cancel = AttemptCancel::new();
    let trigger = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        trigger.cancel();
    });
    let started = Instant::now();
    let result = dial_handshake(conn, &client(1, "d"), Holder::Dialer, None, &cancel);
    assert!(
        matches!(result, Err(HandshakeError::Cancelled)),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    canceller.join().expect("canceller");
    drop(held.join().expect("held"));
}

#[test]
fn cancelling_a_waiting_acceptor_returns_promptly() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(None);
    let cancel = AttemptCancel::new();
    let trigger = cancel.clone();
    let config = s.config.clone();
    let acceptor = std::thread::spawn(move || {
        let mut channel = listener.accept().expect("accept");
        let line = read_first_line(
            &mut channel,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("line");
        let started = Instant::now();
        (
            accept_handshake(channel, &line, Vec::new(), &config, &cancel),
            started.elapsed(),
        )
    });
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    // A valid message 1; no proof follows the challenge.
    let challenge = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "attach", "token": token('a').expose_hex(),
            "device_id": key(1).id().to_string(), "instance_id": "d",
            "nonce": "11".repeat(32), "holder": "dialer", "caps": [],
        }),
    );
    assert!(challenge.get("challenge").is_some());
    std::thread::sleep(Duration::from_millis(100));
    trigger.cancel();
    let (result, took) = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::Cancelled)),
        "{result:?}"
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_silent_peer_times_out_the_handshake() {
    let (transport, listener) = pair(Some(token('a')));
    let held = std::thread::spawn(move || listener.accept().expect("accept"));
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let config = client(1, "d").with_timeouts(LinkTimeouts {
        handshake_timeout: Duration::from_millis(200),
        ..timeouts()
    });
    let result = dial_handshake(conn, &config, Holder::Dialer, None, &AttemptCancel::new());
    assert!(
        matches!(result, Err(HandshakeError::TimedOut)),
        "{result:?}"
    );
    assert!(result.expect_err("timeout").is_retryable());
    drop(held.join().expect("held"));
}

struct Echo;

impl RequestHandler for Echo {
    fn handle(&self, peer: &PeerInfo, method: &str, params: Value) -> Result<Value, Value> {
        match method {
            "system.info" => Ok(json!({"peer": peer.instance_id(), "params": params})),
            other => Err(json!({"code": "not_allowed", "method": other})),
        }
    }
}

#[test]
fn a_request_channel_answers_by_id_and_needs_no_attach() {
    let s = server(Some(token('a')));
    let (transport, listener) = pair(Some(token('a')));
    let acceptor = accept_one(listener, s.config.clone());
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let client = dial_request(conn, &client(1, "d"), &AttemptCancel::new()).expect("request");
    let server = match acceptor.join().expect("acceptor") {
        Ok(Accepted::Request(server)) => server,
        other => panic!("{other:?}"),
    };
    assert_eq!(server.peer().holder(), None);
    let serving = std::thread::spawn(move || server.serve(&Echo));
    let answer = client
        .call("system.info", json!({"x": 1}), Duration::from_secs(5))
        .expect("answer");
    assert_eq!(answer, json!({"peer": "d", "params": {"x": 1}}));
    match client.call("workspace.delete", json!(null), Duration::from_secs(5)) {
        Err(RequestError::Remote(error)) => assert_eq!(error["code"], json!("not_allowed")),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.policy.request_calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
    drop(client);
    serving.join().expect("server").expect("served to the end");
}

#[test]
fn the_p2p_path_uses_the_remote_id_and_a_hello() {
    let s = server(None);
    let (transport, listener) = memory_transport();
    let transport = transport.with_auth(DialAuth::P2p {
        remote_id: key(2).id(),
    });
    let listener = Arc::new(listener);
    let config = s.config.clone();
    let l = Arc::clone(&listener);
    let acceptor = std::thread::spawn(move || {
        (0..2)
            .map(|_| {
                let channel = l.accept().expect("accept");
                accept_p2p_handshake(channel, key(1).id(), 42, &config, &AttemptCancel::new())
            })
            .collect::<Vec<_>>()
    });
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let dialer = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    )
    .expect("p2p dial");
    assert_eq!(dialer.peer().device_id(), key(2).id());
    assert_eq!(dialer.peer().instance_id(), "acceptor-1");
    let _bulk = dialer
        .open_bulk(None)
        .expect("p2p bulk under the same connection");
    let results = acceptor.join().expect("acceptor");
    assert!(matches!(results[0], Ok(Accepted::Link(_))));
    assert!(matches!(results[1], Ok(Accepted::Bulk(_))));
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_p2p_device_id_other_than_the_remote_id_is_malformed() {
    let s = server(None);
    let (transport, listener) = memory_transport();
    let transport = transport.with_auth(DialAuth::P2p {
        remote_id: key(2).id(),
    });
    let config = s.config.clone();
    let acceptor = std::thread::spawn(move || {
        let channel = listener.accept().expect("accept");
        // The connection proved device 3, but message 1 says device 1.
        accept_p2p_handshake(channel, key(3).id(), 1, &config, &AttemptCancel::new())
    });
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert_eq!(refused_reason(result).0, RejectReason::Malformed);
    assert!(acceptor.join().expect("acceptor").is_err());
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_p2p_hello_from_another_device_is_a_mismatch() {
    let (transport, listener) = memory_transport();
    let transport = transport.with_auth(DialAuth::P2p {
        remote_id: key(2).id(),
    });
    let fake = std::thread::spawn(move || {
        let mut channel = listener.accept().expect("accept");
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while channel.read(&mut byte).expect("read") == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let hello = json!({"hello": {"device_id": key(5).id().to_string(), "instance_id": "x", "caps": []}});
        write_frame(
            &mut channel,
            StreamTag::Control,
            hello.to_string().as_bytes(),
        )
        .expect("hello");
        let ack = json!({"ok": true, "proto": 2, "bulk_credential": "cd".repeat(32)});
        write_frame(&mut channel, StreamTag::Control, ack.to_string().as_bytes()).expect("ack");
        channel
    });
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        None,
        &AttemptCancel::new(),
    );
    assert!(
        matches!(result, Err(HandshakeError::PeerMismatch)),
        "{result:?}"
    );
    drop(fake.join().expect("fake"));
}

#[test]
fn a_p2p_first_line_over_the_limit_is_malformed() {
    let s = server(None);
    let (transport, listener) = memory_transport();
    let config = s.config.clone();
    let acceptor = std::thread::spawn(move || {
        let channel = listener.accept().expect("accept");
        accept_p2p_handshake(channel, key(1).id(), 1, &config, &AttemptCancel::new())
    });
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    conn.channel
        .write_all(&vec![b'x'; MAX_HANDSHAKE_LINE + 1])
        .expect("write");
    let ack = read_control(&mut conn.channel);
    assert_eq!(ack["reason"], json!("malformed"));
    drop(conn);
    assert!(matches!(
        acceptor.join().expect("acceptor"),
        Err(HandshakeError::Refused(RejectReason::Malformed))
    ));
}

#[test]
fn the_first_line_limit_counts_the_line_break() {
    for (len, fits) in [(MAX_HANDSHAKE_LINE - 1, true), (MAX_HANDSHAKE_LINE, false)] {
        let (mut a, mut b) = memory_channel_pair();
        let mut line = vec![b'x'; len];
        line.push(b'\n');
        a.write_all(&line).expect("write");
        let read = read_first_line(
            &mut b,
            MAX_HANDSHAKE_LINE,
            Instant::now() + Duration::from_secs(5),
        );
        match (read, fits) {
            (Ok(got), true) => assert_eq!(got.len(), len),
            (Err(FirstLineError::TooLong), false) => {}
            (other, _) => panic!("{len} bytes: {other:?}"),
        }
    }
}

#[test]
fn a_dialer_does_not_send_a_line_over_the_limit() {
    let (transport, listener) = pair(Some(token('a')));
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let mut accepted = listener.accept().expect("accept");
    let ext = "a".repeat(MAX_HANDSHAKE_LINE);
    let result = dial_handshake(
        conn,
        &client(1, "d"),
        Holder::Dialer,
        Some(&ext),
        &AttemptCancel::new(),
    );
    assert!(
        matches!(result, Err(HandshakeError::MessageTooLong)),
        "{result:?}"
    );
    // Nothing came: the dialer's channel closed without a byte.
    let mut byte = [0u8; 1];
    assert_eq!(accepted.read(&mut byte).expect("eof"), 0);
}

#[test]
fn a_timeout_too_large_for_a_deadline_is_a_configuration_error() {
    let huge = LinkTimeouts {
        handshake_timeout: Duration::MAX,
        ..timeouts()
    };
    let (transport, _listener) = pair(Some(token('a')));
    let conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let result = dial_handshake(
        conn,
        &client(1, "d").with_timeouts(huge),
        Holder::Acceptor,
        None,
        &AttemptCancel::new(),
    );
    assert!(
        matches!(result, Err(HandshakeError::InvalidTimeouts)),
        "{result:?}"
    );
    let (a, _b) = memory_channel_pair();
    let config = ChannelConfig {
        drain_cap: Duration::MAX,
        ..ChannelConfig::default()
    };
    let refused = a.framed(&config).expect_err("drain cap too large");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
}

/// A proof that trickles in byte by byte completes after the deadline: the acceptor does not
/// take it (each byte would renew a per-read socket timeout).
#[test]
fn a_frame_that_completes_after_the_deadline_is_not_taken() {
    let policy = Arc::new(TestPolicy::default());
    let config = AcceptConfig::new(key(2), "acceptor-1", Some(token('a')), policy.clone())
        .with_timeouts(LinkTimeouts {
            handshake_timeout: Duration::from_millis(300),
            ..timeouts()
        });
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, config);
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let challenge = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "attach", "token": token('a').expose_hex(),
            "device_id": key(1).id().to_string(), "instance_id": "d",
            "nonce": "00".repeat(32), "holder": "dialer", "caps": [],
        }),
    );
    assert!(challenge.get("challenge").is_some(), "{challenge}");
    let mut frame = Vec::new();
    write_frame(&mut frame, StreamTag::Control, br#"{"proof":{"sig":"00"}}"#).expect("frame");
    let (head, rest) = frame.split_at(6);
    for byte in head {
        conn.channel.write_all(&[*byte]).expect("byte");
        std::thread::sleep(Duration::from_millis(80));
    }
    let _late = conn.channel.write_all(rest);
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::TimedOut)),
        "{result:?}"
    );
    assert_eq!(policy.attach_calls.load(Ordering::SeqCst), 0);
}

/// A dialer that closes while its reversal waits for approval gets no domain callback, even
/// when the approval comes afterwards.
#[test]
fn a_peer_that_closes_during_the_approval_wait_gets_no_domain_call() {
    let mut s = server(Some(token('a')));
    // The proposal is kept and approved only after the dialer has gone.
    let pending = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&pending);
    s.config = s.config.with_role_proposals(move |proposal| {
        *slot.lock().expect("lock") = Some(proposal);
    });
    let (transport, listener) = pair(None);
    let acceptor = accept_one(listener, s.config.clone());
    let mut conn = transport
        .connect(&(), &AttemptCancel::new())
        .expect("connect");
    let challenge = raw_open(
        &mut conn.channel,
        json!({
            "proto": 2, "purpose": "attach", "token": token('a').expose_hex(),
            "device_id": key(1).id().to_string(), "instance_id": "d",
            "nonce": "00".repeat(32), "holder": "acceptor", "caps": [],
        }),
    );
    let challenge = &challenge["challenge"];
    let acceptor_nonce = hex32(challenge["nonce"].as_str().expect("nonce"));
    let digest = transcript_digest(&TranscriptFields {
        holder: "acceptor",
        dialer: key(1).id(),
        acceptor: key(2).id(),
        dialer_instance: "d",
        acceptor_instance: "acceptor-1",
        dialer_nonce: [0; 32],
        acceptor_nonce,
        dialer_caps: &[],
        acceptor_caps: &["net.request"],
        ext: "",
        signer: "dialer",
    });
    let proof = json!({"proof": {"sig": key(1).sign(&digest).to_string()}});
    write_frame(
        &mut conn.channel,
        StreamTag::Control,
        proof.to_string().as_bytes(),
    )
    .expect("proof");
    std::thread::sleep(Duration::from_millis(100));
    drop(conn);
    std::thread::sleep(Duration::from_millis(100));
    if let Some(proposal) = pending.lock().expect("lock").take() {
        proposal.accept();
    }
    let result = acceptor.join().expect("acceptor");
    assert!(
        matches!(result, Err(HandshakeError::PeerClosed)),
        "{result:?}"
    );
    assert_eq!(s.policy.attach_calls.load(Ordering::SeqCst), 0);
}

/// The fields of the signed transcript, rebuilt here from docs/design/handshake.md so the
/// tests sign what a real dialer signs.
struct TranscriptFields<'a> {
    holder: &'a str,
    dialer: tasty_net_identity::DeviceId,
    acceptor: tasty_net_identity::DeviceId,
    dialer_instance: &'a str,
    acceptor_instance: &'a str,
    dialer_nonce: [u8; 32],
    acceptor_nonce: [u8; 32],
    dialer_caps: &'a [&'a str],
    acceptor_caps: &'a [&'a str],
    ext: &'a str,
    signer: &'a str,
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).expect("len").to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_caps(out: &mut Vec<u8>, caps: &[&str]) {
    let mut sorted = caps.to_vec();
    sorted.sort_unstable();
    out.extend_from_slice(&u32::try_from(sorted.len()).expect("len").to_be_bytes());
    for name in sorted {
        put(out, name.as_bytes());
    }
}

/// The SHA-256 a signer signs for an attach handshake.
fn transcript_digest(f: &TranscriptFields<'_>) -> [u8; 32] {
    use sha2::Digest;
    let mut out = Vec::new();
    put(&mut out, b"tasty-net handshake v2");
    put(&mut out, f.signer.as_bytes());
    put(&mut out, &2u32.to_be_bytes());
    put(&mut out, b"attach");
    put(&mut out, f.holder.as_bytes());
    put(&mut out, f.dialer.as_bytes());
    put(&mut out, f.acceptor.as_bytes());
    put(&mut out, f.dialer_instance.as_bytes());
    put(&mut out, f.acceptor_instance.as_bytes());
    put(&mut out, &f.dialer_nonce);
    put(&mut out, &f.acceptor_nonce);
    put_caps(&mut out, f.dialer_caps);
    put_caps(&mut out, f.acceptor_caps);
    put(&mut out, f.ext.as_bytes());
    sha2::Sha256::digest(&out).into()
}

fn hex32(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}
