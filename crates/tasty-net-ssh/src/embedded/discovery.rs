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
use super::session::{Session, open};
use super::{Bounded, Ctx, Paused, bounded, run_blocking};

/// The embedded backend's [`RemoteRun`]: one `exec` channel per command.
pub(crate) struct EmbeddedRun<'a> {
    pub(crate) ctx: Ctx<'a>,
    pub(crate) target: &'a SshTarget,
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
        let set = load(self.ctx.settings).map_err(connection_failed)?;
        let chain = resolve_chain(
            &set,
            self.ctx.settings,
            self.target,
            Purpose::Discovery(&command),
            self.ctx.verify,
        )
        .map_err(connection_failed)?;
        let session = open(self.ctx, &chain).map_err(connection_failed)?;
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
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
    let handle = std::sync::Arc::clone(&session.conn.handle);
    let opened = bounded(
        tokio::time::timeout(open_timeout, handle.channel_open_session()),
        deadline,
        &paused,
        cancel.as_ref(),
        limits.poll_max,
    )
    .await;
    let mut channel = match opened {
        Bounded::Done(Ok(Ok(channel))) => channel,
        Bounded::Done(Ok(Err(e))) => {
            return Err(SshBackendError::Session {
                detail: format!("the exec channel did not open: {e}"),
            });
        }
        Bounded::Done(Err(_)) => {
            return Err(SshBackendError::Session {
                detail: format!("the exec channel did not open in {open_timeout:?}"),
            });
        }
        Bounded::Expired => return Ok(Bounded::Expired),
        Bounded::Cancelled => return Ok(Bounded::Cancelled),
    };
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
    let read = async {
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
        while let Some(msg) = channel.wait().await {
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
        out
    };
    let done = bounded(read, deadline, &paused, cancel.as_ref(), limits.poll_max).await;
    // The channel closes when it drops; a server that still writes sees it closed.
    drop(channel);
    Ok(done)
}
