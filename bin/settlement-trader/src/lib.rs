//! `settlement-trader`: take Kalshi quotes near settlement when
//! the order book signals a near-locked outcome.
//!
//! See [`strategy`] for the rule mechanics + the thesis.

pub mod discovery;
pub mod strategy;

pub use discovery::{DEFAULT_SERIES, DiscoveryConfig, DiscoveryDelta};
pub use strategy::{SettlementConfig, SettlementStrategy};
