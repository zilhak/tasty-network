//! Cancellation of port discovery children, and the lock recovery the child slot needs.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard};

/// Cancels the port discovery children of another thread.
///
/// The discovery children of worker threads that installed this handle (or a clone) with
/// [`SshCancel::scope`] are registered here, and [`SshCancel::cancel`] kills and reaps all of
/// them. Several lookups may run under one handle at the same time; each one only polls and
/// reclaims its own child. Tunnels are owned by `SshTunnel`, not by this handle.
#[derive(Clone, Default)]
pub struct SshCancel {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    /// The discovery children running now, one per lookup, by registration id. The killing side
    /// takes a child out: `Child::kill` needs `&mut self`, so moving the child out makes the
    /// access exclusive.
    children: Mutex<Vec<(u64, Child)>>,
    next_id: AtomicU64,
}

thread_local! {
    /// The cancel handle installed on the current thread ([`SshCancel::scope`]).
    static CURRENT_CANCEL: RefCell<Option<SshCancel>> = const { RefCell::new(None) };
}

impl SshCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs this handle as the port discovery cancel target of the **current thread**. The
    /// returned guard restores the previous installation when it is dropped (nesting is safe).
    pub fn scope(&self) -> SshCancelScope {
        let prev = CURRENT_CANCEL.with(|c| c.borrow_mut().replace(self.clone()));
        SshCancelScope {
            prev,
            _thread_bound: PhantomData,
        }
    }

    /// Marks the handle cancelled without waiting for the child. The discovery that owns the
    /// child sees the flag and kills and reaps the child before its worker returns. This is not
    /// a completion receipt.
    pub fn request_cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }

    /// Marks the handle cancelled, then kills and reaps every registered child. Later discovery
    /// steps under this handle do not start a child. Calling it more than once is safe. When it
    /// returns, every child registered under this handle is killed and reaped.
    pub fn cancel(&self) {
        // The flag is set while the list is locked, and `register` reads it under the same
        // lock: a child is either in the list taken here or refused.
        let children = {
            let mut slot = lock_slot(&self.inner.children);
            self.request_cancel();
            std::mem::take(&mut *slot)
        };
        for (_, mut child) in children {
            kill_and_reap(&mut child);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Registers a running child and returns its id. After cancellation the child is refused
    /// and handed back so the caller kills it at once. The flag is read under the list lock that
    /// `cancel` sets it under, so `cancel` never misses a child registered alongside it.
    pub(crate) fn register(&self, child: Child) -> Result<u64, Child> {
        let mut slot = lock_slot(&self.inner.children);
        if self.is_cancelled() {
            return Err(child);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        slot.push((id, child));
        Ok(id)
    }

    /// How many children are registered now.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> usize {
        lock_slot(&self.inner.children).len()
    }

    /// Takes child `id` back. `None` means [`SshCancel::cancel`] already took and killed it.
    pub(crate) fn reclaim(&self, id: u64) -> Option<Child> {
        let mut children = lock_slot(&self.inner.children);
        let pos = children.iter().position(|(i, _)| *i == id)?;
        Some(children.swap_remove(pos).1)
    }

    /// Runs `f` on child `id`. `None` means the child was taken by cancellation.
    pub(crate) fn with_child<R>(&self, id: u64, f: impl FnOnce(&mut Child) -> R) -> Option<R> {
        lock_slot(&self.inner.children)
            .iter_mut()
            .find(|(i, _)| *i == id)
            .map(|(_, child)| f(child))
    }
}

/// The RAII guard of [`SshCancel::scope`].
///
/// It has no use of its own, but it is public because it is the return type of
/// `SshCancel::scope()`. It restores the installation of the thread that created it, so it is
/// neither `Send` nor `Sync`: it cannot be dropped on another thread.
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// let handle = tasty_net_ssh::SshCancel::new();
/// send(handle.scope());
/// ```
pub struct SshCancelScope {
    prev: Option<SshCancel>,
    /// A raw pointer is neither `Send` nor `Sync`, which binds the guard to its thread.
    _thread_bound: PhantomData<*const ()>,
}

impl Drop for SshCancelScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT_CANCEL.with(|c| *c.borrow_mut() = prev);
    }
}

/// The cancel handle installed on the current thread.
pub(crate) fn current_cancel() -> Option<SshCancel> {
    CURRENT_CANCEL.with(|c| c.borrow().clone())
}

/// Whether the port discovery of the current thread is cancelled (false without a scope).
pub(crate) fn cancel_requested() -> bool {
    current_cancel().is_some_and(|c| c.is_cancelled())
}

/// Recovers a poisoned mutex and reports the first occurrence per `reported` flag.
///
/// Use it only where the guarded data keeps its invariants even if a holder panicked.
pub(crate) fn recover_mutex<'a, T: ?Sized>(
    acquired: LockResult<MutexGuard<'a, T>>,
    what: &str,
    reported: &AtomicBool,
) -> MutexGuard<'a, T> {
    acquired.unwrap_or_else(|poisoned| {
        if !reported.swap(true, Ordering::Relaxed) {
            tracing::error!(
                "{what} lock poisoned — a thread panicked while holding it. Recovering (the \
                 guarded data keeps its invariants); later occurrences are not logged."
            );
        }
        poisoned.into_inner()
    })
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
        tracing::debug!("ssh child kill failed (the child may already have exited): {e}");
    }
    if let Err(e) = child.wait() {
        tracing::debug!("ssh child reaping failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_scope_restores_previous() {
        assert!(current_cancel().is_none());
        let outer = SshCancel::new();
        {
            let _g = outer.scope();
            assert!(current_cancel().is_some());
            let inner = SshCancel::new();
            {
                let _g2 = inner.scope();
                inner.cancel();
                assert!(cancel_requested());
            }
            assert!(
                !cancel_requested(),
                "the outer handle is back after the guard drops"
            );
        }
        assert!(current_cancel().is_none());
    }

    #[test]
    fn request_cancel_only_sets_the_flag() {
        let handle = SshCancel::new();
        assert!(!handle.is_cancelled());
        handle.request_cancel();
        assert!(handle.is_cancelled());
        // Clones share the state.
        assert!(handle.clone().is_cancelled());
    }

    #[test]
    fn a_poisoned_mutex_is_recovered_and_reported_once() {
        let m = Arc::new(Mutex::new(7));
        let poisoner = Arc::clone(&m);
        let joined = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("first lock");
            panic!("poison the mutex");
        })
        .join();
        assert!(joined.is_err(), "the thread panicked");
        assert!(m.is_poisoned());
        let reported = AtomicBool::new(false);
        assert_eq!(*recover_mutex(m.lock(), "test", &reported), 7);
        assert!(reported.load(Ordering::Relaxed));
        // A healthy lock does not touch the flag.
        let healthy = Mutex::new(1);
        let untouched = AtomicBool::new(false);
        assert_eq!(*recover_mutex(healthy.lock(), "test", &untouched), 1);
        assert!(!untouched.load(Ordering::Relaxed));
    }

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
