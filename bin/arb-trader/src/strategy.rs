//! Static intra-venue arb detector.
//!
//! ## The signal
//!
//! Kalshi binary contracts settle at exactly $1 — one of YES/NO pays
//! out $1, the other pays $0. So buying one YES and one NO of the
//! same market locks in $1 of certain payout. The trade is profitable
//! when the round-trip cost is below $1 (less fees):
//!
//! ```text
//! profit_per_pair_¢ = 100
//!                   − best_yes_ask_¢
//!                   − best_no_ask_¢
//!                   − taker_fee(yes_leg)
//!                   − taker_fee(no_leg)
//! ```
//!
//! Kalshi only quotes bids, with asks derived by complement:
//!
//! ```text
//! best_yes_ask_¢ = 100 − best_no_bid_¢
//! best_no_ask_¢  = 100 − best_yes_bid_¢
//! ```
//!
//! Substituting — equivalently — `yes_bid + no_bid > 100 + fees` is
//! the bid-side restatement of "an arb exists." We compute the
//! ask-side form below because the order we'd send is a buy at the
//! ask, so reasoning about the ask price keeps the sizing math
//! aligned with the wire.
//!
//! ## Sizing
//!
//! Top-of-book qty on the YES ask side is the qty resting at the best
//! NO bid (and vice-versa). We cap each leg at `min(top_of_book_qty,
//! configured_size)`. The OMS+risk module re-checks limits before any
//! intent goes to the venue.
//!
//! ## Cooldown
//!
//! Each WS book update can fire `evaluate`. After we propose a pair
//! for a market we set a cooldown timer for that market — without it
//! we'd spam re-submits while the first pair is still being filled.
//! The OMS would reject the duplicates on rate-limit / position
//! grounds, but cheaper to filter here.

use predigy_book::OrderBook;
use predigy_core::fees::taker_fee;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Debug, Clone)]
pub struct ArbConfig {
    /// Minimum net profit per pair, in whole cents, after both taker
    /// fees. Pairs that don't clear this threshold are skipped.
    pub min_edge_cents: u32,
    /// Maximum pairs to lift per opportunity. The risk engine may
    /// downsize further; this is a strategy-level cap independent of
    /// that.
    pub max_size_per_pair: u32,
    /// Cooldown between pair submits on the same market.
    pub cooldown: Duration,
}

impl Default for ArbConfig {
    fn default() -> Self {
        Self {
            min_edge_cents: 1,
            max_size_per_pair: 50,
            cooldown: Duration::from_millis(500),
        }
    }
}

/// One detected arb opportunity. Returned for logging / dry-run modes
/// even when the strategy chooses not to fire (because of cooldown,
/// say) so an operator can see what the strategy is seeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbOpportunity {
    pub market: MarketTicker,
    /// Price at which we'd buy YES (the YES ask, i.e. the complement
    /// of the best NO bid).
    pub yes_buy_price: Price,
    /// Price at which we'd buy NO (the NO ask, i.e. the complement of
    /// the best YES bid).
    pub no_buy_price: Price,
    /// Lesser of the configured cap and either leg's available qty
    /// at the touch.
    pub size: u32,
    /// Net cents per pair after both taker fees.
    pub edge_cents_per_pair: i64,
    /// Total expected edge for the full `size`.
    pub edge_cents_total: i64,
}

/// Stateful strategy. Owns its cooldown timers; consumed by [`Runner`]
/// from a single tokio task so no synchronization is needed.
#[derive(Debug)]
pub struct ArbStrategy {
    config: ArbConfig,
    last_submit_at: HashMap<MarketTicker, Instant>,
}

impl ArbStrategy {
    #[must_use]
    pub fn new(config: ArbConfig) -> Self {
        Self {
            config,
            last_submit_at: HashMap::new(),
        }
    }

    /// Look at the current book and decide what (if anything) to send.
    /// Returns the detected opportunity (whether or not it's actionable)
    /// alongside the intents to submit (empty if the cooldown is
    /// active or the edge doesn't clear `min_edge_cents`).
    pub fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Evaluation {
        let Some(opp) = detect(market, book, &self.config) else {
            return Evaluation {
                opportunity: None,
                intents: Vec::new(),
                throttled: false,
            };
        };

        let throttled = self
            .last_submit_at
            .get(market)
            .is_some_and(|&t| now.duration_since(t) < self.config.cooldown);

        if opp.edge_cents_per_pair < i64::from(self.config.min_edge_cents) || throttled {
            return Evaluation {
                opportunity: Some(opp),
                intents: Vec::new(),
                throttled,
            };
        }

        let intents = build_intents(&opp);
        self.last_submit_at.insert(market.clone(), now);
        Evaluation {
            opportunity: Some(opp),
            intents,
            throttled: false,
        }
    }

