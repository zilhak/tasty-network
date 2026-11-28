//! Port discovery over the embedded backend: a `session` channel with `exec` of the same
//! remote command, read with the same classification rules as the system backend
//! (docs/design/ssh-backends.md "내장 백엔드: 세션 하나에 채널 여럿").

use std::time::{Duration, Instant};

use russh::ChannelMsg;

use crate::SshTarget;
use crate::backend::SshBackendError;
use crate::cancel::{SshCancel, current_cancel};
use crate::discovery::{
    Limits, PortDiscoveryError, PortDiscoveryFailureKind, RemoteRun, cancelled_error,
    classify_by_exit_code, refine_with_elapsed, sanitize_stderr,
};

use super::resolve::{Purpose, load, resolve_chain};
use super::session::{
    ChannelTimer, OpenBound, OpenStop, Session, SessionChannel, close_channel,
    open_session_channel, open_within,
};
use super::{Bounded, Ctx, Paused, blocking, bounded, run_blocking};

/// The embedded backend's [`RemoteRun`]: one `exec` channel per command.
pub(crate) struct EmbeddedRun<'a> {
    pub(crate) ctx: Ctx<'a>,
    pub(crate) target: &'a SshTarget,
    /// The effective target of the resolution the last successful run used.
    pub(crate) reached: std::sync::Mutex<Option<crate::EffectiveTarget>>,
}

impl<'a> EmbeddedRun<'a> {
    pub(crate) fn new(ctx: Ctx<'a>, target: &'a SshTarget) -> Self {
        Self {
            ctx,
            target,
            reached: std::sync::Mutex::new(None),
        }
    }
}

/// What one `exec` channel gave.
struct Output {
    stdout: Vec<u8>,
    stdout_over: bool,
    stderr: Vec<u8>,
    stderr_over: bool,
    code: Option<i32>,
}

impl RemoteRun for EmbeddedRun<'_> {
    fn what(&self) -> String {
        "embedded ssh".to_string()
    }

    fn run(
        &self,
        remote_argv: &[&str],
        budget: Duration,
        what: &str,
        limits: Limits,
    ) -> Result<String, PortDiscoveryError> {
        let cancel = current_cancel();
        if cancel.as_ref().is_some_and(SshCancel::is_cancelled) {
            return Err(cancelled_error(what));
        }
        if budget.is_zero() {
            return Err(PortDiscoveryError::new(
                PortDiscoveryFailureKind::TimedOut,
                format!("{what}: the discovery budget is used up; no channel is opened"),
            ));
        }
        let started = Instant::now();
        // OpenSSH joins the words with spaces into one command string.
        let command = remote_argv.join(" ");
        let connection_failed = |e: SshBackendError| {
            let kind = refine_with_elapsed(
                PortDiscoveryFailureKind::SshConnectionFailed,
                started.elapsed(),
                limits.connect_timeout,
            );
            PortDiscoveryError::new(kind, format!("{what}: {e}"))
        };
        // A refused resolution (this purpose and command only, such as `Match command`) keeps
        // its structure, so the caller sees the refused directive and does not retry.
        let failed = |e: SshBackendError| match e {
            SshBackendError::ConfigRefused { .. } => PortDiscoveryError::refused(what, e),
            other => connection_failed(other),
        };
        // The configuration files are read on a blocking thread under the budget and the
        // cancel: a file that does not answer stops neither, and holds no runtime worker.
        let runtime = self.ctx.shared.runtime(self.ctx.settings).map_err(failed)?;
        let (settings, target, verify) = (
            self.ctx.settings.clone(),
            self.target.clone(),
            self.ctx.verify,
        );
        let line = command.clone();
        let (deadline, poll) = (started + budget, limits.poll_max);
        let watch = cancel.clone();
        let resolved = run_blocking(&runtime, async move {
            Ok(bounded(
                blocking(move || {
                    let set = load(&settings)?;
                    resolve_chain(&set, &settings, &target, Purpose::Discovery(&line), verify)
                }),
                deadline,
                &Paused::default(),
                watch.as_ref(),
                poll,
            )
            .await)
        })
        .map_err(connection_failed)?;
        let chain = match resolved {
            Bounded::Done(read) => read.and_then(|chain| chain).map_err(failed)?,
            Bounded::Cancelled => return Err(cancelled_error(what)),
            Bounded::Expired => {
                return Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::TimedOut,
                    format!("{what}: the SSH configuration was not read within {budget:?}"),
                ));
            }
        };
        // Connecting, the host key check and authentication (agent answers included) stop at
        // the discovery budget and the cancel too.
        let bound = OpenBound {
            deadline,
            cancel: cancel.clone(),
            poll,
        };
        let session = match open_within(self.ctx, &chain, Some(bound)) {
            Ok(session) => session,
            Err(OpenStop::Failed(e)) => return Err(failed(e)),
            Err(OpenStop::Cancelled) => return Err(cancelled_error(what)),
            Err(OpenStop::Expired) => {
                return Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::TimedOut,
                    format!("{what}: the session did not open within {budget:?}"),
                ));
            }
        };
        if cancel.as_ref().is_some_and(SshCancel::is_cancelled) {
            return Err(cancelled_error(what));
        }
        let left = budget.saturating_sub(started.elapsed());
        let env = channel_env(&chain.dest);
        let out = run_blocking(
            &session.runtime,
            exec(
                std::sync::Arc::clone(&session),
                command,
                env,
                left,
                cancel,
                limits,
                self.ctx.settings.channel_open_timeout(),
            ),
        );
        let out = match out {
            Ok(Bounded::Done(out)) => out,
            Ok(Bounded::Cancelled) => return Err(cancelled_error(what)),
            Ok(Bounded::Expired) => {
                return Err(PortDiscoveryError::new(
                    PortDiscoveryFailureKind::TimedOut,
                    format!("{what}: no answer within {budget:?}"),
                ));
            }
            Err(e) => return Err(connection_failed(e)),
        };
        if out.stdout_over && out.code.is_none_or(|c| c == 0) {
            return Err(PortDiscoveryError::new(
                PortDiscoveryFailureKind::PortParseFailed,
                format!(
                    "{what}: the output is longer than {} bytes",
                    limits.output_limit
                ),
            ));
        }
        if out.code != Some(0) {
            let elapsed = started.elapsed();
            let stderr = sanitize_stderr(
                &String::from_utf8_lossy(&out.stderr),
                out.stderr_over,
                limits.stderr_detail_limit,
            );
            let mut error = PortDiscoveryError::new(
                refine_with_elapsed(
                    classify_by_exit_code(out.code),
                    elapsed,
                    limits.connect_timeout,
                ),
                format!("{what}: code={:?} elapsed={elapsed:?}: {stderr}", out.code),
            );
            error.exit_code = out.code;
            return Err(error);
        }
        // The target this run reached, so the tunnel is compared with it and not with a
        // resolution read again later.
        *self
            .reached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(chain.effective());
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn last_target(&self) -> Option<crate::EffectiveTarget> {
        self.reached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The `env` requests of a channel: `SetEnv` values, then the local values `SendEnv` names.
fn channel_env(res: &tasty_net_ssh_config::Resolution) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = res
        .options
        .setenv
        .iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    for pattern in &res.options.send_env {
        let (negate, pattern) = match pattern.strip_prefix('-') {
            Some(p) => (true, p),
            None => (false, pattern.as_str()),
        };
        if negate {
            env.retain(|(k, _)| !glob(pattern, k));
            continue;
        }
        for (k, v) in std::env::vars() {
            if glob(pattern, &k) && !env.iter().any(|(e, _)| *e == k) {
                env.push((k, v));
            }
        }
    }
    env
}

