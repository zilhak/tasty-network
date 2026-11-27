//! Reading both ends' connection tables in the two-process SSH tests
//! (docs/design/testing.md "연결 목록 시험"): the stdin line `table` and its snapshot lines
//! (`tasty_net_demo::table`), and the checks the forward and the role reversed tests share.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use super::process::{Proc, log};
use super::scene::{LOCAL_STEP, Ready, SSH_STEP, Scene};
use tasty_net_demo::table::{EntryView, TableView};

/// How often a test asks again for a table that is not yet what it waits for.
const POLL: Duration = Duration::from_millis(200);

/// Asks `proc` (whose event lines start with `who`) for its connection table and reads the
/// snapshot. Lines of other events that come before the snapshot are skipped.
pub fn read_table(proc: &mut Proc, who: &str) -> TableView {
    proc.send_line("table");
    let begin = format!("{who} table ");
    let first = proc.expect(&format!("{begin:?}"), LOCAL_STEP, |l| l.starts_with(&begin));
    let conn = format!("{who} conn ");
    let link = format!("{who} link ");
    let end = format!("{who} table-end ");
    let mut lines = vec![first];
    loop {
        let line = proc.expect("a snapshot line", LOCAL_STEP, |l| {
            l.starts_with(&conn) || l.starts_with(&link) || l.starts_with(&end)
        });
        let done = line.starts_with(&end);
        lines.push(line);
        if done {
            break;
        }
    }
    TableView::parse(who, &lines)
        .unwrap_or_else(|e| panic!("{who}: the snapshot does not read back ({e}): {lines:#?}"))
}

/// Reads the table of `proc` until `done` holds for it, and returns that snapshot.
pub fn wait_table(
    proc: &mut Proc,
    who: &str,
    what: &str,
    mut done: impl FnMut(&TableView) -> bool,
) -> TableView {
    let deadline = Instant::now() + SSH_STEP;
    loop {
        let view = read_table(proc, who);
        if done(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "{who}: no table with {what} within {SSH_STEP:?}; the last one: {view:#?}"
        );
        std::thread::sleep(POLL);
    }
}

/// The entries of `view` in `state`.
pub fn in_state<'a>(view: &'a TableView, state: &str) -> Vec<&'a EntryView> {
    view.entries
        .iter()
        .filter(|e| e.get("state") == Some(state))
        .collect()
}

/// Whether `entry` carries one attach link, one bulk channel and one request channel.
pub fn carries_all_channels(entry: &EntryView) -> bool {
    entry.links.len() == 1 && entry.get("bulk") == Some("1") && entry.get("requests") == Some("1")
}

/// `text` as a loopback socket address with a port that is not 0.
pub fn loopback_addr(text: &str) -> SocketAddr {
    let addr: SocketAddr = text
        .parse()
        .unwrap_or_else(|e| panic!("{text:?} is not a socket address: {e}"));
    assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST, "{text}");
    assert_ne!(addr.port(), 0, "{text}");
    addr
}

/// Checks the open time and the RTT of an SSH link row: an open time, and no RTT.
fn check_link_times(link: &tasty_net_demo::table::Row) {
    assert!(link.number("opened_at_ms") > 0, "{link:?}");
    assert_eq!(link.get("rtt_ms"), None, "SSH measures no RTT: {link:?}");
}

/// What one direction's entries show (docs/design/transport.md "필드").
pub struct Shape {
    /// The launcher's `path`.
    pub path: &'static str,
    /// Both ends' `direction` of the entry with the attach link.
    pub direction: &'static str,
    /// The link's `holder`.
    pub holder: &'static str,
    /// The link's edge value (`transport`).
    pub transport: &'static str,
}

/// A forward link: the dialer holds.
pub const FORWARD: Shape = Shape {
    path: "ssh_forward",
    direction: "forward",
    holder: "dialer",
    transport: "ssh_forward",
};

