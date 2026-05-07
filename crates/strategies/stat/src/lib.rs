// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-stat`: the statistical-probability trading
//! strategy module. Implements `predigy_engine_core::Strategy`.
//!
//! Logic preserved verbatim from `bin/stat-trader/src/strategy.rs`
//! (the legacy daemon being phased out as Phase 3-5 of the engine
//! refactor lands). The math: per-market model probability vs the
//! current Kalshi-side ask, after fees, sized by Kelly. Fires when
//! after-fee per-contract edge clears `min_edge_cents`.
//!
//! ## How rules flow in
//!
//! Rules live in the `rules` table (see migrations/0001_initial.sql).
//! At startup the strategy loads everything for `strategy='stat'`
//! and subscribes to those market tickers. On every `Event::Tick`
//! it refreshes the rule cache; new rules from a curator's recent
//! upsert become live within one tick interval.
//!
//! ## Cooldown
//!
//! Per-market last-fire timestamp prevents re-firing on every
//! book delta. Default 60 seconds; overrideable via `StatConfig`.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("stat");

/// Tunable per-deployment risk parameters. Matches
/// `bin/stat-trader/src/strategy.rs::StatConfig` so behaviour is
/// preserved across the port.
#[derive(Debug, Clone)]
pub struct StatConfig {
    /// Bankroll in cents. Input to Kelly sizing.
    pub bankroll_cents: u64,
    /// Fractional Kelly modifier ∈ (0, 1]. Quarter-Kelly default.
    pub kelly_factor: f64,
    /// Hard cap on contracts per fire.
    pub max_size: u32,
    /// Per-market cooldown between fires.
    pub cooldown: Duration,
    /// How often to reload the rule cache from Postgres.
    pub rule_refresh_interval: Duration,
}

impl Default for StatConfig {
    fn default() -> Self {
        Self {
            bankroll_cents: 500,
            kelly_factor: 0.25,
            max_size: 3,
            cooldown: Duration::from_secs(60),
            rule_refresh_interval: Duration::from_secs(60),
        }
    }
}

/// In-memory rule loaded from the `rules` table. Indexed by
/// market ticker.
#[derive(Debug, Clone)]
struct CachedRule {
    side: Side,
    model_p: f64,
    min_edge_cents: i32,
}

#[derive(Debug)]
pub struct StatStrategy {
    config: StatConfig,
    rules: HashMap<String, CachedRule>,
    last_fire_at: HashMap<String, Instant>,
    last_rule_refresh: Option<Instant>,
}

impl StatStrategy {
    pub fn new(config: StatConfig) -> Self {
        Self {
            config,
            rules: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_rule_refresh: None,
        }
    }

    async fn refresh_rules(
        &mut self,
        state: &mut StrategyState,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let rows = state.db.active_rules(STRATEGY_ID.0).await?;
        let n = rows.len();
        let mut next: HashMap<String, CachedRule> = HashMap::with_capacity(n);
        for r in rows {
            let side = match r.side.as_str() {
                "yes" => Side::Yes,
                "no" => Side::No,
                other => {
                    warn!(side = other, ticker = %r.ticker, "stat: unknown side; skipping rule");
                    continue;
                }
            };
            next.insert(
                r.ticker,
                CachedRule {
                    side,
                    model_p: r.model_p,
                    min_edge_cents: r.min_edge_cents,
                },
            );
        }
        self.rules = next;
        self.last_rule_refresh = Some(Instant::now());
        info!(n_rules = n, "stat: rule cache refreshed");
        Ok(())
    }

    fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Option<Intent> {
        let key = market.as_str().to_string();
        let rule = self.rules.get(&key)?;
        if let Some(&last) = self.last_fire_at.get(&key)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let (ask_cents, available_qty) = derive_ask(book, rule.side)?;
        let intent = build_intent(market, rule, &self.config, ask_cents, available_qty)?;
        self.last_fire_at.insert(key, now);
        Some(intent)
    }
}