/// `match_pattern` of `*` and `?` (the `SendEnv` patterns).
fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.split_first(), n.split_first()) {
            (None, None) => true,
            (Some((b'*', rest)), _) => {
                go(rest, n) || n.split_first().is_some_and(|(_, t)| go(p, t))
            }
            (Some((b'?', rest)), Some((_, t))) => go(rest, t),
            (Some((a, rest)), Some((b, t))) if a == b => go(rest, t),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// Runs `command` on a new `session` channel and reads it to its end within `budget`, stopping
/// on cancellation.
async fn exec(
    session: std::sync::Arc<Session>,
    command: String,
    env: Vec<(String, String)>,
    budget: Duration,
    cancel: Option<SshCancel>,
    limits: Limits,
    open_timeout: Duration,
) -> Result<Bounded<Output>, SshBackendError> {
    let paused = Paused::default();
    let deadline = Instant::now() + budget;
    // `ChannelTimeout` is not applied yet (shown as `not_supported` by the resolution).
    let opened = open_session_channel(
        std::sync::Arc::clone(&session.conn.handle),
        ChannelTimer::NONE,
        open_timeout,
        deadline,
        &paused,
        cancel.as_ref(),
        limits.poll_max,
    )
    .await?;
    let SessionChannel { mut channel, timer } = match opened {
        Bounded::Done(opened) => opened,
        Bounded::Expired => return Ok(Bounded::Expired),
        Bounded::Cancelled => return Ok(Bounded::Cancelled),
    };
    // The `env` and `exec` requests, then the output, all under the deadline and the cancel.
    let body = async {
        for (k, v) in env {
            channel
                .set_env(false, k, v)
                .await
                .map_err(|e| SshBackendError::Session {
                    detail: format!("env request: {e}"),
                })?;
        }
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshBackendError::Session {
                detail: format!("exec request: {e}"),
            })?;
        let mut out = Output {
            stdout: Vec::new(),
            stdout_over: false,
            stderr: Vec::new(),
            stderr_over: false,
            code: None,
        };
        let keep = |buf: &mut Vec<u8>, over: &mut bool, data: &[u8], limit: usize| {
            let room = limit.saturating_sub(buf.len());
            buf.extend_from_slice(&data[..data.len().min(room)]);
            if data.len() > room {
                *over = true;
            }
        };
        while let Some(msg) = timer.wait(&mut channel).await {
            match msg {
                ChannelMsg::Data { data } => {
                    keep(
                        &mut out.stdout,
                        &mut out.stdout_over,
                        &data,
                        limits.output_limit,
                    );
                    if out.stdout_over {
                        break;
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    keep(
                        &mut out.stderr,
                        &mut out.stderr_over,
                        &data,
                        limits.stderr_limit,
                    );
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    out.code = i32::try_from(exit_status).ok();
                }
                ChannelMsg::Failure => break,
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(out)
    };
    let done = bounded(body, deadline, &paused, cancel.as_ref(), limits.poll_max).await;
    // Every end (the output's end, an error, a cancel, the deadline, the output limit) asks
    // the server to close the channel.
    close_channel(&channel, open_timeout).await;
    match done {
        Bounded::Done(Ok(out)) => Ok(Bounded::Done(out)),
        Bounded::Done(Err(e)) => Err(e),
        Bounded::Expired => Ok(Bounded::Expired),
        Bounded::Cancelled => Ok(Bounded::Cancelled),
    }
}
