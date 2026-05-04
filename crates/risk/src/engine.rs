//! Pre-trade risk engine.
//!
//! [`RiskEngine::check`] is **synchronous and pure** with respect to
//! the engine itself — it borrows `&Limits` and reads `&mut AccountState`
//! (mut so the rate-limit deque can be pruned in-place). Per the plan:
//! "no order leaves OMS without passing `risk::check(order, current_state)`
//! on the calling thread."
//!
//! The check runs every limit; the **first** breach wins (returns
//! `Reject`). Subsequent breaches are not reported in the same response
//! — strategies should retry after fixing whatever's reported, not
//! batch-fix every limit at once.

use crate::limits::Limits;
use crate::state::AccountState;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::side::{Action, Side};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RiskEngine {
    limits: Limits,
}

impl RiskEngine {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Replace the limits at runtime. Used for live re-tuning during
    /// soak tests; production deployments should round-trip via the
    /// config file and require two-person review.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Decide whether `intent` may be sent to the venue.
    ///
    /// Mutating `state` only happens via `orders_in_window` (window
    /// pruning) — a non-rejecting check leaves all observable state
    /// unchanged. The OMS calls [`AccountState::record_order_sent`]
    /// **only** after a successful approval and a successful submit.
    pub fn check(&self, intent: &Intent, state: &mut AccountState, now: Instant) -> Decision {
        if state.kill_switch_active() {
            return Decision::Reject(Reason::KillSwitchActive);
        }

        // Order rate first — cheapest, and a stuck strategy hammering
        // the engine should be caught by this first rather than after
        // we've done arithmetic for it.
        if self.limits.rate.max_orders_per_window > 0 {
            let recent = state.orders_in_window(now, self.limits.rate.window);
            if recent >= self.limits.rate.max_orders_per_window {
                return Decision::Reject(Reason::OrderRateExceeded {
                    recent_count: recent,
                    window: self.limits.rate.window,
                    limit: self.limits.rate.max_orders_per_window,
                });
            }
        }

        // Daily loss breaker. `0` means "disabled" — same convention as
        // every other cap in the engine.
        if self.limits.account.max_daily_loss_cents > 0 {
            let loss = state.daily_realized_loss_cents();
            if loss >= self.limits.account.max_daily_loss_cents {
                return Decision::Reject(Reason::DailyLossBreaker {
                    realized_loss_cents: loss,
                    limit_cents: self.limits.account.max_daily_loss_cents,
                });
            }
        }

        // Project the position and notional changes implied by a full fill.
        let projection = project_full_fill(intent, state);
        let per_market = self.limits.for_market(&intent.market);

        if per_market.max_contracts_per_side > 0
            && projection.would_be_position > per_market.max_contracts_per_side
        {
            return Decision::Reject(Reason::PositionLimitExceeded {
                market: intent.market.clone(),
                side: intent.side,
                current: projection.current_position,
                would_be: projection.would_be_position,
                limit: per_market.max_contracts_per_side,
            });
        }

        if per_market.max_notional_cents_per_side > 0
            && projection.would_be_market_side_notional_cents
                > per_market.max_notional_cents_per_side
        {
            return Decision::Reject(Reason::NotionalLimitExceeded {
                market: intent.market.clone(),
                side: intent.side,
                current_cents: projection.current_market_side_notional_cents,
                would_be_cents: projection.would_be_market_side_notional_cents,
                limit_cents: per_market.max_notional_cents_per_side,
            });
        }

        if self.limits.account.max_gross_notional_cents > 0
            && projection.would_be_gross_notional_cents
                > self.limits.account.max_gross_notional_cents
        {
            return Decision::Reject(Reason::GrossNotionalLimitExceeded {
                current_cents: projection.current_gross_notional_cents,
                would_be_cents: projection.would_be_gross_notional_cents,
                limit_cents: self.limits.account.max_gross_notional_cents,
            });
        }

        Decision::Approve
    }
}

