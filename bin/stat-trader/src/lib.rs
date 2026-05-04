//! Library half of the `stat-trader` binary so the strategy can be
//! unit-tested without the CLI.

pub mod strategy;

pub use strategy::{StatConfig, StatRule, StatStrategy};
