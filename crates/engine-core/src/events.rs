//! Events the engine fans out to strategy modules.
//!
//! `BookUpdate` is the hot path — Kalshi WS book delta arrives,
//! the engine decodes once, dispatches to every strategy that
//! subscribed to that ticker.
//!
//! `ExternalEvent` is the catch-all for non-Kalshi inputs (NWS
//! alert, NBM cycle publish, Polymarket book change). Strategies
//! that consume external feeds opt in via the
//! `Strategy::external_subscriptions` method.

use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;

#[derive(Debug, Clone)]
pub enum Event {
    BookUpdate {
        market: MarketTicker,
        book: OrderBook,
    },
    External(ExternalEvent),
    /// Periodic timer for re-evaluation of held positions.
    /// Cadence is per-strategy (configured at registration).
    Tick,
}

/// Non-Kalshi inputs. Strategies that care subscribe via
/// `Strategy::external_subscriptions`.
#[derive(Debug, Clone)]
pub enum ExternalEvent {
    NwsAlert(predigy_core_compat::NwsAlertPayload),
    NbmCyclePublished {
        cycle_iso: String,
    },
    PolymarketBook {
        asset_id: String,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
    },
}

/// Tiny shim namespace so engine-core doesn't import the whole
/// `predigy-ext-feeds` crate just for one struct shape. Concrete
/// converter lives in the engine crate.
pub mod predigy_core_compat {
    /// Mirrors the fields of `predigy_ext_feeds::nws::NwsAlert` we
    /// actually consume. The engine crate translates between
    /// them at the feed boundary.
    #[derive(Debug, Clone)]
    pub struct NwsAlertPayload {
        pub id: String,
        pub event_type: String,
        pub severity: String,
        pub urgency: String,
        pub area_desc: String,
        pub states: Vec<String>,
        pub effective: Option<String>,
        pub onset: Option<String>,
        pub expires: Option<String>,
        pub headline: Option<String>,
    }
}
