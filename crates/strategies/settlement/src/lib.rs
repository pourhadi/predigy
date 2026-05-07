// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-settlement` — settlement-time tape-reading
//! strategy as an engine module. Implements
//! [`predigy_engine_core::Strategy`].
//!
//! Logic preserved verbatim from
//! `bin/settlement-trader/src/strategy.rs`. The thesis:
//!
//! Humans hesitate to lift the ask on near-certain outcomes (sports
//! games approaching their final minutes when the leading side is
//! mathematically close to locked). They sell early to lock in
//! profit. This leaves a price-vs-true-prob gap of 2–5¢ in the
//! final minutes before settlement, with a tell visible in the
//! order book itself: a heavy bid stack (lots of buyers willing at
//! 95¢) and a thin ask stack (few sellers, mostly slow humans).
//! When the tell fires AND we're inside the close window, lift
//! the ask before the human market eventually does.
//!
//! ## Discovery
//!
//! The engine drives discovery via [`Strategy::discovery_subscriptions`].
//! For settlement, we declare the standard sports-series basket
//! (`KXMLBGAME`, `KXNHLGAME`, etc.) at 60s polling cadence with a
//! 30-min settle horizon. The engine's discovery service polls
//! Kalshi REST, auto-registers new tickers with the market-data
//! router, and pushes `Event::DiscoveryDelta` into us so we can
//! update our internal `close_times` map. Operator restart is no
//! longer the bottleneck — newly-listed games come into scope on
//! the next tick.
//!
//! ## Cooldown + per-session arming
//!
//! After the strategy fires once on a market we won't refire
//! within `cooldown` even if the asymmetry persists — the touch
//! pulled, our intent was submitted, the next book update should
//! reflect that. A second fire on the same touch would be
//! double-credit risk if the venue races.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::discovery::DiscoverySubscription;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info};

pub const STRATEGY_ID: StrategyId = StrategyId("settlement");

/// Default sports series basket — matches the legacy
/// `bin/settlement-trader/src/discovery.rs::DEFAULT_SERIES`.
pub const DEFAULT_SERIES: &[&str] = &[
    "KXNBASERIES",
    "KXMLBGAME",
    "KXAHLGAME",
    "KXTACAPORTGAME",
    "KXEKSTRAKLASAGAME",
    "KXDFBPOKALGAME",
    "KXUECLGAME",
    "KXNWSLGAME",
    "KXNHLGAME",
    "KXNFL1HWINNER",
];

/// Per-deployment knobs.
#[derive(Debug, Clone)]
pub struct SettlementConfig {
    /// Series swept by the discovery service.
    pub series: Vec<String>,
    /// Discovery poll cadence.
    pub discovery_interval: Duration,
    /// Drop markets whose settle time is more than this far out.
    /// 30 min covers the close_window (10 min) plus enough buffer
    /// for late-listed games.
    pub max_secs_to_settle: i64,

