//! SSH role reversal between two OS processes through a real `ssh -L` tunnel
//! (docs/design/done-criteria.md D3, docs/design/transport.md "역할" (b)).
//!
//! The dialer launches ssh, dials and proposes `holder = "acceptor"`: the server, the end
//! that did not launch ssh, holds, and the dialer provides the workspace over the same `-L`
//! tunnel. When the tunnel drops, the dialer (the launcher) connects again. The tests use the
//! scene of `ssh_attach.rs` ([`support::scene`]) and run one at a time for the same reason.
//!
//! Evidence lines go to stderr unconditionally (`[ssh_reverse_attach] ...`): `sshd=<server>`,
//! the process IDs, both ends' role lines, the tunnel PIDs before and after a reconnect, and
//! the connection table entries and links of the connection table tests
//! (docs/design/testing.md "연결 목록 시험").

mod support;

use std::time::Duration;

use support::process::{Proc, log, terminate};
use support::scene::{LOCAL_STEP, Ready, SSH_STEP, Scene, echo, number, one_at_a_time};
use support::table::{
    ROLE_REVERSED, check_reconnected, check_tunnel_entries, dialer_instance, in_state, local_links,
    read_table, wait_table,
};
use tasty_net_demo::table::decode_value;
use tasty_net_demo::{Snapshot, field};

/// The role table of a reversed SSH link, as both ends print it after `local=`.
const REVERSED: &str =
    "launcher=dialer dialer=dialer provider=dialer holder=acceptor redialer=dialer";

/// What follows `local=<side> ` in a roles line.
fn table(line: &str) -> &str {
    let (_, rest) = line
        .split_once(" local=")
        .unwrap_or_else(|| panic!("no local= in {line:?}"));
    rest.split_once(' ').map_or("", |(_, table)| table)
}

/// A server that accepts every role proposal.
fn start_accepting_server(scene: &mut Scene) -> (Proc, Ready) {
    scene.start_server_with(&["--proposals", "accept"])
}

/// A dialer that proposes the role reversal, with extra arguments.
fn start_reversing_dialer(scene: &mut Scene, args: &[&str]) -> Proc {
    let record = scene.record.clone();
    let mut all = vec!["--holder", "acceptor"];
    all.extend_from_slice(args);
    scene.start_dialer_with(&record, &all, &[])
}

/// The server, which holds, sends `text` on its newest link `client`; the dialer, which
/// provides, writes it back.
fn held_echo(server: &mut Proc, dialer: &mut Proc, client: u32, text: &str) {
    server.send_line(&format!("send {text}"));
    let served = format!("dialer served {text}");
    dialer.expect(&format!("{served:?}"), LOCAL_STEP, |l| l == served);
    let echo = format!("server echo client={client} {text}");
    server.expect(&format!("{echo:?}"), LOCAL_STEP, |l| l == echo);
}

