//! Platt-scaling calibration for NBM raw probabilities.
//!
//! Per-airport / per-month calibration coefficients live in a JSON
//! file the curator loads at startup. Each (airport, month) bucket
//! has two scalars `(a, b)` so calibrated probability is:
//!
//! ```text
//! calibrated_p = sigmoid(a + b * logit(raw_p))
//! ```
//!
//! With `(a, b) = (0, 1)` calibration is the identity. A `(a, b) =
//! (-1, 1)` shifts everything down (the model overstates by ~1
//! logit unit). `b > 1` sharpens; `b < 1` softens.
//!
//! ## Why bucket by (airport, month)?
//!
//! Bias varies. NBM may overstate `P(>90F)` at PHX in summer and
//! understate it in winter. Per-airport, per-month is the smallest
//! grouping that's defensibly non-uniform.
//!
//! ## What when no calibration exists?
//!
//! If the file is missing or has no entry for the bucket the
//! caller asks about, [`Calibration::apply_or_identity`] returns
//! the raw probability unchanged. The curator's clamp [0.02, 0.98]
//! at emit time still applies, so even uncalibrated 100% beliefs
//! get softened to 98% before they reach stat-trader.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Identity coefficients — `apply()` is the identity function.
pub const IDENTITY_COEFFS: PlattCoeffs = PlattCoeffs { a: 0.0, b: 1.0 };

/// Platt-scaling coefficients for one (airport, month) bucket.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlattCoeffs {
    pub a: f64,
    pub b: f64,
}

impl PlattCoeffs {
    /// Apply this coefficient pair to a raw probability.
    /// `raw_p` is clamped to (0, 1) internally to avoid logit
    /// blow-up — calibration of saturated probabilities is
    /// meaningless anyway since logit(0) = -inf.
    pub fn apply(&self, raw_p: f64) -> f64 {
        let p = raw_p.clamp(1e-6, 1.0 - 1e-6);
        let logit = (p / (1.0 - p)).ln();
        let z = self.a + self.b * logit;
        sigmoid(z)
    }
}

/// One bucket key. The serialised form is a string
/// `"<airport>:<month>"` for ergonomic JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub airport: String,
    pub month: u8,
}

impl BucketKey {
    pub fn new(airport: impl Into<String>, month: u8) -> Self {
        Self {
            airport: airport.into(),
            month,
        }
    }

    fn to_serial(&self) -> String {
        format!("{}:{:02}", self.airport, self.month)
    }

    fn from_serial(s: &str) -> Option<Self> {
        let (airport, month_s) = s.rsplit_once(':')?;
        let month: u8 = month_s.parse().ok()?;
        if !(1..=12).contains(&month) {
            return None;
        }
        Some(Self::new(airport, month))
    }
}

/// Calibration table — bucket → coefficients, plus metadata about
/// how many samples the fit was based on (so the operator can see
/// at a glance whether a bucket is well-supported or noisy).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Calibration {
    /// Map serialised key → (coefficients, sample_count).
    pub buckets: HashMap<String, BucketEntry>,
    /// When the calibration was fit. Operator-facing only.
    pub fitted_at_iso: Option<String>,
    /// Source description (e.g. "NCEI 2024-01..2024-12") so the
    /// operator can tell if the calibration is stale.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BucketEntry {
    pub coeffs: PlattCoeffs,
    pub n_samples: u32,
}

impl Calibration {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from a JSON file. Returns `None` if the file doesn't
    /// exist (the typical first-deploy state); errors only on
    /// corrupt JSON.
    pub fn load(path: &Path) -> Result<Option<Self>, std::io::Error> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let cal: Calibration = serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                })?;
                Ok(Some(cal))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Atomic save: write to a `.tmp` then rename. The serialised
    /// form is pretty-printed JSON so the operator can eyeball
    /// diffs between fits.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Insert a fitted bucket.
    pub fn set(&mut self, key: BucketKey, coeffs: PlattCoeffs, n_samples: u32) {
        self.buckets.insert(
            key.to_serial(),
            BucketEntry { coeffs, n_samples },
        );
    }

    /// Lookup coefficients. Falls back to the identity if no
    /// bucket exists for `(airport, month)`.
    pub fn lookup(&self, key: &BucketKey) -> PlattCoeffs {
        self.buckets
            .get(&key.to_serial())
            .map(|e| e.coeffs)
            .unwrap_or(IDENTITY_COEFFS)
    }

    /// One-shot apply. If no bucket, returns `raw_p` unchanged.
    pub fn apply_or_identity(&self, key: &BucketKey, raw_p: f64) -> f64 {
        self.lookup(key).apply(raw_p)
    }

    /// Iterate fitted buckets. Useful for the operator-facing
    /// summary table.
    pub fn iter(&self) -> impl Iterator<Item = (BucketKey, BucketEntry)> + '_ {
        self.buckets.iter().filter_map(|(k, v)| {
            BucketKey::from_serial(k).map(|key| (key, *v))
        })
    }
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

