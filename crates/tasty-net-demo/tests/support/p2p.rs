//! One test's processes for the two-process P2P tests: the rendezvous server in the test
//! process, the `p2p-link` processes and their key directories, with the checks that none of
//! them is left (docs/design/testing.md "공통 규칙").

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::TempDir;
use super::process::{Proc, log};
use super::rendezvous::Rendezvous;
use super::scene::check_all_gone;
use tasty_net_demo::field;

/// Bound of one expected event that needs the server or a punch (registration, a link, a
/// bulk transfer). Loopback takes well under a second; the bound absorbs a loaded machine.
pub const P2P_STEP: Duration = Duration::from_secs(60);
/// Link read timeout of both ends.
pub const READ_TIMEOUT_MS: &str = "10000";
/// The test hook of `p2p-link` (its module text): with `1`, the listener's pairing list says
/// "paired" only at the first check of a device. Every `p2p-link` the tests start runs without
/// it (an inherited value is removed); a test turns it on for one process with the `env` of
/// [`P2pScene::listen`].
pub const UNPAIR_AFTER_OFFER: &str = "TASTY_NET_DEMO_UNPAIR_AFTER_OFFER";
/// How long a `p2p-link` may take to end after `quit`: it detaches, turns P2P off and exits.
const END_AFTER_QUIT: Duration = Duration::from_secs(30);

/// One device: its key directory and the device ID `p2p-link --id` printed for it.
pub struct Device {
    pub name: String,
    pub id: String,
    dir: PathBuf,
}

/// One test's server, devices and processes. Every process it starts is checked at the end.
/// A test keeps its [`Proc`]s so that they drop before the scene (locals declared after it,
/// struct fields declared before it): when the test fails before [`P2pScene::finish`], its
/// processes are killed as they drop, then the server is shut down (bounded) and the key
/// directories are removed.
pub struct P2pScene {
    server: Option<Rendezvous>,
    tracked: Vec<(String, u32)>,
    devices: Vec<TempDir>,
}

/// A `p2p-link` command with `args`, without the test hook the test process may have in its
/// environment.
fn link_command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_p2p-link"));
    command.args(args).env_remove(UNPAIR_AFTER_OFFER);
    command
}

impl P2pScene {
    pub fn new() -> Self {
        Self {
            server: Some(Rendezvous::start()),
            tracked: Vec::new(),
            devices: Vec::new(),
        }
    }

    pub fn server(&self) -> &Rendezvous {
        self.server.as_ref().expect("the server runs")
    }

    /// A new device: a key directory, made by `p2p-link --id`, and its device ID.
    pub fn device(&mut self, name: &str) -> Device {
        let dir = TempDir::new(name);
        let key_dir = dir.path().display().to_string();
        let out = link_command(&["--id", "--key-dir", &key_dir])
            .output()
            .expect("p2p-link --id runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "p2p-link --id: {}\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let id = stdout
            .lines()
            .find_map(|line| line.strip_prefix("id "))
            .and_then(|words| field(words, "device"))
            .unwrap_or_else(|| panic!("no device ID in {stdout:?}"))
            .to_string();
        let device = Device {
            name: name.to_string(),
            id,
            dir: dir.path().to_path_buf(),
        };
        self.devices.push(dir);
        device
    }

    /// The options every `p2p-link` of this scene takes: its key directory and the server.
    fn common(&self, device: &Device) -> Vec<String> {
        let server = self.server();
        vec![
            "--key-dir".into(),
            device.dir.display().to_string(),
            "--server".into(),
            server.addr.to_string(),
            "--server-key".into(),
            server.public_key.to_string(),
            "--read-timeout-ms".into(),
            READ_TIMEOUT_MS.into(),
        ]
    }

    fn spawn(&mut self, name: &str, args: &[String], env: &[(&str, &str)]) -> Proc {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut command = link_command(&args);
        command.envs(env.iter().copied());
        let proc = Proc::spawn(name, command);
        self.tracked.push((name.to_string(), proc.pid()));
        proc
    }

    /// `p2p-link --listen` on `device`, paired with `peers`, started with `env`; returns once
    /// it registered on the server.
    pub fn listen(&mut self, device: &Device, peers: &[&Device], env: &[(&str, &str)]) -> Proc {
        self.listen_as(device, peers, env, &[])
    }

    /// [`P2pScene::listen`] with `extra` options.
    pub fn listen_as(
        &mut self,
        device: &Device,
        peers: &[&Device],
        env: &[(&str, &str)],
        extra: &[&str],
    ) -> Proc {
        let mut args = vec!["--listen".to_string()];
        args.extend(self.common(device));
        for peer in peers {
            args.extend(["--peer".to_string(), peer.id.clone()]);
        }
        args.extend(extra.iter().map(|a| (*a).to_string()));
        let mut listener = self.spawn(&format!("listener-{}", device.name), &args, env);
        let ready = listener.expect_prefix("listener ready ", P2P_STEP);
        assert_eq!(field(&ready, "device"), Some(device.id.as_str()), "{ready}");
        listener.expect_prefix("listener registered", P2P_STEP);
        listener
    }

    /// `p2p-link --dial <target>` on `device`.
    pub fn dial(&mut self, device: &Device, target: &Device) -> Proc {
        let mut args = vec!["--dial".to_string(), target.id.clone()];
        args.extend(self.common(device));
        let mut dialer = self.spawn(&format!("dialer-{}", device.name), &args, &[]);
        let ready = dialer.expect_prefix("dialer ready ", P2P_STEP);
        assert_eq!(field(&ready, "device"), Some(device.id.as_str()), "{ready}");
        dialer
    }

    /// Sends `quit` to `proc` (whose lines start with `who`) and checks that it ends with
    /// exit code 0 after its `bye` line.
    pub fn quit(proc: &mut Proc, who: &str) {
        proc.send_line("quit");
        proc.expect_prefix(&format!("{who} bye"), END_AFTER_QUIT);
        let status = proc.wait_exit(END_AFTER_QUIT);
        assert!(
            status.success(),
            "{who} after quit: {status}\n{}",
            proc.transcript.render()
        );
    }

    /// Checks that no process of the scene is left, shuts the server down within its bound
    /// and removes the key directories.
    pub fn finish(mut self) {
        let gone = check_all_gone(&self.tracked, END_AFTER_QUIT);
        log(&format!(
            "processes left after the test: {} of {} tracked",
            gone.left.len(),
            self.tracked.len()
        ));
        assert!(
            gone.left.is_empty() && gone.errors.is_empty(),
            "left: {:?}, failed queries: {:?}",
            gone.left,
            gone.errors
        );
        if let Some(server) = self.server.take() {
            server.stop();
        }
        for dir in std::mem::take(&mut self.devices) {
            dir.remove();
        }
    }
}
