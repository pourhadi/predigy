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

/// Public market-data channels supported by this crate.
///
/// Authenticated channels (`fill`, `user_orders`, `market_positions`, etc.)
/// are out of scope for Phase 1.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Channel {
    OrderbookDelta,
    Ticker,
    Trade,
}

impl Channel {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::OrderbookDelta => "orderbook_delta",
            Self::Ticker => "ticker",
            Self::Trade => "trade",
        }
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
    /// Catch-all for envelope `type`s we don't model yet (e.g. `fill`,
    /// `market_lifecycle_v2`). Surfaces the payload so the caller can log it
    /// or extend handling without a hard parse failure.
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
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
