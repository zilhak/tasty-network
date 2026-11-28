//! The transport's P2P link between two separate OS processes (`p2p-link --listen` and
//! `p2p-link --dial`), introduced by a rendezvous server in the test process on loopback
//! (docs/design/done-criteria.md D4 (c), D9; docs/design/testing.md "P2P 시험", "연결 목록
//! 시험").
//!
//! Every test has its own server and devices, so the tests run in parallel. Each ends both
//! processes with `quit`, checks that none is left, shuts the server down in time (every
//! device closed its server connection) and removes the key directories.

mod support;

use std::net::SocketAddr;

use support::p2p::{Device, P2P_STEP, P2pScene};
use support::process::{Proc, log};
use support::scene::{LOCAL_STEP, decoded};
use support::table::{read_table, wait_table};
use tasty_net_demo::Snapshot;
use tasty_net_demo::field;
use tasty_net_demo::table::{EntryView, TableView};

/// The listener's instance ID; a space in it checks that the dialer's line encodes it.
const LISTENER_INSTANCE: &str = "p2p listener";

/// Exit code of a dialer whose peer refused.
const EXIT_REFUSED: i32 = 3;

/// A listener paired with the dialer and a dialer whose link is up: the `dialer connected`
/// line.
struct Linked {
    scene: P2pScene,
    a: Device,
    b: Device,
    dialer: Proc,
    listener: Proc,
    connected: String,
}

fn linked() -> Linked {
    let mut scene = P2pScene::new();
    let a = scene.device("a");
    let b = scene.device("b");
    let mut listener = scene.listen_as(&b, &[&a], &[], &["--instance-id", LISTENER_INSTANCE]);
    let mut dialer = scene.dial(&a, &b);
    let connected = dialer.expect_prefix("dialer connected ", P2P_STEP);
    log(&format!(
        "pids listener={} dialer={}",
        listener.pid(),
        dialer.pid()
    ));
    assert_ne!(listener.pid(), dialer.pid(), "two separate processes");
    assert_eq!(
        field(&connected, "peer"),
        Some(b.id.as_str()),
        "{connected}"
    );
    assert_eq!(decoded(&connected, "instance"), LISTENER_INSTANCE);
    let attached = listener.expect_prefix("listener attached ", LOCAL_STEP);
    assert_eq!(field(&attached, "client"), Some("1"), "{attached}");
    assert_eq!(field(&attached, "peer"), Some(a.id.as_str()), "{attached}");
    Linked {
        scene,
        a,
        b,
        dialer,
        listener,
        connected,
    }
}

impl Linked {
    /// Detaches, ends both processes, then checks the cleanup.
    fn finish(mut self) {
        detach(&mut self.dialer);
        P2pScene::quit(&mut self.dialer, "dialer");
        listener_link_ended(&mut self.listener);
        P2pScene::quit(&mut self.listener, "listener");
        drop((self.dialer, self.listener));
        self.scene.finish();
    }
}

/// Waits for the listener's end of the link: it read the Detach and the EOF after it, and
/// its own finish ended before the drain cap (its reader was done).
fn listener_link_ended(listener: &mut Proc) {
    let ended = listener.expect_prefix("listener link-ended ", LOCAL_STEP);
    assert_eq!(field(&ended, "end"), Some("detach"), "{ended}");
    assert_eq!(field(&ended, "finish"), Some("reader_ended"), "{ended}");
}

/// Ends the link with `detach`. The listener reads the dialer's EOF after the Detach and
/// then ends its side (docs/design/transport.md "P2P의 닫기 범위"), so the dialer's reader
/// ends before the drain cap.
fn detach(dialer: &mut Proc) {
    dialer.send_line("detach");
    let detached = dialer.expect_prefix("dialer detached ", LOCAL_STEP);
    assert_eq!(
        field(&detached, "finish"),
        Some("reader_ended"),
        "{detached}"
    );
}

/// Sends `text` and waits for its echo.
fn echo(dialer: &mut Proc, text: &str) {
    dialer.send_line(&format!("send {text}"));
    let want = format!("dialer echo {text}");
    dialer.expect(&format!("{want:?}"), LOCAL_STEP, |l| l == want);
}

/// The link comes up between two processes; the listener's first Control frame is its
/// snapshot and Data comes back unchanged.
#[test]
fn p2p_attach_echo() {
    let mut linked = linked();
    let control = linked.dialer.expect_prefix("dialer control ", LOCAL_STEP);
    let snapshot = Snapshot::parse(control.trim_start_matches("dialer control ").as_bytes())
        .unwrap_or_else(|e| panic!("{control}: {e}"));
    assert_eq!(
        snapshot,
        Snapshot {
            instance_id: LISTENER_INSTANCE.to_string(),
            client_id: 1,
        }
    );
    echo(&mut linked.dialer, "hello");
    echo(&mut linked.dialer, "over p2p, twice");
    linked.finish();
}

