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
#[cfg(test)]
pub(crate) mod test_server;
pub(crate) mod tunnel;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::Poll;
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

    /// How many sessions the pool names, live or not (tests).
    #[cfg(all(test, unix))]
    pub(crate) fn pooled(&self) -> usize {
        self.pool().len()
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

/// Runs `work` (file reads: the configuration, known_hosts, key files) on a blocking thread of
/// the runtime, so a file that does not answer holds no runtime worker and the wait for it
/// stays under the caller's deadline and cancel (docs/design/ssh-backends.md "내장 백엔드:
/// 세션 하나에 채널 여럿"). A wait that gives up leaves the read to end on its own thread.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SshBackendError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| SshBackendError::Session {
            detail: format!("a file read of the embedded SSH backend stopped: {e}"),
        })
}

fn runtime_gone() -> SshBackendError {
    SshBackendError::Session {
        detail: "the embedded SSH runtime stopped".to_string(),
    }
}

/// Time spent waiting for [`SshPrompt`] answers, which the connect timeout does not count
/// (docs/design/ssh-backends.md `embedded.prompt_timeout`). While a question is open a
/// deadline does not run out: [`bounded`] waits for the answer, and the time it took is then
/// added to the deadline. The time only grows, and each [`bounded`] adds what grew since it
/// last looked, so nested deadlines that share it each get the whole question time.
#[derive(Debug, Default)]
pub(crate) struct Paused {
    /// Nanoseconds of all ended questions.
    total: AtomicU64,
    /// Questions open now.
    open: AtomicUsize,
    /// Woken when a question ends.
    ended: tokio::sync::Notify,
}

impl Paused {
    pub(crate) fn add(&self, waited: Duration) {
        let nanos = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        self.total.fetch_add(nanos, Ordering::AcqRel);
    }

    fn total(&self) -> Duration {
        Duration::from_nanos(self.total.load(Ordering::Acquire))
    }

    /// The prompt time of ended questions so far, for [`Paused::since`].
    pub(crate) fn mark(&self) -> Duration {
        self.total()
    }

    /// The prompt time of the questions that ended since `mark` ([`Paused::mark`]).
    pub(crate) fn since(&self, mark: Duration) -> Duration {
        self.total().saturating_sub(mark)
    }

    /// Marks a question open until the guard is dropped, which credits its time.
    fn open(&self) -> OpenQuestion<'_> {
        self.open.fetch_add(1, Ordering::AcqRel);
        OpenQuestion {
            paused: self,
            started: Instant::now(),
        }
    }

    fn asking(&self) -> bool {
        self.open.load(Ordering::Acquire) > usize::MIN
    }

    /// Returns when no question is open.
    async fn questions_ended(&self) {
        loop {
            let ended = self.ended.notified();
            let mut ended = std::pin::pin!(ended);
            ended.as_mut().enable();
            if !self.asking() {
                return;
            }
            ended.await;
        }
    }
}

/// An open question of [`Paused`]; its end (also when the asking future is dropped) credits
/// the time it was open.
struct OpenQuestion<'a> {
    paused: &'a Paused,
    started: Instant,
}

impl Drop for OpenQuestion<'_> {
    fn drop(&mut self) {
        self.paused.add(self.started.elapsed());
        self.paused.open.fetch_sub(1, Ordering::AcqRel);
        self.paused.ended.notify_waiters();
    }
}

/// Returns when `cancel` is cancelled (looked at every `poll`); never without one.
async fn cancelled(cancel: Option<&SshCancel>, poll: Duration) {
    match cancel {
        Some(cancel) => {
            while !cancel.is_cancelled() {
                tokio::time::sleep(poll).await;
            }
        }
        None => std::future::pending().await,
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
    // The question time already spent belongs to the deadlines before this one.
    let mut seen = paused.total();
    let mut credit = |deadline: &mut Instant| {
        let now = paused.total();
        *deadline += now.saturating_sub(seen);
        seen = now;
    };
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
                // Whether a question is open is read before the time is: a question that ends
                // in between is then in the time read, and is not lost.
                let asking = paused.asking();
                credit(&mut deadline);
                if asking && Instant::now() >= deadline {
                    // A question is open: the deadline waits for its answer, whose time is
                    // credited when it ends. The future keeps running meanwhile.
                    let mut ended = std::pin::pin!(paused.questions_ended());
                    let mut stop = std::pin::pin!(cancelled(cancel, poll));
                    let woke = std::future::poll_fn(|cx| {
                        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
                            return Poll::Ready(Some(Bounded::Done(out)));
                        }
                        if ended.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(None);
                        }
                        if stop.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(Some(Bounded::Cancelled));
                        }
                        Poll::Pending
                    })
                    .await;
                    if let Some(end) = woke {
                        return end;
                    }
                    credit(&mut deadline);
                    continue;
                }
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
    let open = paused.open();
    let job = tokio::task::spawn_blocking(move || question(prompt.as_ref()));
    let answer = tokio::time::timeout(timeout, job).await;
    drop(open);
    match answer {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) | Err(_) => None,
    }
}

