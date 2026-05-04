//! Trade fill record.

use crate::market::MarketTicker;
use crate::order::OrderId;
use crate::price::{Price, Qty};
use crate::side::{Action, Side};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub market: MarketTicker,
    pub side: Side,
    pub action: Action,
    pub price: Price,
    pub qty: Qty,
    /// True if this fill made (provided liquidity), false if it took.
    pub is_maker: bool,
    /// Exchange-reported fee in whole cents. Already includes the round-up.
    pub fee_cents: u32,
    /// Unix epoch milliseconds, exchange-reported.
    pub ts_ms: u64,
}