/// Waits for one reversed attach: the dialer's `connected` and roles lines, the server's
/// accepted proposal, attach, roles line and the dialer's descriptor. Checks both role tables
/// and returns the tunnel PID.
fn reversed_attach(
    scene: &mut Scene,
    server: &mut Proc,
    ready: &Ready,
    dialer: &mut Proc,
    client: u32,
) -> u32 {
    let line = scene.connected(dialer);
    assert_eq!(field(&line, "instance"), Some(ready.instance.as_str()));
    let tunnel: u32 = number(&line, "tunnel_pid");
    assert_ne!(tunnel, 0, "the dialer reported its tunnel ssh");
    let dialer_roles = dialer.expect_prefix("dialer roles ", LOCAL_STEP);

    let proposal = server.expect_prefix("server proposal ", LOCAL_STEP);
    assert_eq!(field(&proposal, "answer"), Some("accept"), "{proposal}");
    let dialer_instance = field(&proposal, "peer").unwrap_or("").to_string();
    assert_ne!(
        dialer_instance, ready.instance,
        "the proposal came from another instance"
    );
    let attached = format!("server attached client={client} ");
    server.expect(&format!("{attached:?}"), LOCAL_STEP, |l| {
        l.starts_with(&attached)
    });
    let roles_prefix = format!("server roles client={client} ");
    let server_roles = server.expect(&format!("{roles_prefix:?}"), LOCAL_STEP, |l| {
        l.starts_with(&roles_prefix)
    });
    log(&format!(
        "pids server={} dialer={} tunnel_ssh={tunnel} (sshd={})",
        server.pid(),
        dialer.pid(),
        scene.sshd().kind()
    ));
    log(&format!("roles: {dialer_roles} | {server_roles}"));
    assert_ne!(server.pid(), dialer.pid(), "two separate processes");
    assert_eq!(field(&dialer_roles, "local"), Some("dialer"));
    assert_eq!(field(&server_roles, "local"), Some("acceptor"));
    assert_eq!(table(&dialer_roles), REVERSED, "{dialer_roles}");
    assert_eq!(table(&server_roles), REVERSED, "{server_roles}");

    // The provider's descriptor is the link's first Control frame, and it names the dialer.
    let control_prefix = format!("server control client={client} ");
    let control = server.expect(&format!("{control_prefix:?}"), LOCAL_STEP, |l| {
        l.starts_with(&control_prefix)
    });
    let descriptor = Snapshot::parse(&control.as_bytes()[control_prefix.len()..])
        .unwrap_or_else(|e| panic!("the descriptor is not one ({e}): {control}"));
    assert_eq!(
        descriptor,
        Snapshot {
            instance_id: dialer_instance,
            client_id: client,
        },
        "the descriptor names the dialer's instance and the link's client ID: {control}"
    );
    // The dialer provides, so it receives no snapshot.
    assert!(
        !dialer
            .transcript
            .lines()
            .iter()
            .any(|l| l.starts_with("dialer control ")),
        "{}",
        dialer.transcript.render()
    );
    tunnel
}

#[test]
fn reversed_holder_echo() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let mut dialer = start_reversing_dialer(&mut scene, &[]);
    reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 1);
    held_echo(&mut server, &mut dialer, 1, "hello-1");
    held_echo(&mut server, &mut dialer, 1, "hello-2");

    scene.quit_dialer(dialer);
    let ended = server.expect_prefix("server link-ended client=1 ", LOCAL_STEP);
    assert_eq!(field(&ended, "end"), Some("detach"), "{ended}");
    let released = server.expect_prefix("server released ", LOCAL_STEP);
    assert_eq!(field(&released, "live"), Some("0"), "{released}");
    Scene::stop_server(server);
    scene.finish();
}

