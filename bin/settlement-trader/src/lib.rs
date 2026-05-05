//! `settlement-trader`: take Kalshi quotes near settlement when
//! the order book signals a near-locked outcome.
//!
//! See [`strategy`] for the rule mechanics + the thesis.

pub mod strategy;

pub use strategy::{SettlementConfig, SettlementStrategy};
