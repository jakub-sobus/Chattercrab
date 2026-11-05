//! Exponential reconnect backoff: 1 s, 2 s, 4 s, 8 s, 16 s, then 30 s forever.

use std::time::Duration;

/// First retry delay.
pub const INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Upper bound for the retry delay.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// Reconnect delay generator. Call [`Backoff::reset`] after a successful join.
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// Creates a fresh backoff (next delay is [`INITIAL_DELAY`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of delays handed out since the last reset.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the next delay and advances the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let factor = 1u64.checked_shl(self.attempt.min(16)).unwrap_or(u64::MAX);
        let delay = INITIAL_DELAY
            .checked_mul(u32::try_from(factor).unwrap_or(u32::MAX))
            .unwrap_or(MAX_DELAY)
            .min(MAX_DELAY);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Restarts the schedule.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_doubles_then_caps() {
        let mut b = Backoff::new();
        let secs: Vec<u64> = (0..9).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30, 30]);
        assert_eq!(b.attempt(), 9);
    }

    #[test]
    fn reset_restarts() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), INITIAL_DELAY);
    }

    #[test]
    fn never_overflows() {
        let mut b = Backoff::new();
        for _ in 0..1000 {
            assert!(b.next_delay() <= MAX_DELAY);
        }
    }
}
