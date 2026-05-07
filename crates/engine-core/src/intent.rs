//! `Intent` — the desire-to-trade output of a strategy module,
//! passed to the engine's OMS for idempotency, risk, and venue
//! routing.
//!
//! Strategies don't construct venue-specific orders directly —
//! they emit `Intent`s, the engine maps to FIX or REST.

use predigy_core::market::MarketTicker;
use predigy_core::side::Side;

/// Strip characters Kalshi rejects in `client_order_id`. Confirmed
/// by Kalshi's V2 venue: cids containing `.` (e.g. tickers like
/// `KXBRAZILINF-26APR-T4.30`) get rejected with
/// `400 invalid_parameters`. The legacy `CidAllocator` already
/// strips them; engine strategies must do the same when embedding
/// a ticker in a cid format string.
///
/// Returns a `String` rather than `&str` so callers don't have to
/// worry about borrow lifetimes when feeding into `format!`.
#[must_use]
pub fn cid_safe_ticker(ticker: &str) -> String {
    ticker.replace('.', "")
}

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
