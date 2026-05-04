//! Exponential backoff with full jitter for the reconnect loop.
//!
//! Algorithm: `delay = rand_uniform(0, base * 2^n)` clamped at `cap`. "Full
//! jitter" (vs. equal jitter) gives the tightest distribution of retry
//! attempts under correlated failures. Reference: AWS Architecture Blog,
//! "Exponential Backoff And Jitter" (Brooker 2015).
//!
//! Duplicated from `predigy-kalshi-md` rather than introducing a shared
//! crate — both implementations are tiny, stable, and have no domain
//! coupling. If a third consumer arrives, promote.

use rand::Rng as _;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub max_doublings: u32,
}

impl Backoff {
    #[must_use]
    pub const fn default_const() -> Self {
        Self {
            base: Duration::from_millis(250),
            cap: Duration::from_secs(30),
            max_doublings: 8,
        }
    }

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
        for attempt in 0..20 {
            for _ in 0..200 {
                assert!(b.next_delay(attempt) <= b.cap);
            }
        }
    }

    #[test]
    fn does_not_panic_on_huge_attempts() {
        let b = Backoff::default_const();
        let _ = b.next_delay(u32::MAX);
        let _ = b.next_delay(63);
    }
}
