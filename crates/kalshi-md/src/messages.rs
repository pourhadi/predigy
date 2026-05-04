//! Kalshi WebSocket wire protocol — JSON message shapes.
//!
//! Reference: <https://docs.kalshi.com/websockets/>. All schemas reflect the
//! post-Mar-2026 API where prices and sizes are quoted as decimal strings
//! (`yes_dollars_fp`, `delta_fp`, etc.) for forward-compatible precision.
//!
//! Outgoing commands are tagged on the `cmd` field; incoming messages are
//! tagged on the `type` field. `serde(tag = "...")` keeps the JSON shape
//! flat without an enum wrapper.

use predigy_core::side::Side;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Channels

/// Channels supported by this crate. Authenticated channels (`Fill`,
/// `OrderState`, `MarketPositions`) require the WS upgrade to have
/// been signed; the public channels do not.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Channel {
    // Public.
    OrderbookDelta,
    Ticker,
    Trade,
    // Authenticated. Carry the user's own fills, order lifecycle, and
    // position deltas. Used by `WsExecutor` as a push-based
    // alternative to polling `/portfolio/fills` over REST.
    Fill,
    OrderState,
    MarketPositions,
}

impl Channel {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::OrderbookDelta => "orderbook_delta",
            Self::Ticker => "ticker",
            Self::Trade => "trade",
            Self::Fill => "fill",
            Self::OrderState => "order_state",
            Self::MarketPositions => "market_positions",
        }
    }

    /// Whether the channel requires an authenticated upgrade.
    #[must_use]
    pub const fn requires_auth(self) -> bool {
        matches!(self, Self::Fill | Self::OrderState | Self::MarketPositions)
    }
}

// ---------------------------------------------------------------- Outgoing

/// A client → server command. Serialised at the call site via `serde_json`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Outgoing {
    Subscribe { id: u64, params: SubscribeParams },
    Unsubscribe { id: u64, params: UnsubscribeParams },
    UpdateSubscription { id: u64, params: UpdateParams },
}

#[derive(Debug, Clone, Serialize)]
pub struct SubscribeParams {
    pub channels: Vec<String>,
    /// One of `market_ticker` / `market_tickers` should be set; both are
    /// optional so the same struct can serialise either form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_tickers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnsubscribeParams {
    pub sids: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateParams {
    pub sids: Vec<u64>,
    pub action: UpdateAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_tickers: Option<Vec<String>>,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    AddMarkets,
    DeleteMarkets,
    GetSnapshot,
}

// ---------------------------------------------------------------- Incoming

/// The full raw incoming envelope. The interesting variants destructure
/// `msg` — fields like `sid` and `seq` live at the envelope level.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Subscribed {
        #[serde(default)]
        id: Option<u64>,
        msg: SubscribedBody,
    },
    Ok {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        sid: Option<u64>,
        #[serde(default)]
        seq: Option<u64>,
        #[serde(default)]
        msg: serde_json::Value,
    },
    Error {
        #[serde(default)]
        id: Option<u64>,
        msg: ErrorBody,
    },
    OrderbookSnapshot {
        sid: u64,
        seq: u64,
        msg: OrderbookSnapshotBody,
    },
    OrderbookDelta {
        sid: u64,
        seq: u64,
        msg: OrderbookDeltaBody,
    },
    Ticker {
        sid: u64,
        msg: TickerBody,
    },
    Trade {
        sid: u64,
        msg: TradeBody,
    },
    /// User-fill event from the authed `fill` channel. Same payload
    /// shape as `FillRecord` from the REST `/portfolio/fills`
    /// endpoint, with two extras useful for live processing:
    /// `post_position_fp` (the user's position in the market AFTER
    /// this fill, as a venue-authoritative checkpoint) and
    /// `purchased_side` (the contract side that was added to the
    /// user's position — `"yes"` if they bought YES or sold NO,
    /// `"no"` if they bought NO or sold YES).
    Fill {
        sid: u64,
        msg: FillBody,
    },
    /// Position-update event from the authed `market_positions`
    /// subscription (note the wire `type` is singular:
    /// `market_position`). Carries the venue's authoritative view
    /// of (position, exposure, realized P&L, fees) for one market.
    /// Useful as a reconciliation signal vs the OMS's own ledger.
    #[serde(rename = "market_position")]
    MarketPosition {
        sid: u64,
        msg: MarketPositionBody,
    },
    /// Catch-all for envelope `type`s we don't model yet. Surfaces
    /// via [`Event::UnhandledType`] so schema drift is visible.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribedBody {
    pub channel: String,
    pub sid: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub code: i64,
    pub msg: String,
}

