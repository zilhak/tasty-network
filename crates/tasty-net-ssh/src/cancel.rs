//! Cancellation of port discovery, shared by the backends, and the lock recovery their slots
//! need. The system backend registers its discovery children here (`system::cancel`); the
//! embedded backend polls the flag while it waits for its channel.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LockResult, MutexGuard};

/// The tracing target of this module's events: the module path it had in the system
/// backend, so log filters on that path keep selecting them.
const LOG_TARGET: &str = "tasty_net_ssh::cancel";

/// Cancels the port discovery of another thread.
///
/// The port discovery of worker threads that installed this handle (or a clone) with
/// [`SshCancel::scope`] is stopped by [`SshCancel::cancel`]: the system backend's children
/// registered here are killed and reaped, and the embedded backend's discovery sees the flag
/// and closes its channel. Several lookups may run under one handle at the same time; each one
/// only polls and reclaims its own child. Tunnels are owned by `SshTunnel`, not by this handle.
#[derive(Clone, Default)]
pub struct SshCancel {
    pub(crate) inner: Arc<CancelInner>,
}

#[derive(Default)]
pub(crate) struct CancelInner {
    pub(crate) cancelled: AtomicBool,
    /// The system backend's discovery children running now.
    #[cfg(feature = "system-ssh")]
    pub(crate) children: crate::system::cancel::Children,
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

    /// Marks the handle cancelled, then kills and reaps the children registered at the time of the
    /// call. A child that its own lookup has already taken out to reap (at its bound or its
    /// output limit) is reaped by that lookup, which may still be doing so when this returns.
    /// Later discovery steps under this handle do not start a child. Calling it more than once is
    /// safe. An embedded lookup sees the flag and closes its channel.
    pub fn cancel(&self) {
        #[cfg(feature = "system-ssh")]
        self.cancel_children();
        #[cfg(not(feature = "system-ssh"))]
        self.request_cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
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
                target: LOG_TARGET,
                "{what} lock poisoned — a thread panicked while holding it. Recovering (the \
                 guarded data keeps its invariants); later occurrences are not logged.",
            );
        }
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

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
}
