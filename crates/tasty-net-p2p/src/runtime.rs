//! The tokio runtime the endpoint owns, and the bridge from the blocking API to it
//! (docs/design/transport.md "동기·비동기 경계").
//!
//! One blocking owner thread builds the runtime, hands out its handle and, when the endpoint
//! ends, runs the endpoint's shutdown and drops the runtime there. Dropping the last
//! [`crate::P2pEndpoint`] only signals that thread, so the runtime is never dropped inside an
//! async context.

use std::future::Future;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::runtime::Handle;

/// The blocking API was called while the thread runs an async task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InAsyncContext;

/// Runs `fut` on `handle` and blocks the calling thread until it ends.
///
/// Refused inside an async task ([`in_async_task`]), where blocking would stall the runtime
/// and tokio's `block_on` panics.
pub(crate) fn block_on<F: Future>(handle: &Handle, fut: F) -> Result<F::Output, InAsyncContext> {
    if in_async_task() {
        return Err(InAsyncContext);
    }
    Ok(handle.block_on(fut))
}

/// More budget units than tokio gives one poll of a task (128 in tokio 1.54): a probe that
/// takes this many without running out runs without a budget.
const BUDGET_PROBE: usize = u8::MAX as usize;

/// Whether this thread runs an async task now (docs/design/transport.md "동기·비동기 경계":
/// the test is "does this thread run a task now", not "is there a current runtime").
///
/// tokio has no public call for it. A thread outside the runtime has no current runtime. On a
/// thread of the runtime, tokio gives the poll of a task (and `Runtime::block_on`) a
/// cooperative budget, and runs `spawn_blocking`, `block_in_place` and threads that only
/// entered the runtime without one. So the probe takes budget units until tokio says the
/// budget ran out (a task) or more than any budget were taken (no task), and gives every unit
/// back. Nothing panics. A task wrapped in `tokio::task::coop::unconstrained` has no budget
/// and is taken as not a task; blocking there stalls its worker, as any blocking call does.
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

    #[test]
    fn blocking_is_refused_in_a_task_and_allowed_in_spawn_blocking() {
        let mut owner = Owner::start(2, Duration::from_secs(5)).expect("runtime");
        let handle = owner.handle().clone();
        assert_eq!(block_on(&handle, async { 1 }), Ok(1));
        // A task on the runtime's worker, polled on that thread: the refusal must not come
        // from a panic, so the hook watches the worker thread too.
        let inner = handle.clone();
        let in_task = handle.block_on(async move {
            let h = inner.clone();
            tokio::spawn(async move {
                let mut got = None;
                let panics = panics_here(|| got = Some(block_on(&h, async { 2 })));
                (got, panics)
            })
            .await
            .expect("task")
        });
        assert_eq!(in_task, (Some(Err(InAsyncContext)), 0));
        // The budget the probe took is back: the task still runs as many budgeted polls.
        let inner = handle.clone();
        let left = handle.block_on(async move {
            tokio::spawn(async move {
                let _refused = block_on(&inner, async {});
                tokio::task::coop::has_budget_remaining()
            })
            .await
            .expect("task")
        });
        assert!(left);
        // Inside the caller's own block_on (a budgeted poll too).
        let mut nested = None;
        let panics = panics_here(|| {
            nested = Some(handle.block_on(async { block_on(&handle, async { 4 }) }));
        });
        assert_eq!((nested, panics), (Some(Err(InAsyncContext)), 0));
        let inner = handle.clone();
        let in_blocking = handle.block_on(async move {
            tokio::task::spawn_blocking(move || block_on(&inner, async { 3 }))
                .await
                .expect("blocking")
        });
        assert_eq!(in_blocking, Ok(3));
        // A thread that entered the runtime without running a task may block.
        let _entered = handle.enter();
        assert_eq!(block_on(&handle, async { 5 }), Ok(5));
        drop(_entered);
        let finished = owner.finished();
        owner.stop(Box::new(|_| {}));
        assert!(
            finished.wait(Duration::from_secs(10)),
            "the owner thread ended"
        );
    }
}
