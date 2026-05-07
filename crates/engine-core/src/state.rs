//! `StrategyState` — the cross-call mutable handle a strategy
//! holds onto. Wraps the shared `Db` plus per-strategy bookkeeping
//! (last-fire timestamps, in-flight counters).
//!
//! Strategies should NOT carry their own DB pool — the shared
//! `Db` is reused across modules so we don't fragment the
//! connection budget.

use crate::db::Db;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug)]
pub struct StrategyState {
    pub db: Db,
    pub strategy_id: &'static str,
    /// Last-fire wall-clock per market. Used for cooldown logic
    /// inside strategies that don't want to re-fire on every
    /// book delta.
    pub last_fire: HashMap<String, Instant>,
}

impl StrategyState {
    pub fn new(db: Db, strategy_id: &'static str) -> Self {
        Self {
            db,
            strategy_id,
            last_fire: HashMap::new(),
        }
    }
}
