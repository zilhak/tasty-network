//! The redialer: connecting again after a loss with a backoff the settings give
//! (docs/design/transport.md "보관 상한과 재연결 도우미").

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cancel::{AttemptCancel, Cancellable, Cancelled};
use crate::connection_table::{
    Cause, ConnectionId, ConnectionPeer, ConnectionState, ConnectionTable, EntryRef, NewEntry,
    PathKind, Phase, RedialProgress, Via,
};
use crate::hex::random_unit;
use crate::lock::lock;
use crate::reject::HandshakeError;
use crate::role::Side;
use crate::settings::{
    InvalidSetting, REDIAL_GIVE_UP_AFTER, REDIAL_INITIAL, REDIAL_JITTER, REDIAL_MAX_ATTEMPTS,
    REDIAL_MAX_DELAY, REDIAL_MULTIPLIER, Setting, SettingValue, check_redial, default_count,
    default_duration, default_ratio, shown,
};
use crate::transport::ConnectOptions;

/// The redialer's values. A value not given is the default of
/// [`settings::DEFAULTS`](crate::settings::DEFAULTS).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RedialerConfig {
    initial: Option<Duration>,
    multiplier: Option<f64>,
    max_delay: Option<Duration>,
    jitter: Option<f64>,
    give_up_after: Option<Duration>,
    max_attempts: Option<u64>,
}

impl RedialerConfig {
    /// Every value at its default.
    pub fn new() -> Self {
        Self::default()
    }

    /// `redial.initial`: the first wait.
    pub fn with_initial(mut self, wait: Duration) -> Self {
        self.initial = Some(wait);
        self
    }

    /// `redial.multiplier`: the factor between two waits (finite, at least 1).
    pub fn with_multiplier(mut self, factor: f64) -> Self {
        self.multiplier = Some(factor);
        self
    }

    /// `redial.max_delay`: the longest wait.
    pub fn with_max_delay(mut self, wait: Duration) -> Self {
        self.max_delay = Some(wait);
        self
    }

    /// `redial.jitter`: the random width of a wait, a fraction in [0, 1].
    pub fn with_jitter(mut self, fraction: f64) -> Self {
        self.jitter = Some(fraction);
        self
    }

    /// `redial.give_up_after`: how long to try after the loss; zero turns redialing off.
    pub fn with_give_up_after(mut self, limit: Duration) -> Self {
        self.give_up_after = Some(limit);
        self
    }

    /// `redial.max_attempts`: the most attempts; zero is no bound.
    pub fn with_max_attempts(mut self, attempts: u64) -> Self {
        self.max_attempts = Some(attempts);
        self
    }

    /// The `redial.initial` in use.
    pub fn initial(&self) -> Duration {
        self.initial
            .unwrap_or_else(|| default_duration(REDIAL_INITIAL))
    }

    /// The `redial.multiplier` in use.
    pub fn multiplier(&self) -> f64 {
        self.multiplier
            .unwrap_or_else(|| default_ratio(REDIAL_MULTIPLIER))
    }

    /// The `redial.max_delay` in use.
    pub fn max_delay(&self) -> Duration {
        self.max_delay
            .unwrap_or_else(|| default_duration(REDIAL_MAX_DELAY))
    }

    /// The `redial.jitter` in use.
    pub fn jitter(&self) -> f64 {
        self.jitter.unwrap_or_else(|| default_ratio(REDIAL_JITTER))
    }

    /// The `redial.give_up_after` in use.
    pub fn give_up_after(&self) -> Duration {
        self.give_up_after
            .unwrap_or_else(|| default_duration(REDIAL_GIVE_UP_AFTER))
    }

    /// The `redial.max_attempts` in use.
    pub fn max_attempts(&self) -> u64 {
        self.max_attempts
            .unwrap_or_else(|| default_count(REDIAL_MAX_ATTEMPTS))
    }

    /// Every value with its source, in the order of the design table.
    pub fn settings(&self) -> Vec<Setting> {
        vec![
            shown(REDIAL_INITIAL, self.initial.map(SettingValue::Duration)),
            shown(REDIAL_MULTIPLIER, self.multiplier.map(SettingValue::Ratio)),
            shown(REDIAL_MAX_DELAY, self.max_delay.map(SettingValue::Duration)),
            shown(REDIAL_JITTER, self.jitter.map(SettingValue::Ratio)),
            shown(
                REDIAL_GIVE_UP_AFTER,
                self.give_up_after.map(SettingValue::Duration),
            ),
            shown(
                REDIAL_MAX_ATTEMPTS,
                self.max_attempts.map(SettingValue::Count),
            ),
        ]
    }

