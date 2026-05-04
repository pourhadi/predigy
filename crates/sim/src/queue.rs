//! Naïve FIFO queue-position model for resting (GTC) orders.
//!
//! When the strategy submits a resting buy at price P, our order
//! joins the bid queue at level P **behind** whatever qty was
//! already there (`queue_ahead`). Subsequent trade events at the
//! same level consume the queue front-to-back. We fill iff
//! cumulative consumption exceeds `queue_ahead`.
//!
//! ## Mapping Kalshi trade events to queue consumption
//!
//! Kalshi binary contracts: a YES taker hits YES asks (which
//! complement to NO bids); a NO taker hits NO asks (= YES bids).
//! For our resting order, the consumer is the *opposite* taker
//! side at our complement-derived level:
//!
//! ```text
//! resting YES bid at P  ⇐ consumed by Trade { taker_side=NO, yes_price=P, count }
//! resting NO  bid at P  ⇐ consumed by Trade { taker_side=YES, no_price=P, count }
//! ```
//!
//! The strategies in this repo today (`arb-trader`) only use IOC, so
//! this module isn't on their hot path — it's the foundation for the
//! market-making strategy that lands in Phase 4.
//!
//! ## What this model deliberately ignores
//!
//! - **Book deltas at our level by other participants**. If someone
//!   ahead of us cancels (qty drops without a trade event), we'd
//!   advance in the queue in real life, but the recorder only sees
//!   the level total, not which order was cancelled. We assume the
//!   cancels were behind us (worst case for our model).
//! - **Self-trade prevention**. The sim never trades with itself
//!   because the strategy that runs against the sim isn't sized to
//!   produce a resting+taker pair at the same level on the same
//!   venue.

use predigy_core::fill::Fill;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId};
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};

/// One resting order tracked by the sim's queue model.
#[derive(Debug, Clone)]
pub struct RestingOrder {
    pub cid: OrderId,
    pub market: MarketTicker,
    /// `Yes` for a resting YES bid; `No` for a resting NO bid.
    pub side: Side,
    pub price: Price,
    /// Original qty requested by the strategy.
    pub qty: u32,
    /// Qty that was at our level when we joined.
    pub queue_ahead: u32,
    /// Sum of trade counts that have hit our level since we joined.
    pub cumulative_consumed: u32,
    /// Sum of contracts already filled into us.
    pub cumulative_filled: u32,
}

impl RestingOrder {
    #[must_use]
    pub fn from_order(order: &Order, queue_ahead: u32) -> Self {
        Self {
            cid: order.client_id.clone(),
            market: order.market.clone(),
            side: order.side,
            price: order.price,
            qty: order.qty.get(),
            queue_ahead,
            cumulative_consumed: 0,
            cumulative_filled: 0,
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.cumulative_filled >= self.qty
    }

    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.qty.saturating_sub(self.cumulative_filled)
    }
}

/// One side of a trade event the sim ingests. Kept domain-shaped so
/// callers don't need to depend on `predigy-kalshi-md` types.
#[derive(Debug, Clone, Copy)]
pub struct TradePulse {
    /// The side of the **taker**.
    pub taker_side: Side,
    /// YES leg price in cents.
    pub yes_price: Price,
    /// NO leg price in cents.
    pub no_price: Price,
    /// Trade size in contracts.
    pub count: u32,
    pub ts_ms: i64,
}

/// Outcome of running [`TradePulse`] against a `RestingOrder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueAdvance {
    /// New cumulative consumption AT our level.
    pub new_cumulative_consumed: u32,
    /// Contracts that filled us this trade (0 if the queue ahead
    /// hasn't been fully consumed yet).
    pub fill_qty: u32,
    /// True iff the resting order is now fully filled.
    pub terminal: bool,
}

/// Decide whether a `TradePulse` consumes from `order`'s level, and
/// if so how much fills onto our side. Mutates `order.cumulative_*`.
pub fn apply_trade(order: &mut RestingOrder, trade: &TradePulse) -> Option<QueueAdvance> {
    if trade.count == 0 {
        return None;
    }
    if trade.taker_side == order.side {
        // Same-side taker hits the *opposite* book — not our level.
        return None;
    }
    // Match the taker's price leg to our price.
    let level_price = match order.side {
        Side::Yes => trade.yes_price,
        Side::No => trade.no_price,
    };
    if level_price != order.price {
        return None;
    }

    let prev_consumed = order.cumulative_consumed;
    order.cumulative_consumed = order.cumulative_consumed.saturating_add(trade.count);

    // How much of the trade's count is past our queue_ahead and lands
    // on us?
    let consumed_past_us_before = prev_consumed.saturating_sub(order.queue_ahead);
    let consumed_past_us_after = order.cumulative_consumed.saturating_sub(order.queue_ahead);
    let new_fill_into_us = consumed_past_us_after.saturating_sub(consumed_past_us_before);
    // Cap at our remaining qty.
    let fill_qty = new_fill_into_us.min(order.remaining());
    order.cumulative_filled += fill_qty;

    Some(QueueAdvance {
        new_cumulative_consumed: order.cumulative_consumed,
        fill_qty,
        terminal: order.is_terminal(),
    })
}