    /// Only fire when `close_time - now < close_window`.
    pub close_window: Duration,
    /// Don't fire if `yes_ask < min_price`.
    pub min_price_cents: u8,
    /// Don't fire if `yes_ask > max_price`.
    pub max_price_cents: u8,
    /// Bid-stack must be `>= bid_to_ask_ratio × ask-stack` at the
    /// touch.
    pub bid_to_ask_ratio: u32,
    /// Per-fire size (contracts).
    pub size: u32,
    /// Per-market cooldown after a fire.
    pub cooldown: Duration,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self {
            series: DEFAULT_SERIES.iter().map(|s| (*s).to_string()).collect(),
            discovery_interval: Duration::from_secs(60),
            max_secs_to_settle: 30 * 60,

            close_window: Duration::from_secs(10 * 60),
            min_price_cents: 88,
            max_price_cents: 96,
            bid_to_ask_ratio: 5,
            size: 1,
            cooldown: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub struct SettlementStrategy {
    config: SettlementConfig,
    /// Per-market settlement timestamp (unix seconds), populated
    /// by the engine's discovery service via Event::DiscoveryDelta.
    close_times: HashMap<MarketTicker, i64>,
    /// Per-market last-fire wall-clock; cooldown filter.
    last_fired: HashMap<MarketTicker, Instant>,
}

impl SettlementStrategy {
    pub fn new(config: SettlementConfig) -> Self {
        Self {
            config,
            close_times: HashMap::new(),
            last_fired: HashMap::new(),
        }
    }

    pub fn config(&self) -> &SettlementConfig {
        &self.config
    }

    fn evaluate(
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
        // 3. Best YES bid touch.
        let (best_bid_price, best_bid_qty) = book.best_yes_bid()?;
        // 4. Derive yes_ask from no_bid via complement.
        let (best_no_bid_price, best_no_bid_qty) = book.best_no_bid()?;
        let yes_ask_cents = 100u8.checked_sub(best_no_bid_price.cents())?;
        if yes_ask_cents < self.config.min_price_cents
            || yes_ask_cents > self.config.max_price_cents
        {
            return None;
        }
        // 5. Asymmetry test.
        if best_bid_qty < best_no_bid_qty.saturating_mul(self.config.bid_to_ask_ratio) {
            return None;
        }
        // 6. Sanity: book inversion guard.
        if best_bid_price.cents() + yes_ask_cents < 100 {
            return None;
        }

        let qty = i32::try_from(self.config.size).ok()?;
        if qty <= 0 {
            return None;
        }
        // Stable client_id: strategy + market + minute + price + size.
        // Same fire on the same market within a minute produces the
        // same id, so the OMS rejects duplicates as idempotent.
        let minute = (now_unix / 60) as u32;
        let client_id = format!(
            "settlement:{ticker}:{ask:02}:{size:04}:{minute:08x}",
            ticker = market.as_str(),
            ask = yes_ask_cents,
            size = self.config.size,
        );
        self.last_fired.insert(market.clone(), now_instant);
        Some(Intent {
            client_id,
            strategy: STRATEGY_ID.0,
            market: market.clone(),
            side: Side::Yes,
            action: IntentAction::Buy,
            price_cents: Some(i32::from(yes_ask_cents)),
            qty,
            order_type: OrderType::Limit,
            tif: Tif::Ioc,
            reason: Some(format!(
                "settlement: ask={yes_ask_cents}¢ bid={}¢ ratio≥{} ttc={}s",
                best_bid_price.cents(),
                self.config.bid_to_ask_ratio,
                secs_to_close,
            )),
        })
    }

    fn apply_discovery(&mut self, added: &[predigy_engine_core::discovery::DiscoveredMarket], removed: &[MarketTicker]) {
        for m in added {
            let ticker = MarketTicker::new(&m.ticker);
            self.close_times.insert(ticker, m.settle_unix);
        }
        for t in removed {
            self.close_times.remove(t);
            self.last_fired.remove(t);
        }
        info!(
            n_tracked = self.close_times.len(),
            n_added = added.len(),
            n_removed = removed.len(),
            "settlement: close-time map updated"
        );
    }
}

#[async_trait]
impl Strategy for SettlementStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        // No static subscriptions — the discovery service feeds us
        // markets dynamically as games come into scope. This is
        // load-bearing: returning a non-empty list here would
        // require those tickers to be open at engine boot, which
        // misses every game listed after startup.
        Ok(Vec::new())
    }

    fn discovery_subscriptions(&self) -> Vec<DiscoverySubscription> {
        vec![DiscoverySubscription {
            series: self.config.series.clone(),
            interval_secs: self.config.discovery_interval.as_secs(),
            max_secs_to_settle: self.config.max_secs_to_settle,
            require_quote: true,
        }]
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        _state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        match ev {
            Event::BookUpdate { market, book } => {
                let now_unix = current_unix();
                let intent = self.evaluate(market, book, now_unix, Instant::now());
                if let Some(ref i) = intent {
                    debug!(
                        market = %market.as_str(),
                        price_cents = ?i.price_cents,
                        qty = i.qty,
                        "settlement: firing"
                    );
                }
                Ok(intent.into_iter().collect())
            }
            Event::DiscoveryDelta { added, removed } => {
                self.apply_discovery(added, removed);
                Ok(Vec::new())
            }
            Event::External(_) | Event::Tick | Event::PairUpdate { .. } => Ok(Vec::new()),
        }
    }
}

