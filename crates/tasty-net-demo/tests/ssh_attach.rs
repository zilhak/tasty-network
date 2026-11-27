//! SSH forward attach between two OS processes through a real `ssh -L` tunnel
//! (docs/design/done-criteria.md D2).
//!
//! Each test starts a test SSH server ([`support::sshd`]: OpenSSH sshd, or the test-only Rust
//! server when OpenSSH cannot serve), a `demo-server` process and a `demo-dialer` process. The
//! dialer reaches the server with the system `ssh`: record discovery over SSH, the `-L`
//! tunnel, handshake v2. The tests check what the dialer and the server print, and at the end
//! that no sshd, ssh or demo process is left. They run one at a time: the SSH crate reserves a
//! local port and releases it before ssh binds it, so two tunnels at once could meet on one
//! port.
//!
//! Evidence lines go to stderr unconditionally (`[ssh_attach] ...`): `sshd=<server>`, the
//! process IDs, the tunnel PIDs before and after a reconnect, and the connection table
//! entries of `connection_table_reports_ssh_forward` (docs/design/testing.md "연결 목록 시험").

mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use support::process::{Transcript, is_running, log, terminate, wait_all_gone};
use support::scene::{
    DropReport, LOCAL_STEP, SSH_STEP, Scene, connect_and_check, decoded, echo, number,
    one_at_a_time,
};
use support::table::{FORWARD, check_reconnected, check_tunnel_entries, dialer_instance};
use tasty_net_demo::field;
use tasty_net_identity::{ConnectionRecord, ConnectionToken};

#[test]
fn connect_and_echo() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    log(&format!(
        "server pid={} port={} instance={}",
        ready.pid, ready.port, ready.instance
    ));
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    connect_and_check(&mut scene, &mut server, &ready, &mut dialer);
    echo(&mut dialer, "hello-2");
    scene.quit_dialer(dialer);
    let released = server.expect_prefix("server released ", LOCAL_STEP);
    assert_eq!(field(&released, "live"), Some("0"), "{released}");
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn reconnect_after_tunnel_kill() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    let before = connect_and_check(&mut scene, &mut server, &ready, &mut dialer);

    log(&format!("ending the tunnel ssh pid={before}"));
    terminate(before);
    let lost = dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    let failure = field(&lost, "failure").unwrap_or("");
    log(&format!("the dialer classified the loss as {failure}"));
    assert!(
        matches!(failure, "peer_closed" | "heartbeat_timeout"),
        "a dropped tunnel is a closed peer or a heartbeat timeout: {lost}"
    );

    let line = scene.connected(&mut dialer);
    let after: u32 = number(&line, "tunnel_pid");
    log(&format!("tunnel ssh pid before={before} after={after}"));
    assert_ne!(before, after, "the reconnect started a new tunnel ssh");
    assert_eq!(decoded(&line, "instance"), ready.instance, "{line}");
    echo(&mut dialer, "hello-again");

    // The first link's registration is gone and only the new link is live.
    let released = server.expect("\"server released client=1\"", LOCAL_STEP, |l| {
        l.starts_with("server released client=1 ")
    });
    let lines = server.transcript.lines();
    let attached = lines
        .iter()
        .find(|l| l.starts_with("server attached client=2 "))
        .expect("the second attach");
    let released_at = lines.iter().position(|l| *l == released).expect("released");
    let attached_at = lines.iter().position(|l| l == attached).expect("attached");
    let live_now = if released_at > attached_at {
        field(&released, "live")
    } else {
        field(attached, "live")
    };
    log(&format!(
        "server links live after the reconnect: {live_now:?}"
    ));
    assert_eq!(live_now, Some("1"), "{}", server.transcript.render());

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn reconnect_after_server_restart() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer(&record);
    let tunnel_before = connect_and_check(&mut scene, &mut server, &ready, &mut dialer);

    log(&format!("killing demo-server pid={}", server.pid()));
    server.kill();
    let lost = dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    log(&format!("the dialer saw: {lost}"));

    let (mut restarted, ready2) = scene.start_server();
    log(&format!(
        "server instance before={} after={} pid before={} after={}",
        ready.instance, ready2.instance, ready.pid, ready2.pid
    ));
    assert_ne!(ready.instance, ready2.instance);
    // Attempts that still read the old record fail and are retried; the one that reads the
    // new record reaches the new instance.
    let line = scene.connected(&mut dialer);
    assert_eq!(
        decoded(&line, "instance"),
        ready2.instance,
        "the reconnect used the newly discovered record\n{}",
        dialer.transcript.render()
    );
    let tunnel_after: u32 = number(&line, "tunnel_pid");
    log(&format!(
        "tunnel ssh pid before={tunnel_before} after={tunnel_after}"
    ));
    assert_ne!(
        tunnel_before, tunnel_after,
        "a new record gives a new tunnel"
    );
    restarted.expect_prefix("server attached client=1 live=1", LOCAL_STEP);
    echo(&mut dialer, "hello-restarted");

    scene.quit_dialer(dialer);
    Scene::stop_server(restarted);
    drop(server);
    scene.finish();
}

