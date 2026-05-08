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
use predigy_engine_core::cross_strategy::{CrossStrategyEvent, topic};
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::collections::{HashMap, HashSet};
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
    /// Per-market cooldown after an exit attempt before re-entry.
    pub reentry_cooldown: Duration,
    /// Only enter rules whose embedded market date is today's local date.
    pub same_day_only: bool,
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
    /// **Phase 6+ A1 belief-drift exit**: minimum residual edge
    /// (cents) before the strategy holds a position. If the
    /// curator updates `model_p` such that
    /// `model_p_cents - mark_cents < min_residual_edge_cents`,
    /// the original entry thesis is invalidated even before
    /// price moves. Emit a closing IOC. `0` disables.
    pub min_residual_edge_cents: i32,
    /// **Phase 6+ A4 time-decay TP scaling**: how many cents to
    /// shave off the take-profit threshold per hour held. The
    /// longer a position runs the more likely the original
    /// model edge has decayed; tightening TP encourages exits.
    /// Effective TP is clamped at 1¢ minimum. `0` disables.
    pub tp_decay_per_hour_cents: i32,
    /// **Audit A3 — trailing stop**: once the position's
    /// per-contract PnL has reached `trailing_trigger_cents`,
    /// the effective stop floats up to
    /// `high_water_pnl - trailing_distance_cents` (clamped
    /// below by `-stop_loss_cents`). The trailing stop only
    /// ratchets up — never down. `0` for either disables.
    pub trailing_trigger_cents: i32,
    pub trailing_distance_cents: i32,
    /// **Audit I3 — cross-strategy belief augmentation**:
    /// when cross-arb publishes a `PolyMidUpdate` for a Kalshi
    /// market that's also in stat's rule set, blend the poly
    /// mid into the effective probability for the entry edge
    /// calculation:
    ///   effective_p = α × rule.model_p + (1 − α) × poly_mid_yes
    ///
    /// `1.0` = pure rule (no blend, behavior unchanged from
    /// pre-I3); `0.0` = pure poly (all weight on cross-venue
    /// reference); default `0.85` (slight tilt toward poly when
    /// available; rule still dominates).
    ///
    /// When no poly mid is available for a ticker, the rule's
    /// `model_p` is used unchanged (regardless of α).
    pub poly_mid_blend_alpha: f64,
}

