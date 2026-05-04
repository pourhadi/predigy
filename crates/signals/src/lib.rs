//! Probability and sizing primitives for the statistical alpha
//! strategies (Phase 7).
//!
//! Three pure-math modules, all `#![forbid(unsafe_code)]` and
//! allocation-free on the hot path:
//!
//! - [`bayes`]: Beta-Binomial conjugate posterior for binary events.
//!   Strategies use it to update a running probability estimate as
//!   evidence streams in (e.g. pre-game ratings + live in-game
//!   features → updated posterior on outcome).
//! - [`elo`]: paired-competitor rating system. The standard chess /
//!   sports model; calibrate `K` per league.
//! - [`kelly`]: position-sizing fraction for binary contracts at a
//!   given ask, with a fractional-Kelly modifier for real-world
//!   model-mis-calibration robustness.
//!
//! The `predigy_core::price`/`Qty` integration is intentional but
//! light: [`kelly::contracts_to_buy`] returns a `u32` directly so
//! strategies can plug it into `Intent::limit(...)` without a
//! conversion layer.

pub mod bayes;
pub mod elo;
pub mod kelly;

pub use bayes::{BayesError, Posterior};
pub use elo::{Outcome as EloOutcome, Rating, update as elo_update, win_probability};
pub use kelly::{KellyError, contracts_to_buy, fraction as kelly_fraction, fraction_with_factor};
