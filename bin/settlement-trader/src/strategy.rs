//! Settlement-time strategy: lift the touch on near-locked sports
//! markets in the final minutes before close.
//!
//! ## Thesis
//!
//! Humans hesitate to lift the ask on near-certain outcomes. They
//! sell early to lock in profit. This leaves a price-vs-true-prob
//! gap of 2-5¢ in the final minutes before settlement, with a tell
//! visible in the order book itself: a heavy stack on the bid side
//! (lots of buyers willing at 95¢), a thin stack on the ask side
//! (few sellers, mostly slow-moving humans). When the tell fires
//! AND we're close to `close_time`, lift the ask before the human
//! market eventually does.
//!
//! ## What "tell" means here
//!
//! Three conditions must hold simultaneously:
//!
//! 1. `time_to_close < close_window`  — the gap is only profitable
//!    near settlement; earlier in the trade window the price
//!    actually reflects information flow.
//! 2. `yes_ask in [min_price, max_price]` — too low means the
//!    market doesn't think it's locked; too high (≥98¢) means no
//!    edge after fees.
//! 3. `bid_stack_qty >= bid_to_ask_ratio × ask_stack_qty` — the
//!    book asymmetry is the sole proxy we have for "the market
//!    thinks this is locked." Without a sports feed we trust this
//!    signal.
//!
//! The strategy disarms a market after firing once per session, so
//! late-arriving deltas don't double-fire. Restart re-arms.

use predigy_book::OrderBook;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Per-market evaluation tunables.
#[derive(Debug, Clone, Copy)]
pub struct SettlementConfig {
    /// Only fire when `close_time - now < close_window`.
    pub close_window: Duration,
    /// Don't fire if the cheap side (`yes_ask`) is below this; the
    /// market doesn't yet think the outcome is near-locked.
    pub min_price_cents: u8,
    /// Don't fire if the cheap side is above this; no edge after
    /// fees + slippage.
    pub max_price_cents: u8,
    /// Bid-stack must be at least this multiple of ask-stack at the
    /// touch for the rule to consider firing. Default 5×.
    pub bid_to_ask_ratio: u32,
    /// Per-fire size (contracts).
    pub size: u32,
    /// Per-market cooldown after a fire — even if the touch
    /// re-asymmetries, don't refire within this window.
    pub cooldown: Duration,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self {
            close_window: Duration::from_mins(10),
            min_price_cents: 88,
            max_price_cents: 96,
            bid_to_ask_ratio: 5,
            size: 1,
            cooldown: Duration::from_mins(1),
        }
    }
}

/// Per-market signal evaluation.
#[derive(Debug)]
pub struct SettlementStrategy {
    config: SettlementConfig,
    /// Each market's `close_time` as a unix-seconds timestamp.
    /// Operator-supplied at startup. Required because the WS feed
    /// doesn't include `close_time` on every delta.
    close_times: HashMap<MarketTicker, i64>,
    /// `(market) -> last fire Instant`. Cooldown filter.
    last_fired: HashMap<MarketTicker, Instant>,
}

impl SettlementStrategy {
    #[must_use]
    pub fn new(config: SettlementConfig) -> Self {
        Self {
            config,
            close_times: HashMap::new(),
            last_fired: HashMap::new(),
        }
    }

    pub fn set_close_time(&mut self, market: MarketTicker, close_time_unix: i64) {
        self.close_times.insert(market, close_time_unix);
    }

    pub fn config(&self) -> &SettlementConfig {
        &self.config
    }

    pub fn markets(&self) -> impl Iterator<Item = &MarketTicker> {
        self.close_times.keys()
    }

