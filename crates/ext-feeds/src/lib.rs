// Vendor / product names appear frequently in module docs.
#![allow(clippy::doc_markdown)]

//! External data feeds (Phase 6 — news/data latency alpha).
//!
//! Each feed is a long-running tokio task that pushes typed events
//! through an `mpsc::Receiver`. Strategies subscribe to whichever
//! feeds matter and react via the OMS.
//!
//! ## Feeds today
//!
//! - [`nws`]: NWS active-alerts (free, polled). Tornado/severe
//!   warnings, heat advisories, etc. Useful for Kalshi weather
//!   markets.
//! - [`nws_forecast`]: NWS hourly point forecast (free, pull-on-
//!   demand). Used by `wx-stat-curator` to compute model_p for
//!   Kalshi temperature markets.
//!
//! ## Roadmap
//!
//! Per `docs/PLAN.md`:
//!
//! - BLS direct downloads (CPI, NFP) — Phase 6, embargo race.
//! - ESPN play-by-play (free) — Phase 6, sports markets.
//! - Coinbase / Binance WS — Phase 6, crypto reference price.
//! - Bluesky firehose — Phase 6, social signal.
//! - SportRadar / OpticOdds — Phase 6, paid (≥$50k–$250k account).
//! - Bloomberg / Refinitiv — Phase 6/7, deep-pocket macro.
//!
//! Adding a new feed: implement a `spawn(...) -> Result<(Receiver,
//! JoinHandle), Error>` function returning typed events. No common
//! `Feed` trait yet — the event shapes vary too much (binary alerts,
//! tabular macro releases, level-2 books) for a useful unification.
//! When two feeds happen to share a shape we'll extract it.

pub mod error;
pub mod nws;
pub mod nws_forecast;

pub use error::Error;
pub use nws::{MIN_POLL_INTERVAL, NwsAlert, NwsAlertsConfig, parse_collection, spawn as spawn_nws};
pub use nws_forecast::{
    GridPoint, HourlyForecast, HourlyForecastEntry, NwsForecastClient, parse_hourly_response,
};
