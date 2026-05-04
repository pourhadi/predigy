//! Beta-Binomial conjugate-prior estimator for a single binary
//! event probability.
//!
//! Given a prior `Beta(α, β)` and observed evidence — successes `k`
//! out of trials `n` — the posterior is `Beta(α + k, β + n − k)`.
//! The MAP point estimate is `(α + k − 1) / (α + β + n − 2)` (when
//! `α, β ≥ 1`); the posterior mean is `(α + k) / (α + β + n)`.
//! We expose both and let the strategy decide which to act on.
//!
//! Posterior variance is `αβ / ((α + β)² (α + β + 1))` — useful for
//! Kelly sizing where the variance term reduces the fraction.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum BayesError {
    #[error("alpha and beta must be > 0; got alpha={alpha}, beta={beta}")]
    NonPositiveParam { alpha: f64, beta: f64 },
    #[error("successes ({successes}) cannot exceed trials ({trials})")]
    InvalidEvidence { successes: u64, trials: u64 },
}

/// Beta-Binomial posterior.
///
/// `alpha` and `beta` are the posterior pseudocounts: start with
/// your prior, call [`Posterior::observe`] to fold in evidence.
/// Construct with [`Posterior::uniform`] for a flat prior or
/// [`Posterior::with_prior`] to inject expert prior.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Posterior {
    pub alpha: f64,
    pub beta: f64,
}

impl Posterior {
    /// `Beta(1, 1)` — uniform on `[0, 1]`. Maximum entropy when no
    /// prior information is available.
    #[must_use]
    pub fn uniform() -> Self {
        Self {
            alpha: 1.0,
            beta: 1.0,
        }
    }

    /// Construct from `(alpha, beta)`. Both must be strictly positive.
    pub fn with_prior(alpha: f64, beta: f64) -> Result<Self, BayesError> {
        if !alpha.is_finite() || !beta.is_finite() || alpha <= 0.0 || beta <= 0.0 {
            return Err(BayesError::NonPositiveParam { alpha, beta });
        }
        Ok(Self { alpha, beta })
    }

    /// Construct a prior centred on `prior_mean ∈ (0, 1)` with the
    /// given `effective_n` "pseudo-trials." A weak prior (small
    /// `effective_n`) lets observations dominate quickly; a strong
    /// prior anchors the posterior.
    pub fn from_mean_and_strength(prior_mean: f64, effective_n: f64) -> Result<Self, BayesError> {
        // prior_mean must be strictly in (0, 1) — boundary values
        // give degenerate priors. The float-compare lint complains
        // about the equality checks, but with finite rationals these
        // are intentional rejections of exactly-0.0 / exactly-1.0.
        #[allow(clippy::float_cmp)]
        let invalid = !(0.0..=1.0).contains(&prior_mean) || prior_mean == 0.0 || prior_mean == 1.0;
        if invalid {
            return Err(BayesError::NonPositiveParam {
                alpha: prior_mean,
                beta: 1.0 - prior_mean,
            });
        }
        if effective_n <= 0.0 || !effective_n.is_finite() {
            return Err(BayesError::NonPositiveParam {
                alpha: effective_n,
                beta: effective_n,
            });
        }
        let alpha = prior_mean * effective_n;
        let beta = (1.0 - prior_mean) * effective_n;
        Self::with_prior(alpha, beta)
    }

    /// Fold `successes` out of `trials` observations into the
    /// posterior. `successes <= trials`.
    pub fn observe(&mut self, successes: u64, trials: u64) -> Result<(), BayesError> {
        if successes > trials {
            return Err(BayesError::InvalidEvidence { successes, trials });
        }
        // u64 → f64 may lose precision past 2^53, but observation
        // counts in our setting are far below that (trial counts of
        // a few thousand per market per day). Allow the cast.
        #[allow(clippy::cast_precision_loss)]
        {
            self.alpha += successes as f64;
            self.beta += (trials - successes) as f64;
        }
        Ok(())
    }

