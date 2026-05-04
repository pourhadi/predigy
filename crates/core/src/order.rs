//! Order types, identifiers, and lifecycle states.

use crate::market::MarketTicker;
use crate::price::{Price, Qty};
use crate::side::{Action, Side};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Deterministic client-side order id. Built from
/// `(strategy_id, market, intent_seq)` so duplicate sends are no-ops on the
/// exchange and detectable in OMS.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(String);

impl OrderId {
    #[inline]
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    /// Good-til-cancelled.
    Gtc,
    /// Immediate-or-cancel.
    Ioc,
    /// Fill-or-kill.
    Fok,
    /// Post-only (reject if would cross). Critical for capturing maker fee tier.
    PostOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub client_id: OrderId,
    pub market: MarketTicker,
    pub side: Side,
    pub action: Action,
    pub price: Price,
    pub qty: Qty,
    pub order_type: OrderType,
    pub tif: TimeInForce,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderState {
    Pending,
    Acked,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}
