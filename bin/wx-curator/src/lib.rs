//! `wx-curator`: agent that scans Kalshi's open weather markets,
//! reasons about which NWS alert types correlate with each, and
//! produces an EV-positive `LatencyRule` set for `latency-trader`.
//!
//! The reasoning step calls the Anthropic API (Claude Sonnet 4.6).
//! Hand-coded keyword matching would be brittle: Kalshi's market
//! titles are free-form ("Highest temperature in NYC today",
//! "Will a major hurricane hit Florida this season?"), and the
//! mapping to NWS event types ("Excessive Heat Warning",
//! "Hurricane Warning") is exactly the fuzzy semantic problem
//! LLMs handle well.
//!
//! Cost: one Anthropic call per ~10 markets bundled, ~\$0.01-0.05
//! per full scan of Kalshi's ~50-100 active weather markets.
//!
//! Output: a JSON array of [`latency_trader::LatencyRule`] entries.

pub mod agent;
pub mod kalshi_scan;
pub mod prompt;

pub use agent::{CuratedRule, CuratorError, propose_rules};
pub use kalshi_scan::{WeatherMarket, scan_weather_markets};
