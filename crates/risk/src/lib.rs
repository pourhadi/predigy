//! Pre-trade risk checks and breakers.
//!
//! `predigy-risk` is the synchronous, in-process gate every order
//! must pass through before it leaves the OMS. Per the plan
//! (`docs/PLAN.md`): no order leaves OMS without a successful
//! `risk::check(intent, state)` on the calling thread, and changes to
//! this crate require two-person review.
//!
//! ## Quick start
//!
//! ```
//! use predigy_core::{Action, Intent, MarketTicker, Price, Qty, Side};
//! use predigy_risk::{AccountState, Decision, Limits, PerMarketLimits, RiskEngine};
//!
//! let mut limits = Limits::default();
//! limits.per_market = PerMarketLimits {
//!     max_contracts_per_side: 1_000,
//!     max_notional_cents_per_side: 50_000,
//! };
//! let engine = RiskEngine::new(limits);
//! let mut state = AccountState::new();
//!
//! let intent = Intent::limit(
//!     MarketTicker::new("FED-23DEC-T3.00"),
//!     Side::Yes,
//!     Action::Buy,
//!     Price::from_cents(42).unwrap(),
//!     Qty::new(100).unwrap(),
//! );
//! assert!(matches!(
//!     engine.check(&intent, &mut state, std::time::Instant::now()),
//!     Decision::Approve
//! ));
//! ```
//!
//! ## What's covered (and what isn't)
//!
//! Covered:
//! - Per-market position cap (per side).
//! - Per-market notional cap (per side).
//! - Account-wide gross notional cap.
//! - Daily-loss breaker.
//! - Order-rate breaker (sliding window).
//! - Kill switch.
//!
//! Deferred (Phase 2 follow-ups, tracked in `docs/STATUS.md`):
//! - Drawdown breaker over an intraday window (the daily breaker is
//!   the floor; finer-grained breakers come with the OMS).
//! - Margin / cash availability (Kalshi prepays in cash; we'll model
//!   this when the OMS lands).
//! - Implicit "sell-with-no-position → buy-opposite-side" Kalshi
//!   semantics (the OMS rejects ambiguous intents before they reach us).

pub mod engine;
pub mod limits;
pub mod state;

pub use engine::{Decision, Reason, RiskEngine};
pub use limits::{AccountLimits, Limits, PerMarketLimits, RateLimits};
pub use state::{AccountState, PersistedAccountState, PersistedPositionEntry};
