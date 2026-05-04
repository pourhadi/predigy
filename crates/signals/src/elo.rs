//! Elo rating system — paired competitor probability model.
//!
//! Used for sports markets. Each competitor has an integer-ish
//! rating; the predicted probability that A beats B is
//!
//! ```text
//! P(A wins) = 1 / (1 + 10^((Rb − Ra) / 400))
//! ```
//!
//! After a match outcome `S ∈ {1, 0.5, 0}` (win/draw/loss for A) the
//! ratings update by
//!
//! ```text
//! Ra' = Ra + K (S − P(A wins))
//! Rb' = Rb + K ((1 − S) − P(B wins))
//! ```
//!
//! `K` controls update speed. Defaults to 32 (FIDE chess
//! convention); sport leagues use values from 8 (NFL) to 40 (high-
//! variance esports). Pick what calibrates to your data.
//!
//! No allocation, no heap state — call sites typically keep a
//! `HashMap<TeamId, Rating>` and feed into the helper functions
//! here.

use serde::{Deserialize, Serialize};

/// Default K-factor matching the FIDE chess convention.
pub const DEFAULT_K: f64 = 32.0;

/// Rating, in Elo points. Stored as `f64` so partial-credit updates
/// (draws) don't truncate. Convert to integer for display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rating(pub f64);

impl Rating {
    pub const STARTING: Self = Self(1500.0);

    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }
}

/// Predicted probability that `a` beats `b`.
#[must_use]
pub fn win_probability(a: Rating, b: Rating) -> f64 {
    let diff = b.0 - a.0;
    1.0 / (1.0 + 10f64.powf(diff / 400.0))
}

/// Outcome of a match from A's perspective: 1.0 win, 0.5 draw,
/// 0.0 loss.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome(pub f64);

impl Outcome {
    pub const A_WINS: Self = Self(1.0);
    pub const DRAW: Self = Self(0.5);
    pub const A_LOSES: Self = Self(0.0);

    /// Build from a raw score, clamped to `[0, 1]`. Useful for
    /// fractional outcomes (e.g. won-by-margin).
    #[must_use]
    pub fn clamped(score: f64) -> Self {
        Self(score.clamp(0.0, 1.0))
    }
}

/// Updated ratings after a match. K-factor [`DEFAULT_K`].
#[must_use]
pub fn update(a: Rating, b: Rating, outcome: Outcome) -> (Rating, Rating) {
    update_with_k(a, b, outcome, DEFAULT_K)
}

/// As [`update`] but with an explicit K-factor.
#[must_use]
pub fn update_with_k(a: Rating, b: Rating, outcome: Outcome, k: f64) -> (Rating, Rating) {
    let pa = win_probability(a, b);
    let pb = 1.0 - pa;
    let s_a = outcome.0;
    let s_b = 1.0 - s_a;
    (Rating(a.0 + k * (s_a - pa)), Rating(b.0 + k * (s_b - pb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn equal_ratings_imply_50_percent() {
        let p = win_probability(Rating(1500.0), Rating(1500.0));
        assert!(approx(p, 0.5, 1e-12));
    }

    #[test]
    fn higher_rating_implies_higher_probability() {
        let pa = win_probability(Rating(1700.0), Rating(1500.0));
        let pb = win_probability(Rating(1500.0), Rating(1700.0));
        assert!(pa > 0.5);
        assert!(pb < 0.5);
        // Probabilities of disjoint outcomes sum to 1.
        assert!(approx(pa + pb, 1.0, 1e-12));
    }

    #[test]
    fn classic_400_point_gap_is_about_91_percent() {
        // 400 Elo gap → 10^1 = 10:1 odds → 10/11 ≈ 0.909.
        let p = win_probability(Rating(1900.0), Rating(1500.0));
        assert!(approx(p, 10.0 / 11.0, 1e-9));
    }

    #[test]
    fn winning_increases_rating_loser_decreases() {
        let (a, b) = update(Rating(1500.0), Rating(1500.0), Outcome::A_WINS);
        assert!(a.0 > 1500.0);
        assert!(b.0 < 1500.0);
        // Equal-rating win/loss is symmetric: |Δa| == |Δb|.
        assert!(approx(a.0 - 1500.0, 1500.0 - b.0, 1e-12));
    }

    #[test]
    fn draw_between_equal_ratings_is_no_op() {
        let (a, b) = update(Rating(1500.0), Rating(1500.0), Outcome::DRAW);
        assert!(approx(a.0, 1500.0, 1e-12));
        assert!(approx(b.0, 1500.0, 1e-12));
    }

    #[test]
    fn upset_win_moves_rating_more_than_expected_win() {
        // Underdog (1300) beats favourite (1700) — big swing.
        let (a_up, _) = update(Rating(1300.0), Rating(1700.0), Outcome::A_WINS);
        // Favourite (1700) beats underdog (1300) — small swing.
        let (a_fav, _) = update(Rating(1700.0), Rating(1300.0), Outcome::A_WINS);
        let upset_delta = a_up.0 - 1300.0;
        let expected_delta = a_fav.0 - 1700.0;
        assert!(upset_delta > expected_delta);
    }

    #[test]
    fn k_factor_scales_update_magnitude() {
        let (a_low, _) = update_with_k(Rating(1500.0), Rating(1500.0), Outcome::A_WINS, 8.0);
        let (a_high, _) = update_with_k(Rating(1500.0), Rating(1500.0), Outcome::A_WINS, 40.0);
        let low_delta = a_low.0 - 1500.0;
        let high_delta = a_high.0 - 1500.0;
        assert!(approx(high_delta, low_delta * 5.0, 1e-9));
    }

    #[test]
    fn outcome_clamps_to_unit_interval() {
        assert!(approx(Outcome::clamped(2.0).0, 1.0, 1e-12));
        assert!(approx(Outcome::clamped(-0.5).0, 0.0, 1e-12));
        assert!(approx(Outcome::clamped(0.7).0, 0.7, 1e-12));
    }

    #[test]
    fn rating_roundtrip_through_serde() {
        let r = Rating::new(1234.56);
        let s = serde_json::to_string(&r).unwrap();
        let back: Rating = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