impl StatConfig {
    /// **Audit B2 + B3 — env-var overrides.** Read operator
    /// tunables from the environment, falling back to
    /// `Default::default()` for unset vars.
    ///
    /// Recognised vars (all parse as the obvious type):
    /// - `PREDIGY_STAT_BANKROLL_CENTS` (u64)
    /// - `PREDIGY_STAT_KELLY_FACTOR` (f64) — B2: half-Kelly is 0.5
    /// - `PREDIGY_STAT_MAX_SIZE` (u32)
    /// - `PREDIGY_STAT_COOLDOWN_MS` (u64) — B3: per-strategy cooldown
    /// - `PREDIGY_STAT_TAKE_PROFIT_CENTS` (i32)
    /// - `PREDIGY_STAT_STOP_LOSS_CENTS` (i32)
    /// - `PREDIGY_STAT_MIN_RESIDUAL_EDGE_CENTS` (i32) — A1
    /// - `PREDIGY_STAT_TP_DECAY_PER_HOUR_CENTS` (i32) — A4
    /// - `PREDIGY_STAT_TRAILING_TRIGGER_CENTS` (i32) — A3
    /// - `PREDIGY_STAT_TRAILING_DISTANCE_CENTS` (i32) — A3
    #[must_use]
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("PREDIGY_STAT_BANKROLL_CENTS") {
            if let Ok(n) = v.parse() {
                c.bankroll_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_KELLY_FACTOR") {
            if let Ok(n) = v.parse() {
                c.kelly_factor = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_MAX_SIZE") {
            if let Ok(n) = v.parse() {
                c.max_size = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_COOLDOWN_MS") {
            if let Ok(n) = v.parse::<u64>() {
                c.cooldown = Duration::from_millis(n);
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_REENTRY_COOLDOWN_MS") {
            if let Ok(n) = v.parse::<u64>() {
                c.reentry_cooldown = Duration::from_millis(n);
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_SAME_DAY_ONLY") {
            c.same_day_only = !matches!(v.trim(), "0" | "false" | "FALSE" | "False");
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_TAKE_PROFIT_CENTS") {
            if let Ok(n) = v.parse() {
                c.take_profit_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_STOP_LOSS_CENTS") {
            if let Ok(n) = v.parse() {
                c.stop_loss_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_MIN_RESIDUAL_EDGE_CENTS") {
            if let Ok(n) = v.parse() {
                c.min_residual_edge_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_TP_DECAY_PER_HOUR_CENTS") {
            if let Ok(n) = v.parse() {
                c.tp_decay_per_hour_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_TRAILING_TRIGGER_CENTS") {
            if let Ok(n) = v.parse() {
                c.trailing_trigger_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_TRAILING_DISTANCE_CENTS") {
            if let Ok(n) = v.parse() {
                c.trailing_distance_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_STAT_POLY_MID_BLEND_ALPHA") {
            if let Ok(n) = v.parse::<f64>() {
                c.poly_mid_blend_alpha = n.clamp(0.0, 1.0);
            }
        }
        c
    }
}

impl Default for StatConfig {
    fn default() -> Self {
        Self {
            bankroll_cents: 500,
            kelly_factor: 0.25,
            max_size: 3,
            cooldown: Duration::from_secs(60),
            reentry_cooldown: Duration::from_secs(30 * 60),
            same_day_only: true,
            rule_refresh_interval: Duration::from_secs(60),
            // Phase 6.1 defaults: take 8¢ profit, cap 5¢ loss.
            // 0 disables. Operator can tune via the (future)
            // CLI / env-var override surface.
            take_profit_cents: 8,
            stop_loss_cents: 5,
            // A1: residual-edge floor 2¢. Aligns with typical
            // entry min_edge_cents (5¢) — by the time edge has
            // collapsed to <2¢ the thesis is broken.
            min_residual_edge_cents: 2,
            // A4: 1¢ shaved off TP per hour held. After 7 hours
            // TP floors at 1¢ — almost any positive PnL exits.
            tp_decay_per_hour_cents: 1,
            // A3: trailing stop kicks in once we've seen ≥4¢
            // PnL; thereafter we exit if we give back ≥3¢ from
            // the high water. The clamp at -stop_loss_cents
            // means the trailing stop never relaxes our hard
            // floor.
            trailing_trigger_cents: 4,
            trailing_distance_cents: 3,
            // I3: 0.85 — moderately tilt toward poly when
            // available; rule still dominates. Set to 1.0 to
            // turn the blend off entirely.
            poly_mid_blend_alpha: 0.85,
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
    /// When the position was opened. Used by A4 (time-decay TP
    /// scaling).
    opened_at: chrono::DateTime<chrono::Utc>,
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
    /// A3 — high-water-mark of per-contract PnL per position.
    /// Ratchets up as mark moves favorably; consulted by the
    /// trailing-stop branch in evaluate_exit. Cleared when the
    /// position closes (refresh_positions retains only entries
    /// whose key is in the current cache).
    high_water_pnl: HashMap<String, i32>,
    /// I3 — latest Polymarket YES mid (cents 1..=99) per Kalshi
    /// ticker. Populated by Event::CrossStrategy(PolyMidUpdate)
    /// from cross-arb. Used by the entry edge calculation in
    /// `evaluate` to blend the poly reference into the rule's
    /// model_p (per `config.poly_mid_blend_alpha`).
    poly_mid_cents: HashMap<String, u8>,
    subscribed: HashSet<String>,
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
            high_water_pnl: HashMap::new(),
            poly_mid_cents: HashMap::new(),
            subscribed: HashSet::new(),
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
        let added_rule_markets: Vec<MarketTicker> = next
            .keys()
            .filter(|ticker| self.subscribed.insert((*ticker).clone()))
            .map(MarketTicker::new)
            .collect();
        if !added_rule_markets.is_empty() {
            state.subscribe_to_markets(added_rule_markets);
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
        if self.config.same_day_only {
            let today = local_yyyy_mm_dd();
            if embedded_market_date(market.as_str()).as_deref() != Some(today.as_str()) {
                return None;
            }
        }
        if self.has_open_position(market.as_str()) {
            return None;
        }
        if self.recent_exit_attempt(market.as_str(), now) {
            return None;
        }
        if let Some(&last) = self.last_fire_at.get(&key)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let (ask_cents, available_qty) = derive_ask(book, rule.side)?;
        // I3 — blend the poly mid into the rule's model_p when
        // both are available. The rule remains the primary signal
        // (alpha defaults 0.85); poly nudges the belief toward
        // the cross-venue reference. Skip when no poly mid is
        // cached — the rule alone drives.
        let blended_rule = match self.poly_mid_cents.get(&key).copied() {
            Some(poly_yes_cents) if self.config.poly_mid_blend_alpha < 1.0 => {
                let alpha = self.config.poly_mid_blend_alpha;
                let poly_p = f64::from(poly_yes_cents) / 100.0;
                let blended_p = alpha * rule.model_p + (1.0 - alpha) * poly_p;
                CachedRule {
                    side: rule.side,
                    model_p: blended_p.clamp(0.0, 1.0),
                    min_edge_cents: rule.min_edge_cents,
                }
            }
            _ => rule.clone(),
        };
        let intent = build_intent(
            market,
            &blended_rule,
            &self.config,
            ask_cents,
            available_qty,
        )?;
        self.last_fire_at.insert(key, now);
        Some(intent)
    }

    fn has_open_position(&self, ticker: &str) -> bool {
        self.positions
            .keys()
            .any(|key| key.split_once(':').is_some_and(|(t, _)| t == ticker))
    }

    fn recent_exit_attempt(&self, ticker: &str, now: Instant) -> bool {
        self.last_exit_at.iter().any(|(key, last)| {
            key.split_once(':').is_some_and(|(t, _)| t == ticker)
                && now.duration_since(*last) < self.config.reentry_cooldown
        })
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
                    opened_at: r.opened_at,
                },
            );
        }
        // A3 — drop high-water entries for positions that have
        // closed (no longer in `next`). Keeps the map bounded.
        self.high_water_pnl.retain(|k, _| next.contains_key(k));
        let held_markets: Vec<MarketTicker> = next
            .keys()
            .filter_map(|k| k.split_once(':').map(|(ticker, _)| ticker.to_string()))
            .filter(|ticker| self.subscribed.insert(ticker.clone()))
            .map(MarketTicker::new)
            .collect();
        if !held_markets.is_empty() {
            state.subscribe_to_markets(held_markets);
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

            // A4 — time-decay TP scaling. Shave the configured
            // TP by `tp_decay_per_hour_cents` per full hour held;
            // floor at 1¢. After many hours the strategy will
            // exit on almost any positive move.
            let hours_held =
                ((chrono::Utc::now() - pos.opened_at).num_seconds() / 3600).max(0) as i32;
            let effective_tp = if self.config.take_profit_cents > 0 {
                let shaved = self.config.take_profit_cents
                    - hours_held * self.config.tp_decay_per_hour_cents;
                shaved.max(1)
            } else {
                0
            };

            let take = effective_tp > 0 && pnl_per >= effective_tp;
            let stop = self.config.stop_loss_cents > 0 && pnl_per <= -self.config.stop_loss_cents;

            // A3 — trailing stop. Update the high-water mark
            // (ratchets up only) and check against the trailing
            // floor. Trailing fires only after the position has
            // crossed `trailing_trigger_cents`; before that the
            // hard `stop_loss_cents` is the only floor.
            let prev_high = self.high_water_pnl.get(&key).copied().unwrap_or(0);
            let new_high = prev_high.max(pnl_per);
            self.high_water_pnl.insert(key.clone(), new_high);
            let trailing = self.config.trailing_trigger_cents > 0
                && self.config.trailing_distance_cents > 0
                && new_high >= self.config.trailing_trigger_cents
                && pnl_per <= new_high - self.config.trailing_distance_cents;

            // A1 — belief-drift exit. Look up the current rule
            // for this market. If model_p has drifted such that
            // residual edge over the unwind mark is below
            // min_residual_edge_cents, the entry thesis is dead.
            // Effective probability to compare against mark
            // depends on the position side (we hold YES → bet
            // is YES; we hold NO → bet is NO at complement).
            // Note: position cache key is "ticker:side_tag" but
            // rule cache is keyed by ticker alone.
            let ticker_str = market.as_str().to_string();
            let mut belief_drift = false;
            if self.config.min_residual_edge_cents > 0 {
                if let Some(rule) = self.rules.get(&ticker_str) {
                    let bet_p = match pos.side {
                        Side::Yes => rule.model_p,
                        Side::No => 1.0 - rule.model_p,
                    };
                    let belief_cents = (bet_p * 100.0).round() as i32;
                    let residual_edge = belief_cents - mark_cents;
                    if residual_edge < self.config.min_residual_edge_cents {
                        belief_drift = true;
                    }
                } else {
                    // Curator removed the rule — orphan position.
                    // Strong exit signal.
                    belief_drift = true;
                }
            }

            if !(take || stop || trailing || belief_drift) {
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
            // Trigger priority: stop-loss (preserve capital) >
            // trailing stop (lock partial profit) > belief-drift
            // (thesis dead) > take-profit. Each gets a distinct
            // reason_tag for forensic logs + dashboard exit-kind
            // classification.
            let reason_tag = if stop {
                "sl"
            } else if trailing {
                "ts"
            } else if belief_drift {
                "bd"
            } else {
                "tp"
            };
            let client_id = format!(
                "stat-exit:{ticker}:{side_tag}:{tag}:{minute:08x}",
                ticker = cid_safe_ticker(market.as_str()),
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
                    "stat-exit: {reason_tag} entry={}¢ mark={}¢ pnl={}¢/contract \
                     hours_held={hours_held} effective_tp={effective_tp}¢",
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

fn local_yyyy_mm_dd() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn embedded_market_date(ticker: &str) -> Option<String> {
    ticker
        .split('-')
        .find_map(|segment| parse_yymmmdd_to_iso(segment))
}

fn parse_yymmmdd_to_iso(segment: &str) -> Option<String> {
    if segment.len() != 7 {
        return None;
    }
    let yy: u32 = segment.get(..2)?.parse().ok()?;
    let mon = segment.get(2..5)?.to_ascii_uppercase();
    let dd: u32 = segment.get(5..7)?.parse().ok()?;
    if !(1..=31).contains(&dd) {
        return None;
    }
    let mm = match mon.as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    };
    Some(format!("{:04}-{mm:02}-{dd:02}", 2000 + yy))
}

#[async_trait]
impl Strategy for StatStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    fn cross_strategy_subscriptions(&self) -> Vec<&'static str> {
        // Phase 6 — receive cross-arb's poly-mid updates for
        // any Kalshi market in cross-arb's pair set. The signal
        // doesn't change behavior yet; the receive side just
        // logs each update at debug. A future commit will
        // augment stat's belief by blending poly-mid into the
        // existing model_p.
        vec![topic::POLY_MID, topic::MODEL_PROBABILITY]
    }

    async fn subscribed_markets(
        &self,
        state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        let rows = state.db.active_rules(STRATEGY_ID.0).await?;
        let mut tickers: HashSet<String> = rows.into_iter().map(|r| r.ticker).collect();
        for p in state.db.open_positions(Some(STRATEGY_ID.0)).await? {
            tickers.insert(p.ticker);
        }
        Ok(tickers.into_iter().map(MarketTicker::new).collect())
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
                // Exit: if there's an open position for this
                // ticker and the take-profit / stop-loss trips,
                // fire a closing IOC.
                if let Some(exit) = self.evaluate_exit(market, book, now) {
                    intents.push(exit);
                    return Ok(intents);
                }
                // Entry: if there's a rule for this ticker and an
                // edge, fire. Entries are never emitted alongside an
                // exit for the same book update.
                if let Some(entry) = self.evaluate(market, book, now) {
                    intents.push(entry);
                }
                Ok(intents)
            }
            Event::Tick => {
                // Tick is the cache-refresh trigger; we already
                // refreshed above. No intents from a bare tick.
                Ok(Vec::new())
            }
            Event::CrossStrategy { source, payload } => {
                // I3 — receive cross-arb's poly-mid updates and
                // store them by Kalshi ticker. The entry edge
                // calculation in `evaluate` blends the cached
                // mid into the rule's model_p (see
                // `config.poly_mid_blend_alpha`).
                //
                // ModelProbabilityUpdate is logged-only for now;
                // future curators may publish here, in which
                // case stat would short-circuit the rule-poll
                // loop.
                match payload {
                    CrossStrategyEvent::PolyMidUpdate {
                        kalshi_ticker,
                        poly_mid_cents,
                    } => {
                        debug!(
                            source = source.0,
                            ticker = %kalshi_ticker,
                            poly_mid_cents,
                            "stat: poly-mid update received"
                        );
                        self.poly_mid_cents
                            .insert(kalshi_ticker.as_str().to_string(), *poly_mid_cents);
                    }
                    CrossStrategyEvent::ModelProbabilityUpdate {
                        ticker,
                        source: prov,
                        raw_p,
                        model_p,
                    } => debug!(
                        source = source.0,
                        ticker = %ticker,
                        provenance = %prov,
                        raw_p,
                        model_p,
                        "stat: model_p update received"
                    ),
                }
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
        ticker = cid_safe_ticker(market.as_str()),
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
            reentry_cooldown: Duration::from_secs(30 * 60),
            same_day_only: false,
            rule_refresh_interval: Duration::from_secs(60),
            // Exit thresholds chosen so unit tests can drive
            // them deterministically: take 8¢, stop 5¢.
            take_profit_cents: 8,
            stop_loss_cents: 5,
            // A1 + A4 + A3 + I3 disabled by default in tests so
            // existing tests keep their behavior. Tests that
            // exercise belief-drift / time-decay / trailing-stop
            // / poly-blend set them explicitly.
            min_residual_edge_cents: 0,
            tp_decay_per_hour_cents: 0,
            trailing_trigger_cents: 0,
            trailing_distance_cents: 0,
            // alpha=1.0 means pure rule (no blend); existing
            // tests don't exercise the blend.
            poly_mid_blend_alpha: 1.0,
        }
    }

    fn cached_position(side: Side, signed_qty: i32, avg_entry_cents: i32) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents,
            opened_at: chrono::Utc::now(),
        }
    }

    fn cached_position_aged(
        side: Side,
        signed_qty: i32,
        avg_entry_cents: i32,
        hours_old: i64,
    ) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents,
            opened_at: chrono::Utc::now() - chrono::Duration::hours(hours_old),
        }
    }

    fn dated_market_for_today() -> MarketTicker {
        let today = chrono::Local::now()
            .format("%y%b%d")
            .to_string()
            .to_uppercase();
        MarketTicker::new(format!("KXTEST-{today}-T50"))
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

    #[test]
    fn same_day_gate_blocks_non_today_market() {
        let mut s = StatStrategy::new(StatConfig {
            same_day_only: true,
            ..cfg()
        });
        s.rules
            .insert("KXTEST-26APR01-T50".into(), cached_rule(0.85, Side::Yes, 2));
        assert!(
            s.evaluate(
                &MarketTicker::new("KXTEST-26APR01-T50"),
                &book_with_quotes(None, Some(30)),
                Instant::now(),
            )
            .is_none()
        );
    }

    #[test]
    fn same_day_gate_allows_today_market() {
        let market = dated_market_for_today();
        let mut s = StatStrategy::new(StatConfig {
            same_day_only: true,
            ..cfg()
        });
        s.rules
            .insert(market.as_str().to_string(), cached_rule(0.85, Side::Yes, 2));
        assert!(
            s.evaluate(&market, &book_with_quotes(None, Some(30)), Instant::now())
                .is_some()
        );
    }

    #[test]
    fn entry_blocked_while_position_open() {
        let mut s = StatStrategy::new(cfg());
        s.rules
            .insert("KX-HELD".into(), cached_rule(0.85, Side::Yes, 2));
        s.positions.insert(
            position_key("KX-HELD", Side::Yes),
            cached_position(Side::Yes, 1, 70),
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-HELD"),
                &book_with_quotes(None, Some(30)),
                Instant::now(),
            )
            .is_none()
        );
    }

    #[test]
    fn reentry_blocked_after_recent_exit_attempt() {
        let mut s = StatStrategy::new(StatConfig {
            reentry_cooldown: Duration::from_secs(60),
            ..cfg()
        });
        s.rules
            .insert("KX-EXITED".into(), cached_rule(0.85, Side::Yes, 2));
        s.last_exit_at
            .insert(position_key("KX-EXITED", Side::Yes), Instant::now());
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-EXITED"),
                &book_with_quotes(None, Some(30)),
                Instant::now(),
            )
            .is_none()
        );
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

    // ─── A1 belief-drift exit tests ──────────────────────────

    #[test]
    fn exit_belief_drift_when_residual_edge_collapses() {
        // Long YES at 50¢. Mark = 53¢ (+3¢, inside TP/SL band).
        // Rule's model_p drops to 0.54 → belief 54¢ → residual
        // edge = 54-53 = 1¢ < min_residual_edge_cents(2) → exit.
        let mut c = cfg();
        c.min_residual_edge_cents = 2;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-DRIFT-A";
        s.rules
            .insert(ticker.into(), cached_rule(0.54, Side::Yes, 5));
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book_with_quotes(Some(53), Some(40));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("belief-drift fires");
        assert!(intent.client_id.contains(":bd:"));
        assert_eq!(intent.action, IntentAction::Sell);
    }

    #[test]
    fn no_belief_drift_when_edge_still_holds() {
        let mut c = cfg();
        c.min_residual_edge_cents = 2;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-DRIFT-B";
        // Belief 70¢ vs mark 53¢ → residual edge 17¢ ≥ 2 → no drift.
        s.rules
            .insert(ticker.into(), cached_rule(0.70, Side::Yes, 5));
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book_with_quotes(Some(53), Some(40));
        assert!(
            s.evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn exit_when_rule_disappears_from_curator() {
        // Curator removed the rule → orphan position. Belief-drift
        // exit fires regardless of mark.
        let mut c = cfg();
        c.min_residual_edge_cents = 2;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-ORPHAN";
        // No rule inserted.
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book_with_quotes(Some(53), Some(40));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("orphan exit fires");
        assert!(intent.client_id.contains(":bd:"));
    }

    #[test]
    fn belief_drift_no_side_position() {
        // Long NO at 30¢. Mark (no_bid) = 35¢. Rule says
        // model_p_yes = 0.62 → bet_p_no = 0.38 → belief 38¢ →
        // residual = 38-35 = 3¢. With min_residual=4 → drift fires.
        let mut c = cfg();
        c.min_residual_edge_cents = 4;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-DRIFT-NO";
        s.rules
            .insert(ticker.into(), cached_rule(0.62, Side::No, 5));
        s.positions.insert(
            position_key(ticker, Side::No),
            cached_position(Side::No, 3, 30),
        );
        let book = book_with_quotes(Some(60), Some(35));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("NO belief-drift fires");
        assert!(intent.client_id.contains(":N:bd:"));
    }

    #[test]
    fn stop_loss_priority_over_belief_drift() {
        // Both conditions trip; sl wins (capital preservation
        // priority).
        let mut c = cfg();
        c.min_residual_edge_cents = 5;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-PRIO";
        s.rules
            .insert(ticker.into(), cached_rule(0.50, Side::Yes, 5));
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 60),
        );
        let book = book_with_quotes(Some(54), Some(45));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("exit fires");
        assert!(
            intent.client_id.contains(":sl:"),
            "sl should win priority; got {}",
            intent.client_id
        );
    }

    // ─── A4 time-decay TP scaling tests ──────────────────────

    #[test]
    fn tp_threshold_decays_with_age() {
        // Default TP is 8¢, decay 1¢/hr. After 4h, effective TP = 4.
        // Mark gives +5¢ PnL — wouldn't fire at fresh TP=8 but
        // does fire at decayed TP=4.
        let mut c = cfg();
        c.tp_decay_per_hour_cents = 1;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-DECAY";
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position_aged(Side::Yes, 4, 50, 4),
        );
        let book = book_with_quotes(Some(55), Some(40));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("decayed-TP fires");
        assert!(intent.client_id.contains(":tp:"));
    }

    #[test]
    fn tp_floors_at_one_cent() {
        // 20-hour-old position at 1¢/hr decay would shave TP
        // negative, but it floors at 1¢. +1¢ PnL fires.
        let mut c = cfg();
        c.tp_decay_per_hour_cents = 1;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-FLOOR";
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position_aged(Side::Yes, 4, 50, 20),
        );
        let book = book_with_quotes(Some(51), Some(40));
        let intent = s
            .evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("floor-TP fires");
        assert!(intent.client_id.contains(":tp:"));
    }

    #[test]
    fn tp_decay_disabled_when_zero() {
        // tp_decay=0 → fixed TP=8, +5¢ PnL doesn't fire.
        let mut s = StatStrategy::new(cfg());
        let ticker = "KX-NO-DECAY";
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position_aged(Side::Yes, 4, 50, 4),
        );
        let book = book_with_quotes(Some(55), Some(40));
        assert!(
            s.evaluate_exit(&MarketTicker::new(ticker), &book, Instant::now())
                .is_none()
        );
    }

    // ─── A3 trailing stop tests ──────────────────────────────

    #[test]
    fn trailing_stop_fires_after_giveback() {
        // Long YES at 50¢. Trigger 4¢, distance 3¢. Walk:
        //   tick 1: mark 56 → PnL +6 → high_water 6 (≥4 trigger).
        //           pnl 6 > high-distance (6-3=3), no fire.
        //   tick 2: mark 52 → PnL +2 → high_water still 6.
        //           pnl 2 ≤ 3 → trailing fires.
        let mut c = cfg();
        c.trailing_trigger_cents = 4;
        c.trailing_distance_cents = 3;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-TRAIL-A";
        let key = position_key(ticker, Side::Yes);
        s.positions
            .insert(key.clone(), cached_position(Side::Yes, 4, 50));

        // Tick 1: mark 56.
        let book1 = book_with_quotes(Some(56), Some(40));
        assert!(
            s.evaluate_exit(&MarketTicker::new(ticker), &book1, Instant::now())
                .is_none(),
            "first tick within trailing band, no exit"
        );
        assert_eq!(s.high_water_pnl.get(&key).copied(), Some(6));

        // Tick 2: mark drops back to 52.
        // last_exit_at not set yet — proceed.
        let book2 = book_with_quotes(Some(52), Some(40));
        let intent = s
            .evaluate_exit(
                &MarketTicker::new(ticker),
                &book2,
                Instant::now() + Duration::from_secs(120),
            )
            .expect("trailing fires");
        assert!(intent.client_id.contains(":ts:"));
    }

    #[test]
    fn trailing_stop_doesnt_fire_below_trigger() {
        // High water never crosses trigger=4 → trailing inactive.
        let mut c = cfg();
        c.trailing_trigger_cents = 4;
        c.trailing_distance_cents = 3;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-TRAIL-B";
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        // Mark 53 → PnL +3, never crosses 4. Then drops to 50 (PnL 0).
        let book1 = book_with_quotes(Some(53), Some(40));
        assert!(
            s.evaluate_exit(&MarketTicker::new(ticker), &book1, Instant::now())
                .is_none()
        );
        let book2 = book_with_quotes(Some(50), Some(40));
        assert!(
            s.evaluate_exit(
                &MarketTicker::new(ticker),
                &book2,
                Instant::now() + Duration::from_secs(120)
            )
            .is_none(),
            "trailing inactive — high water under trigger"
        );
    }

    #[test]
    fn trailing_high_water_only_ratchets_up() {
        // Mark cycles 50 → 56 → 53 → 60. high_water progression
        // should be 0 → 6 → 6 → 10 (never decreases).
        let mut c = cfg();
        c.trailing_trigger_cents = 4;
        c.trailing_distance_cents = 3;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-RATCHET";
        let key = position_key(ticker, Side::Yes);
        s.positions
            .insert(key.clone(), cached_position(Side::Yes, 4, 50));

        let books = [
            (Some(50), Some(40)), // 0
            (Some(56), Some(40)), // 6
            (Some(53), Some(40)), // 3 — ratchet stays at 6
            (Some(60), Some(40)), // 10
        ];
        let expected = [0, 6, 6, 10];
        let now = Instant::now();
        for (i, (yb, nb)) in books.iter().enumerate() {
            let book = book_with_quotes(*yb, *nb);
            let _ = s.evaluate_exit(
                &MarketTicker::new(ticker),
                &book,
                now + Duration::from_secs((i as u64) * 120),
            );
            assert_eq!(s.high_water_pnl.get(&key).copied(), Some(expected[i]));
        }
    }

    #[test]
    fn trailing_disabled_when_zero() {
        let mut c = cfg();
        c.trailing_trigger_cents = 0;
        c.trailing_distance_cents = 3;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-OFF";
        s.positions.insert(
            position_key(ticker, Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        // Walk through a profitable peak then back. With trigger=0
        // trailing is OFF, pnl_per=2 doesn't trigger anything else.
        let book1 = book_with_quotes(Some(56), Some(40));
        let _ = s.evaluate_exit(&MarketTicker::new(ticker), &book1, Instant::now());
        let book2 = book_with_quotes(Some(52), Some(40));
        assert!(
            s.evaluate_exit(
                &MarketTicker::new(ticker),
                &book2,
                Instant::now() + Duration::from_secs(120)
            )
            .is_none()
        );
    }

    #[test]
    fn trailing_high_water_cleared_on_position_close() {
        // After refresh_positions removes a key, the high_water
        // entry for that key gets pruned.
        let mut s = StatStrategy::new(cfg());
        let ticker = "KX-CLOSED";
        let key = position_key(ticker, Side::Yes);
        s.positions
            .insert(key.clone(), cached_position(Side::Yes, 4, 50));
        s.high_water_pnl.insert(key.clone(), 7);
        // Simulate refresh that finds zero positions.
        let next: HashMap<String, CachedPosition> = HashMap::new();
        s.high_water_pnl.retain(|k, _| next.contains_key(k));
        assert!(s.high_water_pnl.is_empty());
    }

    // ─── I3 cross-strategy belief augmentation tests ─────────

    #[test]
    fn poly_blend_lifts_belief_when_poly_higher() {
        // Rule says model_p=0.55. Poly says 95¢. With α=0.3
        // blended_p = 0.3*0.55 + 0.7*0.95 = 0.83. At ask=70¢,
        // raw edge = 13¢, after fee ≈ 11¢ — clears min_edge=5.
        // Without the blend, edge would be only 55-70 = -15 (no
        // fire).
        let mut c = cfg();
        c.poly_mid_blend_alpha = 0.3;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-BLEND-A";
        s.rules
            .insert(ticker.into(), cached_rule(0.55, Side::Yes, 5));
        s.poly_mid_cents.insert(ticker.into(), 95);
        // YES ask = 100 - 30 = 70. Stack large.
        let book = book_with_quotes(None, Some(30));
        let intent = s
            .evaluate(&MarketTicker::new(ticker), &book, Instant::now())
            .expect("blended fire");
        assert_eq!(intent.price_cents, Some(70));
    }

    #[test]
    fn poly_blend_drops_fire_when_poly_lower_than_rule() {
        // Rule says model_p=0.85 (would fire). Poly says 50¢.
        // With α=0.5, blended_p = 0.675. At ask=70¢, edge after
        // blend is -2.5 — no fire. Without blend: 0.85 vs 0.70 =
        // would fire.
        let mut c = cfg();
        c.poly_mid_blend_alpha = 0.5;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-BLEND-B";
        s.rules
            .insert(ticker.into(), cached_rule(0.85, Side::Yes, 5));
        s.poly_mid_cents.insert(ticker.into(), 50);
        let book = book_with_quotes(None, Some(30));
        assert!(
            s.evaluate(&MarketTicker::new(ticker), &book, Instant::now())
                .is_none(),
            "poly disagreement should kill the entry"
        );
    }

    #[test]
    fn poly_blend_falls_through_to_rule_when_no_poly_cached() {
        // No poly_mid_cents entry → behave as if α=1.0 (pure
        // rule). High-conviction rule should fire normally.
        let mut c = cfg();
        c.poly_mid_blend_alpha = 0.0; // would zero out rule if poly were present
        let mut s = StatStrategy::new(c);
        let ticker = "KX-BLEND-C";
        s.rules
            .insert(ticker.into(), cached_rule(0.85, Side::Yes, 5));
        let book = book_with_quotes(None, Some(30));
        let intent = s.evaluate(&MarketTicker::new(ticker), &book, Instant::now());
        assert!(
            intent.is_some(),
            "no poly mid cached → fall through to pure rule"
        );
    }

    #[test]
    fn poly_blend_pure_rule_when_alpha_one() {
        // α=1.0 → blend is a no-op even if poly mid is cached.
        let mut c = cfg();
        c.poly_mid_blend_alpha = 1.0;
        let mut s = StatStrategy::new(c);
        let ticker = "KX-BLEND-D";
        s.rules
            .insert(ticker.into(), cached_rule(0.85, Side::Yes, 5));
        // Poly says ~zero — would normally trash the edge.
        s.poly_mid_cents.insert(ticker.into(), 5);
        let book = book_with_quotes(None, Some(30));
        let intent = s.evaluate(&MarketTicker::new(ticker), &book, Instant::now());
        assert!(intent.is_some(), "α=1 ignores poly entirely");
    }
}