    /// The wait before attempt `attempt` (1 for the first) with the random value `u` in
    /// [−1, 1]: min(max_delay, initial × multiplier^(attempt − 1)) × (1 + u × jitter), cut to
    /// [0, max_delay].
    pub fn wait_before(&self, attempt: u64, u: f64) -> Duration {
        let max = self.max_delay().as_secs_f64();
        let steps = attempt.saturating_sub(1) as f64;
        let base = (self.initial().as_secs_f64() * self.multiplier().powf(steps)).min(max);
        let wait = (base * (1.0 + u.clamp(-1.0, 1.0) * self.jitter())).clamp(0.0, max);
        Duration::try_from_secs_f64(wait).unwrap_or_else(|_| self.max_delay())
    }
}

/// Why [`Redialer::redial`] gave no connection.
#[derive(Debug)]
#[non_exhaustive]
pub enum RedialError {
    /// The loss is not one to redial: this side closed the entry, or `redial.give_up_after`
    /// is zero. No entry was made.
    NotRedialed,
    /// An attempt failed in a way connecting again does not change
    /// ([`HandshakeError::is_retryable`]).
    Failed(HandshakeError),
    /// `redial.give_up_after` or `redial.max_attempts` ran out; the last attempt's error, if
    /// one ran.
    Exhausted(Option<HandshakeError>),
    /// The attempt was cancelled while waiting.
    Cancelled,
}

impl fmt::Display for RedialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRedialed => f.write_str("the loss is not one to redial"),
            Self::Failed(e) => write!(f, "redialing failed: {e}"),
            Self::Exhausted(_) => f.write_str("redialing gave up"),
            Self::Cancelled => f.write_str("redialing was cancelled"),
        }
    }
}

impl std::error::Error for RedialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(e) | Self::Exhausted(Some(e)) => Some(e),
            _ => None,
        }
    }
}

/// Connects again after a loss, with the waits of a [`RedialerConfig`].
///
/// [`Redialer::redial`] makes one `reconnecting` entry naming the lost one, waits, and calls
/// the consumer's attempt with [`ConnectOptions`] that make the transport fill that entry.
/// Between attempts the entry stays `reconnecting` in the `retry_wait` phase with the attempt
/// number and the time left. Each call starts again from the first wait, so a connection
/// that came up resets the backoff. When to call it stays the consumer's decision.
#[derive(Clone, Debug)]
pub struct Redialer {
    table: ConnectionTable,
    config: RedialerConfig,
}

impl Redialer {
    /// A redialer over `table` with `config`. A multiplier below 1 or not finite, or a
    /// jitter outside [0, 1], is refused.
    pub fn new(table: &ConnectionTable, config: RedialerConfig) -> Result<Self, InvalidSetting> {
        check_redial(config.multiplier(), config.jitter())?;
        Ok(Self {
            table: table.clone(),
            config,
        })
    }

    /// The values in use with their sources.
    pub fn settings(&self) -> Vec<Setting> {
        self.config.settings()
    }

    /// The values.
    pub fn config(&self) -> &RedialerConfig {
        &self.config
    }

