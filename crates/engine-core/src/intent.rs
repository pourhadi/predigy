//! `Intent` — the desire-to-trade output of a strategy module,
//! passed to the engine's OMS for idempotency, risk, and venue
//! routing.
//!
//! Strategies don't construct venue-specific orders directly —
//! they emit `Intent`s, the engine maps to FIX or REST.

use predigy_core::market::MarketTicker;
use predigy_core::side::Side;

#[derive(Debug, Clone)]
pub struct Intent {
    /// Operator-namespaced client order id; uniqueness is the
    /// engine's idempotency key. Convention: `<strategy>:<ticker>:<8-hex>`.
    pub client_id: String,
    pub strategy: &'static str,
    pub market: MarketTicker,
    pub side: Side,
    pub action: IntentAction,
    /// Limit price in cents; `None` for market orders. Most of
    /// our strategies emit IOC limit orders.
    pub price_cents: Option<i32>,
    pub qty: i32,
    pub order_type: OrderType,
    pub tif: Tif,
    /// Free-form rationale. Persisted to `intents.reason`.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Ioc,
    Gtc,
    Fok,
}