/// An instance ID with a quote and a backslash reaches the holder unchanged in the descriptor:
/// the dialer serializes it, the server parses it.
#[test]
fn a_quoted_instance_id_survives_the_descriptor() {
    const ID: &str = "desk\"east\\x";
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let mut dialer = start_reversing_dialer(&mut scene, &["--instance-id", ID]);
    reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 1);
    let proposal = server
        .transcript
        .lines()
        .into_iter()
        .find(|l| l.starts_with("server proposal "))
        .expect("the proposal line");
    assert_eq!(field(&proposal, "peer"), Some(ID), "{proposal}");
    held_echo(&mut server, &mut dialer, 1, "hello-quoted");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn reconnect_after_tunnel_kill() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let mut dialer = start_reversing_dialer(&mut scene, &[]);
    let before = reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 1);
    held_echo(&mut server, &mut dialer, 1, "hello-1");

    log(&format!("ending the tunnel ssh pid={before}"));
    terminate(before);
    let lost = dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    let failure = field(&lost, "failure").unwrap_or("");
    log(&format!("the dialer classified the loss as {failure}"));
    assert!(
        matches!(failure, "peer_closed" | "heartbeat_timeout"),
        "a dropped tunnel is a closed peer or a heartbeat timeout: {lost}"
    );
    // The holder does not dial: its side of the link ends, classified like the dialer's, and
    // it waits for the launcher.
    let ended = server.expect_prefix("server link-ended client=1 ", LOCAL_STEP);
    let server_end = field(&ended, "end").unwrap_or("");
    log(&format!("the server classified the loss as {server_end}"));
    assert!(
        matches!(server_end, "peer_closed" | "heartbeat_timeout"),
        "the holder sees a dropped tunnel as a closed peer or a heartbeat timeout: {ended}"
    );

    let after = reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 2);
    log(&format!("tunnel ssh pid before={before} after={after}"));
    assert_ne!(before, after, "the reconnect started a new tunnel ssh");
    held_echo(&mut server, &mut dialer, 2, "hello-again");
    let proposals = server
        .transcript
        .lines()
        .iter()
        .filter(|l| l.starts_with("server proposal "))
        .count();
    assert_eq!(proposals, 2, "each attach proposed the reversal again");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn refused_proposal_stops_without_retry() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    // The default answers every proposal with a refusal.
    let (mut server, _ready) = scene.start_server();
    let mut dialer = start_reversing_dialer(&mut scene, &[]);

    let rejected = dialer.expect_prefix("dialer rejected ", SSH_STEP);
    log(&format!("the dialer ended with: {rejected}"));
    assert_eq!(
        field(&rejected, "reason"),
        Some("role_refused"),
        "{rejected}"
    );
    let status = dialer.wait_exit(LOCAL_STEP);
    assert_eq!(
        status.code(),
        Some(3),
        "the dialer ends with the rejection exit code"
    );
    scene.track_ssh_children();

    let proposal = server.expect_prefix("server proposal ", LOCAL_STEP);
    assert_eq!(field(&proposal, "answer"), Some("refuse"), "{proposal}");
    server.expect_prefix("server refused reason=role_refused", LOCAL_STEP);
    // The dialer has exited, so nothing more can come; the server saw exactly one attempt.
    std::thread::sleep(Duration::from_secs(1));
    let count =
        |lines: &[String], prefix: &str| lines.iter().filter(|l| l.starts_with(prefix)).count();
    let lines = server.transcript.lines();
    let proposals = count(&lines, "server proposal ");
    let refused = count(&lines, "server refused ");
    let attached = count(&lines, "server attached ");
    let attempts = count(&dialer.transcript.lines(), "dialer attempt ");
    log(&format!(
        "attempts: dialer {attempts}, server proposals {proposals}, refusals {refused}, \
         attaches {attached}"
    ));
    assert_eq!(
        (attempts, proposals, refused, attached),
        (1, 1, 1, 0),
        "{}",
        server.transcript.render()
    );

    drop(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn self_attach_is_rejected() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    // The dialer claims the server's own instance: the link would attach the server to itself.
    let mut dialer = start_reversing_dialer(&mut scene, &["--instance-id", &ready.instance]);

    let rejected = dialer.expect_prefix("dialer rejected ", SSH_STEP);
    log(&format!("the dialer ended with: {rejected}"));
    assert_eq!(
        field(&rejected, "reason"),
        Some("self_attach"),
        "{rejected}"
    );
    let status = dialer.wait_exit(LOCAL_STEP);
    assert_eq!(
        status.code(),
        Some(3),
        "the dialer ends with the rejection exit code"
    );
    scene.track_ssh_children();

    // The server's side ends without a link: a refusal, or a failed handshake when the dialer
    // saw itself first and closed.
    let server_end = server.expect("a refusal or a failed handshake", LOCAL_STEP, |l| {
        l.starts_with("server refused ") || l.starts_with("server handshake-failed ")
    });
    log(&format!(
        "the server ended the handshake with: {server_end}"
    ));
    std::thread::sleep(Duration::from_secs(1));
    let lines = server.transcript.lines();
    let count = |prefix: &str| lines.iter().filter(|l| l.starts_with(prefix)).count();
    let (proposals, attached) = (count("server proposal "), count("server attached "));
    let attempts = dialer
        .transcript
        .lines()
        .iter()
        .filter(|l| l.starts_with("dialer attempt "))
        .count();
    log(&format!(
        "attempts: dialer {attempts}, server proposals {proposals}, attaches {attached}"
    ));
    assert_eq!(
        (attempts, proposals, attached),
        (1, 0, 0),
        "{}",
        server.transcript.render()
    );

    drop(dialer);
    Scene::stop_server(server);
    scene.finish();
}

