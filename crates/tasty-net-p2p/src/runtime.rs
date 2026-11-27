//! The tokio runtime the endpoint owns, and the bridge from the blocking API to it
//! (docs/design/transport.md "동기·비동기 경계").
//!
//! One blocking owner thread builds the runtime, hands out its handle and, when the endpoint
//! ends, runs the endpoint's shutdown and drops the runtime there. Dropping the last
//! [`crate::P2pEndpoint`] only signals that thread, so the runtime is never dropped inside an
//! async context.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tokio::runtime::Handle;

/// The blocking API was called while the thread runs an async task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InAsyncContext;

/// Runs `fut` on `handle` and blocks the calling thread until it ends.
///
/// Refused inside an async task, where blocking would stall the runtime and tokio's `block_on`
/// panics. The test is "does this thread run a task now", not "is there a current runtime": a
/// `spawn_blocking` thread has a current runtime and may block.
pub(crate) fn block_on<F: Future>(handle: &Handle, fut: F) -> Result<F::Output, InAsyncContext> {
    if Handle::try_current().is_ok() && !may_block(handle) {
        return Err(InAsyncContext);
    }
    Ok(handle.block_on(fut))
}

/// Whether this thread, which has a current runtime, may block on `handle`: `block_on` of an
/// empty future succeeds outside an async task and panics inside one before it polls. The
/// probe only runs on threads that already have a current runtime.
fn may_block(handle: &Handle) -> bool {
    catch_unwind(AssertUnwindSafe(|| handle.block_on(async {}))).is_ok()
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

    #[test]
    fn blocking_is_refused_in_a_task_and_allowed_in_spawn_blocking() {
        let mut owner = Owner::start(2, Duration::from_secs(5)).expect("runtime");
        let handle = owner.handle().clone();
        assert_eq!(block_on(&handle, async { 1 }), Ok(1));
        let inner = handle.clone();
        let in_task = handle.block_on(async move {
            let h = inner.clone();
            tokio::spawn(async move { block_on(&h, async { 2 }) })
                .await
                .expect("task")
        });
        assert_eq!(in_task, Err(InAsyncContext));
        let inner = handle.clone();
        let in_blocking = handle.block_on(async move {
            tokio::task::spawn_blocking(move || block_on(&inner, async { 3 }))
                .await
                .expect("blocking")
        });
        assert_eq!(in_blocking, Ok(3));
        let finished = owner.finished();
        owner.stop(Box::new(|_| {}));
        assert!(
            finished.wait(Duration::from_secs(10)),
            "the owner thread ended"
        );
    }
}
