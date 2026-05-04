//! Polymarket WS market-channel wire protocol.
//!
//! Reference: <https://docs.polymarket.com/market-data/websocket/market-channel.md>.
//!
//! Polymarket markets are CLOB-on-token: each binary outcome is a token
//! identified by a hex `asset_id`. Subscribe by `asset_id`, not by
//! "market" (the `market` field on events is the parent market UUID, but
//! the channel is keyed on token).
//!
//! All numeric fields on the wire are decimal strings — Polymarket uses
//! USDC-denominated sizes that are routinely fractional, and tick sizes
//! vary per market, so strings preserve precision.
//!
//! Field-name oddity: the documented subscribe payload uses `assets_ids`
//! (note the trailing `s` in `assets`) — this is not a typo on our part.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- Outgoing

/// Subscribe payload for the market channel.
///
/// Polymarket's WS does not use a tagged command format — the connection
/// is opened to a channel-specific URL and a single subscribe payload is
/// sent first. Subsequent unsubscribe is done by closing the connection
/// (no in-band unsubscribe message exists).
#[derive(Debug, Clone, Serialize)]
pub struct Subscribe {
    /// Note the spelling: `assets_ids` (plural with an `s` on `assets`).
    /// This matches the documented payload verbatim.
    pub assets_ids: Vec<String>,
    /// Always `"market"` for this channel.
    #[serde(rename = "type")]
    pub kind: String,
    pub custom_feature_enabled: bool,
}

impl Subscribe {
    #[must_use]
    pub fn for_assets(assets: Vec<String>) -> Self {
        Self {
            assets_ids: assets,
            kind: "market".into(),
            custom_feature_enabled: false,
        }
    }
}

// ---------------------------------------------------------------- Incoming

/// Tagged on `event_type`. Variants line up 1:1 with the documented event
/// list. We model them as separate structs (rather than a single bag of
/// optional fields) so the consumer can pattern-match without unwrapping.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum Incoming {
    /// Full L2 snapshot for a single token. Sent once on subscribe and
    /// again on resync.
    Book(BookEvent),
    /// One or more incremental price-level changes within a single market.
    PriceChange(PriceChangeEvent),
    /// Most recent trade against a token.
    LastTradePrice(LastTradePriceEvent),
    /// Tick size adjustment (rare; tick can shrink as a market matures).
    TickSizeChange(TickSizeChangeEvent),
    /// Fallback for envelope `event_type`s we don't model yet.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookEvent {
    pub asset_id: String,
    pub market: String,
    #[serde(default)]
    pub bids: Vec<Level>,
    #[serde(default)]
    pub asks: Vec<Level>,
    pub timestamp: String,
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Level {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceChangeEvent {
    pub market: String,
    #[serde(default)]
    pub price_changes: Vec<PriceChange>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceChange {
    pub asset_id: String,
    pub price: String,
    pub size: String,
    /// `"buy"` or `"sell"` — buy adds to bids, sell adds to asks.
    pub side: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub best_bid: Option<String>,
    #[serde(default)]
    pub best_ask: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LastTradePriceEvent {
    pub asset_id: String,
    #[serde(default)]
    pub fee_rate_bps: Option<String>,
    pub market: String,
    pub price: String,
    pub side: String,
    pub size: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TickSizeChangeEvent {
    pub asset_id: String,
    pub market: String,
    pub old_tick_size: String,
    pub new_tick_size: String,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_serialises_with_documented_field_names() {
        let s = Subscribe::for_assets(vec!["0xabc".into(), "0xdef".into()]);
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains(r#""assets_ids":["0xabc","0xdef"]"#), "got: {j}");
        assert!(j.contains(r#""type":"market""#), "got: {j}");
        assert!(j.contains(r#""custom_feature_enabled":false"#), "got: {j}");
    }

    #[test]
    fn parses_book_event() {
        let raw = r#"{
            "event_type":"book",
            "asset_id":"0xabc",
            "market":"0x123",
            "bids":[{"price":"0.42","size":"100"},{"price":"0.41","size":"50"}],
            "asks":[{"price":"0.45","size":"75"}],
            "timestamp":"1700000000000",
            "hash":"deadbeef"
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::Book(b) = m else {
            panic!("wrong variant");
        };
        assert_eq!(b.asset_id, "0xabc");
        assert_eq!(b.bids.len(), 2);
        assert_eq!(b.asks.len(), 1);
        assert_eq!(b.bids[0].price, "0.42");
        assert_eq!(b.bids[0].size, "100");
        assert_eq!(b.hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parses_price_change_event() {
        let raw = r#"{
            "event_type":"price_change",
            "market":"0x123",
            "price_changes":[{
                "asset_id":"0xabc","price":"0.43","size":"60","side":"buy",
                "hash":"abc","best_bid":"0.43","best_ask":"0.45"
            }],
            "timestamp":"1700000000000"
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::PriceChange(p) = m else {
            panic!("wrong variant");
        };
        assert_eq!(p.price_changes.len(), 1);
        let pc = &p.price_changes[0];
        assert_eq!(pc.asset_id, "0xabc");
        assert_eq!(pc.side, "buy");
        assert_eq!(pc.best_bid.as_deref(), Some("0.43"));
        assert_eq!(pc.best_ask.as_deref(), Some("0.45"));
    }

    #[test]
    fn parses_last_trade_price_event() {
        let raw = r#"{
            "event_type":"last_trade_price",
            "asset_id":"0xabc","fee_rate_bps":"50","market":"0x123",
            "price":"0.42","side":"buy","size":"10","timestamp":"1700"
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::LastTradePrice(t) = m else {
            panic!("wrong variant");
        };
        assert_eq!(t.price, "0.42");
        assert_eq!(t.fee_rate_bps.as_deref(), Some("50"));
    }

    #[test]
    fn parses_tick_size_change_event() {
        let raw = r#"{
            "event_type":"tick_size_change","asset_id":"0xabc","market":"0x123",
            "old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1700"
        }"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        let Incoming::TickSizeChange(t) = m else {
            panic!("wrong variant");
        };
        assert_eq!(t.old_tick_size, "0.01");
        assert_eq!(t.new_tick_size, "0.001");
    }

    #[test]
    fn unknown_event_routes_to_other() {
        let raw = r#"{"event_type":"made_up","asset_id":"x"}"#;
        let m: Incoming = serde_json::from_str(raw).unwrap();
        assert!(matches!(m, Incoming::Other));
    }
}
