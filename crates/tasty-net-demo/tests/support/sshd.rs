//! The SSH server a test logs in to with the system `ssh` (docs/design/testing.md "SSH 통합
//! 시험의 sshd").
//!
//! [`SshServer::start`] first runs an unprivileged OpenSSH `sshd -D -e -f <config>` on a free
//! loopback port with keys and a config in a temporary directory. When there is no `sshd`, or
//! its config check, start, login or `-L` forward check fails, it falls back to the
//! test-only Rust server (`russh`, [`super::russh_server`]), which takes the same login and the
//! two requests the tests need (`exec` and `direct-tcpip`). When neither comes up the test
//! fails: the log says `sshd=none` and the test is not counted as passed or as not run.
//!
//! `TASTY_NET_TEST_SSHD` picks the server: `auto` (the default, as above), `openssh` (no
//! fallback), or `russh`. `TASTY_NET_TEST_SSHD_PATH` names the `sshd` binary to try.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::process::{children_of, log};
use super::russh_server::RusshServer;
use super::{TempDir, ssh_option_path, ssh_path};

/// How long sshd may take to listen.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound of one check command (`ssh ... echo`, `ssh-keygen`, `sshd -t`).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

enum Running {
    OpenSsh { child: Reaped, path: PathBuf },
    Russh(RusshServer),
}

/// The sshd child, killed and reaped on drop, so a test that panics leaves no sshd behind.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _killed = self.0.kill().is_ok();
        let _reaped = self.0.wait().is_ok();
    }
}

/// A running test SSH server and what a client needs to log in.
pub struct SshServer {
    running: Running,
    pub port: u16,
    pub user: String,
    pub identity: PathBuf,
    known_hosts: PathBuf,
    global_known_hosts: PathBuf,
    dir: TempDir,
}

impl SshServer {
    /// Starts OpenSSH, or the Rust server when OpenSSH cannot serve (see the module text).
    /// Panics with `sshd=none` when neither comes up.
    pub fn start() -> Self {
        let choice = std::env::var("TASTY_NET_TEST_SSHD").unwrap_or_else(|_| "auto".into());
        let mut reasons = Vec::new();
        if choice == "auto" || choice == "openssh" {
            match start_openssh() {
                Ok(server) => {
                    if let Running::OpenSsh { child, path } = &server.running {
                        log(&format!(
                            "sshd=openssh path={} pid={} port={}",
                            path.display(),
                            child.0.id(),
                            server.port
                        ));
                    }
                    return server;
                }
                Err(reason) => {
                    log(&format!("OpenSSH sshd is not usable: {reason}"));
                    reasons.push(format!("openssh: {reason}"));
                }
            }
        }
        if choice == "auto" || choice == "russh" {
            match start_russh() {
                Ok(server) => {
                    log(&format!(
                        "sshd=russh port={} (test-only Rust SSH server in the test process{})",
                        server.port,
                        if reasons.is_empty() {
                            String::new()
                        } else {
                            format!("; {}", reasons.join("; "))
                        }
                    ));
                    return server;
                }
                Err(reason) => reasons.push(format!("russh: {reason}")),
            }
        }
        if reasons.is_empty() {
            reasons.push(format!("TASTY_NET_TEST_SSHD={choice:?} names no server"));
        }
        log(&format!("sshd=none ({})", reasons.join("; ")));
        panic!(
            "sshd=none: no SSH server for the SSH integration tests ({})",
            reasons.join("; ")
        );
    }