    /// Redials the connection of the entry `lost` with `attempt` (which connects with the
    /// options it is given and runs the handshake) until an attempt succeeds, one fails with
    /// an error that is not retryable, the settings run out, or `cancel` stops it.
    pub fn redial<T>(
        &self,
        lost: ConnectionId,
        cancel: &AttemptCancel,
        mut attempt: impl FnMut(&ConnectOptions) -> Result<T, HandshakeError>,
    ) -> Result<T, RedialError> {
        let config = &self.config;
        let lost_entry = self.table.connection(lost);
        let closed_here = lost_entry
            .as_ref()
            .is_some_and(|e| e.last_cause == Some(Cause::ClosedHere));
        if closed_here || config.give_up_after().is_zero() {
            return Err(RedialError::NotRedialed);
        }
        let started = Instant::now();
        let pending = self.table.open(NewEntry {
            previous: Some(lost),
            parent: None,
            peer: lost_entry
                .as_ref()
                .map(|e| ConnectionPeer {
                    name: e.peer.name.clone(),
                    ..ConnectionPeer::default()
                })
                .unwrap_or_default(),
            path: lost_entry.as_ref().map_or(PathKind::SshForward, |e| e.path),
            local_side: lost_entry.as_ref().map_or(Side::Dialer, |e| e.local_side),
            segments: Vec::new(),
            via: lost_entry.as_ref().map_or(Via::Loopback, |e| e.via.clone()),
            state: ConnectionState::Reconnecting,
            phase: Some(Phase::RetryWait),
            held: true,
            path_fixed: true,
        });
        let applied = config.settings();
        pending.update(|info| info.applied = applied);
        let options = ConnectOptions {
            previous: Some(lost),
            name: lost_entry.as_ref().and_then(|e| e.peer.name.clone()),
            holder: None,
            pending: Some(pending.clone()),
        };
        let mut number: u64 = 1;
        let mut wait = config.wait_before(number, jitter_unit());
        let mut last = None;
        loop {
            if !fits(config, started, wait) {
                return Err(exhausted(&pending, last));
            }
            progress(&pending, number, Some(wait));
            if sleep(wait, cancel).is_err() {
                pending.hold(false, None);
                pending.transition(ConnectionState::Disconnected, None, Some(Cause::Cancelled));
                return Err(RedialError::Cancelled);
            }
            progress(&pending, number, None);
            match attempt(&options) {
                Ok(connected) => {
                    pending.hold(false, None);
                    // An attempt that did not report in this table leaves no entry behind.
                    if matches!(pending.status(), Some((ConnectionState::Reconnecting, _))) {
                        pending.remove();
                    }
                    return Ok(connected);
                }
                Err(error) if !error.is_retryable() => {
                    pending.hold(false, None);
                    pending.transition(
                        ConnectionState::Disconnected,
                        None,
                        Some(Cause::of_handshake(&error)),
                    );
                    return Err(RedialError::Failed(error));
                }
                Err(error) => {
                    let cause = Cause::of_handshake(&error);
                    last = Some(error);
                    number += 1;
                    wait = config.wait_before(number, jitter_unit());
                    let bounded = config.max_attempts();
                    if (bounded != 0 && number > bounded) || !fits(config, started, wait) {
                        return Err(exhausted(&pending, last));
                    }
                    pending.transition(
                        ConnectionState::Reconnecting,
                        Some(Phase::RetryWait),
                        Some(cause),
                    );
                }
            }
        }
    }
}

/// Whether a wait of `wait` from now still ends within `redial.give_up_after` of `started`.
fn fits(config: &RedialerConfig, started: Instant, wait: Duration) -> bool {
    started
        .elapsed()
        .checked_add(wait)
        .is_some_and(|at| at <= config.give_up_after())
}

fn exhausted(pending: &EntryRef, last: Option<HandshakeError>) -> RedialError {
    pending.hold(false, None);
    pending.transition(
        ConnectionState::Disconnected,
        None,
        Some(Cause::RedialExhausted),
    );
    RedialError::Exhausted(last)
}

/// Shows attempt `number` on the entry, and the wait before it if it is waiting.
fn progress(pending: &EntryRef, number: u64, wait: Option<Duration>) {
    pending.hold(true, wait.and_then(|w| Instant::now().checked_add(w)));
    pending.update(|info| {
        info.redial = Some(RedialProgress {
            attempt: number,
            next_attempt_in: wait,
        });
    });
}

/// A uniform value in [−1, 1]; without the OS random source, no jitter.
fn jitter_unit() -> f64 {
    match random_unit() {
        Ok(unit) => unit * 2.0 - 1.0,
        Err(error) => {
            tracing::debug!(%error, "no random value for the redial jitter");
            0.0
        }
    }
}

/// Wakes a sleeping redialer when the attempt is cancelled.
#[derive(Default)]
struct Wake {
    woken: Mutex<bool>,
    cv: Condvar,
}

impl Cancellable for Wake {
    fn cancel(&self) {
        *lock(&self.woken) = true;
        self.cv.notify_all();
    }
}

/// Sleeps `wait`, or until `cancel` stops the attempt.
fn sleep(wait: Duration, cancel: &AttemptCancel) -> Result<(), Cancelled> {
    let wake = Arc::new(Wake::default());
    let _registration = cancel.register(Arc::clone(&wake) as Arc<dyn Cancellable>)?;
    let Some(deadline) = Instant::now().checked_add(wait) else {
        return Ok(());
    };
    let mut woken = lock(&wake.woken);
    while !*woken {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        woken = match wake.cv.wait_timeout(woken, deadline - now) {
            Ok((guard, _)) => guard,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
    Err(Cancelled)
}