/// Snapshot of all resting bids for one market.
///
/// Both `yes_dollars_fp` and `no_dollars_fp` are arrays of
/// `[price_dollars_string, qty_string]` — both string-encoded fixed-point
/// for forward-compatible precision.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookSnapshotBody {
    pub market_ticker: String,
    #[serde(default)]
    pub market_id: Option<String>,
    #[serde(default)]
    pub yes_dollars_fp: Vec<[String; 2]>,
    #[serde(default)]
    pub no_dollars_fp: Vec<[String; 2]>,
}

/// One incremental level change.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderbookDeltaBody {
    pub market_ticker: String,
    #[serde(default)]
    pub market_id: Option<String>,
    /// `"0.960"` — decimal dollars as a string.
    pub price_dollars: String,
    /// Signed fixed-point quantity change, e.g. `"-54.00"`. Positive = added,
    /// negative = removed/lifted.
    pub delta_fp: String,
    pub side: Side,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

/// Ticker fields are mostly informational; we keep the raw decoded shape
/// rather than re-modelling each numeric field into a domain type.
///
/// `Serialize` is provided so `md-recorder` can archive ticker events
/// (and replay them by deserialising back into the same shape).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TickerBody {
    pub market_ticker: String,
    #[serde(default)]
    pub market_id: Option<String>,
    #[serde(default)]
    pub price_dollars: Option<String>,
    #[serde(default)]
    pub yes_bid_dollars: Option<String>,
    #[serde(default)]
    pub yes_ask_dollars: Option<String>,
    #[serde(default)]
    pub volume_fp: Option<String>,
    #[serde(default)]
    pub open_interest_fp: Option<String>,
    #[serde(default)]
    pub yes_bid_size_fp: Option<String>,
    #[serde(default)]
    pub yes_ask_size_fp: Option<String>,
    #[serde(default)]
    pub last_trade_size_fp: Option<String>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TradeBody {
    pub trade_id: String,
    pub market_ticker: String,
    pub yes_price_dollars: String,
    pub no_price_dollars: String,
    pub count_fp: String,
    pub taker_side: Side,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

/// User-fill payload from the authed `fill` channel.
///
/// Mirrors the REST `FillRecord` shape with two extras the WS
/// channel includes for free:
/// - `post_position_fp`: the venue's view of the user's position
///   in this market AFTER this fill, signed (`"1.00"`, `"-3.00"`).
/// - `purchased_side`: which contract side was added to the user's
///   position. `"yes"` for (buy YES) or (sell NO at complement);
///   `"no"` for (buy NO at complement) or (sell YES). Useful as a
///   sanity check against our cid-based tracking.
///
/// Like `FillRecord`, `action` is empty in V2 — the trader's
/// (Side, Action) must come from the originating order's tracking
/// entry, not from this wire field.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FillBody {
    pub trade_id: String,
    pub order_id: String,
    /// The cid we sent on submit (`client_order_id`). Used to
    /// route the fill back to the originating order without going
    /// through the venue id.
    pub client_order_id: String,
    pub market_ticker: String,
    /// `"yes"` or `"no"` — the venue book side, NOT the trader's
    /// intended side (those differ for any (No, *) order, which is
    /// submitted as wire-YES at complement).
    pub side: String,
    /// Trader's purchased side. `"yes"` when the trade increased
    /// (or partially filled an order to increase) the user's YES
    /// position; `"no"` for NO. Reflects the trader's intent.
    pub purchased_side: String,
    /// Empty in V2 — kept for forward compatibility.
    #[serde(default)]
    pub action: String,
    pub yes_price_dollars: String,
    /// Some V2 envelopes only carry `yes_price_dollars`; the NO
    /// leg is implicit at the complement.
    #[serde(default)]
    pub no_price_dollars: Option<String>,
    pub count_fp: String,
    #[serde(default)]
    pub fee_cost: Option<String>,
    pub is_taker: bool,
    /// Venue position after this fill, decimal-string.
    #[serde(default)]
    pub post_position_fp: Option<String>,
    #[serde(default)]
    pub ts: Option<i64>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

/// Position-update payload from the authed `market_positions`
/// subscription. The venue's authoritative view of one market's
/// position state. Cumulative across the account's lifetime —
/// realized P&L and fees are total-since-inception, not deltas.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketPositionBody {
    pub user_id: String,
    pub market_ticker: String,
    pub position_fp: String,
    pub position_cost_dollars: String,
    pub realized_pnl_dollars: String,
    pub fees_paid_dollars: String,
    #[serde(default)]
    pub position_fee_cost_dollars: Option<String>,
    #[serde(default)]
    pub volume_fp: Option<String>,
    #[serde(default)]
    pub subaccount: Option<u32>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_serialises_snake_case() {
        let cmd = Outgoing::Subscribe {
            id: 1,
            params: SubscribeParams {
                channels: vec!["orderbook_delta".into()],
                market_ticker: Some("X".into()),
                market_tickers: None,
            },
        };
        let s = serde_json::to_string(&cmd).unwrap();
        // `cmd` tag, snake_case, omitted optional fields.
        assert!(s.contains(r#""cmd":"subscribe""#), "got: {s}");
        assert!(s.contains(r#""market_ticker":"X""#), "got: {s}");
        assert!(!s.contains("market_tickers"), "got: {s}");
    }

    #[test]
    fn subscribe_with_multiple_markets_uses_plural_field() {
        let cmd = Outgoing::Subscribe {
            id: 7,
            params: SubscribeParams {
                channels: vec!["ticker".into()],
                market_ticker: None,
                market_tickers: Some(vec!["A".into(), "B".into()]),
            },
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""market_tickers":["A","B"]"#), "got: {s}");
        assert!(!s.contains(r#""market_ticker""#), "got: {s}");
    }

    #[test]
    fn unsubscribe_serialises() {
        let cmd = Outgoing::Unsubscribe {
            id: 9,
            params: UnsubscribeParams { sids: vec![1, 2] },
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""cmd":"unsubscribe""#));
        assert!(s.contains(r#""sids":[1,2]"#));
    }

    #[test]
    fn parses_subscribed_envelope() {
        let raw = r#"{"id":1,"type":"subscribed","msg":{"channel":"orderbook_delta","sid":1}}"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::Subscribed { id, msg } = m else {
            panic!("wrong variant");
        };
        assert_eq!(id, Some(1));
        assert_eq!(msg.channel, "orderbook_delta");
        assert_eq!(msg.sid, 1);
    }

    #[test]
    fn parses_error_envelope() {
        let raw = r#"{"id":123,"type":"error","msg":{"code":6,"msg":"Already subscribed"}}"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::Error { id, msg } = m else {
            panic!("wrong variant");
        };
        assert_eq!(id, Some(123));
        assert_eq!(msg.code, 6);
        assert_eq!(msg.msg, "Already subscribed");
    }

    #[test]
    fn parses_orderbook_snapshot_envelope() {
        let raw = r#"{
            "type":"orderbook_snapshot",
            "sid":2,"seq":2,
            "msg":{
                "market_ticker":"FED-23DEC-T3.00",
                "market_id":"9b0f6b43-5b68-4f9f-9f02-9a2d1b8ac1a1",
                "yes_dollars_fp":[["0.0800","300.00"],["0.2200","333.00"]],
                "no_dollars_fp":[["0.5400","20.00"],["0.5600","146.00"]]
            }
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::OrderbookSnapshot { sid, seq, msg } = m else {
            panic!("wrong variant");
        };
        assert_eq!(sid, 2);
        assert_eq!(seq, 2);
        assert_eq!(msg.market_ticker, "FED-23DEC-T3.00");
        assert_eq!(msg.yes_dollars_fp.len(), 2);
        assert_eq!(msg.no_dollars_fp.len(), 2);
        assert_eq!(msg.yes_dollars_fp[0][0], "0.0800");
        assert_eq!(msg.yes_dollars_fp[0][1], "300.00");
    }

    #[test]
    fn parses_orderbook_delta_envelope() {
        let raw = r#"{
            "type":"orderbook_delta",
            "sid":2,"seq":3,
            "msg":{
                "market_ticker":"X","market_id":"u",
                "price_dollars":"0.960","delta_fp":"-54.00","side":"yes","ts_ms":1669149841000
            }
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::OrderbookDelta { msg, seq, .. } = m else {
            panic!("wrong variant");
        };
        assert_eq!(seq, 3);
        assert_eq!(msg.price_dollars, "0.960");
        assert_eq!(msg.delta_fp, "-54.00");
        assert_eq!(msg.side, Side::Yes);
        assert_eq!(msg.ts_ms, Some(1_669_149_841_000));
    }

    #[test]
    fn parses_trade_envelope() {
        let raw = r#"{
            "type":"trade","sid":4,
            "msg":{
                "trade_id":"t-1","market_ticker":"X",
                "yes_price_dollars":"0.42","no_price_dollars":"0.58",
                "count_fp":"10.00","taker_side":"no","ts_ms":1
            }
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::Trade { msg, .. } = m else {
            panic!("wrong variant");
        };
        assert_eq!(msg.trade_id, "t-1");
        assert_eq!(msg.taker_side, Side::No);
    }

    #[test]
    fn unknown_type_routes_to_other() {
        let raw = r#"{"type":"market_lifecycle_v2","sid":99,"msg":{}}"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        assert!(matches!(m, Incoming::Other));
    }
}
