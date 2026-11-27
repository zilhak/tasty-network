//! How this crate starts `ssh`: the program, host key checking, and the child creation hook.
//! Every `ssh` child of this crate is created through a [`SpawnHook`].

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{AppliedOption, SshSettings, SshTarget, push_common_opts, resolve_ssh_path};

/// Signature of a custom child creation function.
pub type SpawnFn = dyn Fn(Command) -> io::Result<Child> + Send + Sync;

/// Creates the `ssh` children of this crate: tunnel children and port discovery children.
///
/// The default hook runs [`Command::spawn`]. A host that must bind its children to its own
/// lifetime (for example so that a tunnel does not outlive a killed host) installs its own
/// function with [`SpawnHook::new`]. The hook is the only place where this crate creates a
/// child. It may run on a short-lived worker thread; if the host needs the fork to happen on a
/// particular thread, the hook hands the command over to that thread itself.
#[derive(Clone, Default)]
pub struct SpawnHook {
    custom: Option<Arc<SpawnFn>>,
}

impl SpawnHook {
    /// A hook that creates every child with `spawn`.
    pub fn new(spawn: impl Fn(Command) -> io::Result<Child> + Send + Sync + 'static) -> Self {
        Self {
            custom: Some(Arc::new(spawn)),
        }
    }

    /// Creates the child of `cmd`. The command's stdio is already set by the caller.
    pub fn spawn(&self, mut cmd: Command) -> io::Result<Child> {
        match &self.custom {
            Some(spawn) => spawn(cmd),
            None => cmd.spawn(),
        }
    }

    /// Whether this is the default hook ([`Command::spawn`]).
    pub fn is_default(&self) -> bool {
        self.custom.is_none()
    }
}

impl fmt::Debug for SpawnHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnHook")
            .field("custom", &self.custom.is_some())
            .finish()
    }
}

/// How to start `ssh`: the program, whether host keys are checked automatically, the child
/// creation hook, and the settings ([`SshSettings`]). Tunnels and port discovery take it.
#[derive(Clone, Debug)]
pub struct SshLauncher {
    program: PathBuf,
    verify: bool,
    spawn_hook: SpawnHook,
    settings: SshSettings,
    /// This ssh refused `ForkAfterAuthentication` (OpenSSH before 8.7). Shared by clones, so
    /// later tunnels leave the option out at once.
    fork_option_refused: Arc<AtomicBool>,
}

impl SshLauncher {
    /// Starts `program` as ssh, without automatic host key checking and with the default hook.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            verify: false,
            spawn_hook: SpawnHook::default(),
            settings: SshSettings::default(),
            fork_option_refused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts the system ssh ([`resolve_ssh_path`]).
    pub fn system() -> Self {
        Self::new(resolve_ssh_path())
    }

    /// With `verify`, `-o StrictHostKeyChecking=accept-new` goes before the target's extra
    /// options and wins over them. Without it this crate passes no host key option, and the
    /// target's options, the ssh config, or the ssh default decide.
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    /// Creates every child of this launcher through `hook`.
    pub fn with_spawn_hook(mut self, hook: SpawnHook) -> Self {
        self.spawn_hook = hook;
        self
    }

    /// Runs with `settings`: its ssh options, port discovery bounds and tunnel waits. Without
    /// this call every value is its default.
    pub fn with_settings(mut self, settings: SshSettings) -> Self {
        self.settings = settings;
        self
    }

    /// The settings in effect, with their sources ([`SshSettings::entries`]).
    pub fn settings(&self) -> &SshSettings {
        &self.settings
    }

