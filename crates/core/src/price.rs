//! Fixed-point price and quantity types.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A Kalshi contract price, in whole cents (1..=99).
///
/// Stored as `u8` because Kalshi binary contracts trade between $0.01 and $0.99
/// inclusive. Constructors reject 0 and 100; those are settlement values, not
/// tradable prices.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(u8);

/// Error returned when constructing a [`Price`] from an out-of-range value.
#[derive(Debug, thiserror::Error)]
#[error("price {0}¢ out of range; must be 1..=99")]
pub struct PriceOutOfRange(pub i32);

impl Price {
    /// Construct from whole cents. Returns an error for values outside `1..=99`.
    pub const fn from_cents(cents: u8) -> Result<Self, PriceOutOfRange> {
        if cents >= 1 && cents <= 99 {
            Ok(Self(cents))
        } else {
            Err(PriceOutOfRange(cents as i32))
        }
    }

    /// Construct from whole cents without bounds checking. Panics in debug.
    #[inline]
    #[must_use]
    pub const fn new_unchecked(cents: u8) -> Self {
        debug_assert!(cents >= 1 && cents <= 99);
        Self(cents)
    }

    /// Return the price in whole cents (1..=99).
    #[inline]
    #[must_use]
    pub const fn cents(self) -> u8 {
        self.0
    }

    /// Return the price as a fraction in `[0.01, 0.99]`.
    #[inline]
    #[must_use]
    pub fn as_dollars(self) -> f64 {
        f64::from(self.0) / 100.0
    }

    /// The complementary price `1 - p`, in cents.
    ///
    /// For binary contracts, the NO side trades at `100 - YES`.
    #[inline]
    #[must_use]
    pub const fn complement(self) -> Self {
        Self(100 - self.0)
    }
}

impl fmt::Debug for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}¢", self.0)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}.{:02}", self.0 / 100, self.0 % 100)
    }
}

/// Contract quantity. Positive integer; zero is not a valid order size.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Qty(u32);

/// Error returned when constructing a [`Qty`] from zero.
#[derive(Debug, thiserror::Error)]
#[error("quantity must be > 0")]
pub struct QtyZero;

impl Qty {
    /// Construct a non-zero quantity.
    pub const fn new(n: u32) -> Result<Self, QtyZero> {
        if n == 0 { Err(QtyZero) } else { Ok(Self(n)) }
    }

    /// Construct without bounds checking. Panics in debug if zero.
    #[inline]
    #[must_use]
    pub const fn new_unchecked(n: u32) -> Self {
        debug_assert!(n > 0);
        Self(n)
    }

    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_construction_valid() {
        assert_eq!(Price::from_cents(50).unwrap().cents(), 50);
        assert_eq!(Price::from_cents(1).unwrap().cents(), 1);
        assert_eq!(Price::from_cents(99).unwrap().cents(), 99);
    }

    #[test]
    fn price_construction_invalid() {
        assert!(Price::from_cents(0).is_err());
        assert!(Price::from_cents(100).is_err());
    }

    #[test]
    fn price_complement() {
        assert_eq!(Price::from_cents(30).unwrap().complement().cents(), 70);
        assert_eq!(Price::from_cents(50).unwrap().complement().cents(), 50);
    }

    #[test]
    fn price_dollars() {
        assert!((Price::from_cents(33).unwrap().as_dollars() - 0.33).abs() < 1e-9);
    }

    #[test]
    fn qty_rejects_zero() {
        assert!(Qty::new(0).is_err());
        assert_eq!(Qty::new(7).unwrap().get(), 7);
    }
}