/// The connection tables of both ends for a forward link whose tunnel also carries a bulk and
/// a request channel, before and after the tunnel is killed and the dialer connects again.
#[test]
fn connection_table_reports_ssh_forward() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer_with(&record, &["--channels"], &[]);
    // The channels open before the snapshot is read, so `connect_and_check` passed the
    // `channels-open` line; the dialer's table shows them.
    let before = connect_and_check(&mut scene, &mut server, &ready, &mut dialer);
    let instance = dialer_instance(&server, 1);
    let tunnel = check_tunnel_entries(
        &scene,
        &mut server,
        &ready,
        &mut dialer,
        &instance,
        &FORWARD,
    );

    log(&format!("ending the tunnel ssh pid={before}"));
    terminate(before);
    dialer.expect_prefix("dialer lost ", LOCAL_STEP);
    let line = scene.connected(&mut dialer);
    assert_ne!(
        number::<u32>(&line, "tunnel_pid"),
        before,
        "a new tunnel ssh"
    );
    check_reconnected(&mut server, &mut dialer, &tunnel, &FORWARD);
    echo(&mut dialer, "hello-again");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}

#[test]
fn wrong_token_is_rejected_without_retry() {
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    // A record with the server's port and instance but another token.
    let wrong = ConnectionRecord {
        port: ready.port,
        token: ConnectionToken::issue().expect("token"),
        instance_id: ready.instance.clone(),
    };
    let bad_record = scene
        .dir
        .as_ref()
        .expect("dir")
        .path()
        .join("wrong-token.json");
    std::fs::write(&bad_record, wrong.to_line()).expect("write the wrong record");
    let mut dialer = scene.start_dialer(&bad_record);

    let rejected = dialer.expect_prefix("dialer rejected ", SSH_STEP);
    log(&format!("the dialer ended with: {rejected}"));
    assert_eq!(
        field(&rejected, "reason"),
        Some("token_invalid"),
        "{rejected}"
    );
    let status = dialer.wait_exit(LOCAL_STEP);
    assert_eq!(
        status.code(),
        Some(3),
        "the dialer ends with the rejection exit code"
    );
    scene.track_ssh_children();

    server.expect_prefix("server refused reason=token_invalid", LOCAL_STEP);
    // The dialer has exited, so nothing more can come; the server saw exactly one attempt.
    std::thread::sleep(Duration::from_secs(1));
    let lines = server.transcript.lines();
    let refused = lines
        .iter()
        .filter(|l| l.starts_with("server refused "))
        .count();
    let attached = lines
        .iter()
        .filter(|l| l.starts_with("server attached "))
        .count();
    let attempts = dialer
        .transcript
        .lines()
        .iter()
        .filter(|l| l.starts_with("dialer attempt "))
        .count();
    log(&format!(
        "attempts: dialer {attempts}, server refusals {refused}, server attaches {attached}"
    ));
    assert_eq!(
        (attempts, refused, attached),
        (1, 1, 0),
        "{}",
        server.transcript.render()
    );

    drop(dialer);
    Scene::stop_server(server);
    scene.finish();
}

