//! Cancellation of port discovery children, and the lock recovery the child slot needs.

use std::cell::RefCell;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard};

/// Cancels the port discovery children of another thread.
///
/// The discovery child of a worker thread that installed this handle with [`SshCancel::scope`]
/// is registered here, and [`SshCancel::cancel`] kills and reaps it. Tunnels are owned by
/// `SshTunnel`, not by this handle.
#[derive(Clone, Default)]
pub struct SshCancel {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    /// The discovery child running now (replaced at every step). The killing side takes it out:
    /// `Child::kill` needs `&mut self`, so moving the child out makes the access exclusive.
    child: Mutex<Option<Child>>,
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
        SshCancelScope { prev }
    }

    /// Marks the handle cancelled without waiting for the child. The discovery that owns the
    /// child sees the flag and kills and reaps the child before its worker returns. This is not
    /// a completion receipt.
    pub fn request_cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }

    /// Marks the handle cancelled, then kills and reaps the registered child. Later discovery
    /// steps under this handle do not start a child. Calling it more than once is safe.
    pub fn cancel(&self) {
        self.request_cancel();
        let child = lock_slot(&self.inner.child).take();
        if let Some(mut child) = child {
            kill_and_reap(&mut child);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Registers a running child. After cancellation the child is refused and handed back so
    /// the caller kills it at once; a child that races with `cancel` does not leak.
    pub(crate) fn register(&self, child: Child) -> Result<(), Child> {
        if self.is_cancelled() {
            return Err(child);
        }
        // Discovery steps run one after another, so the slot should be empty. A child left
        // there belongs to an earlier step that was not reaped; clean it up here.
        if let Some(mut stale) = lock_slot(&self.inner.child).replace(child) {
            kill_and_reap(&mut stale);
        }
        Ok(())
    }

    /// Takes the child back. `None` means [`SshCancel::cancel`] already took and killed it.
    pub(crate) fn reclaim(&self) -> Option<Child> {
        lock_slot(&self.inner.child).take()
    }

    /// Runs `f` on the registered child. `None` means the child was taken by cancellation.
    pub(crate) fn with_child<R>(&self, f: impl FnOnce(&mut Child) -> R) -> Option<R> {
        lock_slot(&self.inner.child).as_mut().map(f)
    }
}

/// The RAII guard of [`SshCancel::scope`].
///
/// It has no use of its own, but it is public because it is the return type of
/// `SshCancel::scope()`.
pub struct SshCancelScope {
    prev: Option<SshCancel>,
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

/// A child slot holds at most one child, which stays valid after a panic. Recover it so the
/// child can still be reaped.
const CHILD_SLOT_WHAT: &str = "the ssh child slot";
static CHILD_SLOT_POISON_REPORTED: AtomicBool = AtomicBool::new(false);

fn lock_slot(m: &Mutex<Option<Child>>) -> MutexGuard<'_, Option<Child>> {
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
    #[cfg(unix)]
    #[test]
    fn cancel_kills_and_reaps_registered_child() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let handle = SshCancel::new();
        let child = Command::new("sleep")
            .arg("60")
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
        assert!(handle.reclaim().is_none());
    }

    /// A child registered after cancellation is refused and handed back.
    #[cfg(unix)]
    #[test]
    fn register_after_cancel_is_rejected() {
        use std::process::{Command, Stdio};
        let handle = SshCancel::new();
        handle.cancel();
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep spawn");
        let mut rejected = handle.register(child).expect_err("refused after cancel");
        kill_and_reap(&mut rejected);
    }
}