#[cfg(test)]
mod paused_tests {
    use super::*;

    fn run<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }

    #[test]
    fn an_open_question_holds_the_deadline_until_it_is_answered() {
        let paused = Paused::default();
        let out = run(bounded(
            async {
                let open = paused.open();
                tokio::time::sleep(Duration::from_millis(300)).await;
                drop(open);
                "answered"
            },
            Instant::now() + Duration::from_millis(50),
            &paused,
            None,
            Duration::from_millis(50),
        ));
        assert!(matches!(out, Bounded::Done("answered")));
    }

    #[test]
    fn the_deadline_still_runs_out_without_a_question_and_after_one() {
        let paused = Paused::default();
        // The sleep is made inside the runtime: a tokio timer needs one when it is created.
        let out = run(bounded(
            async { tokio::time::sleep(Duration::from_millis(300)).await },
            Instant::now() + Duration::from_millis(50),
            &paused,
            None,
            Duration::from_millis(50),
        ));
        assert!(matches!(out, Bounded::Expired));
        // The question's 100 ms are credited, and then the rest of the deadline runs out.
        let started = Instant::now();
        let out = run(bounded(
            async {
                let open = paused.open();
                tokio::time::sleep(Duration::from_millis(100)).await;
                drop(open);
                tokio::time::sleep(Duration::from_secs(5)).await;
            },
            Instant::now() + Duration::from_millis(50),
            &paused,
            None,
            Duration::from_millis(50),
        ));
        assert!(matches!(out, Bounded::Expired));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nested_deadlines_each_get_the_question_time() {
        let paused = Paused::default();
        let deadline = || Instant::now() + Duration::from_millis(50);
        // The question outlasts both deadlines, and the work goes on a while after it.
        let out = run(bounded(
            bounded(
                async {
                    let open = paused.open();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    drop(open);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    "answered"
                },
                deadline(),
                &paused,
                None,
                Duration::from_millis(50),
            ),
            deadline(),
            &paused,
            None,
            Duration::from_millis(50),
        ));
        assert!(
            matches!(out, Bounded::Done(Bounded::Done("answered"))),
            "the outer deadline expired"
        );
    }

    #[test]
    fn a_question_that_ends_as_the_deadline_runs_out_is_credited() {
        // The question runs in another task of a runtime with several threads and ends about
        // when the deadline runs out, so its end falls between the deadline's looks at it.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        for round in 0..40u64 {
            let paused = Arc::new(Paused::default());
            let question = Arc::clone(&paused);
            let length = Duration::from_millis(20 + round % 5);
            let out = runtime.block_on(async {
                let asker = tokio::spawn(async move {
                    let _open = question.open();
                    tokio::time::sleep(length).await;
                });
                bounded(
                    async {
                        tokio::time::sleep(length + Duration::from_millis(10)).await;
                        asker.await.is_ok()
                    },
                    Instant::now() + Duration::from_millis(20),
                    &paused,
                    None,
                    Duration::from_millis(20),
                )
                .await
            });
            assert!(matches!(out, Bounded::Done(true)), "round {round}");
        }
    }

    #[test]
    fn an_open_question_does_not_hold_a_cancel() {
        let paused = Paused::default();
        let cancel = SshCancel::new();
        let flag = cancel.clone();
        let out = run(async {
            let stopper = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                flag.cancel();
            };
            let wait = bounded(
                async {
                    let _open = paused.open();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                },
                Instant::now() + Duration::from_millis(20),
                &paused,
                Some(&cancel),
                Duration::from_millis(20),
            );
            let mut stopper = std::pin::pin!(stopper);
            let mut wait = std::pin::pin!(wait);
            let mut stopped = false;
            std::future::poll_fn(|cx| {
                if !stopped {
                    stopped = stopper.as_mut().poll(cx).is_ready();
                }
                wait.as_mut().poll(cx)
            })
            .await
        });
        assert!(matches!(out, Bounded::Cancelled));
    }
}