    /// `openssh` or `russh`.
    pub fn kind(&self) -> &'static str {
        match self.running {
            Running::OpenSsh { .. } => "openssh",
            Running::Russh(_) => "russh",
        }
    }

    /// `user@127.0.0.1`.
    pub fn destination(&self) -> String {
        format!("{}@127.0.0.1", self.user)
    }

    /// The `-o` options of every client login: this server's known hosts file, the test key
    /// only, no password or keyboard prompt.
    pub fn client_options(&self) -> Vec<String> {
        client_options(&self.known_hosts, &self.global_known_hosts)
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Ends the server after its clients left. For OpenSSH it first waits until sshd has no
    /// session child left, so a session that outlives its client is a failure here. Returns
    /// the PIDs to check afterwards (sshd and the session children seen). Removes the
    /// temporary directory.
    pub fn stop(self) -> Vec<(String, u32)> {
        let mut pids = Vec::new();
        match self.running {
            Running::OpenSsh { mut child, .. } => {
                let pid = child.0.id();
                pids.push(("sshd".to_string(), pid));
                let deadline = Instant::now() + Duration::from_secs(15);
                let sessions_now = || {
                    children_of(pid)
                        .unwrap_or_else(|e| panic!("listing the sessions of sshd {pid}: {e}"))
                };
                let mut sessions = sessions_now();
                while !sessions.is_empty() && Instant::now() < deadline {
                    for s in &sessions {
                        if !pids.iter().any(|(_, p)| p == s) {
                            pids.push(("sshd session".to_string(), *s));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    sessions = sessions_now();
                }
                let _killed = child.0.kill().is_ok();
                let status = child.0.wait();
                log(&format!("sshd pid={pid} ended ({status:?})"));
                assert!(
                    sessions.is_empty(),
                    "sshd still had session children {sessions:?} 15 s after its clients ended"
                );
            }
            Running::Russh(server) => server.stop(),
        }
        self.dir.remove();
        pids
    }
}

fn client_options(known_hosts: &Path, global_known_hosts: &Path) -> Vec<String> {
    vec![
        format!("UserKnownHostsFile={}", ssh_option_path(known_hosts)),
        format!(
            "GlobalKnownHostsFile={}",
            ssh_option_path(global_known_hosts)
        ),
        "StrictHostKeyChecking=accept-new".into(),
        "IdentitiesOnly=yes".into(),
        "PreferredAuthentications=publickey".into(),
        "PasswordAuthentication=no".into(),
        "KbdInteractiveAuthentication=no".into(),
    ]
}

/// The user the server lets in: the user running the test.
fn current_user() -> Result<String, String> {
    for var in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(user) = std::env::var(var)
            && !user.is_empty()
        {
            return Ok(user);
        }
    }
    let out = Command::new("whoami")
        .output()
        .map_err(|e| format!("whoami: {e}"))?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `whoami` on Windows prints `domain\user`.
    let user = user.rsplit('\\').next().unwrap_or("").to_string();
    if user.is_empty() {
        return Err("the user name is unknown".into());
    }
    Ok(user)
}

/// A free loopback port. It is released before the server binds it; the tests run one at a
/// time, so nothing else of this test binary takes it meanwhile.
fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("bind: {e}"))?;
    Ok(listener.local_addr().map_err(|e| e.to_string())?.port())
}

/// Runs `command` with stdout and stderr in files under `dir`, killing it after `timeout`.
fn run_bounded(
    mut command: Command,
    dir: &Path,
    name: &str,
    timeout: Duration,
) -> Result<(ExitStatus, String, String), String> {
    let out_path = dir.join(format!("{name}.out"));
    let err_path = dir.join(format!("{name}.err"));
    let out = File::create(&out_path).map_err(|e| e.to_string())?;
    let err = File::create(&err_path).map_err(|e| e.to_string())?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting {name}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _killed = child.kill().is_ok();
            let _reaped = child.wait().is_ok();
            return Err(format!("{name} did not end within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    Ok((status, read(&out_path), read(&err_path)))
}

fn ssh_keygen_program() -> PathBuf {
    if cfg!(windows)
        && let Ok(windir) = std::env::var("WINDIR")
    {
        let path = PathBuf::from(windir).join(r"System32\OpenSSH\ssh-keygen.exe");
        if path.exists() {
            return path;
        }
    }
    PathBuf::from("ssh-keygen")
}

/// An ed25519 key without passphrase at `path` (and `path.pub`).
fn keygen(dir: &Path, path: &Path) -> Result<(), String> {
    let mut command = Command::new(ssh_keygen_program());
    command
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "tasty-net-test",
            "-f",
        ])
        .arg(path);
    let (status, _, err) = run_bounded(command, dir, "ssh-keygen", COMMAND_TIMEOUT)?;
    if !status.success() {
        return Err(format!("ssh-keygen failed ({status}): {}", err.trim()));
    }
    Ok(())
}

