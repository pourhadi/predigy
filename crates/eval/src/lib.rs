// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-eval` — strategy evaluation framework.
//!
//! See `docs/EVAL_SPEC.md` for the design doc and rationale. This
//! crate is the library — the CLI lives in `bin/predigy-eval/`,
//! and the dashboard's JSON endpoints call into the same
//! functions.
//!
//! ## Top-level flow
//!
//! ```text
//! load_trades(db, window)
//!     -> Vec<Trade>
//! compute_metrics(&trades)
//!     -> HashMap<strategy, StrategyMetrics>
//! diagnose(metrics, &trades)
//!     -> Vec<Diagnosis>
//! recommend(diagnosis)
//!     -> Vec<Recommendation>     (already attached to each Diagnosis)
//! render_markdown_report(&metrics, &diagnoses)
//!     -> String
//! ```
//!
//! All four phases are pure (no IO after `load_trades`), so the
//! library is trivially testable with synthetic trades.

pub mod diagnose;
pub mod ledger;
pub mod metrics;
pub mod optimize;
pub mod recommend;
pub mod report;
pub mod time_window;
pub mod types;

pub use diagnose::{Diagnosis, DiagnosisCode, Severity, diagnose};
pub use ledger::load_trades;
pub use metrics::{StrategyMetrics, compute_metrics};
pub use recommend::{ActionKind, Confidence, Recommendation};
pub use report::render_markdown_report;
pub use time_window::TimeWindow;
pub use types::{ExitReason, Trade};
