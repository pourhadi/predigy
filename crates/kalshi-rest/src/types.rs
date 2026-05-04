//! JSON response types for the subset of Kalshi REST we use in Phase 1.
//!
//! Schemas reflect the post-Mar-2026 Kalshi API where prices are decimals
//! (`yes_price_dollars`) rather than integer cents.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MarketsResponse {
    pub markets: Vec<MarketSummary>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketSummary {
    pub ticker: String,
    pub event_ticker: String,
    pub status: String,
    pub title: String,
    #[serde(default)]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default)]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default)]
    pub last_price_dollars: Option<f64>,
    pub close_time: String, // RFC3339
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketDetailResponse {
    pub market: MarketDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketDetail {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub status: String,
    pub close_time: String,
    #[serde(default)]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default)]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default)]
    pub liquidity_dollars: Option<f64>,
    #[serde(default)]
    pub volume: Option<u64>,
}

/// Raw orderbook response. Kalshi returns `yes_bids` and `no_bids` only —
/// there is no ask side; YES asks are derived from NO bids by complement.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookResponse {
    pub orderbook: Orderbook,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Orderbook {
    /// Each entry: `[price_dollars, qty]`.
    #[serde(default)]
    pub yes: Vec<[f64; 2]>,
    #[serde(default)]
    pub no: Vec<[f64; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PositionsResponse {
    #[serde(default)]
    pub market_positions: Vec<MarketPosition>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketPosition {
    pub ticker: String,
    /// Signed: positive = YES, negative = NO.
    pub position: i64,
    #[serde(default)]
    pub realized_pnl_dollars: Option<f64>,
    #[serde(default)]
    pub fees_paid_dollars: Option<f64>,
    #[serde(default)]
    pub total_traded_dollars: Option<f64>,
}
