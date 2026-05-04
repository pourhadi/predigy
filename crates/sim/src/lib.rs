//! In-process backtesting simulator for predigy strategies.
//!
//! The plan calls for "All strategies must run unchanged in sim and
//! live (same `Strategy` trait, same `Intent` outputs)." The sim
//! achieves that by plugging into the same OMS via the same
//! [`predigy_oms::Executor`] trait — strategy code that runs
//! against `predigy-kalshi-exec::RestExecutor` in production runs
//! unchanged against [`SimExecutor`] here.
//!
//! ## Pieces
//!
//! - [`BookStore`]: shared per-market `OrderBook` state.
//! - [`SimExecutor`]: in-memory `oms::Executor`. Matches IOC orders
//!   against the touch and emits `ExecutionReport`s on the same
//!   channel the real executor would. Mutates the book to consume
//!   matched liquidity.
//! - [`Replay`]: streams `md-recorder` NDJSON files through the
//!   `BookStore`, calling a user-supplied async hook after each book
//!   update so the strategy can run inline.
//! - [`matching::match_ioc`]: the pure matching primitive (exposed
//!   for tests and for callers wanting to drive synthetic events
//!   without going through `Replay`).
//!
//! ## Scope today
//!
//! - **IOC only**. The strategies currently in the repo (intra-venue
//!   arb) only use IOC. GTC + queue-position modelling lands when a
//!   resting-quote strategy (market making) needs it.
//! - **Single-level matching**. The sim walks the touch only — fine
//!   for "lift one level" strategies and good enough for `arb-trader`.
//! - **No fee-aware best-execution**. Fills at the touch price, not
//!   somewhere between bid/ask. Matches Kalshi's actual behaviour for
//!   IOC takers.

pub mod book_store;
pub mod executor;
pub mod matching;
pub mod queue;
pub mod replay;

pub use book_store::BookStore;
pub use executor::SimExecutor;
pub use matching::{Match, match_ioc};
pub use queue::{QueueAdvance, RestingOrder, TradePulse, apply_trade, synth_fill};
pub use replay::{Replay, ReplayError, ReplayUpdate};