/// A device the listener has not paired with gets no link. With the dialer off the
/// listener's list, the listener declines the server's offer (`not_paired`) and the dialer's
/// punch fails as declined. With a pairing withdrawn after the offer (the test hook of
/// `p2p-link`), the QUIC connection comes up and the listener closes it with code 1
/// `not_paired`; the dialer's handshake ends with that code
/// (docs/design/handshake.md "흐름 (P2P 경로)"). Neither reaches the domain check.
#[test]
fn unpaired_peer_is_refused() {
    let mut scene = P2pScene::new();

    // Not on the list at all: refused at the offer.
    let a = scene.device("a");
    let b = scene.device("b");
    let other = scene.device("other");
    let mut listener = scene.listen(&b, &[&other], &[]);
    let mut dialer = scene.dial(&a, &b);
    let declined = dialer.expect_prefix("dialer declined ", P2P_STEP);
    let status = dialer.wait_exit(LOCAL_STEP);
    assert_eq!(status.code(), Some(EXIT_REFUSED), "{declined}");
    let closed = listener.expect_prefix("listener attempt-closed ", LOCAL_STEP);
    assert_eq!(field(&closed, "cause"), Some("not_paired"), "{closed}");

    // Paired at the offer, not after it: the QUIC connection is closed with `not_paired`.
    let c = scene.device("c");
    let d = scene.device("d");
    let mut withdrawn = scene.listen(&d, &[&c], &[("TASTY_NET_DEMO_UNPAIR_AFTER_OFFER", "1")]);
    let mut late = scene.dial(&c, &d);
    let closed_link = late.expect_prefix("dialer closed ", P2P_STEP);
    assert_eq!(
        field(&closed_link, "reason"),
        Some("not_paired"),
        "{closed_link}"
    );
    let status = late.wait_exit(LOCAL_STEP);
    assert_eq!(status.code(), Some(EXIT_REFUSED), "{closed_link}");
    let closed = withdrawn.expect_prefix("listener attempt-closed ", LOCAL_STEP);
    assert_eq!(field(&closed, "cause"), Some("not_paired"), "{closed}");

    for (proc, name) in [(&dialer, "first"), (&late, "second")] {
        let lines = proc.transcript.lines();
        assert!(
            !lines.iter().any(|l| l.starts_with("dialer connected ")),
            "the {name} dialer got no link:\n{}",
            proc.transcript.render()
        );
    }
    for proc in [&listener, &withdrawn] {
        assert!(
            !proc
                .transcript
                .lines()
                .iter()
                .any(|l| l.starts_with("listener attached ")),
            "the domain check saw no attach:\n{}",
            proc.transcript.render()
        );
    }
    P2pScene::quit(&mut listener, "listener");
    P2pScene::quit(&mut withdrawn, "listener");
    drop((dialer, late, listener, withdrawn));
    scene.finish();
}

/// `quit` cancels a dial in progress: with the peer not on the server the dialer asks again,
/// and a `quit` then ends it with exit code 0 well before its patience (`--patience-ms`,
/// default 30 s) runs out.
#[test]
fn quit_cancels_a_dial_in_progress() {
    let mut scene = P2pScene::new();
    let a = scene.device("a");
    // Never started: the server does not know it.
    let b = scene.device("b");
    let mut dialer = scene.dial(&a, &b);
    dialer.expect_prefix("dialer retry ", P2P_STEP);
    dialer.send_line("quit");
    dialer.expect_prefix("dialer cancelled", LOCAL_STEP);
    dialer.expect_prefix("dialer bye", LOCAL_STEP);
    let status = dialer.wait_exit(LOCAL_STEP);
    assert!(
        status.success(),
        "the dialer after quit: {status}\n{}",
        dialer.transcript.render()
    );
    drop(dialer);
    scene.finish();
}