    /// Evaluate the touch of `market` against the strategy. Returns
    /// `Some(Intent)` if the rule fires; `None` otherwise.
    /// `now_unix` is the wall-clock time used to compute time-to-close
    /// (kept as an input rather than `SystemTime::now()` so tests can
    /// drive it deterministically).
    pub fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now_unix: i64,
        now_instant: Instant,
    ) -> Option<Intent> {
        // 1. Cooldown.
        if let Some(&last) = self.last_fired.get(market)
            && now_instant.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        // 2. Time-to-close gate.
        let close_time = self.close_times.get(market).copied()?;
        let secs_to_close = close_time.saturating_sub(now_unix);
        if secs_to_close <= 0
            || u64::try_from(secs_to_close).unwrap_or(u64::MAX)
                >= self.config.close_window.as_secs()
        {
            return None;
        }
        // 3. Best-bid touch must exist.
        let (best_bid_price, best_bid_qty) = book.best_yes_bid()?;
        // 4. Derive yes_ask from no_bid via complement.
        let (best_no_bid_price, best_no_bid_qty) = book.best_no_bid()?;
        let yes_ask_cents = 100u8.checked_sub(best_no_bid_price.cents())?;
        if yes_ask_cents < self.config.min_price_cents
            || yes_ask_cents > self.config.max_price_cents
        {
            return None;
        }
        // 5. Asymmetry test. Bid stack (yes-side bid) must
        //    dominate ask stack (which equals the no-side bid by
        //    book convention). Both `qty` are best-touch only —
        //    we don't dive into deeper levels for v1.
        if best_bid_qty < best_no_bid_qty.saturating_mul(self.config.bid_to_ask_ratio) {
            return None;
        }
        // 6. Sanity: best_bid_price + yes_ask should not be < 100
        //    (would indicate book inversion / fresh-quote race we
        //    don't want to chase). Skip if so.
        if best_bid_price.cents() + yes_ask_cents < 100 {
            return None;
        }

        let _ = best_no_bid_price; // referenced for derivation; unused beyond that
        let price = Price::from_cents(yes_ask_cents).ok()?;
        let qty = Qty::new(self.config.size).ok()?;
        let intent = Intent::limit(market.clone(), Side::Yes, Action::Buy, price, qty)
            .with_tif(TimeInForce::Ioc);
        self.last_fired.insert(market.clone(), now_instant);
        Some(intent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;
    use predigy_core::price::Price;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn cfg() -> SettlementConfig {
        SettlementConfig {
            close_window: Duration::from_mins(10),
            min_price_cents: 88,
            max_price_cents: 96,
            bid_to_ask_ratio: 5,
            size: 1,
            cooldown: Duration::from_mins(1),
        }
    }

    fn book_with(yes_bid: (u8, u32), no_bid: (u8, u32)) -> OrderBook {
        let mut b = OrderBook::new("KX-TEST");
        b.apply_snapshot(Snapshot {
            seq: 1,
            yes_bids: vec![(p(yes_bid.0), yes_bid.1)],
            no_bids: vec![(p(no_bid.0), no_bid.1)],
        });
        b
    }

    #[test]
    fn fires_when_all_conditions_met() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        // yes_ask = 100 - no_bid(7) = 93; bid stack 1000 >> 5*100 = 500.
        let book = book_with((92, 1000), (7, 100));
        let intent = s.evaluate(&m, &book, 1_777_909_700, Instant::now());
        let intent = intent.expect("fired");
        assert_eq!(intent.price.cents(), 93);
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.tif, TimeInForce::Ioc);
    }

    #[test]
    fn no_fire_outside_close_window() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        // 1h before close (3600s) > close_window (600s).
        let intent = s.evaluate(&m, &book, 1_777_906_400, Instant::now());
        assert!(intent.is_none());
    }

    #[test]
    fn no_fire_when_already_settled() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        // After close.
        let intent = s.evaluate(&m, &book, 1_777_910_500, Instant::now());
        assert!(intent.is_none());
    }

    #[test]
    fn no_fire_when_ask_too_high() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        // no_bid 1¢ → yes_ask 99¢ > max(96).
        let book = book_with((97, 1000), (1, 100));
        let intent = s.evaluate(&m, &book, 1_777_909_700, Instant::now());
        assert!(intent.is_none());
    }

    #[test]
    fn no_fire_when_ask_too_low() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        // no_bid 50¢ → yes_ask 50¢ < min(88).
        let book = book_with((48, 1000), (50, 100));
        let intent = s.evaluate(&m, &book, 1_777_909_700, Instant::now());
        assert!(intent.is_none());
    }

    #[test]
    fn no_fire_when_book_too_balanced() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        // Bid stack 200, ask stack 100 → ratio 2 < 5.
        let book = book_with((92, 200), (7, 100));
        let intent = s.evaluate(&m, &book, 1_777_909_700, Instant::now());
        assert!(intent.is_none());
    }

    #[test]
    fn cooldown_blocks_repeat_fire() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        let now = Instant::now();
        let _ = s
            .evaluate(&m, &book, 1_777_909_700, now)
            .expect("first fires");
        // Same instant — within cooldown.
        assert!(s.evaluate(&m, &book, 1_777_909_710, now).is_none());
    }

    #[test]
    fn cooldown_clears_after_window() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        s.set_close_time(m.clone(), 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        let t0 = Instant::now();
        let _ = s.evaluate(&m, &book, 1_777_909_700, t0).expect("first");
        // 61s later — past cooldown (60s).
        let t1 = t0 + Duration::from_secs(61);
        assert!(s.evaluate(&m, &book, 1_777_909_710, t1).is_some());
    }

    #[test]
    fn no_fire_when_market_unknown() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-UNKNOWN");
        // No close_time registered.
        let book = book_with((92, 1000), (7, 100));
        let intent = s.evaluate(&m, &book, 1_777_909_700, Instant::now());
        assert!(intent.is_none());
    }
}
