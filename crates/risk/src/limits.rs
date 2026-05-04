//! Configuration: every limit the risk engine enforces.
//!
//! All numeric values are integer cents (for notional) or contract
//! counts (for position) — no floats anywhere in the limit definitions.
//! Per the plan, this module is one of two (with `oms`) where every
//! change requires two-person review; the structure is therefore as
//! flat and unambiguous as possible.

use predigy_core::market::MarketTicker;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// All limits the engine checks. A request that would breach **any**
/// of these is rejected.
///
/// **Convention: `0` disables the corresponding cap.** `Limits::default()`
/// is fully permissive; production code is expected to set every
/// meaningful cap to a real value before constructing the engine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Default per-market caps. Used for any market not explicitly
    /// covered by `per_market_overrides`.
    pub per_market: PerMarketLimits,
    /// Tighter caps for specific markets (e.g. low-liquidity, high-vol).
    /// Lookup falls back to `per_market` if the ticker isn't here.
    #[serde(default)]
    pub per_market_overrides: HashMap<MarketTicker, PerMarketLimits>,
    /// Account-wide caps.
    pub account: AccountLimits,
    /// Order-rate caps (a slow firewall against runaway strategies).
    pub rate: RateLimits,
}

/// Per-market caps. A `0` value disables that specific check (so the
/// default `Limits::default()` is permissive — populate explicitly).
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerMarketLimits {
    /// Max contracts held on one side of one market.
    pub max_contracts_per_side: u32,
    /// Max notional ($ value) on one side of one market, in cents.
    pub max_notional_cents_per_side: u64,
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountLimits {
    /// Max gross notional across all positions (sum of |contracts| ×
    /// avg-entry across every (market, side)), in cents. `0` disables.
    pub max_gross_notional_cents: u64,
    /// Daily loss breaker. Once realised P&L for the day reaches
    /// `-max_daily_loss_cents`, every check rejects until the next
    /// trading day. `0` disables.
    pub max_daily_loss_cents: u64,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct RateLimits {
    /// Max number of order submissions across the rolling `window`.
    /// `0` means "no rate limit".
    pub max_orders_per_window: u32,
    /// Rolling window over which submissions are counted. Use a few
    /// seconds; longer windows accumulate too much state without
    /// catching the actual failure mode (a stuck loop).
    #[serde(with = "duration_ms")]
    pub window: Duration,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            max_orders_per_window: 0,
            window: Duration::from_secs(1),
        }
    }
}

impl Limits {
    /// Per-market caps in effect for `market` — the override if present,
    /// the default otherwise.
    #[must_use]
    pub fn for_market(&self, market: &MarketTicker) -> PerMarketLimits {
        self.per_market_overrides
            .get(market)
            .copied()
            .unwrap_or(self.per_market)
    }
}

/// Tiny serde helper so `Duration` round-trips as integer milliseconds
/// rather than the default `{secs, nanos}` shape — config files written
/// by hand are much friendlier with one number.
mod duration_ms {
    use serde::{Deserialize, Deserializer, Serialize as _, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        u64::try_from(d.as_millis())
            .unwrap_or(u64::MAX)
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_market_falls_back_to_default() {
        let limits = Limits {
            per_market: PerMarketLimits {
                max_contracts_per_side: 100,
                max_notional_cents_per_side: 5_000,
            },
            ..Limits::default()
        };
        let m = MarketTicker::new("X");
        assert_eq!(limits.for_market(&m).max_contracts_per_side, 100);
    }

    #[test]
    fn for_market_uses_override_when_present() {
        let m = MarketTicker::new("ILLIQUID");
        let mut overrides = HashMap::new();
        overrides.insert(
            m.clone(),
            PerMarketLimits {
                max_contracts_per_side: 5,
                max_notional_cents_per_side: 250,
            },
        );
        let limits = Limits {
            per_market: PerMarketLimits {
                max_contracts_per_side: 100,
                max_notional_cents_per_side: 5_000,
            },
            per_market_overrides: overrides,
            ..Limits::default()
        };
        assert_eq!(limits.for_market(&m).max_contracts_per_side, 5);
    }

    #[test]
    fn round_trips_through_json() {
        // Hand-written JSON with the duration_ms helper engaged.
        let raw = r#"{
            "per_market": { "max_contracts_per_side": 100, "max_notional_cents_per_side": 5000 },
            "per_market_overrides": {},
            "account": { "max_gross_notional_cents": 50000, "max_daily_loss_cents": 25000 },
            "rate": { "max_orders_per_window": 20, "window": 1000 }
        }"#;
        let limits: Limits = serde_json::from_str(raw).unwrap();
        assert_eq!(limits.rate.window, Duration::from_secs(1));
        assert_eq!(limits.rate.max_orders_per_window, 20);

        let back = serde_json::to_string(&limits).unwrap();
        assert!(back.contains(r#""window":1000"#), "got: {back}");
    }
}