/// A test that fails while waiting for the dialer leaves no process behind: the wait runs out
/// (as a reconnect that does not come would) while the tunnel ssh runs and before any ssh
/// child was tracked, and dropping the scene ends the dialer, its ssh children and the server
/// before the SSH server, and then sshd.
#[test]
fn a_failed_wait_leaves_no_ssh_behind() {
    let _one = one_at_a_time();
    let mut dialer_lines: Option<Transcript> = None;
    let mut others: Vec<(String, u32)> = Vec::new();
    let mut tunnel: Option<u32> = None;
    let mut tunnel_ran_at_expiry: Option<bool> = None;
    let report: Arc<Mutex<Option<DropReport>>> = Arc::default();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut scene = Scene::new();
        scene.drop_report = Some(Arc::clone(&report));
        if let Some(pid) = scene.sshd().pid() {
            others.push(("sshd".into(), pid));
        }
        let (server, ready) = scene.start_server();
        others.push(("demo-server".into(), ready.pid));
        let record = scene.record.clone();
        let mut dialer = scene.start_dialer(&record);
        dialer_lines = Some(dialer.transcript.clone());
        let line = dialer.expect("\"role=tunnel\"", SSH_STEP, |l| {
            l.starts_with("dialer ssh-spawned ") && l.ends_with("role=tunnel")
        });
        let pid: u32 = number(&line, "pid");
        tunnel = Some(pid);
        // Never matches: this wait runs out, and the test body panics with its message.
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dialer.expect("that never comes", Duration::from_secs(3), |_| false)
        }));
        let Err(expiry) = waited else {
            unreachable!("a wait that matches nothing returned")
        };
        tunnel_ran_at_expiry =
            Some(is_running(pid).unwrap_or_else(|e| panic!("checking the tunnel ssh {pid}: {e}")));
        let _kept = (server, dialer);
        std::panic::resume_unwind(expiry);
    }));
    assert!(failed.is_err(), "the inner wait was meant to run out");
    let tunnel = tunnel.expect("the dialer started a tunnel ssh");
    assert_eq!(
        tunnel_ran_at_expiry,
        Some(true),
        "the tunnel ssh {tunnel} ran when the wait ran out"
    );

    // What the scene's drop saw before the SSH server ended.
    let seen = report
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("the scene's drop reported");
    assert!(
        seen.tracked.iter().any(|(_, pid)| *pid == tunnel),
        "the scene's drop tracked the tunnel ssh {tunnel}: {:?}",
        seen.tracked
    );
    let left_before_sshd = seen
        .left
        .unwrap_or_else(|e| panic!("checking the processes before sshd ended: {e}"));
    assert!(
        left_before_sshd.is_empty(),
        "still running before sshd ended: {left_before_sshd:?}"
    );

    let lines = dialer_lines.expect("the dialer started").lines();
    let mut pids = others;
    for line in &lines {
        if line.starts_with("dialer pid=") {
            pids.push(("demo-dialer".into(), number(line, "pid")));
        } else if line.starts_with("dialer ssh-spawned ") {
            let role = field(line, "role").unwrap_or("?");
            pids.push((format!("ssh {role}"), number(line, "pid")));
        }
    }
    let tunnels = pids.iter().filter(|(what, _)| what == "ssh tunnel").count();
    assert!(tunnels >= 1, "the dialer started a tunnel ssh: {lines:?}");
    let left = wait_all_gone(&pids, Duration::from_secs(15))
        .unwrap_or_else(|e| panic!("checking the processes: {e}"));
    log(&format!(
        "processes left after a failed wait: {} of {} ({} tunnel ssh, running at expiry; \
         tracked by the drop: {}; left before sshd ended: 0; {})",
        left.len(),
        pids.len(),
        tunnels,
        seen.tracked.len(),
        pids.iter()
            .map(|(what, pid)| format!("{what} {pid}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for (_, pid) in &left {
        terminate(*pid);
    }
    assert!(
        left.is_empty(),
        "processes left after a failed wait: {left:?}"
    );
}

/// The dialer waits on `quit` until its stdin thread has let the link's writer go, which ends
/// the tunnel ssh. The test hook keeps the writer 1.5 s longer: the dialer must still end
/// after that, and leave no ssh.
///
/// Only the dialer's own end is timed: from right after `quit` is sent to the moment the
/// process exited. Listing its children before and checking its ssh after lie outside. The
/// server's `link-ended ... end=detach` line shows the Detach went through, so the writer's
/// hold has begun (the writer finishes when the server ends its side); the dialer must still
/// run then.
#[test]
fn quit_waits_for_the_detaching_writer() {
    const HOLD: Duration = Duration::from_millis(1500);
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server();
    let record = scene.record.clone();
    let hold = HOLD.as_millis().to_string();
    let mut dialer = scene.start_dialer_with(
        &record,
        &[],
        &[("TASTY_NET_DEMO_HOLD_WRITER_MS", hold.as_str())],
    );
    connect_and_check(&mut scene, &mut server, &ready, &mut dialer);
    scene.before_quit(&dialer);
    dialer.send_line("quit");
    let started = Instant::now();
    let ended = server.expect("\"link-ended ... end=detach\"", LOCAL_STEP, |l| {
        l.starts_with("server link-ended ") && field(l, "end") == Some("detach")
    });
    let detached_after = started.elapsed();
    let ran_in_hold = !dialer.has_exited();
    let status = dialer.wait_exit(LOCAL_STEP);
    let took = started.elapsed();
    log(&format!(
        "the dialer ended {} ms after quit with the writer held {} ms (the server saw the \
         Detach after {} ms; the dialer ran then: {ran_in_hold})",
        took.as_millis(),
        HOLD.as_millis(),
        detached_after.as_millis()
    ));
    scene.after_quit(&dialer, status);
    assert!(
        ran_in_hold,
        "the dialer had exited when the server saw its Detach ({ended}), before the writer's \
         hold ran out"
    );
    assert!(
        took >= HOLD,
        "the dialer ended {took:?} after quit, before its writer went ({HOLD:?})"
    );
    drop(dialer);
    let released = server.expect_prefix("server released ", LOCAL_STEP);
    assert_eq!(field(&released, "live"), Some("0"), "{released}");
    Scene::stop_server(server);
    scene.finish();
}

/// The values of the event lines go through the snapshot encoding: the server's instance ID
/// in `server ready instance=`, `dialer connected instance=` and `dialer channels-open
/// instance=`, and a called method in `dialer call method=` and `server request method=`,
/// hold a space, `=`, `%` and `"`, and read back unchanged. Without the encoding the space
/// would cut the value and the decoding of `%` would fail.
#[test]
fn channel_lines_encode_their_values() {
    const SERVER_ID: &str = "desk a=1%2\"b";
    const METHOD: &str = "no such=method%\"x";
    let _one = one_at_a_time();
    let mut scene = Scene::new();
    let (mut server, ready) = scene.start_server_with(&["--instance-id", SERVER_ID]);
    assert_eq!(ready.instance, SERVER_ID, "the ready line keeps the ID");
    let record = scene.record.clone();
    let mut dialer = scene.start_dialer_with(&record, &["--channels", "--also-call", METHOD], &[]);
    let connected = scene.connected(&mut dialer);
    assert_eq!(decoded(&connected, "instance"), SERVER_ID, "{connected}");
    // The request channel discovers the record over SSH and runs a handshake.
    let call = dialer.expect_prefix("dialer call ", SSH_STEP);
    assert_eq!(decoded(&call, "method"), METHOD, "{call}");
    assert_eq!(
        field(&call, "answer"),
        Some("error"),
        "the server knows no such method"
    );
    let open = dialer.expect_prefix("dialer channels-open ", LOCAL_STEP);
    assert_eq!(decoded(&open, "instance"), SERVER_ID, "{open}");
    // The server's lines reach the test through another reader than the dialer's, so both
    // request lines are waited for on the server's stdout, in the order they were answered:
    // first the method of `--also-call`, then `system.info`.
    let first = server.expect_prefix("server request method=", LOCAL_STEP);
    assert_eq!(decoded(&first, "method"), METHOD, "{first}");
    let second = server.expect_prefix("server request method=", LOCAL_STEP);
    assert_eq!(decoded(&second, "method"), "system.info", "{second}");

    scene.quit_dialer(dialer);
    Scene::stop_server(server);
    scene.finish();
}
