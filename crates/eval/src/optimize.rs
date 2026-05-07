//! Optimizer scaffold (v2 placeholder).
//!
//! v1 ships rule-based recommendations only — see `diagnose.rs`.
//! This module defines the `ParameterSpace` + `OptimizationObjective`
//! contracts so a future backtest-replay-driven grid search can
//! plug in without further architectural changes.
//!
//! When the v2 optimizer lands it will:
//!
//! 1. Replay historical fills + book deltas across a parameter
//!    grid (per-strategy env-var ranges).
//! 2. Score each candidate against an `OptimizationObjective`
//!    (default: net PnL; pluggable to Sharpe, expectancy, etc.).
//! 3. Emit `Recommendation` records with concrete proposed values
//!    AND the in-sample / out-of-sample backtest evidence.

use crate::metrics::StrategyMetrics;
use serde::{Deserialize, Serialize};

/// Parameter-search space for one strategy. Each parameter is a
/// numeric env var with a min/max range and step size. The
/// optimizer enumerates (or random-samples) the cross product.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSpace {
    pub strategy: String,
    pub parameters: Vec<NumericParameter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumericParameter {
    /// Env-var name (e.g. `PREDIGY_STAT_TAKE_PROFIT_CENTS`).
    pub env_var: String,
    /// Inclusive range bounds.
    pub min: f64,
    pub max: f64,
    /// Step size for grid search.
    pub step: f64,
}

/// Objective the optimizer maximizes. Implementors return a
/// scalar; higher = better.
pub trait OptimizationObjective {
    fn evaluate(&self, m: &StrategyMetrics) -> f64;
    fn name(&self) -> &'static str;
}

/// Default: maximize net PnL.
#[derive(Debug, Clone, Copy)]
pub struct NetPnlObjective;

impl OptimizationObjective for NetPnlObjective {
    fn evaluate(&self, m: &StrategyMetrics) -> f64 {
        m.net_pnl_cents as f64
    }
    fn name(&self) -> &'static str {
        "net_pnl_cents"
    }
}

/// Sharpe-like — risk-adjusted expectancy.
#[derive(Debug, Clone, Copy)]
pub struct SharpeObjective;

impl OptimizationObjective for SharpeObjective {
    fn evaluate(&self, m: &StrategyMetrics) -> f64 {
        m.sharpe_ratio
    }
    fn name(&self) -> &'static str {
        "sharpe_ratio"
    }
}

/// Expectancy per trade.
#[derive(Debug, Clone, Copy)]
pub struct ExpectancyObjective;

impl OptimizationObjective for ExpectancyObjective {
    fn evaluate(&self, m: &StrategyMetrics) -> f64 {
        m.expectancy_cents
    }
    fn name(&self) -> &'static str {
        "expectancy_cents"
    }
}

/// v2 stub. The CLI surfaces this with a clear "not yet
/// implemented" message; v1 callers should rely on the rule-
/// based diagnoses + recommendations from `diagnose.rs`.
pub fn run_optimization(
    _space: &ParameterSpace,
    _objective: &dyn OptimizationObjective,
) -> Result<Vec<crate::recommend::Recommendation>, NotImplemented> {
    Err(NotImplemented)
}

#[derive(Debug)]
pub struct NotImplemented;

impl std::fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "v2 backtest-replay optimizer is not implemented yet — \
             v1 ships rule-based recommendations only (see `predigy-eval diagnose`)"
        )
    }
}

impl std::error::Error for NotImplemented {}
