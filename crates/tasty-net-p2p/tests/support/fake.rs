//! A scripted rendezvous server for the tests that need what a correct server never sends (a
//! `punch_go` beyond the limits) or a message at a chosen moment. It speaks the coordination
//! protocol of docs/design/rendezvous.md on the mock network, answers plain STUN on its one
//! address, and relays nothing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tasty_net_identity::DeviceId;
use tasty_net_p2p::RendezvousServer;
use tasty_net_rendezvous::packet_io::{DemuxSocket, PacketIo, StunSink};
use tasty_net_rendezvous::tls::{self, TlsOptions};
use tasty_net_rendezvous::wire::ALPN_RENDEZVOUS;
use tasty_net_rendezvous::wire::candidate::CandidateKind;
use tasty_net_rendezvous::wire::frame::{encode_frame, read_frame};
use tasty_net_rendezvous::wire::message::{
    AnswerReason, CancelReason, Candidate, DeviceMessage, Limits, NatMapping, PunchCancel, PunchGo,
    PunchMode, PunchOffer, PunchRefused, PunchToken, RefuseReason, ResultReason, Role,
    ServerMessage, Welcome,
};
use tasty_net_rendezvous::wire::stun::{ServerStunAction, server_answer};
use tokio::sync::mpsc;

use super::{MockIo, Net, Port, key};

/// The values of a correct server with default settings.
pub fn default_limits() -> Limits {
    Limits {
        offer_timeout_ms: 2000,
        start_delay_max_ms: 1000,
        punch_window_ms: 5000,
        quic_handshake_ms: 5000,
        punch_session_ttl_ms: 18_000,
        max_host_candidates: 4,
        max_reported_reflexive: 3,
        max_message_bytes: 16 * 1024,
    }
}

/// What the fake server does.
pub struct Script {
    pub limits: Limits,
    pub start_after_ms: u64,
    /// Changes the `punch_go` sent to the requesting side.
    pub go_a: Box<dyn Fn(&mut PunchGo) + Send + Sync>,
    /// Changes the `punch_go` sent to the receiving side.
    pub go_b: Box<dyn Fn(&mut PunchGo) + Send + Sync>,
    /// Send `punch_cancel{peer_limits_invalid}` to the other side of a `limits_invalid` result
    /// after this delay; none: do not.
    pub forward_limits_invalid: Option<Duration>,
    /// Raw bytes written instead of the `welcome`; none: a `welcome`.
    pub first: Option<Vec<u8>>,
    /// Raw bytes written right after the `welcome`.
    pub after_welcome: Vec<Vec<u8>>,
    /// End the coordination stream after those bytes, then close the connection with code 0.
    pub finish_after: bool,
}

enum Out {
    Msg(ServerMessage),
    Raw(Vec<u8>),
    Finish,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            limits: default_limits(),
            start_after_ms: 100,
            go_a: Box::new(|_| {}),
            go_b: Box::new(|_| {}),
            forward_limits_invalid: Some(Duration::ZERO),
            first: None,
            after_welcome: Vec::new(),
            finish_after: false,
        }
    }
}

struct Dev {
    tx: mpsc::UnboundedSender<Out>,
    remote: SocketAddr,
    host: Vec<String>,
}

struct Punch {
    a: DeviceId,
    b: DeviceId,
    request_id: u64,
}

#[derive(Default)]
struct State {
    devices: HashMap<DeviceId, Dev>,
    punches: HashMap<u64, Punch>,
    next: u64,
    received: Vec<(DeviceId, DeviceMessage)>,
    /// The length of every frame body received, in order (the `hello` first).
    sizes: Vec<usize>,
}

/// The fake server.
pub struct Fake {
    pub info: RendezvousServer,
    pub port: Arc<Port>,
    state: Arc<Mutex<State>>,
    script: Arc<Script>,
    _endpoint: quinn::Endpoint,
}

#[derive(Debug)]
struct Answer;

impl StunSink for Answer {
    fn on_stun(&self, packet: &[u8], from: SocketAddr, io: &dyn PacketIo) {
        if let ServerStunAction::Answer(reply) = server_answer(packet, from) {
            let _sent = io.try_send_to(&reply, from).is_ok();
        }
    }
}

