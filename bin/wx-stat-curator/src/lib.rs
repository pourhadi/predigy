// Vendor / product names appear throughout the doc comments.
#![allow(clippy::doc_markdown)]

//! `wx-stat-curator`: scan Kalshi temperature markets, compute a
//! calibrated `model_p` for each from the NWS hourly point forecast,
//! and emit `StatRule[]` for the existing `stat-trader` to execute.
//!
//! This is the deterministic-forecast cousin of `stat-curator` (which
//! uses Claude for probability calibration). For temperature markets
//! we have a direct quantitative source — NWS — so no LLM is needed
//! in the hot path.
//!
//! See `docs/WX_STAT_PLAN.md` for the full design.

pub mod airports;
pub mod calibration;
pub mod forecast_to_p;
pub mod kalshi_scan;
pub mod nbm_curate;
pub mod nbm_path;
pub mod ticker_parse;

pub use airports::{Airport, lookup_airport};
pub use forecast_to_p::{ForecastDecision, ProbabilityConfig, derive_model_p};
pub use kalshi_scan::{TempMarket, scan_temp_markets};
pub use ticker_parse::{TempMarketSpec, TempStrikeKind, parse_temp_market};
