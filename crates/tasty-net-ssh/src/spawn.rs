//! The child creation hook. Every `ssh` child of this crate is created through a [`SpawnHook`].

use std::fmt;
use std::io;
use std::process::{Child, Command};
use std::sync::Arc;

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
}
