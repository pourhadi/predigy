//! String → numeric helpers for the Polymarket wire format.
//!
//! Polymarket prices are decimal strings in `[0, 1]` with variable tick
//! size (typically 0.01 but may shrink to 0.001 as a market matures), so
//! we represent them as `f64` rather than the integer-cents `Price` from
//! `predigy-core` (which is Kalshi-shaped: cents 1..=99). Sizes are also
//! decimal strings — Polymarket trades fractional USDC, so quantities
//! are not always integers.
//!
//! Used as a reference price only — never sized for execution — so f64
//! precision is more than adequate.

use crate::error::Error;

/// Parse a Polymarket price string (e.g. `"0.42"`). Range-checks `[0, 1]`.
pub fn parse_price(s: &str) -> Result<f64, Error> {
    let n: f64 = s
        .parse()
        .map_err(|_| Error::Invalid(format!("price {s:?} not a number")))?;
    if !n.is_finite() || !(0.0..=1.0).contains(&n) {
        return Err(Error::Invalid(format!("price {s:?} not in [0, 1]")));
    }
    Ok(n)
}

/// Parse a Polymarket size string. Negative or non-finite are rejected;
/// fractional values are accepted (Polymarket sizes are USDC-denominated).
pub fn parse_size(s: &str) -> Result<f64, Error> {
    let n: f64 = s
        .parse()
        .map_err(|_| Error::Invalid(format!("size {s:?} not a number")))?;
    if !n.is_finite() || n < 0.0 {
        return Err(Error::Invalid(format!("size {s:?} not non-negative")));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_parses_typical_values() {
        assert!((parse_price("0.42").unwrap() - 0.42).abs() < 1e-9);
        assert!(parse_price("0").unwrap().abs() < 1e-12);
        assert!((parse_price("1").unwrap() - 1.0).abs() < 1e-12);
        assert!((parse_price("0.001").unwrap() - 0.001).abs() < 1e-12);
    }

    #[test]
    fn price_rejects_out_of_range() {
        assert!(parse_price("-0.01").is_err());
        assert!(parse_price("1.01").is_err());
        assert!(parse_price("nan").is_err());
        assert!(parse_price("inf").is_err());
    }

    #[test]
    fn size_accepts_fractional() {
        assert!((parse_size("100.5").unwrap() - 100.5).abs() < 1e-9);
        assert!(parse_size("0").unwrap().abs() < 1e-12);
    }

    #[test]
    fn size_rejects_negative() {
        assert!(parse_size("-1").is_err());
        assert!(parse_size("garbage").is_err());
    }
}