/// The connection tables of both ends for a role reversed link whose tunnel also carries a
/// bulk and a request channel, before and after the tunnel is killed and the dialer connects
/// again.
#[test]
fn connection_table_reports_ssh_role_reversed() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let mut dialer = start_reversing_dialer(&mut scene, &["--channels"]);
    let before = reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 1);
    // The request channel discovers the record over SSH and runs a handshake.
    dialer.expect_prefix("dialer channels-open ", SSH_STEP);
    held_echo(&mut server, &mut dialer, 1, "hello-1");
    let instance = dialer_instance(&server, 1);
    let tunnel = check_tunnel_entries(
        &scene,
        &mut server,
        &ready,
        &mut dialer,
        &instance,
        &ROLE_REVERSED,
    );

    log(&format!("ending the tunnel ssh pid={before}"));
    terminate(before);
    dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    let after = reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 2);
    assert_ne!(before, after, "a new tunnel ssh");
    check_reconnected(&mut server, &mut dialer, &tunnel, &ROLE_REVERSED);
    held_echo(&mut server, &mut dialer, 2, "hello-again");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

/// Logs both ends' links in topology's input terms and checks that they are the same.
fn matching_links(server: &mut Proc, dialer: &mut Proc, links: usize) -> Vec<[String; 5]> {
    let near = wait_table(dialer, "dialer", "the attach links", |v| {
        local_links(v).len() == links
    });
    let far = wait_table(server, "server", "the attach links", |v| {
        local_links(v).len() == links
    });
    let (near_links, far_links) = (local_links(&near), local_links(&far));
    for (end, rows) in [("dialer", &near_links), ("server", &far_links)] {
        for [link_id, dialer, acceptor, holder, transport] in rows {
            log(&format!(
                "{end} link {link_id}: dialer={dialer} acceptor={acceptor} holder={holder} \
                 transport={transport}"
            ));
        }
    }
    assert_eq!(
        near_links, far_links,
        "both ends report each link with the same values"
    );
    assert_ne!(near.device, far.device);
    near_links
}

