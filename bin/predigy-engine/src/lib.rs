// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `predigy-engine`: the consolidated trading engine.
//!
//! Replaces the per-strategy binaries (`stat-trader`,
//! `cross-arb-trader`, `latency-trader`, `settlement-trader`,
//! `wx-stat-curator`'s daemon role) with a single process that
//! owns the Kalshi connections, the OMS, and a registry of
//! strategy modules.
//!
//! See `docs/ARCHITECTURE.md` for the target architecture and
//! migration plan; this crate is the engine binary itself plus
//! the in-process subsystems (config, OMS, supervisor,
//! market-data router, reconciliation loop).

pub mod config;
pub mod cross_strategy_bus;
pub mod discovery_service;
pub mod exec_data;
pub mod external_feeds;
pub mod market_data;
pub mod oms_db;
pub mod pair_file_service;
pub mod registry;
pub mod supervisor;
pub mod venue_rest;

pub use config::EngineConfig;
pub use oms_db::DbBackedOms;
pub use registry::{StrategyHandle, StrategyRegistry};
pub use supervisor::Supervisor;
