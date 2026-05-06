//! `cross-arb-curator`: scans Kalshi political markets + Polymarket
//! markets, asks Claude to identify high-confidence cross-venue
//! pairs, and writes them to a `cross-arb-pairs.txt` the
//! `cross-arb-trader` binary can consume.
//!
//! The hard problem in cross-arb is settlement divergence —
//! Polymarket resolves on AP/news calls, Kalshi resolves on
//! official certifications. The curator's job is to find pairs
//! where the resolution events match closely enough that the
//! convergence trade is safe. See `prompt.rs` for the criteria.

pub mod agent;
pub mod kalshi_scan;
pub mod keyword_filter;
pub mod poly_scan;
pub mod prompt;

pub use agent::{CuratedPair, CuratorError, propose_pairs};
pub use kalshi_scan::{KalshiMarket, scan_political_markets};
pub use keyword_filter::filter_for_batch;
pub use poly_scan::{PolyError, PolyMarket, scan_top_markets};
