//! `Strategy` trait — every strategy module implements this.
//! The engine drives it; the strategy reacts.
//!
//! Async methods carry network IO (DB queries, REST polls).
//! `&mut self` for the methods that mutate per-strategy in-memory
//! state. The engine serialises calls per strategy — strategies
//! don't need to think about concurrency within their own logic.

use crate::events::{Event, ExternalEvent};
use crate::intent::Intent;
use crate::state::StrategyState;
use async_trait::async_trait;
use predigy_core::market::MarketTicker;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrategyId(pub &'static str);

#[async_trait]
pub trait Strategy: Send + Sync {
    /// Stable identifier. Matches the `strategy` column in the DB.
    fn id(&self) -> StrategyId;

    /// Markets this strategy wants book updates for. Called once
    /// at registration; result is cached. Strategies that need
    /// dynamic subscription changes should signal via emitted
    /// intents and let the engine re-resolve.
    async fn subscribed_markets(
        &self,
        state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>>;

    /// External feeds this strategy wants. Returns the list of
    /// feeds (`"nws_alerts"`, `"nbm_cycles"`, `"polymarket"`) to
    /// subscribe to.
    fn external_subscriptions(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Called for every event the strategy is subscribed to.
    /// Returns intents to submit through the OMS.
    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>>;

    /// Tick interval — engine sends `Event::Tick` at this cadence
    /// for re-evaluating held positions. `None` = no tick.
    fn tick_interval(&self) -> Option<std::time::Duration> {
        None
    }
}

// Suppress dead-code warning during the migration; ExternalEvent
// is referenced in trait method docs above.
#[allow(dead_code)]
fn _ext_alive(_: &ExternalEvent) {}
