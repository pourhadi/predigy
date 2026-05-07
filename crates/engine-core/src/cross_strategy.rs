//! Cross-strategy event bus.
//!
//! Some strategies produce information another strategy can use:
//!
//! - **cross-arb** sees Polymarket book updates for any Kalshi
//!   market in its pair set. Stat-trader's belief on those same
//!   markets could be sharpened by the poly-mid signal.
//! - **curators** (wx-stat-curator, stat-curator) periodically
//!   publish updated `model_p` for markets. Stat-trader's
//!   `rules` table mirrors those, but a same-process delivery is
//!   lower-latency than the curator's rule-write → DB-poll loop.
//! - Future: wx-stat (when ported) emits model_p drift events.
//!
//! Wiring shape:
//!
//! - Producers call `StrategyState::publish_cross_strategy(...)`
//!   from inside `on_event`. The call goes to a shared mpsc
//!   owned by the bus task; non-blocking on `try_send` so a slow
//!   subscriber doesn't backpressure the producer.
//! - Consumers declare topics in
//!   [`crate::Strategy::cross_strategy_subscriptions`] and
//!   receive `Event::CrossStrategy { source, payload }`.
//! - The bus task in the engine binary fans events out to every
//!   supervisor that subscribed to the event's topic.
//!
//! Topics are well-known string constants (so the consumer's
//! topic list matches the producer's emit type without a shared
//! enum). They're listed in the [`topic`] module for discovery.

use predigy_core::market::MarketTicker;

/// Payload of a cross-strategy event. Strategies pattern-match on
/// the variant to decide whether to consume; the bus routes by
/// the variant's [`payload_topic`] tag.
#[derive(Debug, Clone)]
pub enum CrossStrategyEvent {
    /// cross-arb saw a Polymarket book update for a Kalshi
    /// market in its pair set. Carries the derived poly mid
    /// (cents, 1..=99).
    PolyMidUpdate {
        kalshi_ticker: MarketTicker,
        poly_mid_cents: u8,
    },
    /// A curator (or anything else that produces `model_p`)
    /// published a new probability for `ticker`. Used by
    /// stat-trader to short-circuit its rule-poll loop.
    ModelProbabilityUpdate {
        ticker: MarketTicker,
        /// Free-form provenance — `"wx-stat-curator:DEN:cycle=…"` etc.
        source: String,
        /// Pre-calibration probability. `[0.0, 1.0]`.
        raw_p: f64,
        /// Post-calibration probability the strategy uses.
        model_p: f64,
    },
}

impl CrossStrategyEvent {
    /// Topic tag for this event, matching the strings in
    /// [`topic`]. Used by the bus to look up subscribers.
    #[must_use]
    pub fn payload_topic(&self) -> &'static str {
        match self {
            Self::PolyMidUpdate { .. } => topic::POLY_MID,
            Self::ModelProbabilityUpdate { .. } => topic::MODEL_PROBABILITY,
        }
    }
}

/// Well-known topic strings. Producers don't reference these
/// directly (their emitted variant tags it); consumers reference
/// them in their [`crate::Strategy::cross_strategy_subscriptions`]
/// return.
pub mod topic {
    /// `CrossStrategyEvent::PolyMidUpdate` — poly mid for paired
    /// Kalshi markets. Producer: cross-arb.
    pub const POLY_MID: &str = "poly_mid";
    /// `CrossStrategyEvent::ModelProbabilityUpdate` — fresh
    /// model_p for a market. Producers: curators, wx-stat.
    pub const MODEL_PROBABILITY: &str = "model_probability";
}

/// Routed event delivered to a subscribed strategy's queue. Wraps
/// the payload with the producer's strategy id so consumers can
/// gate on source.
#[derive(Debug, Clone)]
pub struct CrossStrategyDelivery {
    pub source: crate::strategy::StrategyId,
    pub payload: CrossStrategyEvent,
}