/// Fit Platt coefficients for one bucket via Newton-Raphson on the
/// negative log-likelihood. `samples` is `(raw_p, observed_outcome)`
/// where `observed_outcome ∈ {0.0, 1.0}`.
///
/// Returns `None` if the sample is too small (`< 10`) or has zero
/// variance in observations (all 0 or all 1) — in either case the
/// fit is meaningless.
pub fn fit_platt(samples: &[(f64, f64)]) -> Option<PlattCoeffs> {
    if samples.len() < 10 {
        return None;
    }
    let n_pos = samples.iter().filter(|(_, y)| *y > 0.5).count();
    if n_pos == 0 || n_pos == samples.len() {
        return None;
    }
    // Convert raw_p → logit, clamp away from 0/1.
    let logits: Vec<f64> = samples
        .iter()
        .map(|(p, _)| {
            let pp = p.clamp(1e-6, 1.0 - 1e-6);
            (pp / (1.0 - pp)).ln()
        })
        .collect();
    let ys: Vec<f64> = samples.iter().map(|(_, y)| *y).collect();

    // Newton-Raphson on (a, b). Initialise at identity.
    let mut a = 0.0;
    let mut b = 1.0;
    for _ in 0..50 {
        let mut grad_a = 0.0;
        let mut grad_b = 0.0;
        let mut h_aa = 0.0;
        let mut h_ab = 0.0;
        let mut h_bb = 0.0;
        for (i, &x) in logits.iter().enumerate() {
            let z = a + b * x;
            let p = sigmoid(z);
            let err = p - ys[i];
            grad_a += err;
            grad_b += err * x;
            let pp = p * (1.0 - p);
            h_aa += pp;
            h_ab += pp * x;
            h_bb += pp * x * x;
        }
        // Solve [[h_aa, h_ab],[h_ab, h_bb]] * delta = grad.
        let det = h_aa * h_bb - h_ab * h_ab;
        if det.abs() < 1e-12 {
            break;
        }
        let da = (h_bb * grad_a - h_ab * grad_b) / det;
        let db = (-h_ab * grad_a + h_aa * grad_b) / det;
        a -= da;
        b -= db;
        if da.abs() < 1e-9 && db.abs() < 1e-9 {
            break;
        }
    }
    if a.is_finite() && b.is_finite() {
        Some(PlattCoeffs { a, b })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use rand::Rng;

    #[test]
    fn identity_round_trip() {
        let c = IDENTITY_COEFFS;
        for raw in [0.05, 0.25, 0.5, 0.75, 0.95] {
            let cal = c.apply(raw);
            assert!(
                (cal - raw).abs() < 1e-6,
                "identity should preserve {raw}, got {cal}"
            );
        }
    }

    #[test]
    fn negative_a_shifts_down() {
        // a = -1 means we believe the model is overconfident; same
        // raw_p maps to a smaller calibrated probability.
        let c = PlattCoeffs { a: -1.0, b: 1.0 };
        assert!(c.apply(0.5) < 0.5);
        assert!(c.apply(0.9) < 0.9);
    }

    #[test]
    fn b_greater_than_one_sharpens() {
        // b > 1 amplifies the logit — extreme probabilities become
        // more extreme. raw_p=0.5 stays at 0.5 (logit 0); raw_p=0.7
        // sharpens upward.
        let c = PlattCoeffs { a: 0.0, b: 2.0 };
        assert!((c.apply(0.5) - 0.5).abs() < 1e-9);
        assert!(c.apply(0.7) > 0.7);
    }

    #[test]
    fn apply_clamps_inputs_at_boundaries() {
        let c = IDENTITY_COEFFS;
        // raw_p = 0 → after clamp ~1e-6 → output very close to 0.
        let v0 = c.apply(0.0);
        assert!(v0 > 0.0 && v0 < 0.001);
        // raw_p = 1 → after clamp ~1-1e-6 → output very close to 1.
        let v1 = c.apply(1.0);
        assert!(v1 < 1.0 && v1 > 0.999);
    }

    fn calibration_with(entries: &[(&str, u8, PlattCoeffs)]) -> Calibration {
        let mut cal = Calibration::empty();
        for (a, m, c) in entries {
            cal.set(BucketKey::new(*a, *m), *c, 100);
        }
        cal
    }

    #[test]
    fn lookup_falls_back_to_identity_for_missing_bucket() {
        let cal = calibration_with(&[("DEN", 5, PlattCoeffs { a: -1.0, b: 1.5 })]);
        let coeffs = cal.lookup(&BucketKey::new("LAX", 5));
        assert_eq!(coeffs, IDENTITY_COEFFS);
    }

    #[test]
    fn lookup_finds_matching_bucket() {
        let coeffs = PlattCoeffs { a: -0.5, b: 1.2 };
        let cal = calibration_with(&[("DEN", 5, coeffs)]);
        assert_eq!(cal.lookup(&BucketKey::new("DEN", 5)), coeffs);
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cal.json");
        let mut cal = Calibration::empty();
        cal.set(
            BucketKey::new("DEN", 5),
            PlattCoeffs { a: -0.42, b: 1.13 },
            150,
        );
        cal.set(
            BucketKey::new("LAX", 7),
            PlattCoeffs { a: 0.31, b: 0.89 },
            200,
        );
        cal.fitted_at_iso = Some("2026-05-07T03:00:00Z".into());
        cal.source = Some("NCEI backfill 2023-01..2025-12".into());
        cal.save(&path).unwrap();

        let loaded = Calibration::load(&path).unwrap().unwrap();
        assert_eq!(loaded.buckets.len(), 2);
        assert_eq!(
            loaded.lookup(&BucketKey::new("DEN", 5)),
            PlattCoeffs { a: -0.42, b: 1.13 }
        );
        assert_eq!(
            loaded.lookup(&BucketKey::new("LAX", 7)),
            PlattCoeffs { a: 0.31, b: 0.89 }
        );
        assert_eq!(loaded.fitted_at_iso.as_deref(), Some("2026-05-07T03:00:00Z"));
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-written.json");
        assert!(Calibration::load(&path).unwrap().is_none());
    }

    #[test]
    fn load_corrupt_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"not-json").unwrap();
        assert!(Calibration::load(&path).is_err());
    }

    #[test]
    fn bucket_key_serial_round_trip() {
        let k = BucketKey::new("DEN", 5);
        assert_eq!(k.to_serial(), "DEN:05");
        let back = BucketKey::from_serial("DEN:05").unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn bucket_key_handles_4_letter_codes() {
        let k = BucketKey::new("PHIL", 12);
        assert_eq!(k.to_serial(), "PHIL:12");
        assert_eq!(BucketKey::from_serial("PHIL:12").unwrap(), k);
    }

    #[test]
    fn bucket_key_rejects_bad_serial() {
        assert!(BucketKey::from_serial("DEN").is_none());
        assert!(BucketKey::from_serial("DEN:13").is_none()); // month out of range
        assert!(BucketKey::from_serial("DEN:00").is_none());
        assert!(BucketKey::from_serial("DEN:abc").is_none());
    }

    /// Synthetic-data calibration test. Generate `(raw_p, outcome)`
    /// pairs from a known overconfident model: the model predicts
    /// `raw_p` but reality has actual probability `q` with
    /// `logit(q) = -0.5 + 0.7 * logit(raw_p)`. The Platt fit should
    /// recover `(a, b) ≈ (-0.5, 0.7)`.
    #[test]
    fn fit_platt_recovers_known_calibration_curve() {
        let mut rng = StdRng::seed_from_u64(42);
        let true_a = -0.5;
        let true_b = 0.7;
        let mut samples: Vec<(f64, f64)> = Vec::new();
        for _ in 0..2000 {
            // Draw a raw_p uniformly from logits in [-3, 3] for
            // good coverage of the calibration curve.
            let logit_raw: f64 = rng.gen_range(-3.0..=3.0);
            let raw_p = sigmoid(logit_raw);
            let logit_true = true_a + true_b * logit_raw;
            let true_p = sigmoid(logit_true);
            let outcome: f64 = if rng.r#gen::<f64>() < true_p { 1.0 } else { 0.0 };
            samples.push((raw_p, outcome));
        }
        let fit = fit_platt(&samples).unwrap();
        assert!(
            (fit.a - true_a).abs() < 0.1,
            "expected a≈{true_a}, got {}",
            fit.a
        );
        assert!(
            (fit.b - true_b).abs() < 0.1,
            "expected b≈{true_b}, got {}",
            fit.b
        );
    }

    #[test]
    fn fit_platt_returns_none_for_too_small_sample() {
        let samples: Vec<(f64, f64)> = (0..5).map(|i| (0.5, if i % 2 == 0 { 1.0 } else { 0.0 })).collect();
        assert!(fit_platt(&samples).is_none());
    }

    #[test]
    fn fit_platt_returns_none_for_constant_outcomes() {
        // All outcomes are 1 — no variance, can't fit logistic.
        let samples: Vec<(f64, f64)> = (0..50).map(|_| (0.7, 1.0)).collect();
        assert!(fit_platt(&samples).is_none());
    }
}
