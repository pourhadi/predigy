//! In-memory L2 order book for a single Kalshi binary market.
//!
//! Kalshi exposes a YES/NO bid book per market. There are no asks: a "sell YES
//! at p" is economically a "buy NO at (1-p)". To present a strategy with the
//! familiar two-sided YES book, we mirror NO bids onto the YES ask side via
//! price complement.
//!
//! The book applies a snapshot then an arbitrary number of deltas. Each delta
//! carries a strictly-increasing exchange sequence number; a gap → caller
//! must resnapshot.

use predigy_core::price::Price;
use predigy_core::side::Side;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single price level: `(price → resting quantity)`.
pub type Levels = BTreeMap<Price, u32>;

/// Outcome of attempting to apply a delta.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Applied cleanly.
    Ok,
    /// Sequence gap detected. Caller must resnapshot. Internal state is
    /// preserved so a subsequent snapshot can replace it atomically.
    Gap { expected: u64, got: u64 },
    /// Delta references a market the book wasn't initialised with.
    WrongMarket,
}

/// Two-sided YES book for one market. NO bids are stored verbatim and also
/// projected to the YES ask side.
#[derive(Debug, Clone)]
pub struct OrderBook {
    market: String,
    /// YES bids (price → qty). Buy YES.
    yes_bids: Levels,
    /// NO bids (price → qty). Buy NO.
    no_bids: Levels,
    /// Last applied exchange sequence number. `None` until first event.
    last_seq: Option<u64>,
}

impl OrderBook {
    #[must_use]
    pub fn new(market: impl Into<String>) -> Self {
        Self {
            market: market.into(),
            yes_bids: BTreeMap::new(),
            no_bids: BTreeMap::new(),
            last_seq: None,
        }
    }

    #[must_use]
    pub fn market(&self) -> &str {
        &self.market
    }

    /// Last sequence number applied (snapshot or delta).
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Best YES bid (highest yes-buy price). `None` if empty.
    #[must_use]
    pub fn best_yes_bid(&self) -> Option<(Price, u32)> {
        self.yes_bids.iter().next_back().map(|(&p, &q)| (p, q))
    }

    /// Best NO bid (highest no-buy price).
    #[must_use]
    pub fn best_no_bid(&self) -> Option<(Price, u32)> {
        self.no_bids.iter().next_back().map(|(&p, &q)| (p, q))
    }

    /// Best YES ask, derived from the best NO bid via price complement.
    /// "Sell YES at q" is economically "Buy NO at (1-q)".
    #[must_use]
    pub fn best_yes_ask(&self) -> Option<(Price, u32)> {
        self.best_no_bid().map(|(p, q)| (p.complement(), q))
    }

    /// YES mid in dollars, or `None` if either side is empty.
    #[must_use]
    pub fn yes_mid_dollars(&self) -> Option<f64> {
        let bid = self.best_yes_bid()?.0.as_dollars();
        let ask = self.best_yes_ask()?.0.as_dollars();
        Some(f64::midpoint(bid, ask))
    }

    /// YES bid-ask spread in cents, or `None` if either side is empty.
    #[must_use]
    pub fn yes_spread_cents(&self) -> Option<u8> {
        let bid = self.best_yes_bid()?.0.cents();
        let ask = self.best_yes_ask()?.0.cents();
        // ask should be >= bid except in transient crossed states; saturate to 0.
        Some(ask.saturating_sub(bid))
    }

    /// Replace the entire book with a snapshot.
    pub fn apply_snapshot(&mut self, snap: Snapshot) {
        self.yes_bids = snap.yes_bids.into_iter().collect();
        self.no_bids = snap.no_bids.into_iter().collect();
        self.last_seq = Some(snap.seq);
    }

    /// Apply an incremental delta. Returns the outcome; on `Gap` the caller
    /// must resnapshot before further deltas will be accepted.
    pub fn apply_delta(&mut self, delta: &Delta) -> ApplyOutcome {
        if delta.market != self.market {
            return ApplyOutcome::WrongMarket;
        }
        if let Some(last) = self.last_seq {
            let expected = last + 1;
            if delta.seq != expected {
                return ApplyOutcome::Gap {
                    expected,
                    got: delta.seq,
                };
            }
        }
        let levels = match delta.side {
            Side::Yes => &mut self.yes_bids,
            Side::No => &mut self.no_bids,
        };
        // delta.qty_delta is signed: positive adds resting size, negative removes.
        // Removing more than rests at a level → level goes to 0 and is removed.
        match levels.get(&delta.price).copied() {
            Some(existing) => {
                let new_qty = i64::from(existing) + i64::from(delta.qty_delta);
                if new_qty <= 0 {
                    levels.remove(&delta.price);
                } else {
                    levels.insert(delta.price, u32::try_from(new_qty).unwrap_or(0));
                }
            }
            None => {
                if delta.qty_delta > 0 {
                    let q = u32::try_from(delta.qty_delta).unwrap_or(0);
                    if q > 0 {
                        levels.insert(delta.price, q);
                    }
                }
                // qty_delta <= 0 against an empty level: ignore (already gone).
            }
        }
        self.last_seq = Some(delta.seq);
        ApplyOutcome::Ok
    }
}