/// A role reversed link: the acceptor holds.
pub const ROLE_REVERSED: Shape = Shape {
    path: "ssh_role_reversed",
    direction: "role_reversed",
    holder: "acceptor",
    transport: "ssh_role_reversed",
};

/// The instance ID the server saw for its attach link `client` (its `server attached` line).
pub fn dialer_instance(server: &Proc, client: u32) -> String {
    let prefix = format!("server attached client={client} ");
    let line = server
        .transcript
        .lines()
        .into_iter()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line\n{}", server.transcript.render()));
    super::scene::decoded(&line, "peer")
}

/// Checks both ends' entries of one tunnel that carries an attach link of `shape`, a bulk
/// channel and a request channel (the dialer's `--channels`). The dialer has one entry for
/// the tunnel; the server one `ssh_inbound` entry per channel, the bulk one naming the
/// attach one as its parent. Returns the dialer's entry.
pub fn check_tunnel_entries(
    scene: &Scene,
    server: &mut Proc,
    ready: &Ready,
    dialer: &mut Proc,
    dialer_instance: &str,
    shape: &Shape,
) -> Tunnel {
    let near = wait_table(
        dialer,
        "dialer",
        "one connected entry with all channels",
        |v| {
            v.entries.len() == 1
                && in_state(v, "connected")
                    .iter()
                    .all(|e| carries_all_channels(e))
                && !in_state(v, "connected").is_empty()
        },
    );
    let far = wait_table(server, "server", "three connected entries", |v| {
        in_state(v, "connected").len() == 3
            && v.entries.iter().map(|e| e.links.len()).sum::<usize>() == 1
    });
    assert_ne!(near.device, far.device, "two devices");
    let sshd = scene.sshd();
    let ssh_target = format!("{}:{}", sshd.destination(), sshd.port);
    let remote = format!("127.0.0.1:{}", ready.port);

    // The end that launched ssh: one entry for the tunnel and everything on it.
    let entry = &near.entries[0];
    log(&format!("dialer entry: {:?}", entry.row));
    assert_eq!(entry.value("path"), shape.path);
    assert_eq!(entry.value("direction"), shape.direction);
    assert_eq!(entry.value("local_side"), "dialer");
    assert_eq!(entry.value("state"), "connected");
    assert_eq!(entry.get("phase"), None);
    assert_eq!(entry.get("last_cause"), None);
    assert_eq!(entry.get("previous"), None);
    assert_eq!(entry.get("parent"), None);
    assert_eq!(
        entry.value("peer_device"),
        far.device,
        "the server's device"
    );
    assert_eq!(entry.value("peer_instance"), ready.instance);
    assert_eq!(entry.value("instance_verified"), "true");
    assert_eq!(entry.value("segments"), "local_forward,ssh,remote_listener");
    let forward_end = loopback_addr(entry.value("seg.local_forward.local"));
    let mut open: Vec<SocketAddr> = entry
        .value("seg.local_forward.remote")
        .split(", ")
        .map(loopback_addr)
        .collect();
    assert_eq!(open.len(), 3, "attach, bulk and request channels: {open:?}");
    open.sort_unstable();
    open.dedup();
    assert_eq!(open.len(), 3, "three channels, three addresses: {open:?}");
    assert!(!open.contains(&forward_end), "{open:?} {forward_end}");
    assert_eq!(entry.get("seg.ssh.local"), None);
    assert_eq!(entry.value("seg.ssh.remote"), ssh_target);
    assert_eq!(entry.get("seg.remote_listener.local"), None);
    assert_eq!(entry.value("seg.remote_listener.remote"), remote);
    // Through ssh, with no rendezvous server.
    assert_eq!(entry.value("via"), "ssh");
    assert_eq!(entry.value("via_target"), ssh_target);
    assert_eq!(entry.value("via_remote_port"), ready.port.to_string());
    assert_eq!(entry.get("rtt_ms"), None, "SSH measures no RTT");
    let steps: Vec<String> = entry
        .history()
        .into_iter()
        .map(|(state, phase, _)| format!("{state}/{}", phase.unwrap_or_default()))
        .collect();
    assert_eq!(
        steps,
        [
            "connecting/port_discovery",
            "connecting/tunnel",
            "connecting/handshake",
            "connected/",
        ],
        "{entry:?}"
    );
    let link = &entry.links[0];
    assert_eq!(link.value("holder"), shape.holder);
    assert_eq!(
        link.value("local_side"),
        "dialer",
        "this end opened the link"
    );
    assert_eq!(link.value("transport"), shape.transport);
    check_link_times(link);

    // The end that accepted the loopback connections: one entry per channel.
    let entries = in_state(&far, "connected");
    for e in &entries {
        log(&format!("server entry: {:?}", e.row));
        assert_eq!(e.value("path"), "ssh_inbound");
        assert_eq!(e.value("local_side"), "acceptor");
        assert_eq!(e.value("via"), "loopback");
        assert_eq!(e.value("segments"), "loopback");
        assert_eq!(e.value("seg.loopback.local"), remote);
        loopback_addr(e.value("seg.loopback.remote"));
        assert_eq!(e.value("peer_device"), near.device, "the dialer's device");
        assert_eq!(e.value("peer_instance"), dialer_instance);
        assert_eq!(e.value("instance_verified"), "true");
        assert_eq!(e.get("previous"), None);
    }
    let attach = entries
        .iter()
        .find(|e| e.links.len() == 1)
        .expect("the attach entry");
    assert_eq!(attach.value("direction"), shape.direction);
    assert_eq!(attach.get("parent"), None);
    let far_link = &attach.links[0];
    assert_eq!(
        far_link.value("link_id"),
        link.value("link_id"),
        "one link ID"
    );
    assert_eq!(far_link.value("holder"), shape.holder);
    assert_eq!(far_link.value("local_side"), "acceptor");
    assert_eq!(far_link.value("transport"), shape.transport);
    check_link_times(far_link);
    let mut peers: Vec<SocketAddr> = entries
        .iter()
        .map(|e| loopback_addr(e.value("seg.loopback.remote")))
        .collect();
    peers.sort_unstable();
    peers.dedup();
    assert_eq!(
        peers.len(),
        3,
        "one loopback connection per channel: {peers:?}"
    );
    let bulk = entries
        .iter()
        .find(|e| e.get("bulk") == Some("1"))
        .expect("the bulk entry");
    assert_eq!(bulk.value("parent"), attach.id().to_string(), "{bulk:?}");
    assert_eq!(bulk.value("direction"), "none");
    let request = entries
        .iter()
        .find(|e| e.get("requests") == Some("1"))
        .expect("the request entry");
    assert_eq!(request.get("parent"), None);
    assert_eq!(request.value("direction"), "none");
    let ids = [attach.id(), bulk.id(), request.id()];
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "{ids:?}"
    );
    Tunnel {
        entry: entry.id(),
        link_id: link.value("link_id").to_string(),
    }
}