/// Outcome of a check. Strategies pattern-match on this; `Reason`
/// carries the diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject(Reason),
}

impl Decision {
    #[must_use]
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approve)
    }
}

/// Why an intent was rejected. Every variant carries the values the
/// caller would need to log a useful operator alert without having to
/// re-derive them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Reason {
    #[error("kill switch is armed")]
    KillSwitchActive,

    #[error(
        "position limit: {market} {side:?} would be {would_be} (current {current}, limit {limit})"
    )]
    PositionLimitExceeded {
        market: MarketTicker,
        side: Side,
        current: u32,
        would_be: u32,
        limit: u32,
    },

    #[error(
        "notional limit: {market} {side:?} would be {would_be_cents}¢ (current {current_cents}¢, limit {limit_cents}¢)"
    )]
    NotionalLimitExceeded {
        market: MarketTicker,
        side: Side,
        current_cents: u64,
        would_be_cents: u64,
        limit_cents: u64,
    },

    #[error(
        "gross-notional limit: would be {would_be_cents}¢ (current {current_cents}¢, limit {limit_cents}¢)"
    )]
    GrossNotionalLimitExceeded {
        current_cents: u64,
        would_be_cents: u64,
        limit_cents: u64,
    },

    #[error("daily-loss breaker: realised loss {realized_loss_cents}¢ ≥ limit {limit_cents}¢")]
    DailyLossBreaker {
        realized_loss_cents: u64,
        limit_cents: u64,
    },

    #[error("order rate: {recent_count} orders in {window:?} ≥ limit {limit} per window")]
    OrderRateExceeded {
        recent_count: u32,
        window: Duration,
        limit: u32,
    },
}

/// Worst-case fill projection: what the position and notional would
/// look like assuming `intent` is filled completely at its limit
/// price. Used by every per-market and account-level limit.
struct Projection {
    current_position: u32,
    would_be_position: u32,
    current_market_side_notional_cents: u64,
    would_be_market_side_notional_cents: u64,
    current_gross_notional_cents: u64,
    would_be_gross_notional_cents: u64,
}