/// Snapshot payload (already parsed from Kalshi REST or WS).
///
/// `Serialize`/`Deserialize` are provided so the `md-recorder` binary can
/// archive snapshots to NDJSON and replay them on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub yes_bids: Vec<(Price, u32)>,
    pub no_bids: Vec<(Price, u32)>,
}

/// One incremental order-book change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub market: String,
    pub seq: u64,
    pub side: Side,
    pub price: Price,
    /// Signed change in resting size at `price`. Positive = added, negative = lifted/cancelled.
    pub qty_delta: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn snap(seq: u64, yes: Vec<(u8, u32)>, no: Vec<(u8, u32)>) -> Snapshot {
        Snapshot {
            seq,
            yes_bids: yes.into_iter().map(|(c, q)| (p(c), q)).collect(),
            no_bids: no.into_iter().map(|(c, q)| (p(c), q)).collect(),
        }
    }

    fn delta(market: &str, seq: u64, side: Side, price: u8, qd: i32) -> Delta {
        Delta {
            market: market.into(),
            seq,
            side,
            price: p(price),
            qty_delta: qd,
        }
    }

    #[test]
    fn snapshot_and_best_levels() {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(1, vec![(40, 100), (45, 50)], vec![(50, 75), (55, 25)]));
        assert_eq!(b.best_yes_bid(), Some((p(45), 50)));
        assert_eq!(b.best_no_bid(), Some((p(55), 25)));
        // YES ask = complement of best NO bid: 100 - 55 = 45.
        assert_eq!(b.best_yes_ask(), Some((p(45), 25)));
        // Note: yes_bid=45 == yes_ask=45 → crossed/locked. Real markets won't be locked,
        // but the math is what it is.
        assert_eq!(b.yes_spread_cents(), Some(0));
    }

    #[test]
    fn mid_when_uncrossed() {
        let mut b = OrderBook::new("X");
        // YES bid 40, NO bid 50 → YES ask 50. Spread 10¢, mid 45¢.
        b.apply_snapshot(snap(1, vec![(40, 100)], vec![(50, 100)]));
        assert!((b.yes_mid_dollars().unwrap() - 0.45).abs() < 1e-9);
        assert_eq!(b.yes_spread_cents(), Some(10));
    }

    #[test]
    fn delta_adds_and_removes() {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(1, vec![(40, 100)], vec![(60, 100)]));
        // Add 25 at 41 (new level on YES side).
        assert_eq!(
            b.apply_delta(&delta("X", 2, Side::Yes, 41, 25)),
            ApplyOutcome::Ok
        );
        assert_eq!(b.best_yes_bid(), Some((p(41), 25)));
        // Lift 30 from 41 → level decreases to 0 → removed.
        assert_eq!(
            b.apply_delta(&delta("X", 3, Side::Yes, 41, -30)),
            ApplyOutcome::Ok
        );
        assert_eq!(b.best_yes_bid(), Some((p(40), 100)));
    }

    #[test]
    fn delta_sequence_gap_detected() {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(10, vec![(40, 100)], vec![(60, 100)]));
        // Gap: expected 11, got 13.
        let out = b.apply_delta(&delta("X", 13, Side::Yes, 41, 25));
        assert_eq!(
            out,
            ApplyOutcome::Gap {
                expected: 11,
                got: 13
            }
        );
        // last_seq is unchanged on gap so a fresh snapshot can resync.
        assert_eq!(b.last_seq(), Some(10));
    }

    #[test]
    fn wrong_market_rejected() {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(1, vec![(40, 100)], vec![(60, 100)]));
        assert_eq!(
            b.apply_delta(&delta("Y", 2, Side::Yes, 41, 25)),
            ApplyOutcome::WrongMarket
        );
    }

    #[test]
    fn delta_against_empty_level_remove_is_ignored() {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(1, vec![], vec![]));
        // Remove from empty: no panic, no effect.
        assert_eq!(
            b.apply_delta(&delta("X", 2, Side::Yes, 50, -10)),
            ApplyOutcome::Ok
        );
        assert!(b.best_yes_bid().is_none());
    }
}
