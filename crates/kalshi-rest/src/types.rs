//! JSON response types for the subset of Kalshi REST we use in Phase 1.
//!
//! Schemas reflect the post-Mar-2026 Kalshi API where prices are decimals
//! (`yes_price_dollars`) rather than integer cents.

use serde::{Deserialize, Serialize};

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

// -------------------------------------------------------------- Orders

/// Body posted to `POST /portfolio/events/orders` (V2). Kalshi's
/// `side` field encodes a direction on the **YES** book only:
/// `"bid"` = buy YES, `"ask"` = sell YES. NO orders are sent as their
/// YES equivalent at the complement price (callers are expected to
/// handle the mapping; the executor crate does this).
///
/// Numeric fields are decimal strings ("0.4200", "100.00") matching
/// the post-Mar-2026 fixed-point schema.
#[derive(Debug, Clone, Serialize)]
pub struct CreateOrderRequest {
    pub ticker: String,
    pub client_order_id: String,
    pub side: OrderSideV2,
    /// Contract count, decimal-string fixed-point ("100.00").
    pub count: String,
    /// Limit price in dollars, decimal-string fixed-point ("0.4200").
    pub price: String,
    pub time_in_force: TimeInForceV2,
    pub self_trade_prevention_type: SelfTradePreventionV2,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSideV2 {
    /// Buy YES (or, by complement, sell NO).
    Bid,
    /// Sell YES (or, by complement, buy NO).
    Ask,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForceV2 {
    GoodTillCanceled,
    ImmediateOrCancel,
    FillOrKill,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfTradePreventionV2 {
    /// Cancel the incoming (taker) order on cross. Common default.
    TakerAtCross,
    /// Cancel the resting (maker) order on cross.
    Maker,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateOrderResponse {
    pub order_id: String,
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Decimal string. Contracts filled immediately (e.g. on an IOC).
    #[serde(default)]
    pub fill_count: Option<String>,
    #[serde(default)]
    pub remaining_count: Option<String>,
    #[serde(default)]
    pub average_fill_price: Option<String>,
    #[serde(default)]
    pub average_fee_paid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelOrderResponse {
    pub order_id: String,
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// Decimal string. Contracts that were still resting at cancel time.
    #[serde(default)]
    pub reduced_by: Option<String>,
}

// -------------------------------------------------------------- Fills

#[derive(Debug, Clone, Deserialize)]
pub struct FillsResponse {
    #[serde(default)]
    pub fills: Vec<FillRecord>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FillRecord {
    pub fill_id: String,
    #[serde(default)]
    pub trade_id: Option<String>,
    pub order_id: String,
    /// Newer responses use `market_ticker`; older may use `ticker`.
    #[serde(default)]
    pub market_ticker: Option<String>,
    #[serde(default)]
    pub ticker: Option<String>,
    /// `"yes"` or `"no"`. We model as a string here so that callers can
    /// use whichever Side parser they like.
    pub side: String,
    /// `"buy"` or `"sell"`.
    pub action: String,
    /// Decimal-string contract count ("10.00").
    pub count_fp: String,
    /// Decimal-string YES price.
    pub yes_price_dollars: String,
    /// Decimal-string NO price.
    pub no_price_dollars: String,
    /// True if this fill took liquidity.
    #[serde(default)]
    pub is_taker: Option<bool>,
    /// Decimal-string fee charge for this fill, in dollars.
    #[serde(default)]
    pub fee_cost: Option<String>,
    /// Unix epoch seconds (Kalshi's `ts` field is seconds; `ts_ms` if
    /// present is milliseconds).
    #[serde(default)]
    pub ts: Option<i64>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

impl FillRecord {
    /// `market_ticker` if present, else `ticker`, else `""`.
    #[must_use]
    pub fn ticker_str(&self) -> &str {
        self.market_ticker
            .as_deref()
            .or(self.ticker.as_deref())
            .unwrap_or("")
    }
}
