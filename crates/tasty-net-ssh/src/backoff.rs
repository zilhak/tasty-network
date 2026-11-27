//! The reconnect wait both backends share.

use std::time::Duration;

use crate::settings::SshSettings;

/// Reconnect wait that grows by a factor up to a maximum. [`Backoff::reset`] returns to the
/// minimum after a success. When to reconnect is the consumer's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backoff {
    cur: Duration,
    min: Duration,
    max: Duration,
    factor: u32,
}

impl Backoff {
    /// The default settings: starts at 500 ms, doubles, at most 30 s.
    pub fn new() -> Self {
        Self::from_settings(&SshSettings::default())
    }

    /// Starts at `backoff.initial`, multiplies by `backoff.multiplier`, at most
    /// `backoff.max_delay`. The cap holds for every wait: a first wait above it starts (and
    /// resets) at the cap.
    pub fn from_settings(settings: &SshSettings) -> Self {
        let max = settings.backoff_max_delay();
        let min = settings.backoff_initial().min(max);
        Self {
            cur: min,
            min,
            max,
            factor: settings.backoff_multiplier(),
        }
    }

    /// Sleeps for the current interval, then multiplies it by the factor (up to the maximum).
    pub fn sleep(&mut self) {
        std::thread::sleep(self.cur);
        self.advance();
    }

    /// The current interval, without sleeping. A scheduler that cannot block (a GUI main loop)
    /// uses it to compute the next retry time.
    pub fn current(&self) -> Duration {
        self.cur
    }

    /// Moves to the next interval without sleeping (multiplies by the factor, up to the
    /// maximum). [`Backoff::sleep`] is this plus a blocking sleep.
    pub fn advance(&mut self) {
        self.cur = self
            .cur
            .checked_mul(self.factor)
            .unwrap_or(self.max)
            .min(self.max);
    }

    /// Returns to the minimum after a successful connection.
    pub fn reset(&mut self) {
        self.cur = self.min;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}