    /// Posterior mean — `α / (α + β)`. The default point estimate.
    #[must_use]
    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    /// MAP estimate — `(α − 1) / (α + β − 2)`. Equivalent to the
    /// posterior mean when `α = β = 1` (uniform prior, MLE).
    /// Returns the posterior mean if either pseudocount drops below
    /// one (e.g. very weak prior), where the MAP isn't well-defined.
    #[must_use]
    pub fn map(&self) -> f64 {
        let denom = self.alpha + self.beta - 2.0;
        if self.alpha < 1.0 || self.beta < 1.0 || denom <= 0.0 {
            return self.mean();
        }
        (self.alpha - 1.0) / denom
    }

    /// Posterior variance — `αβ / ((α + β)² (α + β + 1))`.
    #[must_use]
    pub fn variance(&self) -> f64 {
        let s = self.alpha + self.beta;
        (self.alpha * self.beta) / (s * s * (s + 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn uniform_prior_is_half() {
        let p = Posterior::uniform();
        assert!(approx(p.mean(), 0.5, 1e-12));
        assert!(approx(p.map(), 0.5, 1e-12));
    }

    #[test]
    fn observation_pulls_mean_toward_evidence() {
        let mut p = Posterior::uniform();
        p.observe(70, 100).unwrap();
        // Beta(71, 31). Mean = 71 / 102 ≈ 0.696
        assert!(approx(p.mean(), 71.0 / 102.0, 1e-12));
    }

    #[test]
    fn map_matches_mle_on_uniform_prior() {
        let mut p = Posterior::uniform();
        p.observe(7, 10).unwrap();
        // Beta(8, 4). MAP = (8-1) / (12-2) = 7/10 = 0.7 = MLE.
        assert!(approx(p.map(), 0.7, 1e-12));
    }

    #[test]
    fn strong_prior_resists_evidence() {
        // Prior strongly anchored at 0.5 with N=1000 pseudo-trials.
        let mut p = Posterior::from_mean_and_strength(0.5, 1000.0).unwrap();
        p.observe(3, 3).unwrap();
        // Posterior ≈ Beta(503, 500). Mean ≈ 0.5015 — barely moved.
        assert!(approx(p.mean(), 503.0 / 1003.0, 1e-9));
        assert!((p.mean() - 0.5).abs() < 0.01);
    }

    #[test]
    fn weak_prior_lets_evidence_dominate() {
        let mut p = Posterior::from_mean_and_strength(0.5, 2.0).unwrap();
        p.observe(80, 100).unwrap();
        // Posterior = Beta(81, 21). Mean = 81/102 ≈ 0.794 — close to 0.8.
        assert!((p.mean() - 0.8).abs() < 0.01);
    }

    #[test]
    fn variance_decreases_as_evidence_accumulates() {
        let mut p = Posterior::uniform();
        let v0 = p.variance();
        p.observe(50, 100).unwrap();
        let v1 = p.variance();
        p.observe(50, 100).unwrap();
        let v2 = p.variance();
        assert!(
            v0 > v1 && v1 > v2,
            "variance must shrink: {v0} > {v1} > {v2}"
        );
    }

    #[test]
    fn rejects_evidence_with_more_successes_than_trials() {
        let mut p = Posterior::uniform();
        let err = p.observe(10, 5).unwrap_err();
        match err {
            BayesError::InvalidEvidence { successes, trials } => {
                assert_eq!(successes, 10);
                assert_eq!(trials, 5);
            }
            BayesError::NonPositiveParam { .. } => {
                panic!("expected InvalidEvidence, got NonPositiveParam")
            }
        }
    }

    #[test]
    fn rejects_non_positive_alpha() {
        assert!(Posterior::with_prior(0.0, 1.0).is_err());
        assert!(Posterior::with_prior(-1.0, 1.0).is_err());
        assert!(Posterior::with_prior(f64::NAN, 1.0).is_err());
    }

    #[test]
    fn rejects_invalid_mean_for_from_mean_and_strength() {
        assert!(Posterior::from_mean_and_strength(0.0, 10.0).is_err());
        assert!(Posterior::from_mean_and_strength(1.0, 10.0).is_err());
        assert!(Posterior::from_mean_and_strength(0.5, 0.0).is_err());
    }

    #[test]
    fn map_falls_back_to_mean_under_weak_pseudocounts() {
        // Beta(0.5, 0.5) — Jeffreys prior. MAP isn't defined; we
        // return the mean.
        let p = Posterior::with_prior(0.5, 0.5).unwrap();
        assert!(approx(p.map(), p.mean(), 1e-12));
    }
}
