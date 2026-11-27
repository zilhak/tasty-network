//! The tokio runtime the endpoint owns, and the bridge from the blocking API to it
//! (docs/design/transport.md "동기·비동기 경계").
//!
//! One blocking owner thread builds the runtime, hands out its handle and, when the endpoint
//! ends, runs the endpoint's shutdown and drops the runtime there. Dropping the last
//! [`crate::P2pEndpoint`] only signals that thread, so the runtime is never dropped inside an
//! async context.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::runtime::Handle;

/// More budget units than tokio gives one poll of a task (128 in tokio 1.54): a probe that
/// takes this many without running out runs without a budget.
const BUDGET_PROBE: usize = u8::MAX as usize;

/// Whether this thread runs an async task now, where the blocking API is refused
/// (docs/design/transport.md "동기·비동기 경계": the test is "does this thread run a task
/// now", not "is there a current runtime").
///
/// tokio has no public call for it. A thread outside the runtime has no current runtime. On a
/// thread of the runtime, tokio gives the poll of a task (and `Runtime::block_on`) a
/// cooperative budget, and runs `spawn_blocking`, `block_in_place` and threads that only
/// entered the runtime without one. So the probe takes budget units until tokio says the
/// budget ran out (a task) or more than any budget were taken (no task), and gives every unit
/// back. Nothing panics.
///
/// The limit: a task wrapped in `tokio::task::coop::unconstrained` has no budget either, and
/// tokio's public API cannot tell it from `spawn_blocking` or `block_in_place` without a panic
/// (`tokio::task::try_id` is `Some` in all three). Such a task is taken as not a task: the call
/// is not refused, and [`block_on`] blocks that worker thread until the call ends, as any
/// blocking call does. It does not panic. If the caller's task runs on the endpoint's own
/// runtime and that runtime has one worker (`runtime.worker_threads` = 1), the blocked worker is
/// the one that drives the runtime's timers and sockets, and the call can deadlock.
pub(crate) fn in_async_task() -> bool {
    if Handle::try_current().is_err() {
        return false;
    }
    let mut cx = Context::from_waker(Waker::noop());
    let mut taken = Vec::new();
    let budgeted = loop {
        match tokio::task::coop::poll_proceed(&mut cx) {
            Poll::Ready(unit) => {
                taken.push(unit);
                if taken.len() > BUDGET_PROBE {
                    break false;
                }
            }
            Poll::Pending => break true,
        }
    };
    // Each unit puts back the budget it found, so the first one taken goes back last.
    while let Some(unit) = taken.pop() {
        drop(unit);
    }
    budgeted
}

/// Poll `fut` on this thread until it ends, parking the thread between polls: the bridge of
/// the blocking API. The runtime's worker threads drive its timers and sockets, and
/// `Handle::enter` lets the future find the runtime. Unlike `Handle::block_on` it does not
/// check whether the thread already runs inside a runtime, so no context makes it panic (see
/// [`in_async_task`] for the contexts where it blocks a worker).
pub(crate) fn block_on<F: std::future::Future>(handle: &Handle, fut: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let _entered = handle.enter();
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        // A wake between the poll and the park leaves the token, so the park returns at once.
        std::thread::park();
    }
}

/// The shutdown work the owner thread runs before it drops the runtime.
pub(crate) type Shutdown = Box<dyn FnOnce(&Handle) + Send>;

/// The owner of the runtime.
pub(crate) struct Owner {
    handle: Handle,
    stop: Option<mpsc::Sender<Shutdown>>,
    finished: Arc<Finished>,
}

/// Set when the owner thread ended: the runtime and its threads are gone.
#[derive(Default)]
pub(crate) struct Finished {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Finished {
    fn set(&self) {
        *self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    /// Waits up to `limit` for the owner thread to end.
    pub(crate) fn wait(&self, limit: Duration) -> bool {
        let done = self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (done, _) = self
            .cv
            .wait_timeout_while(done, limit, |done| !*done)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *done
    }
}

impl Owner {
    /// Start the owner thread with a runtime of `workers` worker threads; on shutdown it waits
    /// up to `shutdown_timeout` for the runtime's tasks (`runtime.worker_threads`,
    /// `runtime.shutdown_timeout`).
    pub(crate) fn start(workers: usize, shutdown_timeout: Duration) -> std::io::Result<Self> {
        let (tx_handle, rx_handle) = mpsc::channel();
        let (tx_stop, rx_stop) = mpsc::channel::<Shutdown>();
        let finished = Arc::new(Finished::default());
        let done = Arc::clone(&finished);
        std::thread::Builder::new()
            .name("tasty-net-p2p".into())
            .spawn(move || {
                struct SetOnExit(Arc<Finished>);
                impl Drop for SetOnExit {
                    fn drop(&mut self) {
                        self.0.set();
                    }
                }
                let _exit = SetOnExit(done);
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(workers)
                    .thread_name("tasty-net-p2p-worker")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _sent = tx_handle.send(Err(e)).is_ok();
                        return;
                    }
                };
                if tx_handle.send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }
                // Every sender gone without a stop also ends the runtime.
                if let Ok(shutdown) = rx_stop.recv() {
                    shutdown(runtime.handle());
                }
                runtime.shutdown_timeout(shutdown_timeout);
            })?;
        let handle = rx_handle
            .recv()
            .map_err(|_| std::io::Error::other("the runtime thread ended early"))??;
        Ok(Self {
            handle,
            stop: Some(tx_stop),
            finished,
        })
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Hands `shutdown` to the owner thread, which runs it before dropping the runtime.
    pub(crate) fn stop(&mut self, shutdown: Shutdown) {
        if let Some(stop) = self.stop.take() {
            let _sent = stop.send(shutdown).is_ok();
        }
    }