/// A bulk channel under the link carries more than many bulk chunks: the listener reads the
/// same number of file bytes and chunks with the same SHA-256 as the dialer sent, and the
/// dialer reads them all back unchanged. The link still echoes afterwards.
#[test]
fn bulk_transfer_matches_bytes() {
    let mut linked = linked();
    // Not a multiple of the chunk size, so the last chunk is a short one.
    let size: u64 = 4 * 1024 * 1024 + 12_345;
    linked.dialer.send_line(&format!("bulk {size}"));
    let done = linked.dialer.expect_prefix("dialer bulk-", P2P_STEP);
    assert!(done.starts_with("dialer bulk-done "), "{done}");
    let number = |line: &str, key: &str| -> u64 {
        field(line, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no {key}= in {line:?}"))
    };
    let chunk_bytes = number(&done, "chunk_bytes");
    let chunks = number(&done, "chunks");
    assert_eq!(number(&done, "bytes_sent"), size, "{done}");
    assert_eq!(number(&done, "bytes_received"), size, "{done}");
    assert_eq!(chunks, size.div_ceil(chunk_bytes), "{done}");
    assert!(chunks > 8, "more than a few bulk chunks: {done}");
    let sent = field(&done, "sha256_sent").expect("sha256_sent");
    assert_eq!(sent.len(), 64, "{done}");
    assert_eq!(field(&done, "sha256_received"), Some(sent), "{done}");
    assert_eq!(field(&done, "match"), Some("true"), "{done}");

    let accepted = linked
        .listener
        .expect_prefix("listener bulk-accepted ", LOCAL_STEP);
    assert_eq!(
        field(&accepted, "peer"),
        Some(linked.a.id.as_str()),
        "{accepted}"
    );
    let ended = linked
        .listener
        .expect_prefix("listener bulk-ended ", LOCAL_STEP);
    assert_eq!(number(&ended, "bytes"), size, "{ended}");
    assert_eq!(number(&ended, "chunks"), chunks, "{ended}");
    assert_eq!(field(&ended, "sha256"), Some(sent), "{ended}");
    assert_eq!(field(&ended, "end"), Some("eof"), "{ended}");
    assert_eq!(field(&ended, "finish"), Some("reader_ended"), "{ended}");

    echo(&mut linked.dialer, "after the bulk transfer");
    linked.finish();
}

/// For the listener's ended entry: the transition that ended it carries `p2p_closed` (the
/// dialer closed the connection). Whether the entry's last cause was then replaced by its
/// link's end (`ended`): the transport may note the link's end on the entry after the
/// connection's end once the listener finishes its link at once (its reader done), so the
/// test accepts that case and logs it.
fn listener_end_replaced(who: &str, entry: &EntryView) -> bool {
    if who != "listener" {
        return false;
    }
    let history = entry.history();
    let ending = history.last().and_then(|(_, _, cause)| cause.as_deref());
    assert_eq!(ending, Some("p2p_closed"), "{who}: {entry:#?}");
    let replaced = entry.value("last_cause") == "ended";
    if replaced {
        log(
            "listener last_cause=ended after the ending transition p2p_closed (link end noted after the connection end)",
        );
    }
    replaced
}

/// The three values of a `udp` segment's local side.
struct UdpLocal {
    bind: SocketAddr,
    ip: String,
    reflexive: Option<SocketAddr>,
}

fn udp_local(entry: &EntryView) -> UdpLocal {
    let text = entry.value("seg.udp.local");
    let value = |key: &str| {
        text.split(", ")
            .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("no {key} in {text:?}"))
    };
    let address = |key: &str| value(key).parse::<SocketAddr>().ok();
    UdpLocal {
        bind: address("local_bind").unwrap_or_else(|| panic!("local_bind in {text:?}")),
        ip: value("local_ip").to_string(),
        reflexive: address("local_reflexive"),
    }
}

/// The one `connected` entry of `view`.
fn connected_entry(view: &TableView) -> &EntryView {
    let up: Vec<_> = view
        .entries
        .iter()
        .filter(|e| e.get("state") == Some("connected"))
        .collect();
    assert_eq!(up.len(), 1, "one connected entry: {view:#?}");
    up[0]
}

