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
    /// **Phase 6.1 active exits**:
    /// take-profit threshold in cents per contract. When the
    /// open position's current mark exceeds entry by this
    /// amount, emit a closing IOC. `0` disables.
    pub take_profit_cents: i32,
    /// Stop-loss threshold in cents per contract. When mark
    /// drops below entry by this amount, emit a closing IOC.
    /// `0` disables.
    pub stop_loss_cents: i32,
}

impl Default for StatConfig {
    fn default() -> Self {
        Self {
            bankroll_cents: 500,
            kelly_factor: 0.25,
            max_size: 3,
            cooldown: Duration::from_secs(60),
            rule_refresh_interval: Duration::from_secs(60),
            // Phase 6.1 defaults: take 8¢ profit, cap 5¢ loss.
            // 0 disables. Operator can tune via the (future)
            // CLI / env-var override surface.
            take_profit_cents: 8,
            stop_loss_cents: 5,
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

/// In-memory open-position snapshot. Refreshed on Tick + first
/// call. Stale up to `rule_refresh_interval` (default 60s). For
/// adverse-drift / take-profit exits this is acceptable —
/// exits don't need to react in milliseconds; second-scale lag
/// is fine.
#[derive(Debug, Clone)]
struct CachedPosition {
    side: Side,
    /// Signed: positive = long (buy-side fills), negative = short.
    signed_qty: i32,
    avg_entry_cents: i32,
}

#[derive(Debug)]
pub struct StatStrategy {
    config: StatConfig,
    rules: HashMap<String, CachedRule>,
    /// Per-(ticker, side) open positions. Key is `"{ticker}:{side_tag}"`
    /// since the OMS allows one open position per (strategy, ticker, side).
    positions: HashMap<String, CachedPosition>,
    last_fire_at: HashMap<String, Instant>,
    last_exit_at: HashMap<String, Instant>,
    last_rule_refresh: Option<Instant>,
    last_position_refresh: Option<Instant>,
}

impl StatStrategy {
    pub fn new(config: StatConfig) -> Self {
        Self {
            config,
            rules: HashMap::new(),
            positions: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_exit_at: HashMap::new(),
            last_rule_refresh: None,
            last_position_refresh: None,
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

    /// Phase 6.1 — refresh the in-memory open-position cache from
    /// Postgres. Called on Tick + first event. The cache is keyed
    /// by `"{ticker}:{side_tag}"` since the OMS guarantees at most
    /// one open position per (strategy, ticker, side).
    async fn refresh_positions(
        &mut self,
        state: &mut StrategyState,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let rows = state.db.open_positions(Some(STRATEGY_ID.0)).await?;
        let n = rows.len();
        let mut next: HashMap<String, CachedPosition> = HashMap::with_capacity(n);
        for r in rows {
            let side = match r.side.as_str() {
                "yes" => Side::Yes,
                "no" => Side::No,
                _ => continue,
            };
            let key = position_key(&r.ticker, side);
            next.insert(
                key,
                CachedPosition {
                    side,
                    signed_qty: r.current_qty,
                    avg_entry_cents: r.avg_entry_cents,
                },
            );
        }
        self.positions = next;
        self.last_position_refresh = Some(Instant::now());
        debug!(n_positions = n, "stat: position cache refreshed");
        Ok(())
    }

    /// Phase 6.1 — evaluate active-exit conditions against an
    /// open position for the given market. Returns a closing
    /// `Intent` (sell-IOC at the current best opposite-bid) when
    /// either the take-profit or stop-loss threshold trips.
    /// Returns `None` if the position should keep running.
    fn evaluate_exit(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Option<Intent> {
        // Per-position cooldown so a still-favorable book doesn't
        // re-fire the close intent every book delta. The
        // OMS/idempotency layer would dedupe anyway via the
        // deterministic exit cid below, but the cooldown saves
        // round trips.
        for side in [Side::Yes, Side::No] {
            let key = position_key(market.as_str(), side);
            let pos = match self.positions.get(&key) {
                Some(p) => p.clone(),
                None => continue,
            };
            if pos.signed_qty == 0 {
                continue;
            }
            if let Some(&last) = self.last_exit_at.get(&key)
                && now.duration_since(last) < self.config.cooldown
            {
                continue;
            }

            // Mark = price we'd realize unwinding. For a long YES
            // we'd sell into best_yes_bid. For a long NO we'd sell
            // into best_no_bid. (Short positions are not normally
            // produced by this strategy's IOC-buy intents, but we
            // handle them symmetrically for safety.)
            let mark_cents = match (pos.side, pos.signed_qty.is_positive()) {
                (Side::Yes, true) => book.best_yes_bid()?.0.cents() as i32,
                (Side::No, true) => book.best_no_bid()?.0.cents() as i32,
                (Side::Yes, false) => 100i32 - book.best_no_bid()?.0.cents() as i32,
                (Side::No, false) => 100i32 - book.best_yes_bid()?.0.cents() as i32,
            };

            // Per-contract P&L in cents. For a long: mark - entry.
            // For a short: entry - mark (we shorted high, want to
            // cover low).
            let pnl_per = if pos.signed_qty > 0 {
                mark_cents - pos.avg_entry_cents
            } else {
                pos.avg_entry_cents - mark_cents
            };

            let take =
                self.config.take_profit_cents > 0 && pnl_per >= self.config.take_profit_cents;
            let stop = self.config.stop_loss_cents > 0 && pnl_per <= -self.config.stop_loss_cents;
            if !(take || stop) {
                continue;
            }

            // Construct the closing intent. Sell on the same
            // contract leg we hold; buy if we're short. Limit
            // price = current mark so the IOC takes the touch.
            let action = if pos.signed_qty > 0 {
                IntentAction::Sell
            } else {
                IntentAction::Buy
            };
            let abs_qty = pos.signed_qty.unsigned_abs() as i32;
            // Mark is in [0, 100]; clamp to [1, 99] so we don't
            // try to submit at boundary (Kalshi rejects).
            let limit_cents = mark_cents.clamp(1, 99);
            // Stable client_id: minute bucket so the same exit
            // condition firing across multiple book deltas in the
            // same minute collapses idempotently in the OMS.
            let minute = (chrono::Utc::now().timestamp() / 60) as u32;
            let side_tag = match pos.side {
                Side::Yes => "Y",
                Side::No => "N",
            };
            let reason_tag = if take { "tp" } else { "sl" };
            let client_id = format!(
                "stat-exit:{ticker}:{side_tag}:{tag}:{minute:08x}",
                ticker = market.as_str(),
                tag = reason_tag,
            );

            let intent = Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: market.clone(),
                side: pos.side,
                action,
                price_cents: Some(limit_cents),
                qty: abs_qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "stat-exit: {reason_tag} entry={}¢ mark={}¢ pnl={}¢/contract",
                    pos.avg_entry_cents, mark_cents, pnl_per
                )),
            };
            info!(
                market = %market.as_str(),
                side = ?pos.side,
                signed_qty = pos.signed_qty,
                avg_entry = pos.avg_entry_cents,
                mark = mark_cents,
                pnl_per,
                trigger = reason_tag,
                "stat: emitting exit"
            );
            self.last_exit_at.insert(key, now);
            return Some(intent);
        }
        None
    }
}

fn position_key(ticker: &str, side: Side) -> String {
    let tag = match side {
        Side::Yes => 'y',
        Side::No => 'n',
    };
    format!("{ticker}:{tag}")
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
        let needs_rule_refresh = self
            .last_rule_refresh
            .is_none_or(|t| t.elapsed() >= self.config.rule_refresh_interval);
        if needs_rule_refresh {
            self.refresh_rules(state).await?;
        }

        // Phase 6.1 — refresh the open-position cache on the same
        // cadence. Stale up to one rule_refresh_interval (default
        // 60s). For exit logic this is acceptable; new positions
        // become exit-eligible within one cadence of the fill.
        let needs_position_refresh = self
            .last_position_refresh
            .is_none_or(|t| t.elapsed() >= self.config.rule_refresh_interval);
        if needs_position_refresh {
            self.refresh_positions(state).await?;
        }

        match ev {
            Event::BookUpdate { market, book } => {
                let now = Instant::now();
                let mut intents = Vec::new();
                // Entry: if there's a rule for this ticker and an
                // edge, fire.
                if let Some(entry) = self.evaluate(market, book, now) {
                    intents.push(entry);
                }
                // Exit: if there's an open position for this
                // ticker and the take-profit / stop-loss trips,
                // fire a closing IOC.
                if let Some(exit) = self.evaluate_exit(market, book, now) {
                    intents.push(exit);
                }
                Ok(intents)
            }
            Event::Tick => {
                // Tick is the cache-refresh trigger; we already
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
            // Exit thresholds chosen so unit tests can drive
            // them deterministically: take 8¢, stop 5¢.
            take_profit_cents: 8,
            stop_loss_cents: 5,
        }
    }

    fn cached_position(side: Side, signed_qty: i32, avg_entry_cents: i32) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents,
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

    // ─── Phase 6.1 active-exit tests ─────────────────────────

    #[test]
    fn exit_take_profit_long_yes() {
        // Long YES at 50¢. Mark (best YES bid) = 60¢. PnL = +10¢
        // ≥ take_profit (8¢) → fire a sell-IOC.
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-A");
        s.positions.insert(
            position_key("KX-EXIT-A", Side::Yes),
            cached_position(Side::Yes, 5, 50),
        );
        let book = book_with_quotes(Some(60), Some(35));
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("take-profit fires");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 5);
        assert_eq!(intent.price_cents, Some(60));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(intent.client_id.starts_with("stat-exit:KX-EXIT-A:Y:tp:"));
    }

    #[test]
    fn exit_stop_loss_long_yes() {
        // Long YES at 50¢. Mark = 44¢. PnL = -6¢ ≤ -stop_loss (5¢)
        // → fire a sell-IOC at the loss-cap.
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-B");
        s.positions.insert(
            position_key("KX-EXIT-B", Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book_with_quotes(Some(44), Some(50));
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("stop-loss fires");
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 4);
        assert_eq!(intent.price_cents, Some(44));
        assert!(intent.client_id.contains(":sl:"));
    }

    #[test]
    fn no_exit_when_inside_band() {
        // Long YES at 50¢. Mark = 53¢. PnL = +3¢ inside [-5, +8].
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-C");
        s.positions.insert(
            position_key("KX-EXIT-C", Side::Yes),
            cached_position(Side::Yes, 3, 50),
        );
        let book = book_with_quotes(Some(53), Some(40));
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }

    #[test]
    fn exit_take_profit_long_no() {
        // Long NO at 30¢. Mark (best NO bid) = 40¢. PnL = +10¢
        // ≥ take_profit (8¢) → fire sell-NO IOC.
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-D");
        s.positions.insert(
            position_key("KX-EXIT-D", Side::No),
            cached_position(Side::No, 6, 30),
        );
        let book = book_with_quotes(Some(55), Some(40));
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("NO-side take-profit fires");
        assert_eq!(intent.side, Side::No);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.price_cents, Some(40));
    }

    #[test]
    fn exit_cooldown_blocks_repeat() {
        // First exit fires; immediate retry within cooldown is
        // suppressed even though the trigger persists.
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-E");
        s.positions.insert(
            position_key("KX-EXIT-E", Side::Yes),
            cached_position(Side::Yes, 5, 50),
        );
        let book = book_with_quotes(Some(60), Some(35));
        let now = Instant::now();
        assert!(s.evaluate_exit(&market, &book, now).is_some());
        assert!(s.evaluate_exit(&market, &book, now).is_none());
    }

    #[test]
    fn exit_only_for_known_position() {
        // No cached position for this market → no exit.
        let mut s = StatStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-F");
        let book = book_with_quotes(Some(60), Some(35));
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }

    #[test]
    fn exit_disabled_when_thresholds_zero() {
        // take_profit_cents=0 + stop_loss_cents=0 → never fires.
        let mut cfg_off = cfg();
        cfg_off.take_profit_cents = 0;
        cfg_off.stop_loss_cents = 0;
        let mut s = StatStrategy::new(cfg_off);
        let market = MarketTicker::new("KX-EXIT-G");
        s.positions.insert(
            position_key("KX-EXIT-G", Side::Yes),
            cached_position(Side::Yes, 5, 50),
        );
        // Mark would normally trigger take-profit.
        let book = book_with_quotes(Some(95), Some(2));
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }
}
