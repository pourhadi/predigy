//! Recommendation type — paired with each `Diagnosis` in
//! `diagnose.rs`.
//!
//! v1 recommendations are produced inline by the diagnostic rules
//! (since each diagnosis has a small fixed set of remediation
//! actions). The future ML-driven optimizer (v2 / `optimize.rs`)
//! will produce richer `Recommendation` records with backtest
//! evidence.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub strategy: String,
    pub action: ActionKind,
    pub current_value: serde_json::Value,
    pub proposed_value: serde_json::Value,
    pub rationale: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionKind {
    RaiseMinEdge { current: i32, proposed: i32 },
    LowerMinEdge { current: i32, proposed: i32 },
    TightenStopLoss { current: i32, proposed: i32 },
    WidenStopLoss { current: i32, proposed: i32 },
    AddTrailingStop { trigger: i32, distance: i32 },
    LowerThreshold { which: String, current: f64, proposed: f64 },
    RaiseThreshold { which: String, current: f64, proposed: f64 },
    RaiseRiskCap { which: String, current: i64, proposed: i64 },
    DisableStrategy { reason: String },
    EnableStrategy { reason: String },
    Investigate { what: String },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// Suggested visual/log prefix for reports.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}