/// Build a `predigy_core::Fill` from a queue-advance match. The
/// price stamped on the fill is our resting price (we got our
/// limit), in the appropriate side leg.
#[must_use]
pub fn synth_fill(order: &RestingOrder, advance: &QueueAdvance, ts_ms: i64) -> Option<Fill> {
    if advance.fill_qty == 0 {
        return None;
    }
    let qty = Qty::new(advance.fill_qty).ok()?;
    let action = Action::Buy; // resting orders modelled here are bids
    Some(Fill {
        order_id: order.cid.clone(),
        market: order.market.clone(),
        side: order.side,
        action,
        price: order.price,
        qty,
        is_maker: true,
        // Maker rebate is computed by predigy_core::fees::maker_fee at
        // billing time; the sim leaves fee_cents=0 on the synth fill
        // and the OMS doesn't double-count.
        fee_cents: predigy_core::fees::maker_fee(order.price, qty),
        ts_ms: u64::try_from(ts_ms).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::order::{OrderType, TimeInForce};

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn order(side: Side, price: u8, qty: u32) -> Order {
        Order {
            client_id: OrderId::new("c"),
            market: MarketTicker::new("X"),
            side,
            action: Action::Buy,
            price: p(price),
            qty: q(qty),
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
        }
    }

    fn pulse(taker: Side, yes_p: u8, no_p: u8, count: u32) -> TradePulse {
        TradePulse {
            taker_side: taker,
            yes_price: p(yes_p),
            no_price: p(no_p),
            count,
            ts_ms: 0,
        }
    }

    #[test]
    fn queue_ahead_must_be_consumed_before_we_fill() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 10), 5);
        // 3 contracts trade at our level — still all in front of us.
        let a = apply_trade(&mut r, &pulse(Side::No, 40, 60, 3)).unwrap();
        assert_eq!(a.fill_qty, 0);
        assert_eq!(a.new_cumulative_consumed, 3);
        assert!(!a.terminal);
        // 2 more — exactly clear the queue ahead. Still no fill (the
        // consumption == queue_ahead boundary).
        let a = apply_trade(&mut r, &pulse(Side::No, 40, 60, 2)).unwrap();
        assert_eq!(a.fill_qty, 0);
        assert_eq!(a.new_cumulative_consumed, 5);
    }

    #[test]
    fn fills_kick_in_once_queue_ahead_is_passed() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 10), 5);
        // 8 contracts in one trade: first 5 clear the queue ahead,
        // remaining 3 fill us.
        let a = apply_trade(&mut r, &pulse(Side::No, 40, 60, 8)).unwrap();
        assert_eq!(a.fill_qty, 3);
        assert!(!a.terminal);
        assert_eq!(r.cumulative_filled, 3);
    }

    #[test]
    fn fills_cap_at_remaining_qty() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 4), 0);
        // 100 contracts arrive — fill our 4 and stop.
        let a = apply_trade(&mut r, &pulse(Side::No, 40, 60, 100)).unwrap();
        assert_eq!(a.fill_qty, 4);
        assert!(a.terminal);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn no_fill_when_taker_is_same_side() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 10), 0);
        // YES taker hits YES asks (= NO bids), not our YES bid level.
        assert!(apply_trade(&mut r, &pulse(Side::Yes, 60, 40, 5)).is_none());
        assert_eq!(r.cumulative_consumed, 0);
    }

    #[test]
    fn no_fill_when_price_doesnt_match() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 10), 0);
        // NO taker but at a different YES level.
        assert!(apply_trade(&mut r, &pulse(Side::No, 41, 59, 5)).is_none());
        assert_eq!(r.cumulative_consumed, 0);
    }

    #[test]
    fn no_bid_consumed_by_yes_taker_at_no_price() {
        let mut r = RestingOrder::from_order(&order(Side::No, 60, 10), 0);
        // YES taker buys YES at 40 = sells NO at 60 = hits our NO bid
        // at 60. Trade.no_price = 60.
        let a = apply_trade(&mut r, &pulse(Side::Yes, 40, 60, 5)).unwrap();
        assert_eq!(a.fill_qty, 5);
    }

    #[test]
    fn synth_fill_emits_maker_fee_at_resting_price() {
        let r = RestingOrder::from_order(&order(Side::Yes, 40, 100), 0);
        let advance = QueueAdvance {
            new_cumulative_consumed: 25,
            fill_qty: 25,
            terminal: false,
        };
        let f = synth_fill(&r, &advance, 0).unwrap();
        assert!(f.is_maker);
        assert_eq!(f.price.cents(), 40);
        assert_eq!(f.qty.get(), 25);
        assert_eq!(f.fee_cents, predigy_core::fees::maker_fee(p(40), q(25)));
    }

    #[test]
    fn zero_count_trade_is_noop() {
        let mut r = RestingOrder::from_order(&order(Side::Yes, 40, 10), 0);
        assert!(apply_trade(&mut r, &pulse(Side::No, 40, 60, 0)).is_none());
    }
}
