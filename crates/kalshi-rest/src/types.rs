//! JSON response types for the subset of Kalshi REST we use in Phase 1.
//!
//! Schemas reflect the post-Mar-2026 Kalshi API where prices are decimals
//! (`yes_price_dollars`) rather than integer cents.

use serde::{Deserialize, Serialize};

/// Decimal-string-or-number `_dollars` deserializer. Kalshi's
/// post-Mar-2026 REST schema serialises every `*_dollars` /
/// `*_fp` field as a quoted decimal string (`"0.5300"`), but
/// older snapshots and some test fixtures still emit raw floats.
/// Accept both, parse to `f64`, and fold empty strings to `None`.
mod de_dollars {
    use serde::Deserialize;
    use serde::de::{Deserializer, Error};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(f64),
        Str(String),
    }

    pub fn opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        match Option::<Repr>::deserialize(d)? {
            None => Ok(None),
            Some(Repr::Num(n)) => Ok(Some(n)),
            Some(Repr::Str(s)) if s.is_empty() => Ok(None),
            Some(Repr::Str(s)) => s
                .parse::<f64>()
                .map(Some)
                .map_err(|e| D::Error::custom(format!("parse f64 from '{s}': {e}"))),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketsResponse {
    pub markets: Vec<MarketSummary>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /portfolio/balance` response. All monetary fields are
/// integer cents.
#[derive(Debug, Clone, Deserialize)]
pub struct BalanceResponse {
    /// Settled cash, in cents.
    pub balance: i64,
    /// Mark-to-market value of open positions, in cents.
    #[serde(default)]
    pub portfolio_value: i64,
    /// Unix-seconds timestamp of the last venue-side update.
    #[serde(default)]
    pub updated_ts: Option<i64>,
}

/// `GET /series?category=...` response. The `series` field can be
/// `null` (not just empty) when the category matches nothing —
/// `serde(default)` handles both.
#[derive(Debug, Clone, Deserialize)]
pub struct SeriesListResponse {
    #[serde(default)]
    pub series: Vec<SeriesSummary>,
}

/// One series row. Kalshi exposes more fields than these (frequency,
/// settlement source, contract URL, etc.); we keep only what the
/// scanner needs.
#[derive(Debug, Clone, Deserialize)]
pub struct SeriesSummary {
    pub ticker: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketSummary {
    pub ticker: String,
    pub event_ticker: String,
    pub status: String,
    pub title: String,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
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
    /// Calendar close time of the auction. For per-event markets
    /// (sports games) this is often weeks past the actual settlement
    /// window — Kalshi keeps the order book legally open until then
    /// for late corrections. Use `effective_close_unix()` instead of
    /// reading this directly when scheduling.
    pub close_time: String,
    /// Expected per-event settlement time (RFC3339). Set for markets
    /// with `can_close_early=true` — sports, occurrence markets,
    /// anything that resolves on a real-world event before the
    /// calendar `close_time`. Often lands hours/days before
    /// `close_time` for sports.
    #[serde(default)]
    pub expected_expiration_time: Option<String>,
    #[serde(default)]
    pub can_close_early: Option<bool>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub yes_bid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub yes_ask_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub liquidity_dollars: Option<f64>,
    #[serde(default)]
    pub volume: Option<u64>,
}

/// Raw orderbook response. Kalshi wraps the levels in `orderbook_fp`
/// and names the sides `yes_dollars` / `no_dollars`; each level is a
/// two-element array of decimal strings: `["0.4200", "100.00"]`.
/// There is no ask side — YES asks are derived from NO bids by
/// complement at the book layer.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookResponse {
    pub orderbook_fp: Orderbook,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Orderbook {
    /// Each entry: `[price_dollars_str, qty_str]`. Decimal strings.
    #[serde(default)]
    pub yes_dollars: Vec<[String; 2]>,
    #[serde(default)]
    pub no_dollars: Vec<[String; 2]>,
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
    /// Wire field: `position_fp`, decimal-string contract count
    /// (`"0.00"`, `"-3.00"`). Signed: positive = YES, negative = NO.
    /// Decimal because Kalshi supports fractional contracts on some
    /// markets (off by default for elections / sports).
    #[serde(default, deserialize_with = "de_dollars::opt", rename = "position_fp")]
    pub position_contracts: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub realized_pnl_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub fees_paid_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub total_traded_dollars: Option<f64>,
    #[serde(default, deserialize_with = "de_dollars::opt")]
    pub market_exposure_dollars: Option<f64>,
    #[serde(default)]
    pub resting_orders_count: Option<u32>,
    /// RFC3339 timestamp.
    #[serde(default)]
    pub last_updated_ts: Option<String>,
}

// -------------------------------------------------------------- Orders

/// Body posted to `POST /trade-api/v2/portfolio/orders`. Production
/// wire shape verified empirically against the live API May 2026 by
/// reading back Kalshi's error details on rejected probe submits:
///
/// - `side` is `"bid" | "ask"` — Kalshi explicitly errors with
///   `"side must be bid or ask"` when sent `"yes" | "no"`. (The web
///   docs are misleading on this point.) `bid` is the YES book bid
///   side; `ask` is the YES book ask side.
/// - `action` is `"buy" | "sell"` (separate required field)
/// - `count` is a decimal-string ("1.00"). Kalshi's Go struct uses
///   string here despite the docs claiming integer — verified by the
///   error `cannot unmarshal number into ... field count of type
///   string`.
/// - Limit price rides the leg matching the ECONOMIC side: a YES
///   intent (whether buy or sell) sets `yes_price`; a NO intent sets
///   `no_price`. Both are integer cents 1..=99.
///
/// The (Side, Action) → (side, action, price-leg) mapping is in
/// `kalshi-exec/src/mapping.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct CreateOrderRequest {
    pub ticker: String,
    pub client_order_id: String,
    pub side: OrderSideV2,
    pub action: OrderAction,
    /// Whole-contract count, decimal-string fixed-point ("1.00").
    pub count: String,
    /// Limit price as decimal-string dollars ("0.0100" .. "0.9900").
    /// Single field, regardless of which side the order is on. The
    /// `yes_price`/`no_price`/`_dollars` variants documented in the
    /// public docs all returned "empty decimal string" errors when
    /// probed live; only this `price` field is accepted.
    pub price: String,
    pub time_in_force: TimeInForceV2,
    pub self_trade_prevention_type: SelfTradePreventionV2,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSideV2 {
    /// YES book bid side: matches a buy-YES intent or sell-NO.
    Bid,
    /// YES book ask side: matches a sell-YES intent or buy-NO.
    Ask,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderAction {
    Buy,
    Sell,
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

/// Body posted to `DELETE /portfolio/events/orders/batched`. Each
/// element in `orders` names one venue order id to cancel. Kalshi
/// processes the batch atomically per-order — partial success is
/// expected and surfaced via per-element `error` in the response.
#[derive(Debug, Clone, Serialize)]
pub struct BatchCancelOrdersRequest {
    pub orders: Vec<BatchCancelOrderEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchCancelOrderEntry {
    pub order_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchCancelOrdersResponse {
    #[serde(default)]
    pub orders: Vec<BatchCancelOrderResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchCancelOrderResult {
    pub order_id: String,
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub reduced_by: Option<String>,
    /// Per-order error if Kalshi couldn't cancel this specific id (e.g.
    /// 404 Not Found, already filled). Surfaced verbatim so callers can
    /// distinguish recoverable vs fatal cases.
    #[serde(default)]
    pub error: Option<serde_json::Value>,
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