impl Fake {
    /// Start at `addr` on `net`, in `rt`.
    pub fn start(
        net: &Net,
        rt: &tokio::runtime::Runtime,
        addr: SocketAddr,
        script: Script,
    ) -> Self {
        let server_key = key(250);
        let port = net.public(addr);
        let tls = tls::server_config(
            &server_key,
            ALPN_RENDEZVOUS,
            TlsOptions {
                max_fragment_size: None,
            },
        )
        .expect("tls");
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("quic tls");
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let endpoint = {
            let _enter = rt.enter();
            let socket = Arc::new(DemuxSocket::new(
                Arc::new(MockIo(port.clone())),
                Arc::new(Answer),
            ));
            let mut config = quinn::EndpointConfig::default();
            config.grease_quic_bit(false);
            quinn::Endpoint::new_with_abstract_socket(
                config,
                Some(server_config),
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .expect("fake endpoint")
        };
        let state = Arc::new(Mutex::new(State::default()));
        let script = Arc::new(script);
        {
            let (endpoint, state, script) = (endpoint.clone(), state.clone(), script.clone());
            rt.spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    let (state, script) = (state.clone(), script.clone());
                    tokio::spawn(async move {
                        if let Ok(conn) = incoming.await {
                            serve(conn, state, script).await;
                        }
                    });
                }
            });
        }
        Fake {
            info: RendezvousServer {
                addr,
                public_key: server_key.id(),
            },
            port,
            state,
            script,
            _endpoint: endpoint,
        }
    }

    /// Send a message to a registered device.
    pub fn send(&self, to: DeviceId, msg: ServerMessage) {
        if let Some(d) = self.state.lock().unwrap().devices.get(&to) {
            let _sent = d.tx.send(Out::Msg(msg)).is_ok();
        }
    }

    /// Write raw bytes on a device's coordination stream.
    pub fn send_raw(&self, to: DeviceId, bytes: Vec<u8>) {
        if let Some(d) = self.state.lock().unwrap().devices.get(&to) {
            let _sent = d.tx.send(Out::Raw(bytes)).is_ok();
        }
    }

    /// The body lengths of the frames received.
    pub fn sizes(&self) -> Vec<usize> {
        self.state.lock().unwrap().sizes.clone()
    }

    /// The newest punch ID.
    pub fn last_punch(&self) -> Option<u64> {
        self.state.lock().unwrap().punches.keys().max().copied()
    }

    /// Messages received so far.
    pub fn received(&self) -> Vec<(DeviceId, DeviceMessage)> {
        self.state.lock().unwrap().received.clone()
    }

    /// Whether `device` is registered.
    pub fn has(&self, device: &DeviceId) -> bool {
        self.state.lock().unwrap().devices.contains_key(device)
    }

    /// The limits this server sends.
    pub fn limits(&self) -> Limits {
        self.script.limits
    }
}

fn send_to(state: &State, to: &DeviceId, msg: ServerMessage) {
    if let Some(d) = state.devices.get(to) {
        let _sent = d.tx.send(Out::Msg(msg)).is_ok();
    }
}

fn candidates(d: &Dev) -> Vec<Candidate> {
    let mut out = vec![Candidate {
        addr: d.remote,
        kind: CandidateKind::ServerReflexive,
    }];
    out.extend(
        d.host
            .iter()
            .filter_map(|h| h.parse().ok())
            .map(|addr| Candidate {
                addr,
                kind: CandidateKind::Host,
            }),
    );
    out
}

