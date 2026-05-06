// Polymarket / Kalshi product names show up frequently in docs; same
// rationale as the kalshi-fix crate's allow.
#![allow(clippy::doc_markdown)]

//! Library half of the `cross-arb-trader` binary so the strategy
//! can be exercised by unit tests without dragging in the CLI.

pub mod pair_file;
pub mod strategy;

pub use strategy::{CrossArbConfig, CrossArbStrategy, PolyRef};