fn find_sshd() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("TASTY_NET_TEST_SSHD_PATH") {
        return Some(PathBuf::from(path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        if let Ok(windir) = std::env::var("WINDIR") {
            candidates.push(PathBuf::from(windir).join(r"System32\OpenSSH\sshd.exe"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(pf).join(r"OpenSSH\sshd.exe"));
        }
    } else {
        for p in [
            "/usr/sbin/sshd",
            "/usr/local/sbin/sshd",
            "/opt/homebrew/sbin/sshd",
        ] {
            candidates.push(PathBuf::from(p));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// The sshd config. `penalties` adds `PerSourcePenalties no` (OpenSSH 9.8 and later), which
/// keeps sshd from refusing the test's quick reconnects from one address.
fn sshd_config(dir: &Path, port: u16, penalties: bool) -> String {
    let q = |name: &str| format!("\"{}\"", ssh_path(&dir.join(name)));
    let mut lines = vec![
        format!("Port {port}"),
        "ListenAddress 127.0.0.1".into(),
        format!("HostKey {}", q("host_key")),
        format!("AuthorizedKeysFile {}", q("authorized_keys")),
        "StrictModes no".into(),
        "AllowTcpForwarding yes".into(),
        "PubkeyAuthentication yes".into(),
        "PasswordAuthentication no".into(),
        "KbdInteractiveAuthentication no".into(),
    ];
    if cfg!(unix) {
        lines.push(format!("PidFile {}", q("sshd.pid")));
        lines.push("UsePAM no".into());
    }
    if penalties {
        lines.push("PerSourcePenalties no".into());
    }
    lines.join("\n") + "\n"
}

fn start_openssh() -> Result<SshServer, String> {
    let sshd = find_sshd().ok_or("no sshd binary found")?;
    let user = current_user()?;
    let dir = TempDir::try_new("sshd")?;
    let d = dir.path();
    keygen(d, &d.join("host_key"))?;
    keygen(d, &d.join("client_key"))?;
    std::fs::copy(d.join("client_key.pub"), d.join("authorized_keys"))
        .map_err(|e| format!("authorized_keys: {e}"))?;
    let config = d.join("sshd_config");

    // A taken port shows as sshd exiting before it listens; try another one then.
    let mut last = String::new();
    for _ in 0..3 {
        let port = free_port()?;
        let penalties = write_checked_config(&sshd, d, &config, port)?;
        let log_path = d.join("sshd.log");
        let log_file = File::create(&log_path).map_err(|e| e.to_string())?;
        let mut child = Command::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .map_err(|e| format!("starting {}: {e}", sshd.display()))?;
        match wait_listening(&mut child, &log_path) {
            Ok(()) => {
                let server = SshServer {
                    running: Running::OpenSsh {
                        child: Reaped(child),
                        path: sshd.clone(),
                    },
                    port,
                    user,
                    identity: d.join("client_key"),
                    known_hosts: d.join("known_hosts"),
                    global_known_hosts: d.join("global_known_hosts"),
                    dir,
                };
                if let Err(reason) = verify(&server) {
                    let log_text = std::fs::read_to_string(&log_path).unwrap_or_default();
                    // Dropping the server kills sshd and removes the directory.
                    let _pids = stop_quietly(server);
                    return Err(format!(
                        "{reason} (penalties option {}; sshd log: {})",
                        if penalties { "set" } else { "unsupported" },
                        log_text.trim()
                    ));
                }
                return Ok(server);
            }
            Err(reason) => {
                let _killed = child.kill().is_ok();
                let _reaped = child.wait().is_ok();
                last = reason;
            }
        }
    }
    Err(last)
}

/// Writes the config and checks it with `sshd -t`. Drops `PerSourcePenalties` when this sshd
/// does not know it. Returns whether the option stayed.
fn write_checked_config(sshd: &Path, dir: &Path, config: &Path, port: u16) -> Result<bool, String> {
    let mut last = String::new();
    for penalties in [true, false] {
        std::fs::write(config, sshd_config(dir, port, penalties))
            .map_err(|e| format!("writing the sshd config: {e}"))?;
        let mut command = Command::new(sshd);
        command.arg("-t").arg("-f").arg(config);
        let (status, out, err) = run_bounded(command, dir, "sshd-t", COMMAND_TIMEOUT)?;
        if status.success() {
            return Ok(penalties);
        }
        last = format!("sshd -t failed ({status}): {} {}", out.trim(), err.trim());
    }
    Err(last)
}

/// Waits until sshd logs that it listens (no connection is made, so nothing counts as a
/// failed login).
fn wait_listening(child: &mut Child, log_path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + LISTEN_TIMEOUT;
    loop {
        let text = std::fs::read_to_string(log_path).unwrap_or_default();
        if text.contains("Server listening on") {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "sshd exited before listening ({status}): {}",
                text.trim()
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "sshd did not listen within {LISTEN_TIMEOUT:?}: {}",
                text.trim()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_russh() -> Result<SshServer, String> {
    let user = current_user()?;
    let dir = TempDir::try_new("russh")?;
    let d = dir.path();
    let server = RusshServer::start(&d.join("client_key"))?;
    let port = server.port();
    let server = SshServer {
        running: Running::Russh(server),
        port,
        user,
        identity: d.join("client_key"),
        known_hosts: d.join("known_hosts"),
        global_known_hosts: d.join("global_known_hosts"),
        dir,
    };
    if let Err(reason) = verify(&server) {
        let _pids = stop_quietly(server);
        return Err(reason);
    }
    Ok(server)
}

/// Ends a server that failed its checks; its PIDs are not tracked by a test.
fn stop_quietly(server: SshServer) -> Vec<(String, u32)> {
    match server.running {
        // Dropping it kills and reaps sshd.
        Running::OpenSsh { child, .. } => drop(child),
        Running::Russh(s) => s.stop(),
    }
    Vec::new()
}

/// The base `ssh` command of a check: the test key, this server's port and options, no
/// prompts.
fn ssh_command(server: &SshServer) -> Command {
    let mut command = Command::new(tasty_net_ssh::resolve_ssh_path());
    command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    for option in server.client_options() {
        command.arg("-o").arg(option);
    }
    command
        .arg("-i")
        .arg(&server.identity)
        .arg("-p")
        .arg(server.port.to_string());
    command
}

/// The checks of docs/design/testing.md: one `ssh -o BatchMode=yes` login that runs a command,
/// and one `-L` forward that carries bytes both ways.
fn verify(server: &SshServer) -> Result<(), String> {
    let mut login = ssh_command(server);
    login
        .arg(server.destination())
        .arg("echo tasty-net-login-ok");
    let (status, out, err) = run_bounded(login, server.dir(), "check-login", COMMAND_TIMEOUT)?;
    if !status.success() || !out.contains("tasty-net-login-ok") {
        return Err(format!(
            "the login check failed ({status}): stdout {:?}, stderr {:?}",
            out.trim(),
            err.trim()
        ));
    }
    check_forward(server)
}

fn check_forward(server: &SshServer) -> Result<(), String> {
    let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let target = echo.local_addr().map_err(|e| e.to_string())?.port();
    let echo_thread = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = echo.accept() {
            let mut buf = [0u8; 64];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 || stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    });
    let local = free_port()?;
    let err_path = server.dir().join("check-forward.err");
    let err_file = File::create(&err_path).map_err(|e| e.to_string())?;
    let mut command = ssh_command(server);
    command
        .args(["-o", "ExitOnForwardFailure=yes", "-N", "-L"])
        .arg(format!("127.0.0.1:{local}:127.0.0.1:{target}"))
        .arg(server.destination())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file));
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting the forward check: {e}"))?;
    let result = carry_through(&mut child, local);
    let _killed = child.kill().is_ok();
    let _reaped = child.wait().is_ok();
    // The echo thread ends when the forwarded connection closes; when no connection came,
    // waking its accept ends it.
    drop(TcpStream::connect((Ipv4Addr::LOCALHOST, target)));
    let _joined = echo_thread.join().is_ok();
    result.map_err(|reason| {
        format!(
            "the -L forward check failed: {reason}; ssh stderr {:?}",
            std::fs::read_to_string(&err_path)
                .unwrap_or_default()
                .trim()
        )
    })
}

fn carry_through(child: &mut Child, local: u16) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, local));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("ssh exited ({status})"));
        }
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            break stream;
        }
        if Instant::now() >= deadline {
            return Err("the local end did not open".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let probe = b"tasty-net-forward-ok";
    stream.write_all(probe).map_err(|e| format!("write: {e}"))?;
    let mut got = vec![0u8; probe.len()];
    stream
        .read_exact(&mut got)
        .map_err(|e| format!("read back: {e}"))?;
    drop(stream.shutdown(Shutdown::Both));
    if got != probe {
        return Err(format!("read back {got:?}"));
    }
    Ok(())
}
