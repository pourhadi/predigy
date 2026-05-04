//! Per-market position state.

use crate::market::MarketTicker;
use crate::price::Price;
use crate::side::Side;
use serde::{Deserialize, Serialize};

/// Net position in a single market.
///
/// Kalshi positions are signed integer contracts on a chosen side; we
/// represent them with `(side, qty)` where `qty == 0` means flat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub market: MarketTicker,
    pub side: Side,
    pub qty: u32,
    /// Volume-weighted average entry price in whole cents. Zero if flat.
    pub avg_entry_cents: u16,
}

impl Position {
    #[inline]
    #[must_use]
    pub fn flat(market: MarketTicker, side: Side) -> Self {
        Self {
            market,
            side,
            qty: 0,
            avg_entry_cents: 0,
        }
    }

    #[inline]
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.qty == 0
    }

    /// Mark-to-market value in cents at the given mark price.
    #[inline]
    #[must_use]
    pub fn unrealized_pnl_cents(&self, mark: Price) -> i64 {
        if self.qty == 0 {
            return 0;
        }
        let q = i64::from(self.qty);
        let mark_c = i64::from(mark.cents());
        let entry_c = i64::from(self.avg_entry_cents);
        (mark_c - entry_c) * q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_pnl_zero() {
        let p = Position::flat(MarketTicker::new("X"), Side::Yes);
        assert_eq!(p.unrealized_pnl_cents(Price::from_cents(50).unwrap()), 0);
    }

    #[test]
    fn unrealized_pnl_basic() {
        let p = Position {
            market: MarketTicker::new("X"),
            side: Side::Yes,
            qty: 100,
            avg_entry_cents: 40,
        };
        // Mark at 55¢, bought at 40¢, 100 contracts → +1500¢ = +$15.
        assert_eq!(p.unrealized_pnl_cents(Price::from_cents(55).unwrap()), 1500);
    }
}
