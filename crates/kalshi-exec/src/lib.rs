//! REST-based Kalshi executor for the OMS.
//!
//! Implements [`predigy_oms::Executor`] over `predigy_kalshi_rest`.
//! Submits and cancels go straight to the REST V2 endpoints; fills
//! flow through a polling task that hits `/portfolio/fills` on a
//! configurable interval and emits `ExecutionReport`s on the same
//! channel as the synchronous Acked/Rejected/Cancelled events.
//!
//! ## Why REST first
//!
//! The plan calls for FIX 4.4 as the primary execution path with a
//! REST fallback "behind the same trait." This crate is the REST
//! variant, deliberately built first to unblock the intra-venue arb
//! strategy (which doesn't need sub-millisecond fill latency). FIX
//! lands as a sibling crate (`predigy-kalshi-fix`) once a market-
//! making strategy is on the roadmap.
//!
//! ## Quick start
//!
//! ```no_run
//! use predigy_kalshi_exec::{PollerConfig, RestExecutor};
//! use predigy_kalshi_rest::Client as RestClient;
//! # async fn run(rest: RestClient) -> Result<(), Box<dyn std::error::Error>> {
//! let (executor, reports) = RestExecutor::spawn(rest, PollerConfig::default());
//! // hand both into Oms::spawn:
//! //   let oms = Oms::spawn(oms_config, risk_engine, executor, reports);
//! # Ok(()) }
//! ```

pub mod error;
pub mod executor;
pub mod mapping;

pub use error::Error;
pub use executor::{PollerConfig, RestExecutor};