/// The dialer's entry of a tunnel and the link ID of its attach link.
pub struct Tunnel {
    pub entry: u64,
    pub link_id: String,
}

/// After the tunnel of the dialer's entry `tunnel.entry` (the old one) was killed and the dialer connected again:
/// the old entry is `disconnected` with its cause, and a new entry with a new ID names it,
/// went from `reconnecting` to `connected` and carries the channels again. The server's
/// entries of the old tunnel end too.
pub fn check_reconnected(server: &mut Proc, dialer: &mut Proc, tunnel: &Tunnel, shape: &Shape) {
    let old = tunnel.entry;
    let near = wait_table(dialer, "dialer", "the old entry ended and a new one", |v| {
        v.entry(old)
            .is_some_and(|e| e.get("state") == Some("disconnected"))
            && v.entries.iter().any(|e| {
                e.get("previous") == Some(old.to_string().as_str())
                    && e.get("state") == Some("connected")
                    && carries_all_channels(e)
            })
    });
    let ended = near.entry(old).expect("the old entry");
    let cause = ended.get("last_cause").unwrap_or("-");
    log(&format!(
        "the old entry {old} ended with {cause} ({})",
        ended.get("cause_text").unwrap_or("-")
    ));
    assert!(
        matches!(cause, "tunnel_exited" | "lost"),
        "a killed tunnel ends its entry as exited or lost: {ended:?}"
    );
    assert!(ended.links.is_empty(), "{ended:?}");
    // The redial may first have joined the old tunnel while it still ran: that entry names
    // the old one too, and ended as a shared tunnel. The new connection is the connected one.
    let naming: Vec<&EntryView> = near
        .entries
        .iter()
        .filter(|e| e.get("previous") == Some(old.to_string().as_str()))
        .collect();
    let connected: Vec<&&EntryView> = naming
        .iter()
        .filter(|e| e.get("state") == Some("connected"))
        .collect();
    assert_eq!(
        connected.len(),
        1,
        "one connected reconnection: {naming:#?}"
    );
    let new = *connected[0];
    for other in naming.iter().filter(|e| e.id() != new.id()) {
        log(&format!("another entry naming {old}: {:?}", other.row));
        assert_eq!(other.value("state"), "disconnected", "{other:?}");
        assert!(other.get("last_cause").is_some(), "{other:?}");
    }
    log(&format!("the new entry: {:?}", new.row));
    assert!(new.id() > old, "a new ID: {new:?}");
    assert_eq!(new.value("previous_dropped"), "false");
    assert_eq!(new.value("path"), shape.path);
    assert_eq!(new.value("direction"), shape.direction);
    assert!(new.get("redial_attempt").is_some(), "the redialer made it");
    let history = new.history();
    assert_eq!(
        history.first().map(|h| h.0.as_str()),
        Some("reconnecting"),
        "{new:?}"
    );
    assert_eq!(
        history.last().map(|h| h.0.as_str()),
        Some("connected"),
        "{new:?}"
    );
    assert!(
        history
            .iter()
            .all(|h| matches!(h.0.as_str(), "reconnecting" | "connected")),
        "it went from reconnecting to connected: {new:?}"
    );
    assert_ne!(
        new.links[0].value("link_id"),
        tunnel.link_id,
        "a new link, a new ID"
    );

    let far = wait_table(server, "server", "the old channels ended", |v| {
        in_state(v, "connected").len() == 3 && in_state(v, "disconnected").len() == 3
    });
    for e in in_state(&far, "disconnected") {
        log(&format!(
            "server entry {} ended with {}",
            e.id(),
            e.get("last_cause").unwrap_or("-")
        ));
        assert!(e.get("last_cause").is_some(), "{e:?}");
    }
}

/// The links of `view` as topology's `LocalLink` input would name them, sorted: link ID, the
/// dialer's and the acceptor's device, the holder's device and the edge value
/// (docs/design/testing.md "토폴로지 입력"). Only connected entries count.
pub fn local_links(view: &TableView) -> Vec<[String; 5]> {
    let mut links = Vec::new();
    for entry in in_state(view, "connected") {
        let peer = entry.value("peer_device").to_string();
        for link in &entry.links {
            let (dialer, acceptor) = match link.value("local_side") {
                "dialer" => (view.device.clone(), peer.clone()),
                _ => (peer.clone(), view.device.clone()),
            };
            let holder = match link.value("holder") {
                "dialer" => dialer.clone(),
                _ => acceptor.clone(),
            };
            links.push([
                link.value("link_id").to_string(),
                dialer,
                acceptor,
                holder,
                link.value("transport").to_string(),
            ]);
        }
    }
    links.sort();
    links
}