#[async_trait]
impl Strategy for StatStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        let rows = state.db.active_rules(STRATEGY_ID.0).await?;
        Ok(rows
            .into_iter()
            .map(|r| MarketTicker::new(&r.ticker))
            .collect())
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // Rule cache refresh — periodic + lazy. Always refresh on
        // first call.
        let needs_refresh = self
            .last_rule_refresh
            .is_none_or(|t| t.elapsed() >= self.config.rule_refresh_interval);
        if needs_refresh {
            self.refresh_rules(state).await?;
        }

        match ev {
            Event::BookUpdate { market, book } => {
                let now = Instant::now();
                Ok(self.evaluate(market, book, now).into_iter().collect())
            }
            Event::Tick => {
                // Tick is the rule-refresh trigger; we already
                // refreshed above. No intents from a bare tick.
                Ok(Vec::new())
            }
            Event::External(_) | Event::DiscoveryDelta { .. } | Event::PairUpdate { .. } => {
                Ok(Vec::new())
            }
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.rule_refresh_interval)
    }
}

/// `(ask_cents, qty_at_touch)` for the requested side, using the
/// complement-of-opposite-bid trick. Identical to the legacy
/// stat-trader's `derive_ask`.
fn derive_ask(book: &OrderBook, side: Side) -> Option<(u8, u32)> {
    let (px, qty) = match side {
        Side::Yes => book.best_no_bid()?,
        Side::No => book.best_yes_bid()?,
    };
    let ask = 100u8.checked_sub(px.cents())?;
    Some((ask, qty))
}

