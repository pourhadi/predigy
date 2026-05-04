//! Library half of the `latency-trader` binary so the strategy can
//! be exercised by unit tests without the CLI.

pub mod strategy;

pub use strategy::{LatencyRule, LatencyStrategy, Severity};
