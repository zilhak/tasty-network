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
    /// [0, max_delay]. A zero `initial` gives zero; a product past `max_delay` stops there
    /// without being computed, and a `u` that is not a number counts as 0.
    pub fn wait_before(&self, attempt: u64, u: f64) -> Duration {
        let max = self.max_delay().as_secs_f64();
        let initial = self.initial().as_secs_f64();
        let steps = attempt.saturating_sub(1) as f64;
        // multiplier ≥ 1 and finite, so its logarithm is finite and not negative.
        let base = if initial == 0.0 {
            0.0
        } else if initial.ln() + steps * self.multiplier().ln() >= max.ln() {
            max
        } else {
            (initial * self.multiplier().powf(steps)).min(max)
        };
        let u = if u.is_nan() { 0.0 } else { u.clamp(-1.0, 1.0) };
        let wait = (base * (1.0 + u * self.jitter())).clamp(0.0, max);
        Duration::try_from_secs_f64(wait)
            .map_or_else(|_| self.max_delay(), |w| w.min(self.max_delay()))
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
#[derive(Clone)]
pub struct Redialer {
    table: ConnectionTable,
    config: RedialerConfig,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Redialer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redialer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Where the redialer reads the time and waits; the system clock outside the unit tests.
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    /// Waits `wait`, or until `cancel` stops the attempt.
    fn sleep(&self, wait: Duration, cancel: &AttemptCancel) -> Result<(), Cancelled>;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, wait: Duration, cancel: &AttemptCancel) -> Result<(), Cancelled> {
        sleep(wait, cancel)
    }
}

impl Redialer {
    /// A redialer over `table` with `config`. A multiplier below 1 or not finite, a jitter
    /// outside [0, 1], or a time too long to add to the present instant is refused.
    pub fn new(table: &ConnectionTable, config: RedialerConfig) -> Result<Self, InvalidSetting> {
        Self::with_clock(table, config, Arc::new(SystemClock))
    }