    /// The signal of the owner thread's end.
    pub(crate) fn finished(&self) -> Arc<Finished> {
        Arc::clone(&self.finished)
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Only signals: the endpoint's last reference may drop inside an async task.
        self.stop.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panics on this thread seen by the panic hook.
    fn panics_here(f: impl FnOnce()) -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static WATCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        let before = SEEN.load(Ordering::SeqCst);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if WATCHED.with(std::cell::Cell::get) {
                SEEN.fetch_add(1, Ordering::SeqCst);
            }
            prev(info);
        }));
        WATCHED.with(|w| w.set(true));
        f();
        WATCHED.with(|w| w.set(false));
        SEEN.load(Ordering::SeqCst) - before
    }

    /// The bridge never panics: not in the main future of another runtime's `block_on` (where
    /// `Handle::block_on` panics) and not in an unconstrained task. The callers run on a
    /// runtime other than the owner's, so the owner's workers stay free.
    #[test]
    fn the_bridge_does_not_panic_inside_another_runtime() {
        let mut owner = Owner::start(1, Duration::from_secs(5)).expect("runtime");
        let handle = owner.handle().clone();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("caller runtime");
        let wait = Duration::from_millis(10);
        let mut got = Vec::new();
        let panics = panics_here(|| {
            let h = handle.clone();
            got.push(caller.block_on(async move {
                block_on(&h, async move {
                    tokio::time::sleep(wait).await;
                    1
                })
            }));
            let h = handle.clone();
            got.push(caller.block_on(async move {
                tokio::spawn(tokio::task::coop::unconstrained(async move {
                    block_on(&h, async move {
                        tokio::time::sleep(wait).await;
                        2
                    })
                }))
                .await
                .expect("task")
            }));
        });
        assert_eq!((got, panics), (vec![1, 2], 0));
        drop(caller);
        let finished = owner.finished();
        owner.stop(Box::new(|_| {}));
        assert!(finished.wait(Duration::from_secs(10)));
    }

    #[test]
    fn blocking_is_refused_in_a_task_and_allowed_in_spawn_blocking() {
        let mut owner = Owner::start(2, Duration::from_secs(5)).expect("runtime");
        let handle = owner.handle().clone();
        assert!(!in_async_task());
        // A task on the runtime's worker: the answer must not come from a panic, so the hook
        // watches the worker thread.
        let in_task = handle.block_on(async {
            tokio::spawn(async {
                let mut got = None;
                let panics = panics_here(|| got = Some(in_async_task()));
                (got, panics)
            })
            .await
            .expect("task")
        });
        assert_eq!(in_task, (Some(true), 0));
        // The budget the probe took is back: the task still has its budget.
        let left = handle.block_on(async {
            tokio::spawn(async {
                let _probed = in_async_task();
                tokio::task::coop::has_budget_remaining()
            })
            .await
            .expect("task")
        });
        assert!(left);
        // Inside the caller's own block_on (a budgeted poll too).
        let mut nested = None;
        let panics = panics_here(|| nested = Some(handle.block_on(async { in_async_task() })));
        assert_eq!((nested, panics), (Some(true), 0));
        let in_blocking = handle.block_on(async {
            tokio::task::spawn_blocking(in_async_task)
                .await
                .expect("blocking")
        });
        assert!(!in_blocking);
        // A thread that entered the runtime without running a task may block.
        let entered = handle.enter();
        assert!(!in_async_task());
        drop(entered);
        let finished = owner.finished();
        owner.stop(Box::new(|_| {}));
        assert!(
            finished.wait(Duration::from_secs(10)),
            "the owner thread ended"
        );
    }
}