/// Whether the platform gives the destination IP of a received packet to quinn, so the
/// accepting side (the QUIC server) knows its `local_ip`. Linux and macOS do (seen in this
/// test); on other systems (Windows) the test accepts either and logs which it was.
const DESTINATION_IP_KNOWN: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Both ends report the link's QUIC connection as one `p2p_udp` entry: the peer device, the
/// `udp` segment (the dialer's bound port and reflexive address, its `local_ip` unknown; each
/// end's remote address is the other's reflexive one), the introducing server's address and
/// key, the candidate kind that worked, the link with the same `link_id`; after the link's
/// end both entries are `disconnected` with a cause.
#[test]
fn connection_table_reports_p2p() {
    let mut linked = linked();
    echo(&mut linked.dialer, "before the table");
    let link_id = field(&linked.connected, "link_id")
        .expect("link_id")
        .to_string();
    let server = linked.scene.server();
    let (server_addr, server_key) = (server.addr.to_string(), server.public_key.to_string());

    let a_view = read_table(&mut linked.dialer, "dialer");
    assert_eq!(a_view.device, linked.a.id);
    // The accepting end counts the link once its handshake is done; ask until it shows.
    let b_view = wait_table(&mut linked.listener, "listener", "the link", |view| {
        view.entries
            .iter()
            .any(|e| e.get("state") == Some("connected") && e.links.len() == 1)
    });
    assert_eq!(b_view.device, linked.b.id);
    let a = connected_entry(&a_view);
    let b = connected_entry(&b_view);

    for (entry, side, peer) in [(a, "dialer", &linked.b), (b, "acceptor", &linked.a)] {
        assert_eq!(entry.value("path"), "p2p_udp", "{entry:#?}");
        assert_eq!(entry.value("local_side"), side, "{entry:#?}");
        assert_eq!(entry.value("peer_device"), peer.id, "{entry:#?}");
        assert_eq!(entry.value("via"), "rendezvous", "{entry:#?}");
        assert_eq!(entry.value("via_server"), server_addr, "{entry:#?}");
        assert_eq!(entry.value("via_server_key"), server_key, "{entry:#?}");
        // On loopback the only candidate is the address the server saw: a loopback host
        // address is not sent (docs/design/testing.md "실제 망 확인").
        assert_eq!(
            entry.value("via_candidate"),
            "server_reflexive",
            "{entry:#?}"
        );
        assert!(entry.get("p2p_id").is_some(), "{entry:#?}");
        assert_eq!(entry.value("segments"), "udp", "{entry:#?}");
        assert_eq!(entry.links.len(), 1, "{entry:#?}");
        let link = &entry.links[0];
        assert_eq!(link.value("link_id"), link_id, "{entry:#?}");
        assert_eq!(link.value("transport"), "p2p_udp", "{entry:#?}");
        assert_eq!(link.value("holder"), "dialer", "{entry:#?}");
        assert_eq!(link.value("local_side"), side, "{entry:#?}");
    }
    assert_eq!(a.value("peer_instance"), LISTENER_INSTANCE);
    assert_eq!(a.value("instance_verified"), "true");
    let history = a.history();
    assert!(
        history
            .iter()
            .any(|(_, phase, _)| phase.as_deref() == Some("Requesting")),
        "the dialer's history has its P2P steps: {history:?}"
    );

    let (a_udp, b_udp) = (udp_local(a), udp_local(b));
    assert_ne!(a_udp.bind.port(), 0, "{a:#?}");
    assert_eq!(
        a_udp.ip, "unknown",
        "the QUIC client knows no local IP: {a:#?}"
    );
    let a_reflexive = a_udp.reflexive.expect("the dialer's reflexive address");
    assert_eq!(
        a_reflexive.port(),
        a_udp.bind.port(),
        "no NAT on loopback: {a:#?}"
    );
    let b_reflexive = b_udp.reflexive.expect("the listener's reflexive address");
    assert_eq!(a.value("seg.udp.remote"), b_reflexive.to_string(), "{a:#?}");
    assert_eq!(b.value("seg.udp.remote"), a_reflexive.to_string(), "{b:#?}");
    log(&format!(
        "listener local_ip={} on {} (destination IP expected: {DESTINATION_IP_KNOWN})",
        b_udp.ip,
        std::env::consts::OS
    ));
    if DESTINATION_IP_KNOWN || b_udp.ip != "unknown" {
        assert_eq!(b_udp.ip, b_reflexive.ip().to_string(), "{b:#?}");
    }

    // The link's end ends both entries, each with its cause.
    let (a_id, b_id) = (a.id(), b.id());
    detach(&mut linked.dialer);
    listener_link_ended(&mut linked.listener);
    // The dialer closed it after its last channel (`ended`); the listener saw the close with
    // code 0 (`p2p_closed`, docs/design/transport.md "P2P의 닫기 범위").
    for (proc, who, id, want) in [
        (&mut linked.dialer, "dialer", a_id, "ended"),
        (&mut linked.listener, "listener", b_id, "p2p_closed"),
    ] {
        let view = wait_table(proc, who, "the entry's end", |view| {
            view.entry(id)
                .is_some_and(|e| e.get("state") == Some("disconnected"))
        });
        let entry = view.entry(id).expect("the ended entry is kept");
        log(&format!(
            "{who} entry {id} ended with {} ({})",
            entry.value("last_cause"),
            entry.get("cause_text").unwrap_or("-")
        ));
        if !listener_end_replaced(who, entry) {
            assert_eq!(entry.value("last_cause"), want, "{who}: {entry:#?}");
        }
    }
    P2pScene::quit(&mut linked.dialer, "dialer");
    P2pScene::quit(&mut linked.listener, "listener");
    drop((linked.dialer, linked.listener));
    linked.scene.finish();
}
