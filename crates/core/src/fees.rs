//! Kalshi fee schedule (Feb 2026).
//!
//! Per the published fee schedule:
//! ```text
//! taker_fee = ceil(0.07   * C * P * (1 - P))
//! maker_fee = ceil(0.0175 * C * P * (1 - P))
//! ```
//! where `C` is contracts, `P` is price in dollars (0.01..=0.99). Fees are
//! billed in whole cents (round-up to nearest cent). All math here is in
//! integer cents to avoid float drift on the hot path.

use crate::price::{Price, Qty};

const TAKER_NUMER: u64 = 700; // 0.07  * 10_000 — keeps the math in integer space
const MAKER_NUMER: u64 = 175; // 0.0175 * 10_000

/// Taker fee in whole cents for `qty` contracts at `price`. Round-up to ¢.
#[inline]
#[must_use]
pub fn taker_fee(price: Price, qty: Qty) -> u32 {
    fee_cents(price, qty, TAKER_NUMER)
}

/// Maker fee in whole cents for `qty` contracts at `price`. Round-up to ¢.
#[inline]
#[must_use]
pub fn maker_fee(price: Price, qty: Qty) -> u32 {
    fee_cents(price, qty, MAKER_NUMER)
}

#[inline]
fn fee_cents(price: Price, qty: Qty, numer_x10k: u64) -> u32 {
    // Closed-form: ceil(numer_x10k * C * p_cents * (100 - p_cents) / 1_000_000)
    //
    // Derivation:
    //   fee_$ = numer * C * p * (1 - p)
    //         = (numer_x10k / 10_000) * C * (p_c / 100) * ((100 - p_c) / 100)
    //         = numer_x10k * C * p_c * (100 - p_c) / 1_000_000_0000
    //   fee_¢ = fee_$ * 100
    //         = numer_x10k * C * p_c * (100 - p_c) / 1_000_000
    //
    // u64 headroom check: numer_x10k ≤ 700, C ≤ u32::MAX (≈4.3e9), p_c*(100-p_c)
    // ≤ 2500. Worst case ≈ 700 * 4.3e9 * 2500 ≈ 7.5e15, well within u64::MAX
    // (1.8e19). Safe.
    let p_c = u64::from(price.cents());
    let term = p_c * (100 - p_c); // 0..=2500
    let num = numer_x10k * u64::from(qty.get()) * term;
    let fee_c = num.div_ceil(1_000_000);
    u32::try_from(fee_c).expect("fee fits in u32")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    #[test]
    fn taker_at_50_per_contract_is_2_cents_after_roundup() {
        // Per-contract: 0.07 * 0.5 * 0.5 = 0.0175 = 1.75¢. Round up → 2¢.
        assert_eq!(taker_fee(p(50), q(1)), 2);
    }

    #[test]
    fn taker_scales_with_quantity() {
        // 100 contracts @ 50¢: 0.07 * 100 * 0.25 = 1.75 = $1.75 = 175¢ exactly.
        assert_eq!(taker_fee(p(50), q(100)), 175);
        // 1000 contracts @ 50¢: 1750¢.
        assert_eq!(taker_fee(p(50), q(1000)), 1750);
    }

    #[test]
    fn maker_is_quarter_of_taker() {
        // 100 contracts @ 50¢: maker = 0.0175 * 100 * 0.25 = 0.4375 = ~44¢
        // (round up of 0.4375 dollars = 44 cents exactly? 0.4375$ = 43.75¢ → 44¢).
        assert_eq!(maker_fee(p(50), q(100)), 44);
        // Round-trip maker on 100 @ 50¢ = 88¢ ≈ 0.88% of $100 notional. Matches plan.
    }

    #[test]
    fn taker_at_extremes_is_cheap() {
        // 100 @ 90¢: 0.07 * 100 * 0.9 * 0.1 = 0.63 = 63¢.
        assert_eq!(taker_fee(p(90), q(100)), 63);
        // 100 @ 10¢: same.
        assert_eq!(taker_fee(p(10), q(100)), 63);
    }

    #[test]
    fn round_up_for_tiny_orders() {
        // 1 @ 50¢ taker = ceil(1.75¢) = 2¢. Round-up dominates small orders.
        assert_eq!(taker_fee(p(50), q(1)), 2);
        // 1 @ 50¢ maker = ceil(0.4375¢) = 1¢.
        assert_eq!(maker_fee(p(50), q(1)), 1);
    }

    #[test]
    fn round_up_at_extreme_prices() {
        // 1 @ 99¢ taker = ceil(0.07 * 0.99 * 0.01) = ceil(0.0693¢) = 1¢.
        assert_eq!(taker_fee(p(99), q(1)), 1);
        assert_eq!(taker_fee(p(1), q(1)), 1);
    }

    #[test]
    fn no_overflow_at_realistic_max_qty() {
        // 10M contracts is far above any plausible single-order size on Kalshi
        // (current liquid markets show < 100k depth at touch). Internal math
        // must not overflow and the result must fit in u32.
        let fee = taker_fee(p(50), q(10_000_000));
        // 0.07 * 10_000_000 * 0.25 = 175_000 dollars = 17_500_000 cents.
        assert_eq!(fee, 17_500_000);
    }
}