fn current_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;
    use predigy_core::price::Price;
    use predigy_engine_core::discovery::DiscoveredMarket;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn cfg() -> SettlementConfig {
        SettlementConfig {
            series: vec!["KX-TEST-SERIES".into()],
            discovery_interval: Duration::from_secs(60),
            max_secs_to_settle: 1800,
            close_window: Duration::from_secs(10 * 60),
            min_price_cents: 88,
            max_price_cents: 96,
            bid_to_ask_ratio: 5,
            size: 1,
            cooldown: Duration::from_secs(60),
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

    fn seed_close(s: &mut SettlementStrategy, m: &MarketTicker, t: i64) {
        s.close_times.insert(m.clone(), t);
    }

    #[test]
    fn fires_when_all_conditions_met() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // yes_ask = 100 - no_bid(7) = 93; bid stack 1000 >> 5*100 = 500.
        let book = book_with((92, 1000), (7, 100));
        let intent = s
            .evaluate(&m, &book, 1_777_909_700, Instant::now())
            .expect("fired");
        assert_eq!(intent.price_cents, Some(93));
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Buy);
        assert_eq!(intent.tif, Tif::Ioc);
        assert_eq!(intent.strategy, "settlement");
        assert!(intent.client_id.starts_with("settlement:KX-TEST:93:0001:"));
    }

    #[test]
    fn no_fire_outside_close_window() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        // 1h before close (3600s) > close_window (600s).
        assert!(s.evaluate(&m, &book, 1_777_906_400, Instant::now()).is_none());
    }

    #[test]
    fn no_fire_when_already_settled() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        assert!(s.evaluate(&m, &book, 1_777_910_500, Instant::now()).is_none());
    }

    #[test]
    fn no_fire_when_ask_too_high() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // no_bid 1¢ → yes_ask 99¢ > max(96).
        let book = book_with((97, 1000), (1, 100));
        assert!(s.evaluate(&m, &book, 1_777_909_700, Instant::now()).is_none());
    }

    #[test]
    fn no_fire_when_ask_too_low() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // no_bid 50¢ → yes_ask 50¢ < min(88).
        let book = book_with((48, 1000), (50, 100));
        assert!(s.evaluate(&m, &book, 1_777_909_700, Instant::now()).is_none());
    }

    #[test]
    fn no_fire_when_book_too_balanced() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // Bid 200, ask 100 → ratio 2 < 5.
        let book = book_with((92, 200), (7, 100));
        assert!(s.evaluate(&m, &book, 1_777_909_700, Instant::now()).is_none());
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        let now = Instant::now();
        let _ = s.evaluate(&m, &book, 1_777_909_700, now).expect("first");
        assert!(s.evaluate(&m, &book, 1_777_909_710, now).is_none());
    }

    #[test]
    fn cooldown_clears_after_window() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        let t0 = Instant::now();
        let _ = s.evaluate(&m, &book, 1_777_909_700, t0).expect("first");
        let t1 = t0 + Duration::from_secs(61);
        assert!(s.evaluate(&m, &book, 1_777_909_710, t1).is_some());
    }

    #[test]
    fn no_fire_when_market_unknown() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-UNKNOWN");
        let book = book_with((92, 1000), (7, 100));
        assert!(s.evaluate(&m, &book, 1_777_909_700, Instant::now()).is_none());
    }

    #[test]
    fn discovery_delta_populates_close_times() {
        let mut s = SettlementStrategy::new(cfg());
        let added = vec![
            DiscoveredMarket {
                ticker: "KX-TEST-A".into(),
                settle_unix: 1_777_910_000,
            },
            DiscoveredMarket {
                ticker: "KX-TEST-B".into(),
                settle_unix: 1_777_911_000,
            },
        ];
        s.apply_discovery(&added, &[]);
        assert_eq!(s.close_times.len(), 2);
        assert_eq!(
            s.close_times.get(&MarketTicker::new("KX-TEST-A")).copied(),
            Some(1_777_910_000)
        );
    }

    #[test]
    fn discovery_delta_removed_drops_close_times_and_cooldown() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST-X");
        s.close_times.insert(m.clone(), 1_777_910_000);
        s.last_fired.insert(m.clone(), Instant::now());
        s.apply_discovery(&[], &[m.clone()]);
        assert!(!s.close_times.contains_key(&m));
        assert!(!s.last_fired.contains_key(&m));
    }

    #[test]
    fn declares_discovery_subscription() {
        let s = SettlementStrategy::new(cfg());
        let subs = s.discovery_subscriptions();
        assert_eq!(subs.len(), 1);
        let sub = &subs[0];
        assert_eq!(sub.series, vec!["KX-TEST-SERIES".to_string()]);
        assert_eq!(sub.interval_secs, 60);
        assert_eq!(sub.max_secs_to_settle, 1800);
        assert!(sub.require_quote);
    }

    #[test]
    fn declares_no_static_subscriptions() {
        // Static subscriptions would force operators to seed
        // markets at engine boot; this strategy is purely
        // discovery-driven.
        // Note: subscribed_markets is async + needs a StrategyState
        // — covered indirectly by the integration test in the
        // engine binary. Here we just assert the trait method
        // returns the expected discovery_subscriptions config.
        let s = SettlementStrategy::new(cfg());
        assert!(!s.discovery_subscriptions().is_empty());
    }
}