/// The first line of `proc` that starts with `prefix`, waiting for it up to [`SSH_STEP`].
fn transcript_line(proc: &Proc, prefix: &str) -> String {
    let deadline = std::time::Instant::now() + SSH_STEP;
    loop {
        if let Some(line) = proc
            .transcript
            .lines()
            .into_iter()
            .find(|l| l.starts_with(prefix))
        {
            return line;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no line {prefix:?} within {SSH_STEP:?}\n{}",
            proc.transcript.render()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Both ends of one role reversed SSH link give the same `LocalLink` input: link ID, dialer,
/// acceptor, holder and edge value. The server's bulk and request entries carry no link.
/// The dialer's instance ID has characters that would break a raw event value; the server's
/// bulk and request lines keep it, encoded as in the snapshot.
#[test]
fn both_ends_produce_matching_local_links() {
    const ID: &str = "desk\"e=1%x";
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let mut dialer = start_reversing_dialer(&mut scene, &["--channels", "--instance-id", ID]);
    reversed_attach(&mut scene, &mut server, &ready, &mut dialer, 1);
    let open = dialer.expect_prefix("dialer channels-open ", SSH_STEP);
    let value = |line: &str, key: &str| {
        field(line, key)
            .and_then(decode_value)
            .unwrap_or_else(|| panic!("no {key}= value in {line:?}"))
    };
    assert_eq!(value(&open, "instance"), ready.instance, "{open}");
    // The bulk channel's line comes from another server thread than the link's lines, so it
    // is looked for in the transcript.
    let bulk = transcript_line(&server, "server bulk-accepted ");
    assert_eq!(value(&bulk, "peer"), ID, "{bulk}");
    let request = transcript_line(&server, "server request ");
    assert_eq!(value(&request, "method"), "system.info", "{request}");
    let links = matching_links(&mut server, &mut dialer, 1);
    let [_, dialer_device, acceptor_device, holder, transport] = &links[0];
    assert_ne!(dialer_device, acceptor_device);
    assert_eq!(holder, acceptor_device, "the acceptor holds");
    assert_eq!(transport, "ssh_role_reversed");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

/// One tunnel carries a forward link and a role reversed link: the entry of the end that
/// launched ssh is `mixed`, the server has one entry per link with that link's direction, and
/// each link has the same values on both ends (one `ssh_forward`, one `ssh_role_reversed`).
#[test]
fn mixed_holders_on_one_tunnel() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = start_accepting_server(&mut scene);
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer_with(&record, &["--also-holder", "acceptor"], &[]);
    let line = scene.connected(&mut dialer);
    assert_eq!(field(&line, "instance"), Some(ready.instance.as_str()));
    // The second link discovers the record over SSH and runs a handshake.
    let extra = dialer.expect_prefix("dialer extra-connected ", SSH_STEP);
    assert_eq!(field(&extra, "holder"), Some("acceptor"), "{extra}");
    server.expect_prefix("server control client=2 ", SSH_STEP);
    // Both links work: the dialer holds the first, the server the second.
    echo(&mut dialer, "hello-forward");
    server.send_line("send hello-reversed");
    let served = "dialer extra-served hello-reversed";
    dialer.expect(&format!("{served:?}"), LOCAL_STEP, |l| l == served);
    let echoed = "server echo client=2 hello-reversed";
    server.expect(&format!("{echoed:?}"), LOCAL_STEP, |l| l == echoed);

    let links = matching_links(&mut server, &mut dialer, 2);
    let transports: Vec<&str> = links.iter().map(|l| l[4].as_str()).collect();
    let mut sorted = transports.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, ["ssh_forward", "ssh_role_reversed"], "{links:?}");

    let near = read_table(&mut dialer, "dialer");
    let tunnel = in_state(&near, "connected");
    assert_eq!(tunnel.len(), 1, "one tunnel: {near:#?}");
    let entry = tunnel[0];
    log(&format!("dialer entry: {:?}", entry.row));
    assert_eq!(entry.value("direction"), "mixed");
    assert_eq!(
        entry.value("path"),
        "ssh_forward",
        "the profile's direction"
    );
    assert_eq!(entry.value("local_side"), "dialer");
    assert_eq!(entry.links.len(), 2);
    assert!(
        entry
            .links
            .iter()
            .all(|l| l.value("local_side") == "dialer"),
        "this end opened both links: {entry:?}"
    );
    let far = read_table(&mut server, "server");
    let mut directions: Vec<&str> = in_state(&far, "connected")
        .iter()
        .map(|e| e.value("direction"))
        .collect();
    directions.sort_unstable();
    assert_eq!(directions, ["forward", "role_reversed"], "{far:#?}");

    // Link by link: two IDs, and the holder, the edge value and the server entry's direction
    // of each ID belong to one direction (docs/design/transport.md "토폴로지와의 관계").
    let ids: Vec<&str> = entry.links.iter().map(|l| l.value("link_id")).collect();
    assert_ne!(ids[0], ids[1], "two links, two IDs: {ids:?}");
    for link in &entry.links {
        let id = link.value("link_id");
        let (transport, direction) = match link.value("holder") {
            "dialer" => ("ssh_forward", "forward"),
            "acceptor" => ("ssh_role_reversed", "role_reversed"),
            other => panic!("holder {other:?} in {link:?}"),
        };
        assert_eq!(link.value("transport"), transport, "{link:?}");
        let far_entry = in_state(&far, "connected")
            .into_iter()
            .find(|e| e.links.iter().any(|l| l.value("link_id") == id))
            .unwrap_or_else(|| panic!("no server entry with link {id}: {far:#?}"));
        assert_eq!(far_entry.links.len(), 1, "{far_entry:?}");
        let far_link = &far_entry.links[0];
        assert_eq!(
            far_link.value("holder"),
            link.value("holder"),
            "{far_link:?}"
        );
        assert_eq!(far_link.value("transport"), transport, "{far_link:?}");
        assert_eq!(far_entry.value("direction"), direction, "{far_entry:?}");
        assert!(link.number("opened_at_ms") > 0 && far_link.number("opened_at_ms") > 0);
        assert_eq!((link.get("rtt_ms"), far_link.get("rtt_ms")), (None, None));
    }

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}
