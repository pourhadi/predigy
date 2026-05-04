//! Kelly-fraction position sizing for binary contracts.
//!
//! Given a model probability `p` that a contract pays out and a
//! market ask price `a` (in dollars, `0 < a < 1`), buying a contract
//! costs `a` and pays $1 if right, $0 if wrong. The expected
//! growth-maximising fraction of bankroll to allocate is
//!
//! ```text
//! f* = max(0, (p − a) / (1 − a))
//! ```
//!
//! (The general Kelly formula `f = (bp − q) / b` reduces to this
//! when `b = (1 − a) / a` and `q = 1 − p` for a binary contract.)
//!
//! Real strategies use a *fractional* Kelly (typically 0.1×–0.5×) to
//! cushion against model error. We expose both the full Kelly and a
//! `with_factor` variant.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KellyError {
    #[error("price must be in (0, 1); got {0}")]
    BadPrice(String),
    #[error("probability must be in [0, 1]; got {0}")]
    BadProbability(String),
    #[error("kelly factor must be in [0, 1]; got {0}")]
    BadFactor(String),
}

/// Full-Kelly fraction of bankroll for a binary buy at `ask` with
/// model probability `model_p`. Returns `0.0` when there's no edge
/// (Kelly never instructs you to short, and short-on-Kalshi is just
/// "buy the other side" anyway).
pub fn fraction(model_p: f64, ask: f64) -> Result<f64, KellyError> {
    validate(model_p, ask)?;
    let raw = (model_p - ask) / (1.0 - ask);
    Ok(raw.clamp(0.0, 1.0))
}

/// As [`fraction`] but scaled by `factor ∈ [0, 1]`. Use 0.25–0.5 in
/// practice; full Kelly is volatile and very intolerant of model
/// mis-calibration.
pub fn fraction_with_factor(model_p: f64, ask: f64, factor: f64) -> Result<f64, KellyError> {
    if !(0.0..=1.0).contains(&factor) || !factor.is_finite() {
        return Err(KellyError::BadFactor(factor.to_string()));
    }
    Ok(fraction(model_p, ask)? * factor)
}

/// Convert a Kelly fraction into an integer contract size given a
/// bankroll (in cents) and the ask price (in cents). Caps at
/// `max_contracts` to honour strategy- or risk-level position
/// limits.
///
/// `bankroll_cents × kelly_fraction / ask_cents` is the unrounded
/// contract count. We floor — rounding up risks slipping over the
/// intended exposure.
#[must_use]
pub fn contracts_to_buy(
    bankroll_cents: u64,
    ask_cents: u8,
    kelly_fraction: f64,
    max_contracts: u32,
) -> u32 {
    if kelly_fraction <= 0.0 || ask_cents == 0 || ask_cents >= 100 {
        return 0;
    }
    // Bankroll values in cents fit comfortably in f64's mantissa
    // (53 bits = ~$90 trillion in cents). Allow the precision-loss
    // lint here since we're well below that.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let count = {
        let target_dollars = (bankroll_cents as f64 / 100.0) * kelly_fraction.clamp(0.0, 1.0);
        let ask_dollars = f64::from(ask_cents) / 100.0;
        let raw = (target_dollars / ask_dollars).floor();
        if raw <= 0.0 {
            return 0;
        }
        (raw as u64).min(u64::from(max_contracts))
    };
    u32::try_from(count).unwrap_or(max_contracts)
}

fn validate(p: f64, ask: f64) -> Result<(), KellyError> {
    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
        return Err(KellyError::BadProbability(p.to_string()));
    }
    if !ask.is_finite() || ask <= 0.0 || ask >= 1.0 {
        return Err(KellyError::BadPrice(ask.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn fraction_is_zero_when_market_already_priced_above_model() {
        // Model says 40% but market is asking 50¢ — no edge.
        let f = fraction(0.4, 0.5).unwrap();
        assert!(approx(f, 0.0, 1e-12));
    }

    #[test]
    fn fraction_matches_canonical_formula_on_clear_edge() {
        // Model 70%, ask 50¢. Edge = (0.7 - 0.5) / (1 - 0.5) = 0.4.
        let f = fraction(0.7, 0.5).unwrap();
        assert!(approx(f, 0.4, 1e-12));
    }

    #[test]
    fn fraction_caps_at_one() {
        // Model 100% certain at ask 1¢ would suggest betting > 100%
        // of bankroll on full Kelly — cap to 1.
        let f = fraction(1.0, 0.01).unwrap();
        assert!(approx(f, 1.0, 1e-12));
    }

    #[test]
    fn fraction_with_factor_scales_linearly() {
        let full = fraction(0.7, 0.5).unwrap();
        let quarter = fraction_with_factor(0.7, 0.5, 0.25).unwrap();
        assert!(approx(quarter, full * 0.25, 1e-12));
    }

    #[test]
    fn fraction_with_factor_zero_returns_zero() {
        assert!(approx(
            fraction_with_factor(0.7, 0.5, 0.0).unwrap(),
            0.0,
            1e-12
        ));
    }

    #[test]
    fn fraction_rejects_invalid_inputs() {
        assert!(fraction(1.5, 0.5).is_err());
        assert!(fraction(-0.1, 0.5).is_err());
        assert!(fraction(0.5, 0.0).is_err());
        assert!(fraction(0.5, 1.0).is_err());
        assert!(fraction(f64::NAN, 0.5).is_err());
    }

    #[test]
    fn factor_must_be_in_unit_interval() {
        assert!(fraction_with_factor(0.7, 0.5, 1.5).is_err());
        assert!(fraction_with_factor(0.7, 0.5, -0.1).is_err());
        assert!(fraction_with_factor(0.7, 0.5, f64::NAN).is_err());
    }

    #[test]
    fn contracts_to_buy_floors_to_integer_count() {
        // $5 bankroll × 0.4 Kelly = $2 target. Ask 50¢ → 4 contracts.
        let n = contracts_to_buy(500, 50, 0.4, 1_000);
        assert_eq!(n, 4);
    }

    #[test]
    fn contracts_to_buy_caps_at_max_contracts() {
        let n = contracts_to_buy(1_000_000, 50, 1.0, 100);
        assert_eq!(n, 100);
    }

    #[test]
    fn contracts_to_buy_zero_for_zero_kelly() {
        assert_eq!(contracts_to_buy(500, 50, 0.0, 1_000), 0);
        assert_eq!(contracts_to_buy(500, 50, -0.1, 1_000), 0);
    }

    #[test]
    fn contracts_to_buy_zero_for_invalid_ask() {
        assert_eq!(contracts_to_buy(500, 0, 0.5, 1_000), 0);
        assert_eq!(contracts_to_buy(500, 100, 0.5, 1_000), 0);
    }
}