    /// Force-clear the cooldown (e.g. after a pair fully fills and
    /// flattens). Currently unused by the runner; exposed for tests
    /// and future tuning.
    pub fn reset_cooldown(&mut self, market: &MarketTicker) {
        self.last_submit_at.remove(market);
    }
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub opportunity: Option<ArbOpportunity>,
    pub intents: Vec<Intent>,
    pub throttled: bool,
}

fn detect(market: &MarketTicker, book: &OrderBook, config: &ArbConfig) -> Option<ArbOpportunity> {
    let (best_yes_bid_px, best_yes_bid_qty) = book.best_yes_bid()?;
    let (best_no_bid_px, best_no_bid_qty) = book.best_no_bid()?;

    // Asks derived by complement. Reject if either complement is at
    // 0¢ or 100¢ (settlement levels are not tradable).
    let yes_ask_cents = 100u8.checked_sub(best_no_bid_px.cents())?;
    let no_ask_cents = 100u8.checked_sub(best_yes_bid_px.cents())?;
    let yes_ask_px = Price::from_cents(yes_ask_cents).ok()?;
    let no_ask_px = Price::from_cents(no_ask_cents).ok()?;

    // Cap size at the lesser of the two legs' touch qty and the
    // strategy cap. If either side is empty at 0, no arb.
    let size = config
        .max_size_per_pair
        .min(best_no_bid_qty)
        .min(best_yes_bid_qty);
    if size == 0 {
        return None;
    }
    let qty = Qty::new(size).ok()?;

    let total_ask_cents = u32::from(yes_ask_cents) + u32::from(no_ask_cents);
    if total_ask_cents >= 100 {
        // No headroom even before fees; fast-out.
        debug!(
            market = %market,
            yes_ask = yes_ask_cents,
            no_ask = no_ask_cents,
            "no arb; total ask >= 100¢"
        );
        return None;
    }
    let yes_fee = taker_fee(yes_ask_px, qty);
    let no_fee = taker_fee(no_ask_px, qty);
    // Per-pair P&L is (100 − yes_ask − no_ask), in cents per pair.
    // Fees are denominated for the full size (not per-pair), so divide.
    let raw_per_pair = 100i64 - i64::from(total_ask_cents);
    let total_raw = raw_per_pair * i64::from(size);
    let total_edge_cents = total_raw - i64::from(yes_fee) - i64::from(no_fee);
    let per_pair_edge = total_edge_cents / i64::from(size);

    Some(ArbOpportunity {
        market: market.clone(),
        yes_buy_price: yes_ask_px,
        no_buy_price: no_ask_px,
        size,
        edge_cents_per_pair: per_pair_edge,
        edge_cents_total: total_edge_cents,
    })
}

