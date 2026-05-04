//! Per-order tracking record.
//!
//! One [`OrderRecord`] per submitted order, keyed by
//! [`OrderId`](predigy_core::order::OrderId). The OMS keeps records in
//! a `HashMap` for the order's lifetime and removes them after the
//! reconciliation window passes (a live order plus optionally a few
//! seconds of post-terminal grace for late audits).

use predigy_core::order::{Order, OrderId, OrderState};
use predigy_core::price::Price;
use std::time::Instant;

/// State of one OMS-tracked order.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    pub cid: OrderId,
    pub order: Order,
    pub state: OrderState,
    /// Cumulative filled quantity across all reports.
    pub cumulative_qty: u32,
    /// VWAP of fills against this order, in whole cents. Zero until the
    /// first fill arrives.
    pub avg_fill_price_cents: u16,
    /// True once a cancel has been requested but the venue's response
    /// (Cancelled or Rejected) has not yet arrived.
    pub cancel_in_flight: bool,
    /// Venue-side order id, populated on the first `Acked` report.
    pub venue_order_id: Option<String>,
    /// When the OMS first submitted this order. Used for stale-order
    /// alerts.
    pub submitted_at: Instant,
    /// Last time the record was updated (submit, ack, fill, cancel,
    /// reject).
    pub last_event_at: Instant,
}

impl OrderRecord {
    #[must_use]
    pub fn new(order: Order, now: Instant) -> Self {
        Self {
            cid: order.client_id.clone(),
            order,
            state: OrderState::Pending,
            cumulative_qty: 0,
            avg_fill_price_cents: 0,
            cancel_in_flight: false,
            venue_order_id: None,
            submitted_at: now,
            last_event_at: now,
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected
        )
    }

    /// Update the record on a fill. Returns the *delta* fill quantity
    /// (how much was newly filled) so the caller can feed it into the
    /// position-update math without recomputing from `cumulative_qty`.
    ///
    /// Reports must be monotonic in `cumulative_qty`; out-of-order
    /// reports are silently ignored (`returned delta = 0`).
    pub fn apply_fill(
        &mut self,
        fill_price: Price,
        new_cumulative: u32,
        now: Instant,
        terminal: bool,
    ) -> u32 {
        if new_cumulative <= self.cumulative_qty {
            return 0;
        }
        let delta = new_cumulative - self.cumulative_qty;
        // Update VWAP: blend the existing avg with the new partial.
        let prev_qty = u32::from(self.cumulative_qty > 0) * self.cumulative_qty;
        let prev_total_cents = u64::from(prev_qty) * u64::from(self.avg_fill_price_cents);
        let new_total_cents = prev_total_cents + u64::from(delta) * u64::from(fill_price.cents());
        let new_avg = new_total_cents / u64::from(new_cumulative);
        self.avg_fill_price_cents = u16::try_from(new_avg).unwrap_or(u16::MAX);
        self.cumulative_qty = new_cumulative;
        self.last_event_at = now;
        self.state = if terminal {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        delta
    }

    pub fn mark_acked(&mut self, venue_order_id: String, now: Instant) {
        self.venue_order_id = Some(venue_order_id);
        if self.state == OrderState::Pending {
            self.state = OrderState::Acked;
        }
        self.last_event_at = now;
    }

    pub fn mark_cancelled(&mut self, now: Instant) {
        self.state = OrderState::Cancelled;
        self.cancel_in_flight = false;
        self.last_event_at = now;
    }

    pub fn mark_rejected(&mut self, now: Instant) {
        self.state = OrderState::Rejected;
        self.cancel_in_flight = false;
        self.last_event_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::market::MarketTicker;
    use predigy_core::order::{OrderType, TimeInForce};
    use predigy_core::price::Qty;
    use predigy_core::side::{Action, Side};

    fn make_order(qty: u32, price: u8) -> Order {
        Order {
            client_id: OrderId::new("arb:X:00000001"),
            market: MarketTicker::new("X"),
            side: Side::Yes,
            action: Action::Buy,
            price: Price::from_cents(price).unwrap(),
            qty: Qty::new(qty).unwrap(),
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
        }
    }

    #[test]
    fn fresh_record_is_pending_with_zero_cumulative() {
        let r = OrderRecord::new(make_order(100, 42), Instant::now());
        assert_eq!(r.state, OrderState::Pending);
        assert_eq!(r.cumulative_qty, 0);
        assert_eq!(r.avg_fill_price_cents, 0);
        assert!(!r.is_terminal());
    }

    #[test]
    fn apply_fill_partial_then_final_blends_vwap() {
        let mut r = OrderRecord::new(make_order(100, 42), Instant::now());
        // 40 @ 41¢
        let delta1 = r.apply_fill(Price::from_cents(41).unwrap(), 40, Instant::now(), false);
        assert_eq!(delta1, 40);
        assert_eq!(r.cumulative_qty, 40);
        assert_eq!(r.avg_fill_price_cents, 41);
        assert_eq!(r.state, OrderState::PartiallyFilled);

        // 60 more @ 42¢, final → cum 100, VWAP = (40*41 + 60*42)/100 = 4160/100 = 41.6 → 41 (integer).
        let delta2 = r.apply_fill(Price::from_cents(42).unwrap(), 100, Instant::now(), true);
        assert_eq!(delta2, 60);
        assert_eq!(r.cumulative_qty, 100);
        assert_eq!(r.avg_fill_price_cents, 41); // integer floor of 41.6
        assert_eq!(r.state, OrderState::Filled);
        assert!(r.is_terminal());
    }

    #[test]
    fn apply_fill_drops_out_of_order_reports() {
        let mut r = OrderRecord::new(make_order(100, 42), Instant::now());
        r.apply_fill(Price::from_cents(42).unwrap(), 50, Instant::now(), false);
        // Stale report at cumulative=30 — ignored.
        let delta = r.apply_fill(Price::from_cents(99).unwrap(), 30, Instant::now(), false);
        assert_eq!(delta, 0);
        assert_eq!(r.cumulative_qty, 50);
        assert_eq!(r.avg_fill_price_cents, 42);
    }

    #[test]
    fn mark_acked_only_advances_from_pending() {
        let mut r = OrderRecord::new(make_order(100, 42), Instant::now());
        // Partial fill arrives before the ack — state should not regress
        // back to Acked when the late ack lands.
        r.apply_fill(Price::from_cents(42).unwrap(), 50, Instant::now(), false);
        assert_eq!(r.state, OrderState::PartiallyFilled);
        r.mark_acked("V1".into(), Instant::now());
        assert_eq!(r.state, OrderState::PartiallyFilled);
        assert_eq!(r.venue_order_id.as_deref(), Some("V1"));
    }

    #[test]
    fn mark_cancelled_clears_in_flight_and_terminates() {
        let mut r = OrderRecord::new(make_order(100, 42), Instant::now());
        r.cancel_in_flight = true;
        r.mark_cancelled(Instant::now());
        assert_eq!(r.state, OrderState::Cancelled);
        assert!(!r.cancel_in_flight);
        assert!(r.is_terminal());
    }
}
