//! Apply a fill to a position. Pure arithmetic, isolated for property
//! tests and easy review.
//!
//! For a `Buy` fill the position grows and the VWAP is blended:
//!
//!   `new_avg = (old_qty × old_avg + fill_qty × fill_price) / new_qty`
//!
//! For a `Sell` fill the position shrinks and the VWAP is unchanged
//! (we're closing inventory at its booked cost). Realised P&L picks
//! up `fill_qty × (fill_price − old_avg)`.
//!
//! Sells against a zero position are documented as out of scope by
//! `predigy-risk` (Kalshi's implicit auto-flip) and rejected by the
//! OMS upstream of this function — `apply_fill` returns the
//! [`PositionUpdate::flat()`] no-op result if it sees one anyway,
//! rather than panic.

use predigy_core::price::Price;
use predigy_core::side::Action;

/// Outcome of applying one fill to a `(qty, avg_entry_cents)` position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionUpdate {
    pub new_qty: u32,
    pub new_avg_entry_cents: u16,
    /// Realised P&L delta in cents (signed). Non-zero only on sells
    /// that close part or all of the existing inventory.
    pub realized_pnl_delta_cents: i64,
}

impl PositionUpdate {
    #[must_use]
    pub fn flat() -> Self {
        Self {
            new_qty: 0,
            new_avg_entry_cents: 0,
            realized_pnl_delta_cents: 0,
        }
    }
}

/// Apply a fill of `fill_qty @ fill_price` (with `action`) to a current
/// position of `(current_qty, current_avg_entry_cents)`.
#[must_use]
pub fn apply_fill(
    action: Action,
    current_qty: u32,
    current_avg_entry_cents: u16,
    fill_qty: u32,
    fill_price: Price,
) -> PositionUpdate {
    if fill_qty == 0 {
        return PositionUpdate {
            new_qty: current_qty,
            new_avg_entry_cents: current_avg_entry_cents,
            realized_pnl_delta_cents: 0,
        };
    }
    match action {
        Action::Buy => apply_buy(current_qty, current_avg_entry_cents, fill_qty, fill_price),
        Action::Sell => apply_sell(current_qty, current_avg_entry_cents, fill_qty, fill_price),
    }
}

fn apply_buy(
    current_qty: u32,
    current_avg_entry_cents: u16,
    fill_qty: u32,
    fill_price: Price,
) -> PositionUpdate {
    let new_qty = current_qty.saturating_add(fill_qty);
    if new_qty == 0 {
        return PositionUpdate::flat();
    }
    let prev_total_cents = u64::from(current_qty) * u64::from(current_avg_entry_cents);
    let added_cents = u64::from(fill_qty) * u64::from(fill_price.cents());
    // Round to nearest cent rather than floor — over many fills
    // floored division accumulates a downward bias in the booked
    // entry price.
    let new_avg = (prev_total_cents + added_cents + u64::from(new_qty / 2)) / u64::from(new_qty);
    PositionUpdate {
        new_qty,
        new_avg_entry_cents: u16::try_from(new_avg).unwrap_or(u16::MAX),
        realized_pnl_delta_cents: 0,
    }
}

fn apply_sell(
    current_qty: u32,
    current_avg_entry_cents: u16,
    fill_qty: u32,
    fill_price: Price,
) -> PositionUpdate {
    if current_qty == 0 {
        // Should not happen: the OMS rejects sells with no position.
        return PositionUpdate::flat();
    }
    let closing = fill_qty.min(current_qty);
    let new_qty = current_qty - closing;
    let new_avg = if new_qty == 0 {
        0
    } else {
        current_avg_entry_cents
    };
    let pnl_per = i64::from(fill_price.cents()) - i64::from(current_avg_entry_cents);
    let realized = pnl_per * i64::from(closing);
    PositionUpdate {
        new_qty,
        new_avg_entry_cents: new_avg,
        realized_pnl_delta_cents: realized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    #[test]
    fn buy_into_empty_position() {
        let u = apply_fill(Action::Buy, 0, 0, 100, p(42));
        assert_eq!(u.new_qty, 100);
        assert_eq!(u.new_avg_entry_cents, 42);
        assert_eq!(u.realized_pnl_delta_cents, 0);
    }

    #[test]
    fn buy_blends_vwap_with_rounding() {
        // 40 @ 41¢, then 60 @ 42¢ → (40*41 + 60*42)/100 = 4160/100 = 41.6 → 42 (round).
        let u = apply_fill(Action::Buy, 40, 41, 60, p(42));
        assert_eq!(u.new_qty, 100);
        assert_eq!(u.new_avg_entry_cents, 42);
    }

    #[test]
    fn sell_realises_pnl_against_avg_entry() {
        // Bought 100 @ 40, sell 30 @ 50 → realised = 30 × (50−40) = +300¢.
        let u = apply_fill(Action::Sell, 100, 40, 30, p(50));
        assert_eq!(u.new_qty, 70);
        assert_eq!(u.new_avg_entry_cents, 40);
        assert_eq!(u.realized_pnl_delta_cents, 300);
    }

    #[test]
    fn sell_loss_is_negative_pnl() {
        // Bought 100 @ 60, sell 50 @ 40 → realised = 50 × (40−60) = −1000¢.
        let u = apply_fill(Action::Sell, 100, 60, 50, p(40));
        assert_eq!(u.new_qty, 50);
        assert_eq!(u.realized_pnl_delta_cents, -1000);
    }

    #[test]
    fn sell_clearing_position_zeros_avg() {
        let u = apply_fill(Action::Sell, 100, 40, 100, p(50));
        assert_eq!(u.new_qty, 0);
        assert_eq!(u.new_avg_entry_cents, 0);
        assert_eq!(u.realized_pnl_delta_cents, 1000);
    }

    #[test]
    fn sell_more_than_held_caps_at_held() {
        // Sell 200 with only 100 held → only 100 closes; the rest is a no-op.
        let u = apply_fill(Action::Sell, 100, 40, 200, p(50));
        assert_eq!(u.new_qty, 0);
        assert_eq!(u.realized_pnl_delta_cents, 1000); // 100 × (50−40)
    }

    #[test]
    fn sell_with_no_position_is_flat() {
        let u = apply_fill(Action::Sell, 0, 0, 50, p(42));
        assert_eq!(u, PositionUpdate::flat());
    }

    #[test]
    fn fill_qty_zero_is_noop() {
        let u = apply_fill(Action::Buy, 100, 40, 0, p(42));
        assert_eq!(u.new_qty, 100);
        assert_eq!(u.new_avg_entry_cents, 40);
        assert_eq!(u.realized_pnl_delta_cents, 0);
    }

    #[test]
    fn buy_overflow_saturates_at_u32_max() {
        let u = apply_fill(Action::Buy, u32::MAX - 5, 50, 100, p(50));
        assert_eq!(u.new_qty, u32::MAX);
    }
}