    fn with_clock(
        table: &ConnectionTable,
        config: RedialerConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, InvalidSetting> {
        check_redial(
            config.multiplier(),
            config.jitter(),
            [
                (REDIAL_INITIAL, config.initial()),
                (REDIAL_MAX_DELAY, config.max_delay()),
                (REDIAL_GIVE_UP_AFTER, config.give_up_after()),
            ],
        )?;
        Ok(Self {
            table: table.clone(),
            config,
            clock,
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
        let clock = &*self.clock;
        let lost_entry = self.table.connection(lost);
        let closed_here = lost_entry
            .as_ref()
            .is_some_and(|e| e.last_cause == Some(Cause::ClosedHere));
        if closed_here || config.give_up_after().is_zero() {
            return Err(RedialError::NotRedialed);
        }
        let started = clock.now();
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
            // The lost entry's path until the attempt's options or its first link say.
            path_fixed: false,
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
            if !fits(config, clock.since(started), wait) {
                return Err(exhausted(&pending, last));
            }
            progress(&pending, clock, number, Some(wait));
            if clock.sleep(wait, cancel).is_err() {
                pending.hold(false, None);
                pending.transition(ConnectionState::Disconnected, None, Some(Cause::Cancelled));
                return Err(RedialError::Cancelled);
            }
            progress(&pending, clock, number, None);
            // A sleep that woke late may already be past the limit.
            if clock.since(started) > config.give_up_after() {
                return Err(exhausted(&pending, last));
            }
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
                    if (bounded != 0 && number > bounded)
                        || !fits(config, clock.since(started), wait)
                    {
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

impl dyn Clock + '_ {
    /// The time since `started`.
    fn since(&self, started: Instant) -> Duration {
        self.now().saturating_duration_since(started)
    }
}

/// Whether a wait of `wait` after `elapsed` still ends within `redial.give_up_after`.
fn fits(config: &RedialerConfig, elapsed: Duration, wait: Duration) -> bool {
    elapsed
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
fn progress(pending: &EntryRef, clock: &dyn Clock, number: u64, wait: Option<Duration>) {
    pending.hold(true, wait.and_then(|w| clock.now().checked_add(w)));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that moves only when the redialer sleeps (by the wait plus `late`) or when an
    /// attempt says so.
    struct FakeClock {
        now: Mutex<Instant>,
        late: Duration,
        waits: Mutex<Vec<Duration>>,
    }

    impl FakeClock {
        fn new(late: Duration) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Instant::now()),
                late,
                waits: Mutex::new(Vec::new()),
            })
        }

        fn advance(&self, by: Duration) {
            let mut now = lock(&self.now);
            *now += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *lock(&self.now)
        }

        fn sleep(&self, wait: Duration, cancel: &AttemptCancel) -> Result<(), Cancelled> {
            let _registration = cancel.register(Arc::new(Wake::default()))?;
            lock(&self.waits).push(wait);
            self.advance(wait + self.late);
            Ok(())
        }
    }

    const LOST: ConnectionId = ConnectionId(1000);

    /// Redials `LOST` on a fake clock with attempts that take `takes` and time out; gives the
    /// result, the attempt count, the waits and the table.
    fn fake_redial(
        config: RedialerConfig,
        late: Duration,
        takes: Duration,
    ) -> (Result<(), RedialError>, u64, Vec<Duration>, ConnectionTable) {
        let table = ConnectionTable::default();
        let clock = FakeClock::new(late);
        let redialer = Redialer::with_clock(&table, config, Arc::clone(&clock) as Arc<dyn Clock>)
            .expect("valid settings");
        let mut attempts = 0;
        let result = redialer.redial(LOST, &AttemptCancel::new(), |_| {
            attempts += 1;
            clock.advance(takes);
            Err::<(), _>(HandshakeError::TimedOut)
        });
        let waits = lock(&clock.waits).clone();
        (result, attempts, waits, table)
    }

    fn last_cause(table: &ConnectionTable) -> Option<Cause> {
        table.connections().pop().and_then(|e| e.last_cause)
    }

    #[test]
    fn attempts_run_out_after_max_attempts_with_the_set_waits() {
        let ms = Duration::from_millis;
        let config = RedialerConfig::new()
            .with_initial(ms(40))
            .with_multiplier(2.0)
            .with_max_delay(ms(100))
            .with_jitter(0.0)
            .with_max_attempts(4);
        let (result, attempts, waits, table) = fake_redial(config, Duration::ZERO, ms(5));
        assert!(
            matches!(
                result,
                Err(RedialError::Exhausted(Some(HandshakeError::TimedOut)))
            ),
            "{result:?}"
        );
        assert_eq!(attempts, 4);
        assert_eq!(waits, [ms(40), ms(80), ms(100), ms(100)]);
        assert_eq!(last_cause(&table), Some(Cause::RedialExhausted));
    }

    #[test]
    fn attempts_stop_when_the_next_wait_ends_past_give_up_after() {
        let ms = Duration::from_millis;
        // Waits of 100 ms end at 100, 200 and 300 ms; a fourth would end at 400, past 350.
        let config = RedialerConfig::new()
            .with_initial(ms(100))
            .with_multiplier(1.0)
            .with_jitter(0.0)
            .with_give_up_after(ms(350));
        let (result, attempts, waits, table) = fake_redial(config.clone(), Duration::ZERO, ms(0));
        assert!(
            matches!(result, Err(RedialError::Exhausted(_))),
            "{result:?}"
        );
        assert_eq!(attempts, 3);
        assert_eq!(waits.len(), 3);
        assert_eq!(last_cause(&table), Some(Cause::RedialExhausted));

        // Attempts that take 60 ms each: the third wait would end at 100 + 60 + 100 + 60 +
        // 100 = 420, past 350, so two attempts.
        let (_, attempts, _, _) = fake_redial(config, Duration::ZERO, ms(60));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_late_wake_past_give_up_after_makes_no_attempt() {
        let ms = Duration::from_millis;
        // The first wait fits (100 ≤ 350) but the sleep wakes at 400.
        let config = RedialerConfig::new()
            .with_initial(ms(100))
            .with_jitter(0.0)
            .with_give_up_after(ms(350));
        let (result, attempts, waits, table) = fake_redial(config, ms(300), Duration::ZERO);
        assert!(
            matches!(result, Err(RedialError::Exhausted(None))),
            "{result:?}"
        );
        assert_eq!((attempts, waits.len()), (0, 1));
        assert_eq!(last_cause(&table), Some(Cause::RedialExhausted));
    }

    #[test]
    fn waits_stay_finite_at_the_bounds() {
        let ms = Duration::from_millis;
        let defaults = RedialerConfig::new();
        // 2^1024 does not fit in a float: the wait stops at max_delay.
        assert_eq!(defaults.wait_before(1025, 0.0), defaults.max_delay());
        assert_eq!(defaults.wait_before(u64::MAX, 1.0), defaults.max_delay());
        assert_eq!(
            defaults.wait_before(0, 0.0),
            defaults.initial(),
            "as attempt 1"
        );
        // A zero first wait stays zero however many attempts, not 0 × ∞.
        let zero = RedialerConfig::new().with_initial(Duration::ZERO);
        assert_eq!(zero.wait_before(1025, 0.0), Duration::ZERO);
        assert_eq!(zero.wait_before(u64::MAX, 1.0), Duration::ZERO);
        // A zero max_delay caps every wait at zero.
        let capped = RedialerConfig::new().with_max_delay(Duration::ZERO);
        assert_eq!(capped.wait_before(1, 1.0), Duration::ZERO);
        assert_eq!(capped.wait_before(1025, 0.0), Duration::ZERO);
        // A random value that is not a number counts as 0.
        let exact = RedialerConfig::new().with_initial(ms(40)).with_jitter(0.5);
        assert_eq!(exact.wait_before(1, f64::NAN), ms(40));
        // The largest multiplier the settings allow.
        let steep = RedialerConfig::new()
            .with_multiplier(f64::MAX)
            .with_jitter(0.5);
        assert_eq!(steep.wait_before(2, 0.0), steep.max_delay());
        assert_eq!(steep.wait_before(u64::MAX, -1.0), steep.max_delay() / 2);
    }

    #[test]
    fn times_that_cannot_be_counted_from_now_are_refused() {
        let table = ConnectionTable::default();
        let cases = [
            (
                RedialerConfig::new().with_initial(Duration::MAX),
                REDIAL_INITIAL,
            ),
            (
                RedialerConfig::new().with_max_delay(Duration::MAX),
                REDIAL_MAX_DELAY,
            ),
            (
                RedialerConfig::new().with_give_up_after(Duration::MAX),
                REDIAL_GIVE_UP_AFTER,
            ),
        ];
        for (config, key) in cases {
            match Redialer::new(&table, config) {
                Err(error) => assert_eq!(error.key, key),
                Ok(_) => panic!("{key}: Duration::MAX was accepted"),
            }
        }
        // A long but countable limit is kept.
        let year = Duration::from_secs(365 * 24 * 60 * 60);
        assert!(Redialer::new(&table, RedialerConfig::new().with_give_up_after(year)).is_ok());
    }
}
