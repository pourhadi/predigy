//! Library half of the `arb-trader` binary — pulled into a separate
//! library so the strategy can be exercised by unit tests (and a
//! future backtester) without dragging in the binary's CLI.

pub mod runner;
pub mod strategy;

pub use runner::{Runner, RunnerConfig};
pub use strategy::{ArbConfig, ArbOpportunity, ArbStrategy, Evaluation};
