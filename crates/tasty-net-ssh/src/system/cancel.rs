//! The system backend's part of [`SshCancel`]: the port discovery children registered under a
//! handle, and killing and reaping them.

use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::cancel::{SshCancel, recover_mutex};

/// The tracing target of this module's events: its module path before it moved into
/// `system`, so log filters on the old path keep selecting them.
const LOG_TARGET: &str = "tasty_net_ssh::cancel";

/// The discovery children running now, one per lookup, by registration id. The killing side
/// takes a child out: `Child::kill` needs `&mut self`, so moving the child out makes the access
/// exclusive.
#[derive(Default)]
pub(crate) struct Children {
    list: Mutex<Vec<(u64, Child)>>,
    next_id: AtomicU64,
}

impl SshCancel {
    /// [`SshCancel::cancel`] of a build with the system backend: sets the flag, then kills and
    /// reaps the children registered at the time of the call.
    pub(crate) fn cancel_children(&self) {
        // The flag is set while the list is locked, and `register` reads it under the same
        // lock: a child is either in the list taken here or refused.
        let children = {
            let mut slot = lock_slot(&self.inner.children.list);
            self.request_cancel();
            std::mem::take(&mut *slot)
        };
        for (_, mut child) in children {
            kill_and_reap(&mut child);
        }
    }

    /// Registers a running child and returns its id. After cancellation the child is refused
    /// and handed back so the caller kills it at once. The flag is read under the list lock that
    /// `cancel` sets it under, so `cancel` never misses a child registered alongside it.
    pub(crate) fn register(&self, child: Child) -> Result<u64, Child> {
        let mut slot = lock_slot(&self.inner.children.list);
        if self.is_cancelled() {
            return Err(child);
        }
        let id = self.inner.children.next_id.fetch_add(1, Ordering::Relaxed);
        slot.push((id, child));
        Ok(id)
    }

    /// How many children are registered now.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> usize {
        lock_slot(&self.inner.children.list).len()
    }

    /// Takes child `id` back. `None` means [`SshCancel::cancel`] already took and killed it.
    pub(crate) fn reclaim(&self, id: u64) -> Option<Child> {
        let mut children = lock_slot(&self.inner.children.list);
        let pos = children.iter().position(|(i, _)| *i == id)?;
        Some(children.swap_remove(pos).1)
    }

    /// Runs `f` on child `id`. `None` means the child was taken by cancellation.
    pub(crate) fn with_child<R>(&self, id: u64, f: impl FnOnce(&mut Child) -> R) -> Option<R> {
        lock_slot(&self.inner.children.list)
            .iter_mut()
            .find(|(i, _)| *i == id)
            .map(|(_, child)| f(child))
    }
}

/// The child list stays valid after a panic (each entry is pushed or removed whole). Recover it
/// so the children can still be reaped.
const CHILD_SLOT_WHAT: &str = "the ssh child list";
static CHILD_SLOT_POISON_REPORTED: AtomicBool = AtomicBool::new(false);

fn lock_slot<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    recover_mutex(m.lock(), CHILD_SLOT_WHAT, &CHILD_SLOT_POISON_REPORTED)
}

/// Kills and waits for a child so that no zombie is left. `kill` can fail on a child that
/// already exited; the wait still reaps it.
pub(crate) fn kill_and_reap(child: &mut Child) {
    if let Err(e) = child.kill() {
        tracing::debug!(
            target: LOG_TARGET,
            "ssh child kill failed (the child may already have exited): {e}",
        );
    }
    if let Err(e) = child.wait() {
        tracing::debug!(target: LOG_TARGET, "ssh child reaping failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A registered child is killed and reaped by `cancel()`. `cancel()` also waits, so the
    /// test ending well before the child's 60 seconds shows that the kill worked.
    #[test]
    fn cancel_kills_and_reaps_registered_child() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        let handle = SshCancel::new();
        let child = crate::test_helper::helper_command("sleep")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep spawn");
        handle.register(child).expect("not cancelled yet");

        const CEILING: Duration = Duration::from_secs(10);
        let t0 = Instant::now();
        handle.cancel();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < CEILING,
            "reaped right after the kill: {elapsed:?}"
        );
        assert!(handle.is_cancelled());
        assert!(handle.reclaim(0).is_none());
    }

    /// A child registered after cancellation is refused and handed back.
    #[test]
    fn register_after_cancel_is_rejected() {
        use std::process::Stdio;
        let handle = SshCancel::new();
        handle.cancel();
        let child = crate::test_helper::helper_command("sleep")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep spawn");
        let mut rejected = handle.register(child).expect_err("refused after cancel");
        kill_and_reap(&mut rejected);
        assert_eq!(handle.registered(), 0);
    }

    /// Registrations racing a cancel either land in the list `cancel` takes or are refused:
    /// none is left registered after `cancel` returns.
    #[test]
    fn no_child_stays_registered_past_a_racing_cancel() {
        use std::process::Stdio;
        let handle = SshCancel::new();
        let registrars: Vec<_> = (0..4)
            .map(|_| {
                let h = handle.clone();
                std::thread::spawn(move || {
                    let child = crate::test_helper::helper_command("sleep")
                        .stdin(Stdio::null())
                        .spawn()
                        .expect("sleep spawn");
                    if let Err(mut rejected) = h.register(child) {
                        kill_and_reap(&mut rejected);
                    }
                })
            })
            .collect();
        handle.cancel();
        let left = handle.registered();
        for r in registrars {
            r.join().expect("registrar");
        }
        // Children registered before cancel were taken by it; later ones were refused.
        assert_eq!(left, 0);
        assert_eq!(handle.registered(), 0);
    }
}
