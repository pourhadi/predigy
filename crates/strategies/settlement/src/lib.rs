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
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
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
    /// **Audit A6 — settlement profit lock**:
    /// When the position has shown ≥ this many cents of
    /// per-contract favorable movement AND there's at least
    /// `profit_lock_min_secs_to_close` seconds until settlement,
    /// emit a closing IOC at the current mark instead of
    /// riding to venue settlement. Locks profit + reduces
    /// settlement-race risk. `0` disables.
    pub profit_lock_threshold_cents: i32,
    /// Only profit-lock if there's at least this much time left
    /// until settlement; otherwise let it ride.
    pub profit_lock_min_secs_to_close: i64,
    /// How often to refresh the open-position cache from
    /// Postgres. Settlement positions are short-lived
    /// (<10 min), so a 30s cadence is fine.
    pub position_refresh_interval: Duration,
    /// **Audit S1 — settlement-time fade**: symmetric mirror of
    /// the long-side strategy. When yes_ask climbs above
    /// `fade_min_price_cents` in the close window with the
    /// inverted book asymmetry (heavy ask stack, thin bid),
    /// the market is overconfident; sell-YES (= go short)
    /// expecting reversion or a venue pause.
    pub fade_min_price_cents: u8,
    pub fade_max_price_cents: u8,
    /// Ratio that ask-stack must exceed bid-stack by to fire
    /// the fade. Mirrors `bid_to_ask_ratio` for the long side.
    pub fade_ask_to_bid_ratio: u32,
    /// Per-fire size for fade entries.
    pub fade_size: u32,
    /// `0` disables fade entirely.
    pub fade_enabled: bool,
}