fn build_intent(
    market: &MarketTicker,
    rule: &CachedRule,
    config: &StatConfig,
    ask_cents: u8,
    available_qty: u32,
) -> Option<Intent> {
    if !(0.01..=0.99).contains(&rule.model_p) {
        return None;
    }
    if ask_cents == 0 || ask_cents >= 100 {
        return None;
    }
    let ask_dollars = f64::from(ask_cents) / 100.0;
    let bet_p = match rule.side {
        Side::Yes => rule.model_p,
        Side::No => 1.0 - rule.model_p,
    };
    let kelly_f =
        predigy_signals::kelly::fraction_with_factor(bet_p, ask_dollars, config.kelly_factor)
            .ok()?;
    if kelly_f <= 0.0 {
        return None;
    }
    let raw_edge_cents = (bet_p - ask_dollars) * 100.0;
    let kalshi_price = predigy_core::price::Price::from_cents(ask_cents).ok()?;
    let probe_qty = predigy_core::price::Qty::new(1).ok()?;
    let fee_per_contract = predigy_core::fees::taker_fee(kalshi_price, probe_qty);
    if (raw_edge_cents - f64::from(fee_per_contract)) < f64::from(rule.min_edge_cents) {
        debug!(
            market = %market.as_str(),
            raw_edge = raw_edge_cents,
            fee_per_contract,
            min_edge = rule.min_edge_cents,
            "stat: edge below threshold"
        );
        return None;
    }
    let target = predigy_signals::kelly::contracts_to_buy(
        config.bankroll_cents,
        ask_cents,
        kelly_f,
        config.max_size,
    );
    if target == 0 {
        return None;
    }
    let size = target.min(available_qty);
    if size == 0 {
        return None;
    }
    // Stable client_id: strategy + market + price + size.
    // Same fire on the same market within the cooldown produces
    // the same id (idempotent in the OMS).
    let client_id = format!(
        "stat:{ticker}:{ask:02}:{size:04}:{ts:08x}",
        ticker = market.as_str(),
        ask = ask_cents,
        size = size,
        ts = chrono::Utc::now().timestamp() as u32 / 60,
    );
    let qty = i32::try_from(size).ok()?;
    Some(Intent {
        client_id,
        strategy: STRATEGY_ID.0,
        market: market.clone(),
        side: rule.side,
        action: IntentAction::Buy,
        price_cents: Some(i32::from(ask_cents)),
        qty,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some(format!(
            "stat fire: model_p={:.3} ask={}c edge={:.1}c size={}",
            rule.model_p, ask_cents, raw_edge_cents, size
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::price::Price;

    fn book_with_quotes(yes_bid: Option<u8>, no_bid: Option<u8>) -> OrderBook {
        let mut b = OrderBook::new("KX-TEST");
        let snap = predigy_book::Snapshot {
            seq: 1,
            yes_bids: yes_bid
                .map(|c| vec![(Price::from_cents(c).unwrap(), 100)])
                .unwrap_or_default(),
            no_bids: no_bid
                .map(|c| vec![(Price::from_cents(c).unwrap(), 100)])
                .unwrap_or_default(),
        };
        b.apply_snapshot(snap);
        b
    }

    fn cached_rule(model_p: f64, side: Side, min_edge: i32) -> CachedRule {
        CachedRule {
            side,
            model_p,
            min_edge_cents: min_edge,
        }
    }

    fn cfg() -> StatConfig {
        StatConfig {
            bankroll_cents: 10_000,
            kelly_factor: 0.25,
            max_size: 100,
            cooldown: Duration::from_secs(60),
            rule_refresh_interval: Duration::from_secs(60),
        }
    }

    #[test]
    fn derive_ask_yes_uses_complement_of_no_bid() {
        let book = book_with_quotes(None, Some(60));
        // YES ask = 100 - NO bid 60 = 40.
        let (ask, qty) = derive_ask(&book, Side::Yes).unwrap();
        assert_eq!(ask, 40);
        assert_eq!(qty, 100);
    }

    #[test]
    fn derive_ask_returns_none_when_book_empty() {
        let book = OrderBook::new("KX-TEST");
        assert!(derive_ask(&book, Side::Yes).is_none());
        assert!(derive_ask(&book, Side::No).is_none());
    }

    #[test]
    fn build_intent_fires_when_edge_clears_threshold() {
        let market = MarketTicker::new("KX-TEST-A");
        let rule = cached_rule(0.70, Side::Yes, 5);
        // YES ask 50¢, model_p 0.70 → raw edge 20¢ before fees.
        let intent = build_intent(&market, &rule, &cfg(), 50, 100).unwrap();
        assert_eq!(intent.market.as_str(), "KX-TEST-A");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Buy);
        assert_eq!(intent.price_cents, Some(50));
        assert!(intent.qty > 0);
    }

    #[test]
    fn build_intent_skips_when_edge_below_threshold() {
        let market = MarketTicker::new("KX-TEST-B");
        // YES ask 65¢, model_p 0.70 → raw edge ~5¢, after fees < 10¢.
        let rule = cached_rule(0.70, Side::Yes, 10);
        assert!(build_intent(&market, &rule, &cfg(), 65, 100).is_none());
    }

    #[test]
    fn build_intent_skips_when_ask_at_rails() {
        let market = MarketTicker::new("KX-TEST-C");
        let rule = cached_rule(0.70, Side::Yes, 5);
        // ask = 0 → no edge possible (would mean YES is free).
        assert!(build_intent(&market, &rule, &cfg(), 0, 100).is_none());
        // ask = 100 → fully priced, no edge.
        assert!(build_intent(&market, &rule, &cfg(), 100, 100).is_none());
    }

    #[test]
    fn build_intent_skips_when_model_p_outside_valid_range() {
        let market = MarketTicker::new("KX-TEST-D");
        let too_low = cached_rule(0.005, Side::Yes, 5);
        let too_high = cached_rule(0.995, Side::Yes, 5);
        assert!(build_intent(&market, &too_low, &cfg(), 50, 100).is_none());
        assert!(build_intent(&market, &too_high, &cfg(), 50, 100).is_none());
    }

    #[test]
    fn build_intent_size_capped_by_available_qty() {
        let market = MarketTicker::new("KX-TEST-E");
        let rule = cached_rule(0.70, Side::Yes, 5);
        // Plenty of edge, but only 2 contracts available.
        let intent = build_intent(&market, &rule, &cfg(), 50, 2).unwrap();
        assert!(intent.qty <= 2);
    }

    #[test]
    fn no_side_evaluation_uses_complement_probability() {
        let market = MarketTicker::new("KX-TEST-F");
        // Model says YES is 30% likely → NO is 70% likely.
        let rule = cached_rule(0.30, Side::No, 5);
        // For a NO bet at NO-ask 50¢, bet_p = 0.70.
        let intent = build_intent(&market, &rule, &cfg(), 50, 100).unwrap();
        assert_eq!(intent.side, Side::No);
    }

    #[test]
    fn evaluate_respects_cooldown() {
        let mut s = StatStrategy::new(cfg());
        s.rules
            .insert("KX-TEST-G".into(), cached_rule(0.70, Side::Yes, 5));
        let market = MarketTicker::new("KX-TEST-G");
        let book = book_with_quotes(None, Some(50)); // YES ask = 50

        let now = Instant::now();
        let first = s.evaluate(&market, &book, now);
        assert!(first.is_some());
        let second = s.evaluate(&market, &book, now);
        assert!(
            second.is_none(),
            "second fire within cooldown should be suppressed"
        );

        // Fast-forward past the cooldown.
        let later = now + Duration::from_secs(120);
        let third = s.evaluate(&market, &book, later);
        assert!(third.is_some(), "fire should resume after cooldown");
    }
}
