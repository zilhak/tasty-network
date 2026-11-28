//! The embedded SSH backend: an in-process client (`embedded-ssh`, docs/design/ssh-backends.md
//! "내장 백엔드: 세션 하나에 채널 여럿").
//!
//! russh does the connection, the cryptography and the channels; the OpenSSH port
//! (`tasty-net-ssh-config`) reads the configuration and known_hosts files the way `ssh` does.
//! The API stays synchronous: the blocking calls run on a runtime this backend owns
//! (`embedded.runtime.worker_threads`) and the caller waits for the result.
//!
//! Modules: [`resolve`] (the configuration of a purpose), [`hostkey`] (the host key check and
//! `accept-new` records), [`algorithms`], [`session`] (connect, authenticate, jump hosts and
//! the session pool), [`tunnel`] and [`discovery`].

pub(crate) mod algorithms;
pub(crate) mod discovery;
pub(crate) mod hostkey;
pub(crate) mod resolve;
pub(crate) mod session;
pub(crate) mod tunnel;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::backend::SshBackendError;
use crate::cancel::{SshCancel, recover_mutex};
use crate::prompt::SshPrompt;
use crate::settings::SshSettings;

/// The tracing target of the embedded backend's events.
pub(crate) const LOG_TARGET: &str = "tasty_net_ssh::embedded";

/// The runtime of a connector's embedded sessions. It is made on first use and shut down
/// without waiting when the last owner (the connector or a session) drops.
pub(crate) struct Runtime {
    rt: Option<tokio::runtime::Runtime>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl Runtime {
    fn handle(&self) -> Option<&tokio::runtime::Handle> {
        self.rt.as_ref().map(tokio::runtime::Runtime::handle)
    }
}

/// What a connector shares with its clones for the embedded backend: the runtime and the
/// sessions open now.
#[derive(Default)]
pub(crate) struct Embedded {
    runtime: OnceLock<Result<Arc<Runtime>, String>>,
    /// Open sessions by their key (docs/design/ssh-backends.md "용도마다 해석하고, 같을 때만
    /// 세션을 나눈다"). A session lives while a handle on it does.
    pool: Mutex<HashMap<String, Weak<session::Session>>>,
    pool_poison_reported: AtomicBool,
}

impl fmt::Debug for Embedded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedded")
            .field("runtime", &self.runtime.get().map(Result::is_ok))
            .finish_non_exhaustive()
    }
}

impl Embedded {
    /// The runtime, made with `settings` on first use.
    pub(crate) fn runtime(&self, settings: &SshSettings) -> Result<Arc<Runtime>, SshBackendError> {
        self.runtime
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(settings.worker_threads())
                    .thread_name("tasty-net-ssh")
                    .enable_io()
                    .enable_time()
                    .build()
                    .map(|rt| Arc::new(Runtime { rt: Some(rt) }))
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(|detail| SshBackendError::Session {
                detail: format!("the embedded SSH runtime could not start: {detail}"),
            })
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, HashMap<String, Weak<session::Session>>> {
        recover_mutex(
            self.pool.lock(),
            "embedded session pool",
            &self.pool_poison_reported,
        )
    }
}

/// What one embedded operation needs from its connector.
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub(crate) shared: &'a Embedded,
    pub(crate) settings: &'a SshSettings,
    pub(crate) prompt: Option<&'a Arc<dyn SshPrompt>>,
    pub(crate) hook: Option<&'a SessionHook>,
    /// The launcher's `with_verify`: `StrictHostKeyChecking=accept-new` at place 3 of the
    /// option order.
    pub(crate) verify: bool,
    pub(crate) next_session: &'a AtomicU64,
}

/// Runs `fut` on the runtime and waits for its result on the calling thread. It does not need
/// the caller to be outside a runtime (it does not use `block_on`).
pub(crate) fn run_blocking<T: Send + 'static>(
    rt: &Runtime,
    fut: impl Future<Output = Result<T, SshBackendError>> + Send + 'static,
) -> Result<T, SshBackendError> {
    let Some(handle) = rt.handle() else {
        return Err(runtime_gone());
    };
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn(async move {
        // The waiter can be gone only if it panicked; then nobody needs the result.
        tx.send(fut.await).unwrap_or_default();
    });
    rx.recv().unwrap_or_else(|_| Err(runtime_gone()))
}