fn build_intents(opp: &ArbOpportunity) -> Vec<Intent> {
    let Ok(qty) = Qty::new(opp.size) else {
        return Vec::new();
    };
    let yes = Intent::limit(
        opp.market.clone(),
        Side::Yes,
        Action::Buy,
        opp.yes_buy_price,
        qty,
    )
    .with_tif(TimeInForce::Ioc);
    let no = Intent::limit(
        opp.market.clone(),
        Side::No,
        Action::Buy,
        opp.no_buy_price,
        qty,
    )
    .with_tif(TimeInForce::Ioc);
    vec![yes, no]
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;
    use predigy_core::price::Price;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn snap(seq: u64, yes_bids: &[(u8, u32)], no_bids: &[(u8, u32)]) -> Snapshot {
        Snapshot {
            seq,
            yes_bids: yes_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
            no_bids: no_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
        }
    }

    fn book(yes_bids: &[(u8, u32)], no_bids: &[(u8, u32)]) -> OrderBook {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(snap(1, yes_bids, no_bids));
        b
    }

    #[test]
    fn no_arb_when_market_balances() {
        // YES bid 50, NO bid 50 → YES ask 50, NO ask 50, sum 100. No arb.
        let mut s = ArbStrategy::new(ArbConfig::default());
        let m = MarketTicker::new("X");
        let ev = s.evaluate(&m, &book(&[(50, 100)], &[(50, 100)]), Instant::now());
        assert!(ev.opportunity.is_none());
        assert!(ev.intents.is_empty());
    }

    #[test]
    fn detects_arb_with_meaningful_edge() {
        // YES bid 60, NO bid 50 → YES ask 50, NO ask 40, sum 90.
        // Profit before fees: 10¢/pair × 100 = 1000¢.
        // Fees: taker(50¢, 100) = 175¢, taker(40¢, 100) = ceil(0.07*100*0.4*0.6)=168¢.
        // Net edge: 1000 − 175 − 168 = 657¢.
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 100,
            cooldown: Duration::from_secs(1),
        });
        let m = MarketTicker::new("X");
        let ev = s.evaluate(&m, &book(&[(60, 100)], &[(50, 100)]), Instant::now());
        let opp = ev.opportunity.expect("opportunity");
        assert_eq!(opp.yes_buy_price.cents(), 50);
        assert_eq!(opp.no_buy_price.cents(), 40);
        assert_eq!(opp.size, 100);
        assert_eq!(opp.edge_cents_total, 1000 - 175 - 168);
        assert_eq!(ev.intents.len(), 2);
        assert!(!ev.throttled);
    }

    #[test]
    fn intents_are_buy_yes_and_buy_no_at_the_asks() {
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 100,
            cooldown: Duration::from_secs(1),
        });
        let m = MarketTicker::new("X");
        let ev = s.evaluate(&m, &book(&[(60, 50)], &[(50, 50)]), Instant::now());
        assert_eq!(ev.intents.len(), 2);
        let yes = &ev.intents[0];
        assert_eq!(yes.side, Side::Yes);
        assert_eq!(yes.action, Action::Buy);
        assert_eq!(yes.price.cents(), 50);
        assert_eq!(yes.qty.get(), 50);
        assert_eq!(yes.tif, TimeInForce::Ioc);
        let no = &ev.intents[1];
        assert_eq!(no.side, Side::No);
        assert_eq!(no.action, Action::Buy);
        assert_eq!(no.price.cents(), 40);
    }

    #[test]
    fn size_capped_by_thinnest_leg_at_touch() {
        // Plenty of YES bid, only 5 NO bid → cap at 5.
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 1000,
            cooldown: Duration::from_secs(1),
        });
        let m = MarketTicker::new("X");
        let ev = s.evaluate(&m, &book(&[(60, 200)], &[(50, 5)]), Instant::now());
        assert_eq!(ev.opportunity.unwrap().size, 5);
    }

    #[test]
    fn cooldown_blocks_repeat_submit_on_same_market() {
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 100,
            cooldown: Duration::from_millis(500),
        });
        let m = MarketTicker::new("X");
        let now = Instant::now();
        let first = s.evaluate(&m, &book(&[(60, 100)], &[(50, 100)]), now);
        assert_eq!(first.intents.len(), 2);

        let second = s.evaluate(
            &m,
            &book(&[(60, 100)], &[(50, 100)]),
            now + Duration::from_millis(100),
        );
        assert!(second.intents.is_empty(), "still throttled");
        assert!(second.throttled);

        let third = s.evaluate(
            &m,
            &book(&[(60, 100)], &[(50, 100)]),
            now + Duration::from_millis(600),
        );
        assert_eq!(third.intents.len(), 2, "cooldown elapsed");
    }

    #[test]
    fn min_edge_blocks_marginal_opportunity() {
        // YES bid 51, NO bid 50 → asks 50, 49, sum 99 → 1¢ pre-fee profit
        // per pair × 100 = 100¢. Fees ≈ 175 + 174 = 349¢ → net loss.
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 100,
            cooldown: Duration::from_secs(1),
        });
        let m = MarketTicker::new("X");
        let ev = s.evaluate(&m, &book(&[(51, 100)], &[(50, 100)]), Instant::now());
        let opp = ev.opportunity.expect("detected");
        assert!(opp.edge_cents_total < 0, "fees > raw edge");
        assert!(ev.intents.is_empty());
    }

    #[test]
    fn no_arb_with_empty_book_side() {
        let mut s = ArbStrategy::new(ArbConfig::default());
        let m = MarketTicker::new("X");
        // Empty NO bids → no NO ask → no arb.
        let ev = s.evaluate(&m, &book(&[(60, 100)], &[]), Instant::now());
        assert!(ev.opportunity.is_none());
    }

    #[test]
    fn reset_cooldown_clears_throttle() {
        let mut s = ArbStrategy::new(ArbConfig {
            min_edge_cents: 1,
            max_size_per_pair: 100,
            cooldown: Duration::from_mins(1),
        });
        let m = MarketTicker::new("X");
        let now = Instant::now();
        let _ = s.evaluate(&m, &book(&[(60, 100)], &[(50, 100)]), now);
        s.reset_cooldown(&m);
        let again = s.evaluate(
            &m,
            &book(&[(60, 100)], &[(50, 100)]),
            now + Duration::from_millis(10),
        );
        assert_eq!(again.intents.len(), 2);
    }
}