fn project_full_fill(intent: &Intent, state: &AccountState) -> Projection {
    let current_position = state.position(&intent.market, intent.side);
    let current_market_side_notional = state.notional_cents(&intent.market, intent.side);
    let current_gross = state.gross_notional_cents();

    let qty = intent.qty.get();
    let intent_notional = intent.notional_cents();

    match intent.action {
        Action::Buy => {
            // Buying adds to the position on `intent.side` at
            // `intent.price`. Worst-case position = current + qty;
            // worst-case (market, side) notional = current + qty × price.
            let would_be_position = current_position.saturating_add(qty);
            let would_be_msn = current_market_side_notional.saturating_add(intent_notional);
            let would_be_gross = current_gross.saturating_add(intent_notional);
            Projection {
                current_position,
                would_be_position,
                current_market_side_notional_cents: current_market_side_notional,
                would_be_market_side_notional_cents: would_be_msn,
                current_gross_notional_cents: current_gross,
                would_be_gross_notional_cents: would_be_gross,
            }
        }
        Action::Sell => {
            // Sells cap at zero on the side. Notional decreases at the
            // current avg entry price (we're closing inventory we
            // already hold, not opening new exposure). For risk
            // purposes the position can only shrink — so the limit
            // checks are effectively no-ops on a sell. Kalshi's
            // implicit "sell with no position → buy of opposite side"
            // is **not** modelled here: strategies should send a Buy
            // intent on the opposite side directly, and the OMS can
            // reject the ambiguous form before we ever see it. See the
            // module docs.
            let filled = qty.min(current_position);
            let would_be_position = current_position - filled;
            // Reduction in notional uses the avg_entry, not the limit
            // price — closing inventory removes its booked value.
            let avg = u64::from(state.avg_entry_cents(&intent.market, intent.side));
            let reduction = u64::from(filled) * avg;
            let would_be_msn = current_market_side_notional.saturating_sub(reduction);
            let would_be_gross = current_gross.saturating_sub(reduction);
            Projection {
                current_position,
                would_be_position,
                current_market_side_notional_cents: current_market_side_notional,
                would_be_market_side_notional_cents: would_be_msn,
                current_gross_notional_cents: current_gross,
                would_be_gross_notional_cents: would_be_gross,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{AccountLimits, PerMarketLimits, RateLimits};
    use predigy_core::price::{Price, Qty};
    use std::collections::HashMap;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn buy_yes(market: &str, price: u8, qty: u32) -> Intent {
        Intent::limit(
            MarketTicker::new(market),
            Side::Yes,
            Action::Buy,
            p(price),
            q(qty),
        )
    }

    fn permissive() -> Limits {
        Limits {
            per_market: PerMarketLimits {
                max_contracts_per_side: 10_000,
                max_notional_cents_per_side: 1_000_000,
            },
            per_market_overrides: HashMap::default(),
            account: AccountLimits {
                max_gross_notional_cents: 10_000_000,
                max_daily_loss_cents: u64::MAX,
            },
            rate: RateLimits {
                max_orders_per_window: 0,
                window: Duration::from_secs(1),
            },
        }
    }

    #[test]
    fn approves_when_well_under_limits() {
        let engine = RiskEngine::new(permissive());
        let mut state = AccountState::new();
        let intent = buy_yes("X", 50, 10);
        assert_eq!(
            engine.check(&intent, &mut state, Instant::now()),
            Decision::Approve
        );
    }

    #[test]
    fn rejects_when_kill_switch_armed() {
        let engine = RiskEngine::new(permissive());
        let mut state = AccountState::new();
        state.arm_kill_switch();
        let intent = buy_yes("X", 50, 10);
        assert!(matches!(
            engine.check(&intent, &mut state, Instant::now()),
            Decision::Reject(Reason::KillSwitchActive)
        ));
    }

    #[test]
    fn rejects_per_market_position_limit() {
        let mut limits = permissive();
        limits.per_market.max_contracts_per_side = 100;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        state.set_position(MarketTicker::new("X"), Side::Yes, 95, 40);
        // Buy 10 → projected 105, over 100.
        let intent = buy_yes("X", 50, 10);
        let d = engine.check(&intent, &mut state, Instant::now());
        match d {
            Decision::Reject(Reason::PositionLimitExceeded {
                current,
                would_be,
                limit,
                ..
            }) => {
                assert_eq!(current, 95);
                assert_eq!(would_be, 105);
                assert_eq!(limit, 100);
            }
            other => panic!("expected position-limit reject, got {other:?}"),
        }
    }

    #[test]
    fn per_market_override_tightens_limit() {
        let mut limits = permissive();
        limits.per_market_overrides.insert(
            MarketTicker::new("ILL"),
            PerMarketLimits {
                max_contracts_per_side: 5,
                max_notional_cents_per_side: 250,
            },
        );
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        // Buy 10 on the override'd market → over 5.
        let d = engine.check(&buy_yes("ILL", 25, 10), &mut state, Instant::now());
        assert!(matches!(
            d,
            Decision::Reject(Reason::PositionLimitExceeded { limit: 5, .. })
        ));
        // Same buy on a non-override'd market → fine.
        let d = engine.check(&buy_yes("X", 25, 10), &mut state, Instant::now());
        assert_eq!(d, Decision::Approve);
    }

    #[test]
    fn rejects_per_market_notional() {
        let mut limits = permissive();
        limits.per_market.max_notional_cents_per_side = 1_000;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        // Buy 30 @ 50¢ = 1500¢ > 1000¢.
        let d = engine.check(&buy_yes("X", 50, 30), &mut state, Instant::now());
        assert!(matches!(
            d,
            Decision::Reject(Reason::NotionalLimitExceeded { .. })
        ));
    }

    #[test]
    fn rejects_account_gross_notional() {
        let mut limits = permissive();
        limits.account.max_gross_notional_cents = 5_000;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        state.set_position(MarketTicker::new("OTHER"), Side::Yes, 100, 40); // 4000
        // Buy 30 @ 50¢ = +1500 → 5500 > 5000.
        let d = engine.check(&buy_yes("X", 50, 30), &mut state, Instant::now());
        assert!(matches!(
            d,
            Decision::Reject(Reason::GrossNotionalLimitExceeded { .. })
        ));
    }

    #[test]
    fn rejects_daily_loss_breaker() {
        let mut limits = permissive();
        limits.account.max_daily_loss_cents = 100;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        state.add_realized_pnl(-150);
        let d = engine.check(&buy_yes("X", 50, 1), &mut state, Instant::now());
        assert!(matches!(
            d,
            Decision::Reject(Reason::DailyLossBreaker { .. })
        ));
    }

    #[test]
    fn rejects_order_rate_after_quota() {
        let mut limits = permissive();
        limits.rate.max_orders_per_window = 3;
        limits.rate.window = Duration::from_secs(1);
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        let t0 = Instant::now();
        // Pre-load 3 recent submits within the window.
        for i in 0..3 {
            state.record_order_sent(t0 + Duration::from_millis(100 * i));
        }
        // Check at t0 + 200ms — three sits within `[t0-800ms, t0+200ms]`,
        // so a fourth order should be rejected.
        let d = engine.check(
            &buy_yes("X", 50, 1),
            &mut state,
            t0 + Duration::from_millis(200),
        );
        assert!(matches!(
            d,
            Decision::Reject(Reason::OrderRateExceeded { .. })
        ));
    }

    #[test]
    fn rate_window_eventually_drains() {
        let mut limits = permissive();
        limits.rate.max_orders_per_window = 1;
        limits.rate.window = Duration::from_millis(100);
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        let t0 = Instant::now();
        state.record_order_sent(t0);
        // At t0 + 200ms the older entry is outside the 100ms window.
        let d = engine.check(
            &buy_yes("X", 50, 1),
            &mut state,
            t0 + Duration::from_millis(200),
        );
        assert_eq!(d, Decision::Approve);
    }

    #[test]
    fn sell_does_not_increase_position_or_notional() {
        let mut limits = permissive();
        limits.per_market.max_contracts_per_side = 50;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        state.set_position(MarketTicker::new("X"), Side::Yes, 50, 60);
        // We're at the position limit; a SELL must still pass
        // (it can only shrink the position).
        let intent = Intent::limit(
            MarketTicker::new("X"),
            Side::Yes,
            Action::Sell,
            p(70),
            q(20),
        );
        assert_eq!(
            engine.check(&intent, &mut state, Instant::now()),
            Decision::Approve
        );
    }

    #[test]
    fn zero_limit_disables_check() {
        // PerMarketLimits.max_contracts_per_side = 0 means "no limit"
        // by convention. We rely on this so a default `Limits` is
        // permissive on individual axes; the production config flips
        // every meaningful axis to a real value.
        let mut limits = permissive();
        limits.per_market.max_contracts_per_side = 0;
        let engine = RiskEngine::new(limits);
        let mut state = AccountState::new();
        // Million-contract intent should not be rejected by the
        // (disabled) per-market position cap. It would still hit the
        // notional cap (1,000,000¢ in `permissive()`) — pick a price
        // small enough that it doesn't.
        let intent = buy_yes("X", 1, 999_000);
        // Notional = 999_000¢, just under permissive's 1_000_000¢ cap.
        assert_eq!(
            engine.check(&intent, &mut state, Instant::now()),
            Decision::Approve
        );
    }
}
