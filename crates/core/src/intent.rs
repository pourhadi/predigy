//! Strategy → OMS proposed-order type.
//!
//! An [`Intent`] is what a strategy emits: a fully-specified target
//! action that has not yet been assigned a client order id. The OMS
//! converts an `Intent` into an [`Order`](crate::order::Order) by
//! assigning a deterministic [`OrderId`](crate::order::OrderId), running
//! pre-trade risk, and dispatching to the venue executor.
//!
//! Splitting `Intent` from `Order` keeps strategy code decoupled from
//! the OMS's id allocator and its retry/duplicate-detection logic, and
//! lets the risk module reason purely about "what would change" without
//! caring whether an id has been assigned yet.

use crate::market::MarketTicker;
use crate::order::{OrderType, TimeInForce};
use crate::price::{Price, Qty};
use crate::side::{Action, Side};
use serde::{Deserialize, Serialize};

/// A proposed order, pre-id-assignment. Strategies emit these; the OMS
/// turns them into [`Order`](crate::order::Order)s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub market: MarketTicker,
    pub side: Side,
    pub action: Action,
    pub price: Price,
    pub qty: Qty,
    pub order_type: OrderType,
    pub tif: TimeInForce,
}

impl Intent {
    /// Limit order with the given side/action/price/qty. Defaults to
    /// `Gtc` time-in-force, which is the dominant case for resting
    /// quotes; flip to `Ioc`/`Fok`/`PostOnly` via the dedicated builder
    /// helpers below when required.
    #[must_use]
    pub fn limit(market: MarketTicker, side: Side, action: Action, price: Price, qty: Qty) -> Self {
        Self {
            market,
            side,
            action,
            price,
            qty,
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
        }
    }

    /// Notional value of this intent, in whole cents, assuming a full
    /// fill at the limit price. Always non-negative; the sign of the
    /// position change is determined by `action`.
    #[must_use]
    pub fn notional_cents(&self) -> u64 {
        u64::from(self.price.cents()) * u64::from(self.qty.get())
    }

    #[must_use]
    pub fn with_tif(mut self, tif: TimeInForce) -> Self {
        self.tif = tif;
        self
    }

    #[must_use]
    pub fn with_order_type(mut self, order_type: OrderType) -> Self {
        self.order_type = order_type;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    #[test]
    fn notional_is_price_times_qty_in_cents() {
        let intent = Intent::limit(
            MarketTicker::new("X"),
            Side::Yes,
            Action::Buy,
            p(42),
            q(100),
        );
        // 42¢ × 100 = 4200¢ = $42.00
        assert_eq!(intent.notional_cents(), 4200);
    }

    #[test]
    fn defaults_are_limit_gtc() {
        let intent = Intent::limit(MarketTicker::new("X"), Side::No, Action::Sell, p(60), q(1));
        assert_eq!(intent.order_type, OrderType::Limit);
        assert_eq!(intent.tif, TimeInForce::Gtc);
    }

    #[test]
    fn with_tif_overrides() {
        let intent = Intent::limit(MarketTicker::new("X"), Side::Yes, Action::Buy, p(50), q(1))
            .with_tif(TimeInForce::PostOnly);
        assert_eq!(intent.tif, TimeInForce::PostOnly);
    }
}
