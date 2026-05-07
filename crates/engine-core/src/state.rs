//! `StrategyState` — the cross-call mutable handle a strategy
//! holds onto. Wraps the shared `Db` plus per-strategy bookkeeping
//! (last-fire timestamps, in-flight counters).
//!
//! Strategies should NOT carry their own DB pool — the shared
//! `Db` is reused across modules so we don't fragment the
//! connection budget.

use crate::cross_strategy::CrossStrategyEvent;
use crate::db::Db;
use crate::strategy::StrategyId;
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::warn;

/// Routed envelope sent from `StrategyState::publish_cross_strategy`
/// to the bus task. The bus task dispatches by
/// `payload.payload_topic()` to subscribed strategies.
#[derive(Debug, Clone)]
pub struct PublishedCrossStrategyEvent {
    pub source: StrategyId,
    pub payload: CrossStrategyEvent,
}

#[derive(Debug)]
pub struct StrategyState {
    pub db: Db,
    pub strategy_id: &'static str,
    /// Last-fire wall-clock per market. Used for cooldown logic
    /// inside strategies that don't want to re-fire on every
    /// book delta.
    pub last_fire: HashMap<String, Instant>,
    /// Phase 6 — handle to the cross-strategy bus. `None` during
    /// unit tests + early boot; the engine binary populates this
    /// when wiring supervisors. `publish_cross_strategy` is a
    /// no-op when the handle is absent.
    cross_strategy_tx: Option<mpsc::Sender<PublishedCrossStrategyEvent>>,
}

impl StrategyState {
    pub fn new(db: Db, strategy_id: &'static str) -> Self {
        Self {
            db,
            strategy_id,
            last_fire: HashMap::new(),
            cross_strategy_tx: None,
        }
    }

    /// Phase 6 — attach a cross-strategy bus tx. Called once by
    /// the engine binary when constructing per-supervisor states.
    /// Returns `self` for chaining.
    #[must_use]
    pub fn with_cross_strategy_tx(mut self, tx: mpsc::Sender<PublishedCrossStrategyEvent>) -> Self {
        self.cross_strategy_tx = Some(tx);
        self
    }

    /// Phase 6 — emit a cross-strategy event to the bus. The bus
    /// fans it out to every supervisor that subscribed to the
    /// event's topic. Non-blocking: if the bus's queue is full
    /// the event is dropped with a warn log (same as our other
    /// fan-out paths) — a slow consumer must never backpressure
    /// a producer's hot path.
    ///
    /// No-op when no bus tx is attached (unit tests; engine
    /// boots with zero supervisors).
    pub fn publish_cross_strategy(&self, payload: CrossStrategyEvent) {
        let Some(tx) = &self.cross_strategy_tx else {
            return;
        };
        let envelope = PublishedCrossStrategyEvent {
            source: StrategyId(self.strategy_id),
            payload,
        };
        if let Err(e) = tx.try_send(envelope) {
            warn!(
                source = self.strategy_id,
                error = %e,
                "cross-strategy publish dropped (bus queue full or closed)"
            );
        }
    }
}