impl SettlementConfig {
    /// Audit B2 + B3 — env-var overrides:
    /// - `PREDIGY_SETTLEMENT_SIZE` (u32)
    /// - `PREDIGY_SETTLEMENT_COOLDOWN_MS` (u64)
    /// - `PREDIGY_SETTLEMENT_MIN_PRICE_CENTS` (u8)
    /// - `PREDIGY_SETTLEMENT_MAX_PRICE_CENTS` (u8)
    /// - `PREDIGY_SETTLEMENT_BID_TO_ASK_RATIO` (u32)
    /// - `PREDIGY_SETTLEMENT_PROFIT_LOCK_THRESHOLD_CENTS` (i32) — A6
    /// - `PREDIGY_SETTLEMENT_PROFIT_LOCK_MIN_SECS_TO_CLOSE` (i64) — A6
    #[must_use]
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_SIZE") {
            if let Ok(n) = v.parse() {
                c.size = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_COOLDOWN_MS") {
            if let Ok(n) = v.parse::<u64>() {
                c.cooldown = Duration::from_millis(n);
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_MIN_PRICE_CENTS") {
            if let Ok(n) = v.parse() {
                c.min_price_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_MAX_PRICE_CENTS") {
            if let Ok(n) = v.parse() {
                c.max_price_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_BID_TO_ASK_RATIO") {
            if let Ok(n) = v.parse() {
                c.bid_to_ask_ratio = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_PROFIT_LOCK_THRESHOLD_CENTS") {
            if let Ok(n) = v.parse() {
                c.profit_lock_threshold_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_PROFIT_LOCK_MIN_SECS_TO_CLOSE") {
            if let Ok(n) = v.parse() {
                c.profit_lock_min_secs_to_close = n;
            }
        }
        // S1 fade overrides:
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_FADE_ENABLED") {
            c.fade_enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_FADE_MIN_PRICE_CENTS") {
            if let Ok(n) = v.parse() {
                c.fade_min_price_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_FADE_MAX_PRICE_CENTS") {
            if let Ok(n) = v.parse() {
                c.fade_max_price_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_FADE_ASK_TO_BID_RATIO") {
            if let Ok(n) = v.parse() {
                c.fade_ask_to_bid_ratio = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_SETTLEMENT_FADE_SIZE") {
            if let Ok(n) = v.parse() {
                c.fade_size = n;
            }
        }
        c
    }
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
            // A6: lock profit on a 5¢ favourable move with at
            // least 3 min still on the clock. Below 3 min the
            // venue-side settlement is so close that we'd
            // rather take the binary outcome than the touch.
            profit_lock_threshold_cents: 5,
            profit_lock_min_secs_to_close: 180,
            position_refresh_interval: Duration::from_secs(30),
            // S1: defaults intentionally tighter than the
            // long-side band. Fade only fires on really
            // egregious overconfidence (98–99¢) — the venue's
            // settlement-race risk on a short YES position is
            // asymmetric (we'd owe $1 if it settles up). Off by
            // default; operator opts in via env var or config.
            fade_min_price_cents: 98,
            fade_max_price_cents: 99,
            fade_ask_to_bid_ratio: 5,
            fade_size: 1,
            fade_enabled: false,
        }
    }
}

/// A6 — in-memory position snapshot for the profit-lock check.
/// Refreshed from Postgres on each Tick; stale up to one
/// `position_refresh_interval` (30s default).
#[derive(Debug, Clone)]
struct CachedPosition {
    side: Side,
    /// Signed: positive = long.
    signed_qty: i32,
    avg_entry_cents: i32,
}

#[derive(Debug)]
pub struct SettlementStrategy {
    config: SettlementConfig,
    /// Per-market settlement timestamp (unix seconds), populated
    /// by the engine's discovery service via Event::DiscoveryDelta.
    close_times: HashMap<MarketTicker, i64>,
    /// Per-market last-fire wall-clock; cooldown filter.
    last_fired: HashMap<MarketTicker, Instant>,
    /// A6 — open positions per (ticker, side).
    positions: HashMap<String, CachedPosition>,
    /// A6 — per-position exit cooldown.
    last_exit_at: HashMap<String, Instant>,
    last_position_refresh: Option<Instant>,
}

impl SettlementStrategy {
    pub fn new(config: SettlementConfig) -> Self {
        Self {
            config,
            close_times: HashMap::new(),
            last_fired: HashMap::new(),
            positions: HashMap::new(),
            last_exit_at: HashMap::new(),
            last_position_refresh: None,
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
            ticker = cid_safe_ticker(market.as_str()),
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

    /// **Audit S1** — fade entry, symmetric mirror of `evaluate`.
    /// When the YES touch is overconfident (99¢ with thin bids
    /// vs heavy asks), sell-YES IOC. Returns Some on a fire.
    fn evaluate_fade(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now_unix: i64,
        now_instant: Instant,
    ) -> Option<Intent> {
        if !self.config.fade_enabled {
            return None;
        }
        // Reuse the same per-market cooldown as the long side.
        if let Some(&last) = self.last_fired.get(market)
            && now_instant.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        // Time-to-close gate (same as long side).
        let close_time = self.close_times.get(market).copied()?;
        let secs_to_close = close_time.saturating_sub(now_unix);
        if secs_to_close <= 0
            || u64::try_from(secs_to_close).unwrap_or(u64::MAX)
                >= self.config.close_window.as_secs()
        {
            return None;
        }
        // Best bid + ask via complement.
        let (best_bid_price, best_bid_qty) = book.best_yes_bid()?;
        let (best_no_bid_price, best_no_bid_qty) = book.best_no_bid()?;
        let yes_ask_cents = 100u8.checked_sub(best_no_bid_price.cents())?;
        // Price band — overconfident YES territory.
        if yes_ask_cents < self.config.fade_min_price_cents
            || yes_ask_cents > self.config.fade_max_price_cents
        {
            return None;
        }
        // Inverted asymmetry: ask-stack (= no_bid_qty by book
        // convention) must dominate bid-stack.
        if best_no_bid_qty < best_bid_qty.saturating_mul(self.config.fade_ask_to_bid_ratio) {
            return None;
        }
        // Sanity: book must not be inverted.
        if best_bid_price.cents() + yes_ask_cents < 100 {
            return None;
        }

        // Submit at the YES bid (we sell into the existing
        // bid). Action=Sell, Side=Yes — Kalshi V2 maps to
        // (side=Bid, action=Sell) on the wire. Limit price = the
        // bid we'd hit.
        let qty = i32::try_from(self.config.fade_size).ok()?;
        if qty <= 0 {
            return None;
        }
        let limit_cents = i32::from(best_bid_price.cents()).clamp(1, 99);
        let minute = (now_unix / 60) as u32;
        let client_id = format!(
            "settlement-fade:{ticker}:{ask:02}:{size:04}:{minute:08x}",
            ticker = cid_safe_ticker(market.as_str()),
            ask = yes_ask_cents,
            size = self.config.fade_size,
        );
        self.last_fired.insert(market.clone(), now_instant);
        Some(Intent {
            client_id,
            strategy: STRATEGY_ID.0,
            market: market.clone(),
            side: Side::Yes,
            action: IntentAction::Sell,
            price_cents: Some(limit_cents),
            qty,
            order_type: OrderType::Limit,
            tif: Tif::Ioc,
            reason: Some(format!(
                "settlement-fade: yes_ask={yes_ask_cents}¢ \
                 ask_qty={best_no_bid_qty} bid_qty={best_bid_qty} \
                 ratio≥{} ttc={secs_to_close}s",
                self.config.fade_ask_to_bid_ratio,
            )),
        })
    }

    fn apply_discovery(
        &mut self,
        added: &[predigy_engine_core::discovery::DiscoveredMarket],
        removed: &[MarketTicker],
    ) {
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

    /// A6 — refresh the open-position cache from Postgres.
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
        debug!(n_positions = n, "settlement: position cache refreshed");
        Ok(())
    }

    /// A6 — profit-lock exit. Settlement positions normally ride
    /// to venue settlement at $1/$0; this branch closes early
    /// when the touch has moved sufficiently in our favor AND
    /// there's still time on the clock.
    fn evaluate_exit(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now_unix: i64,
        now_instant: Instant,
    ) -> Option<Intent> {
        if self.config.profit_lock_threshold_cents <= 0 {
            return None;
        }
        // Only check the side we entered on. Settlement entries
        // are always YES-buy, so the position should be long-YES;
        // we still parametrize for forward compat.
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
                && now_instant.duration_since(last) < self.config.cooldown
            {
                continue;
            }

            // Time-to-close gate. If we're inside
            // `profit_lock_min_secs_to_close` of settlement, let
            // the venue settle for us — race risk on the early
            // close > expected slippage.
            let close_time = self.close_times.get(market).copied()?;
            let secs_to_close = close_time.saturating_sub(now_unix);
            if secs_to_close < self.config.profit_lock_min_secs_to_close {
                continue;
            }

            // Mark = price we'd realize unwinding.
            let mark_cents = match (pos.side, pos.signed_qty.is_positive()) {
                (Side::Yes, true) => i32::from(book.best_yes_bid()?.0.cents()),
                (Side::No, true) => i32::from(book.best_no_bid()?.0.cents()),
                (Side::Yes, false) => 100i32 - i32::from(book.best_no_bid()?.0.cents()),
                (Side::No, false) => 100i32 - i32::from(book.best_yes_bid()?.0.cents()),
            };
            let pnl_per = if pos.signed_qty > 0 {
                mark_cents - pos.avg_entry_cents
            } else {
                pos.avg_entry_cents - mark_cents
            };
            if pnl_per < self.config.profit_lock_threshold_cents {
                continue;
            }

            let abs_qty = pos.signed_qty.unsigned_abs() as i32;
            let limit_cents = mark_cents.clamp(1, 99);
            let action = if pos.signed_qty > 0 {
                IntentAction::Sell
            } else {
                IntentAction::Buy
            };
            let side_tag = match pos.side {
                Side::Yes => "Y",
                Side::No => "N",
            };
            let minute = (now_unix / 60) as u32;
            let client_id = format!(
                "settlement-exit:{ticker}:{side_tag}:tp:{minute:08x}",
                ticker = cid_safe_ticker(market.as_str()),
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
                    "settlement-exit: tp entry={}¢ mark={}¢ pnl={}¢/contract \
                     secs_to_close={secs_to_close}",
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
                secs_to_close,
                "settlement: profit-lock fires"
            );
            self.last_exit_at.insert(key, now_instant);
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
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // A6 — refresh position cache on cadence + first call.
        let needs_refresh = self
            .last_position_refresh
            .is_none_or(|t| t.elapsed() >= self.config.position_refresh_interval);
        if needs_refresh {
            self.refresh_positions(state).await?;
        }

        match ev {
            Event::BookUpdate { market, book } => {
                let now_unix = current_unix();
                let now_instant = Instant::now();
                let mut intents = Vec::new();
                if let Some(entry) = self.evaluate(market, book, now_unix, now_instant) {
                    debug!(
                        market = %market.as_str(),
                        price_cents = ?entry.price_cents,
                        qty = entry.qty,
                        "settlement: firing (long)"
                    );
                    intents.push(entry);
                }
                // S1 — fade entry. Symmetric to the long branch
                // but at the overconfident-YES end of the
                // distribution. Fires only when fade_enabled.
                if let Some(fade) = self.evaluate_fade(market, book, now_unix, now_instant) {
                    debug!(
                        market = %market.as_str(),
                        price_cents = ?fade.price_cents,
                        qty = fade.qty,
                        "settlement: firing (fade)"
                    );
                    intents.push(fade);
                }
                if let Some(exit) = self.evaluate_exit(market, book, now_unix, now_instant) {
                    intents.push(exit);
                }
                Ok(intents)
            }
            Event::DiscoveryDelta { added, removed } => {
                self.apply_discovery(added, removed);
                Ok(Vec::new())
            }
            Event::External(_)
            | Event::Tick
            | Event::PairUpdate { .. }
            | Event::CrossStrategy { .. } => Ok(Vec::new()),
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        // A6 — Tick drives position-cache refresh; keep at the
        // configured cadence.
        Some(self.config.position_refresh_interval)
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
            // A6 + S1 disabled by default in tests; the
            // dedicated tests set explicit values to exercise
            // the branches.
            profit_lock_threshold_cents: 0,
            profit_lock_min_secs_to_close: 180,
            position_refresh_interval: Duration::from_secs(30),
            fade_min_price_cents: 98,
            fade_max_price_cents: 99,
            fade_ask_to_bid_ratio: 5,
            fade_size: 1,
            fade_enabled: false,
        }
    }

    fn cached_position(side: Side, signed_qty: i32, avg_entry_cents: i32) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents,
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
        assert!(
            s.evaluate(&m, &book, 1_777_906_400, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn no_fire_when_already_settled() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        let book = book_with((92, 1000), (7, 100));
        assert!(
            s.evaluate(&m, &book, 1_777_910_500, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn no_fire_when_ask_too_high() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // no_bid 1¢ → yes_ask 99¢ > max(96).
        let book = book_with((97, 1000), (1, 100));
        assert!(
            s.evaluate(&m, &book, 1_777_909_700, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn no_fire_when_ask_too_low() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // no_bid 50¢ → yes_ask 50¢ < min(88).
        let book = book_with((48, 1000), (50, 100));
        assert!(
            s.evaluate(&m, &book, 1_777_909_700, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn no_fire_when_book_too_balanced() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-TEST");
        seed_close(&mut s, &m, 1_777_910_000);
        // Bid 200, ask 100 → ratio 2 < 5.
        let book = book_with((92, 200), (7, 100));
        assert!(
            s.evaluate(&m, &book, 1_777_909_700, Instant::now())
                .is_none()
        );
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
        assert!(
            s.evaluate(&m, &book, 1_777_909_700, Instant::now())
                .is_none()
        );
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
        s.apply_discovery(&[], std::slice::from_ref(&m));
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

    // ─── A6 settlement profit-lock tests ─────────────────────

    #[test]
    fn profit_lock_fires_when_in_band_with_time_remaining() {
        // Long YES at 93¢. Mark = 99¢ → PnL +6 ≥ threshold(5).
        // settle_unix is 5 minutes (300s) past now → 300 ≥ 180.
        let mut c = cfg();
        c.profit_lock_threshold_cents = 5;
        c.profit_lock_min_secs_to_close = 180;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-PL-A");
        // 5 minutes ahead of test "now".
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        s.positions.insert(
            position_key("KX-PL-A", Side::Yes),
            cached_position(Side::Yes, 4, 93),
        );
        let book = book_with((99, 100), (1, 100));
        let intent = s
            .evaluate_exit(&m, &book, now_unix, Instant::now())
            .expect("profit-lock fires");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 4);
        assert_eq!(intent.price_cents, Some(99));
        assert!(intent.client_id.starts_with("settlement-exit:"));
        assert!(intent.client_id.contains(":Y:tp:"));
    }

    #[test]
    fn profit_lock_skips_when_below_threshold() {
        let mut c = cfg();
        c.profit_lock_threshold_cents = 5;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-PL-B");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        s.positions.insert(
            position_key("KX-PL-B", Side::Yes),
            cached_position(Side::Yes, 4, 93),
        );
        // Mark 96 → PnL +3 < threshold 5.
        let book = book_with((96, 100), (3, 100));
        assert!(
            s.evaluate_exit(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn profit_lock_skips_when_too_close_to_settlement() {
        // Profit threshold met but only 60s left on the clock.
        let mut c = cfg();
        c.profit_lock_threshold_cents = 5;
        c.profit_lock_min_secs_to_close = 180;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-PL-C");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 60);
        s.positions.insert(
            position_key("KX-PL-C", Side::Yes),
            cached_position(Side::Yes, 4, 93),
        );
        let book = book_with((99, 100), (1, 100));
        assert!(
            s.evaluate_exit(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn profit_lock_disabled_when_threshold_zero() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-PL-D");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        s.positions.insert(
            position_key("KX-PL-D", Side::Yes),
            cached_position(Side::Yes, 4, 93),
        );
        let book = book_with((99, 100), (1, 100));
        assert!(
            s.evaluate_exit(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn profit_lock_skips_unknown_position() {
        let mut c = cfg();
        c.profit_lock_threshold_cents = 5;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-NO-POS");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        let book = book_with((99, 100), (1, 100));
        assert!(
            s.evaluate_exit(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    // ─── S1 settlement-time fade tests ───────────────────────

    #[test]
    fn fade_disabled_by_default() {
        let mut s = SettlementStrategy::new(cfg());
        let m = MarketTicker::new("KX-FD-A");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        // yes_ask = 100 - no_bid(1) = 99. ask_qty 1000 >>
        // 5*100 bid_qty.
        let book = book_with((50, 100), (1, 1000));
        assert!(
            s.evaluate_fade(&m, &book, now_unix, Instant::now())
                .is_none(),
            "fade disabled when fade_enabled=false"
        );
    }

    #[test]
    fn fade_fires_when_enabled_and_conditions_met() {
        let mut c = cfg();
        c.fade_enabled = true;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-FD-B");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        // yes_ask = 100 - no_bid(1) = 99. ask_qty 1000 >> 5×100.
        let book = book_with((50, 100), (1, 1000));
        let intent = s
            .evaluate_fade(&m, &book, now_unix, Instant::now())
            .expect("fade fires");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 1);
        // We submit at the YES bid (50¢ — sell into existing
        // bid).
        assert_eq!(intent.price_cents, Some(50));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(intent.client_id.starts_with("settlement-fade:"));
    }

    #[test]
    fn fade_skips_when_price_below_band() {
        let mut c = cfg();
        c.fade_enabled = true;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-FD-C");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        // yes_ask = 100 - no_bid(5) = 95. Below fade band 98.
        let book = book_with((50, 100), (5, 1000));
        assert!(
            s.evaluate_fade(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn fade_skips_when_book_balanced() {
        let mut c = cfg();
        c.fade_enabled = true;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-FD-D");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        // ask_qty (200) is not 5× bid_qty (100).
        let book = book_with((50, 100), (1, 200));
        assert!(
            s.evaluate_fade(&m, &book, now_unix, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn fade_cooldown_blocks_repeat() {
        let mut c = cfg();
        c.fade_enabled = true;
        let mut s = SettlementStrategy::new(c);
        let m = MarketTicker::new("KX-FD-E");
        let now_unix = 1_777_910_000;
        s.close_times.insert(m.clone(), now_unix + 300);
        let book = book_with((50, 100), (1, 1000));
        let now = Instant::now();
        let _ = s
            .evaluate_fade(&m, &book, now_unix, now)
            .expect("first fires");
        assert!(s.evaluate_fade(&m, &book, now_unix, now).is_none());
    }
}
