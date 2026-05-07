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

use crate::cross_strategy::CrossStrategyEvent;
use crate::discovery::DiscoveredMarket;
use crate::strategy::StrategyId;
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
    /// The discovery service found new markets that match this
    /// strategy's [`crate::DiscoverySubscription`], and/or some
    /// previously-tracked markets fell out of the window.
    ///
    /// The engine has already auto-registered the `added` tickers
    /// with the market-data router — book updates will start
    /// flowing on the next snapshot. Strategies should update
    /// their internal per-market state (close_time, etc.) from
    /// the `added` payload.
    DiscoveryDelta {
        added: Vec<DiscoveredMarket>,
        removed: Vec<MarketTicker>,
    },
    /// The pair-file dispatcher (cross-arb's plumbing) saw the
    /// configured pair file change and emitted the diff. The
    /// engine has already registered the `added` Kalshi tickers
    /// with the market-data router AND subscribed the
    /// corresponding Polymarket assets via the external-feed
    /// dispatcher. Strategies should update their internal
    /// kalshi→poly mapping from `added` and drop bookkeeping for
    /// `removed`.
    PairUpdate {
        added: Vec<KalshiPolyPair>,
        removed: Vec<MarketTicker>,
    },
    /// Phase 6 — cross-strategy bus delivery. The engine routes
    /// events emitted by one strategy (via
    /// `StrategyState::publish_cross_strategy`) to every
    /// supervisor that subscribed to the event's topic in
    /// [`crate::Strategy::cross_strategy_subscriptions`]. The
    /// `source` field carries the producer's strategy id so
    /// consumers can gate behavior on origin.
    CrossStrategy {
        source: StrategyId,
        payload: CrossStrategyEvent,
    },
}

/// One Kalshi ↔ Polymarket pair as published by cross-arb-curator.
/// The kalshi side is the venue we trade on; the poly side is
/// reference-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KalshiPolyPair {
    pub kalshi_ticker: MarketTicker,
    pub poly_asset_id: String,
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