async fn serve(conn: quinn::Connection, state: Arc<Mutex<State>>, script: Arc<Script>) {
    let Some(id) = tls::peer_device_id(&conn) else {
        return;
    };
    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
        return;
    };
    // The fake reads whatever the device sends; the tests check the sizes.
    let limit = u32::MAX >> 8;
    let Ok(Some(body)) = read_frame(&mut recv, limit, Duration::from_secs(5)).await else {
        return;
    };
    let Ok(DeviceMessage::Hello(hello)) = DeviceMessage::parse(&body) else {
        return;
    };
    state.lock().unwrap().sizes.push(body.len());
    let remote = super::canonical(conn.remote_address());
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let first = match &script.first {
        Some(raw) => Out::Raw(raw.clone()),
        None => Out::Msg(ServerMessage::Welcome(Welcome {
            reflexive: remote,
            stun: Vec::new(),
            limits: script.limits,
        })),
    };
    let _sent = tx.send(first);
    for raw in &script.after_welcome {
        let _sent = tx.send(Out::Raw(raw.clone()));
    }
    if script.finish_after {
        let _sent = tx.send(Out::Finish);
    }
    state.lock().unwrap().devices.insert(
        id,
        Dev {
            tx,
            remote,
            host: hello.host_candidates,
        },
    );
    let writer_conn = conn.clone();
    tokio::spawn(async move {
        let conn = writer_conn;
        while let Some(m) = rx.recv().await {
            let bytes = match m {
                Out::Finish => {
                    let _finished = send.finish().is_ok();
                    let _sent = send.stopped().await;
                    conn.close(quinn::VarInt::from_u32(0), b"closed");
                    break;
                }
                Out::Raw(b) => b,
                Out::Msg(m) => {
                    let Ok(json) = m.to_json() else { continue };
                    let Ok(frame) = encode_frame(&json, u32::MAX >> 8) else {
                        continue;
                    };
                    frame
                }
            };
            if send.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    loop {
        let body = tokio::select! {
            r = read_frame(&mut recv, limit, Duration::from_secs(5)) => r,
            _ = conn.closed() => break,
        };
        let Ok(Some(body)) = body else { break };
        state.lock().unwrap().sizes.push(body.len());
        let Ok(msg) = DeviceMessage::parse(&body) else {
            continue;
        };
        on_message(&state, &script, id, msg);
    }
    state.lock().unwrap().devices.remove(&id);
}

fn on_message(state: &Arc<Mutex<State>>, script: &Arc<Script>, from: DeviceId, msg: DeviceMessage) {
    let mut st = state.lock().unwrap();
    st.received.push((from, msg.clone()));
    match msg {
        DeviceMessage::PunchRequest(r) => {
            if !st.devices.contains_key(&r.peer) {
                send_to(
                    &st,
                    &from,
                    ServerMessage::PunchRefused(PunchRefused {
                        request_id: r.request_id,
                        reason: RefuseReason::NotFound,
                    }),
                );
                return;
            }
            st.next += 1;
            let punch_id = st.next;
            st.punches.insert(
                punch_id,
                Punch {
                    a: from,
                    b: r.peer,
                    request_id: r.request_id,
                },
            );
            send_to(
                &st,
                &r.peer,
                ServerMessage::PunchOffer(PunchOffer { punch_id, from }),
            );
        }
        DeviceMessage::PunchAnswer(ans) => {
            let Some(p) = st.punches.get(&ans.punch_id) else {
                return;
            };
            let (a, b, request_id) = (p.a, p.b, p.request_id);
            if !ans.accept {
                let reason = match ans.reason {
                    Some(AnswerReason::LimitsInvalid) => RefuseReason::PeerLimitsInvalid,
                    _ => RefuseReason::Declined,
                };
                send_to(
                    &st,
                    &a,
                    ServerMessage::PunchRefused(PunchRefused { request_id, reason }),
                );
                return;
            }
            let (Some(da), Some(db)) = (st.devices.get(&a), st.devices.get(&b)) else {
                return;
            };
            let (ta, tb) = (PunchToken::random().unwrap(), PunchToken::random().unwrap());
            let go = |peer: DeviceId, role, request_id, cands, local, remote| PunchGo {
                punch_id: ans.punch_id,
                request_id,
                peer,
                role,
                mode: PunchMode::All,
                candidates: cands,
                peer_nat: NatMapping::Independent,
                token_local: local,
                token_peer: remote,
                start_after_ms: script.start_after_ms,
                window_ms: script.limits.punch_window_ms,
                handshake_ms: script.limits.quic_handshake_ms,
            };
            let mut go_a = go(
                b,
                Role::Controlling,
                Some(request_id),
                candidates(db),
                ta,
                tb,
            );
            let mut go_b = go(a, Role::Controlled, None, candidates(da), tb, ta);
            (script.go_a)(&mut go_a);
            (script.go_b)(&mut go_b);
            send_to(&st, &a, ServerMessage::PunchGo(go_a));
            send_to(&st, &b, ServerMessage::PunchGo(go_b));
        }
        DeviceMessage::PunchResult(r) if r.reason == Some(ResultReason::LimitsInvalid) => {
            let (Some(delay), Some(p)) =
                (script.forward_limits_invalid, st.punches.get(&r.punch_id))
            else {
                return;
            };
            let other = if p.a == from { p.b } else { p.a };
            let Some(tx) = st.devices.get(&other).map(|d| d.tx.clone()) else {
                return;
            };
            let msg = ServerMessage::PunchCancel(PunchCancel {
                punch_id: r.punch_id,
                reason: CancelReason::PeerLimitsInvalid,
            });
            drop(st);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _sent = tx.send(Out::Msg(msg)).is_ok();
            });
        }
        _ => {}
    }
}