    /// The ssh options from settings as ssh uses them for `target`: `ServerAliveInterval`,
    /// `ServerAliveCountMax` and `ConnectTimeout`, each with its value and source. The first two
    /// go before the target's options and win over them; a `ConnectTimeout` in the target's
    /// options wins over the setting (source `target`).
    pub fn applied_options(&self, target: &SshTarget) -> Vec<AppliedOption> {
        crate::applied_options(&self.settings, target)
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn verifies_host_keys(&self) -> bool {
        self.verify
    }

    pub fn spawn_hook(&self) -> &SpawnHook {
        &self.spawn_hook
    }

    /// Whether ssh accepted `ForkAfterAuthentication` so far.
    pub(crate) fn fork_option_supported(&self) -> bool {
        !self.fork_option_refused.load(Ordering::Relaxed)
    }

    /// Records that this ssh does not know `ForkAfterAuthentication`.
    pub(crate) fn mark_fork_option_refused(&self) {
        self.fork_option_refused.store(true, Ordering::Relaxed);
    }

    /// The ssh command for `target`: common options with `forced` before the target's extra
    /// options, then `before` (flags), the destination, then `after` (the remote command).
    pub(crate) fn command(
        &self,
        target: &SshTarget,
        forced: &[&str],
        before: &[String],
        after: &[&str],
    ) -> Command {
        let mut args: Vec<String> = Vec::new();
        push_common_opts(&mut args, target, self.verify, forced, &self.settings);
        args.extend(before.iter().cloned());
        args.push(target.destination.clone());
        args.extend(after.iter().map(|a| (*a).to_string()));
        let mut cmd = Command::new(&self.program);
        cmd.args(&args);
        // A GUI host (Windows subsystem, no console) runs this in process. Without
        // CREATE_NO_WINDOW Windows opens a new console window for ssh.exe.
        hide_console(&mut cmd);
        cmd
    }
}

/// Keeps a console subsystem child from opening a new console window on Windows
/// (`CREATE_NO_WINDOW`). It does nothing on other systems.
pub(crate) fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW of winbase.h, written out to avoid a windows-sys dependency.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn exits_at_once() -> Command {
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd");
            c.args(["/c", "exit 0"]);
            c
        }
        #[cfg(not(windows))]
        {
            Command::new("true")
        }
    }

    #[test]
    fn the_default_hook_spawns_the_command() {
        let hook = SpawnHook::default();
        assert!(hook.is_default());
        let mut cmd = exits_at_once();
        cmd.stdin(Stdio::null());
        let mut child = hook.spawn(cmd).expect("spawn");
        assert!(child.wait().expect("wait").success());
    }

    #[test]
    fn a_custom_hook_receives_every_command() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let hook = SpawnHook::new(move |mut cmd| {
            seen.fetch_add(1, Ordering::SeqCst);
            cmd.spawn()
        });
        assert!(!hook.is_default());
        let cloned = hook.clone();
        for h in [&hook, &cloned] {
            let mut child = h.spawn(exits_at_once()).expect("spawn");
            child.wait().expect("wait");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(format!("{hook:?}").contains("custom: true"));
    }

    #[test]
    fn the_launcher_puts_the_destination_between_options_and_the_remote_command() {
        let launcher = SshLauncher::new("/opt/ssh").with_verify(true);
        assert_eq!(launcher.program(), Path::new("/opt/ssh"));
        assert!(launcher.verifies_host_keys());
        assert!(launcher.spawn_hook().is_default());
        let mut target = SshTarget::parse("me@box");
        target.extra_options = vec!["ProxyJump=bastion".into()];
        let cmd = launcher.command(
            &target,
            &["ExitOnForwardFailure=yes"],
            &["-N".to_string()],
            &["prog", "port"],
        );
        assert_eq!(cmd.get_program(), "/opt/ssh");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let dest = args
            .iter()
            .position(|a| a == "me@box")
            .expect("destination");
        let n = args.iter().position(|a| a == "-N").expect("-N");
        let jump = args
            .iter()
            .position(|a| a == "ProxyJump=bastion")
            .expect("extra option");
        let forced = args
            .iter()
            .position(|a| a == "ExitOnForwardFailure=yes")
            .expect("forced option");
        assert!(forced < jump && jump < n && n < dest, "{args:?}");
        assert_eq!(&args[dest + 1..], ["prog", "port"]);
    }
}