fn runtime_gone() -> SshBackendError {
    SshBackendError::Session {
        detail: "the embedded SSH runtime stopped".to_string(),
    }
}

/// Time spent waiting for [`SshPrompt`] answers, which the connect timeout does not count
/// (docs/design/ssh-backends.md `embedded.prompt_timeout`).
#[derive(Debug, Default)]
pub(crate) struct Paused {
    /// Nanoseconds not yet credited to a deadline.
    pending: AtomicU64,
}

impl Paused {
    pub(crate) fn add(&self, waited: Duration) {
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.pending.fetch_add(nanos, Ordering::AcqRel);
    }

    fn take(&self) -> Duration {
        Duration::from_nanos(self.pending.swap(0, Ordering::AcqRel))
    }
}

/// How a bounded wait ended.
pub(crate) enum Bounded<T> {
    Done(T),
    Expired,
    Cancelled,
}

/// Waits for `fut` until `deadline`, pushed back by the prompt time in `paused`, and stops
/// early when `cancel` is cancelled (looked at every `poll`).
pub(crate) async fn bounded<F: Future>(
    fut: F,
    mut deadline: Instant,
    paused: &Paused,
    cancel: Option<&SshCancel>,
    poll: Duration,
) -> Bounded<F::Output> {
    let mut fut = std::pin::pin!(fut);
    loop {
        if cancel.is_some_and(SshCancel::is_cancelled) {
            return Bounded::Cancelled;
        }
        let step = match cancel {
            Some(_) => deadline.min(Instant::now() + poll),
            None => deadline,
        };
        match tokio::time::timeout_at(step.into(), &mut fut).await {
            Ok(out) => return Bounded::Done(out),
            Err(_) => {
                deadline += paused.take();
                if Instant::now() >= deadline {
                    return Bounded::Expired;
                }
            }
        }
    }
}

/// Called with each embedded SSH session right after it is set up, like the system backend's
/// [`crate::SpawnHook`] sees each `ssh` child. Tests use it to end a session from outside.
#[derive(Clone)]
pub struct SessionHook(Arc<dyn Fn(SessionControl) + Send + Sync>);

impl SessionHook {
    /// A hook that calls `f` with each new session.
    pub fn new(f: impl Fn(SessionControl) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub(crate) fn call(&self, control: SessionControl) {
        (self.0)(control);
    }
}

impl fmt::Debug for SessionHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionHook")
    }
}

/// One embedded SSH session, as [`SessionHook`] sees it. It does not keep the session open.
#[derive(Clone)]
pub struct SessionControl {
    id: u64,
    session: Weak<session::Conn>,
}

impl SessionControl {
    /// The session's number within its connector (`session_id` of the connection info).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Ends the session now (`SSH_MSG_DISCONNECT`); its channels end with it. Nothing happens
    /// when it already ended.
    pub fn disconnect(&self) {
        if let Some(conn) = self.session.upgrade() {
            conn.disconnect();
        }
    }
}

impl fmt::Debug for SessionControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionControl")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Asks `prompt` on a blocking worker and waits up to `timeout` for the answer. The time
/// waited goes into `paused`. `None` when it gave no answer in time.
pub(crate) async fn ask<T: Send + 'static>(
    prompt: Arc<dyn SshPrompt>,
    timeout: Duration,
    paused: &Paused,
    question: impl FnOnce(&dyn SshPrompt) -> T + Send + 'static,
) -> Option<T> {
    let started = Instant::now();
    let job = tokio::task::spawn_blocking(move || question(prompt.as_ref()));
    let answer = tokio::time::timeout(timeout, job).await;
    paused.add(started.elapsed());
    match answer {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) | Err(_) => None,
    }
}
