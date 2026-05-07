//! Strategy registry — strategies register at engine startup
//! with their tick interval + per-strategy risk overrides + a
//! constructor closure. The engine's supervisor walks the
//! registry, spawns a tokio task per strategy, and routes events.
//!
//! Strategies are owned by the engine, not the registry; the
//! registry holds construction recipes.

use predigy_engine_core::oms::RiskCaps;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

type StrategyFactory =
    Box<dyn Fn() -> Box<dyn Strategy> + Send + Sync + 'static>;

pub struct StrategyHandle {
    pub id: StrategyId,
    /// Tick interval override; if `None` engine uses
    /// `EngineConfig::default_strategy_tick_interval`.
    pub tick_interval: Option<Duration>,
    /// Per-strategy risk cap override; if `None` engine uses
    /// `EngineConfig::default_risk_caps`.
    pub risk_caps: Option<RiskCaps>,
    factory: StrategyFactory,
}

impl std::fmt::Debug for StrategyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyHandle")
            .field("id", &self.id)
            .field("tick_interval", &self.tick_interval)
            .field("risk_caps", &self.risk_caps)
            .finish()
    }
}

impl StrategyHandle {
    pub fn new<F>(id: StrategyId, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Strategy> + Send + Sync + 'static,
    {
        Self {
            id,
            tick_interval: None,
            risk_caps: None,
            factory: Box::new(factory),
        }
    }

    #[must_use]
    pub fn with_tick_interval(mut self, dur: Duration) -> Self {
        self.tick_interval = Some(dur);
        self
    }

    #[must_use]
    pub fn with_risk_caps(mut self, caps: RiskCaps) -> Self {
        self.risk_caps = Some(caps);
        self
    }

    pub fn instantiate(&self) -> Box<dyn Strategy> {
        (self.factory)()
    }
}

#[derive(Debug, Default)]
pub struct StrategyRegistry {
    handles: Arc<Mutex<HashMap<StrategyId, StrategyHandle>>>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, handle: StrategyHandle) {
        let id = handle.id;
        let mut map = self.handles.lock().await;
        if map.contains_key(&id) {
            tracing::warn!(?id, "registry: replacing existing strategy");
        }
        map.insert(id, handle);
    }

    pub async fn iter_ids(&self) -> Vec<StrategyId> {
        self.handles.lock().await.keys().copied().collect()
    }

    pub async fn instantiate_all(&self) -> Vec<(StrategyId, Box<dyn Strategy>)> {
        let map = self.handles.lock().await;
        map.iter()
            .map(|(id, h)| (*id, h.instantiate()))
            .collect()
    }
}
