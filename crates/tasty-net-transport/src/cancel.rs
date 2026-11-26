//! Cancellation of one connection attempt (docs/design/transport.md "시도 취소, 닫기, 회수").

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::channel::ChannelControl;
use crate::end::shutdown_quietly;
use crate::lock::lock;

/// Something an attempt owns and closes on cancellation: a socket or stream of the attempt,
/// or a tunnel or connection not yet shared with other channels.
pub trait Cancellable: Send + Sync {
    /// Closes the resource and wakes whoever blocks on it. Called once, from the thread that
    /// cancels.
    fn cancel(&self);
}

struct ChannelResource(Arc<dyn ChannelControl>);

impl Cancellable for ChannelResource {
    fn cancel(&self) {
        shutdown_quietly(self.0.as_ref());
    }
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    resources: Mutex<Vec<(u64, Arc<dyn Cancellable>)>>,
    next_id: AtomicU64,
}

/// The cancel handle of one connection attempt.
///
/// The attempt registers what it opens. [`AttemptCancel::cancel`] marks the attempt cancelled
/// and closes every resource registered at that time; a resource registered afterwards is
/// refused and closed at once. A `connect` or handshake that runs under the handle then
/// returns `Cancelled` within its read or connect bound. Clones share one state.
#[derive(Clone, Default)]
pub struct AttemptCancel {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AttemptCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptCancel")
            .field("cancelled", &self.is_cancelled())
            .field("registered", &lock(&self.inner.resources).len())
            .finish()
    }
}

/// The attempt was cancelled before the resource could be registered. The resource was
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection attempt was cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl AttemptCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the attempt cancelled and closes every registered resource. Safe to call more
    /// than once and from any thread.
    pub fn cancel(&self) {
        let resources = {
            // The flag is set under the list lock that `register` reads it under, so a
            // resource is either in the list taken here or refused there.
            let mut list = lock(&self.inner.resources);
            self.inner.cancelled.store(true, Ordering::SeqCst);
            std::mem::take(&mut *list)
        };
        for (_, resource) in resources {
            resource.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Registers `resource` until the returned registration drops. After cancellation the
    /// resource is closed at once and [`Cancelled`] returned.
    pub fn register(&self, resource: Arc<dyn Cancellable>) -> Result<Registration, Cancelled> {
        let mut list = lock(&self.inner.resources);
        if self.is_cancelled() {
            drop(list);
            resource.cancel();
            return Err(Cancelled);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        list.push((id, resource));
        Ok(Registration {
            inner: Arc::downgrade(&self.inner),
            id,
        })
    }

    /// [`AttemptCancel::register`] for a channel: cancellation closes it both ways and wakes
    /// a blocked read.
    pub fn register_channel(
        &self,
        channel: Arc<dyn ChannelControl>,
    ) -> Result<Registration, Cancelled> {
        self.register(Arc::new(ChannelResource(channel)))
    }
}

/// A resource registered with an [`AttemptCancel`]. Dropping it removes the resource from the
/// attempt without closing it: the attempt handed it on (or finished with it).
#[derive(Debug)]
pub struct Registration {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            lock(&inner.resources).retain(|(id, _)| *id != self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Count(Arc<AtomicUsize>);
    impl Cancellable for Count {
        fn cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancel_closes_what_is_registered_and_refuses_what_comes_later() {
        let cancel = AttemptCancel::new();
        let closed = Arc::new(AtomicUsize::new(0));
        let kept = cancel
            .register(Arc::new(Count(Arc::clone(&closed))))
            .expect("register");
        let released = cancel
            .register(Arc::new(Count(Arc::clone(&closed))))
            .expect("register");
        drop(released);
        cancel.clone().cancel();
        assert_eq!(closed.load(Ordering::SeqCst), 1, "only the registered one");
        assert!(cancel.is_cancelled());
        let late = cancel.register(Arc::new(Count(Arc::clone(&closed))));
        assert_eq!(late.err(), Some(Cancelled));
        assert_eq!(
            closed.load(Ordering::SeqCst),
            2,
            "a late one closes at once"
        );
        cancel.cancel();
        assert_eq!(
            closed.load(Ordering::SeqCst),
            2,
            "cancel twice closes nothing more"
        );
        drop(kept);
    }

    #[test]
    fn cancel_wakes_a_read_blocked_on_a_registered_channel() {
        use std::io::Read;
        use std::time::{Duration, Instant};
        let (mut a, _b) = crate::memory::memory_channel_pair();
        let cancel = AttemptCancel::new();
        let registration = cancel.register_channel(a.control()).expect("register");
        let blocked = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            a.read(&mut byte)
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancel.cancel();
        assert_eq!(blocked.join().expect("reader").expect("eof"), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(registration);
    }
}
