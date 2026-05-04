//! Exponential backoff with full jitter for the reconnect loop.
//!
//! Algorithm: `delay = rand_uniform(0, base * 2^n)` clamped at `cap`. "Full
//! jitter" (vs. equal jitter) gives the tightest distribution of retry
//! attempts under correlated failures — if a hundred clients all reconnect
//! at once, full jitter spreads them across `[0, cap]` instead of bunching
//! near `cap/2`. Reference: AWS Architecture Blog, "Exponential Backoff
//! And Jitter" (Brooker 2015).

use rand::Rng as _;
use std::time::Duration;

/// Configuration for the reconnect-backoff schedule.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    /// Doubling stops at this attempt count to avoid pathological
    /// `2^n` overflow on very long-running clients. Beyond it the window
    /// is fixed at `cap`.
    pub max_doublings: u32,
}

impl Backoff {
    /// Sensible defaults for a market-data feed: 250ms base, 30s cap, 8
    /// doublings (`2^8 * 250ms = 64s` exceeds cap).
    #[must_use]
    pub const fn default_const() -> Self {
        Self {
            base: Duration::from_millis(250),
            cap: Duration::from_secs(30),
            max_doublings: 8,
        }
    }

    /// Compute the next sleep duration for `attempt` (0-indexed). Each call
    /// draws fresh jitter — repeated calls with the same attempt number do
    /// not return the same value.
    #[must_use]
    pub fn next_delay(&self, attempt: u32) -> Duration {
        let exp = attempt.min(self.max_doublings);
        let factor = 1u64 << exp;
        let upper = self
            .base
            .saturating_mul(u32::try_from(factor).unwrap_or(u32::MAX))
            .min(self.cap);
        let upper_ms = u64::try_from(upper.as_millis()).unwrap_or(u64::MAX);
        if upper_ms == 0 {
            return Duration::ZERO;
        }
        let jittered = rand::thread_rng().gen_range(0..=upper_ms);
        Duration::from_millis(jittered)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::default_const()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_never_exceeds_cap() {
        let b = Backoff::default_const();
        // After many attempts, the upper bound is `cap`; jitter draws in
        // `[0, cap]`. Sample a lot of values and confirm.
        for attempt in 0..20 {
            for _ in 0..200 {
                let d = b.next_delay(attempt);
                assert!(d <= b.cap, "attempt {attempt}: {d:?} > cap {:?}", b.cap);
            }
        }
    }

    #[test]
    fn upper_bound_grows_until_capped() {
        // Force a tiny base so the ceiling is observable in `[0, base*2^n]`
        // before the cap clamps it.
        let b = Backoff {
            base: Duration::from_millis(1),
            cap: Duration::from_millis(64),
            max_doublings: 6,
        };
        // attempt 0: in [0, 1ms]
        for _ in 0..100 {
            assert!(b.next_delay(0) <= Duration::from_millis(1));
        }
        // attempt 6+: in [0, 64ms] (cap)
        for _ in 0..100 {
            assert!(b.next_delay(6) <= Duration::from_millis(64));
            assert!(b.next_delay(99) <= Duration::from_millis(64));
        }
    }

    #[test]
    fn does_not_panic_on_huge_attempts() {
        // Past max_doublings, naive `1u64 << attempt` would overflow at 64.
        let b = Backoff::default_const();
        let _ = b.next_delay(u32::MAX);
        let _ = b.next_delay(63);
        let _ = b.next_delay(1000);
    }
}
