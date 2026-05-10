// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-book-maker` — Kalshi maker-mode quoting on
//! a curated market set.
//!
//! ## Why this exists
//!
//! Kalshi pays **0% maker fee** on standard binary markets versus
//! the `ceil(0.07 × N × P × (1-P))` taker fee. Whelan's 2026 UCD
//! paper *"Makers and Takers: Economics of the Kalshi Prediction
//! Market"* explicitly argues that pure-taker strategies are
//! structurally unprofitable; you must capture the spread to
//! survive. Every other strategy in predigy is a taker. This is
//! the first maker.
//!
//! ## What it does
//!
//! For each configured market:
//!
//! 1. Watches the `BookUpdate` stream for that ticker.
//! 2. Computes a desired pair of YES quotes — a YES bid 1¢ inside
//!    the current best bid, and a YES sell-ask 1¢ inside the
//!    current best YES ask.
//! 3. Applies inventory skew: when long N contracts, both quotes
//!    shift down (encouraging a sell-fill, discouraging a
//!    buy-fill). When short, both shift up.
//! 4. Compares desired quotes to the strategy's currently-active
//!    intents on this ticker. For mismatches it queues cancel
//!    requests via `drain_pending_cancels`. For missing quotes it
//!    emits a fresh `Intent` with `Tif::Gtc, post_only=true`.
//! 5. Refuses to add inventory beyond `max_inventory_contracts`.
//!
//! ## What it does NOT do (deferred to follow-ups)
//!
//! - **No NO-side quoting.** Kalshi's YES and NO books are
//!   separate; quoting both sides on NO would double the fill rate
//!   but also double the inventory risk and code complexity. MVP
//!   ships YES-side only. Same alpha source either way: YES bid +
//!   YES ask = capture the spread on YES.
//! - **No cancel-on-news heuristic.** A live maker should cancel
//!   both quotes when the book widens beyond a threshold (informed
//!   flow likely about to hit). MVP relies on per-quote post-only +
//!   tight inventory caps to bound damage.
//! - **No Stoikov-optimal skew formula.** MVP uses a linear
//!   `inventory × cents-per-contract` skew. The optimal is a
//!   nonlinear function of risk aversion and arrival intensities.
//!   Linear is the right MVP approximation.
//! - **No automatic market discovery.** The list of "makeable"
//!   tickers is operator-curated in a JSON config. Future iteration
//!   can add an `arb-config-curator` style scanner that promotes
//!   liquid wide-spread markets automatically.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("book-maker");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MakerMarket {
    /// Kalshi binary market ticker.
    pub ticker: String,
    /// Cap on absolute open contracts (long or short) on this
    /// market. Quoting on the side that would exceed this cap is
    /// suppressed — we still quote the flattening side.
    pub max_inventory_contracts: i32,
    /// Contracts per quote. MVP: 1 contract per side.
    #[serde(default = "default_quote_size")]
    pub quote_size: i32,
    /// If `desired_ask - desired_bid < min_spread_cents`, suppress
    /// quoting (the book is too tight to profitably make).
    #[serde(default = "default_min_spread")]
    pub min_spread_cents: i32,
}

fn default_quote_size() -> i32 {
    1
}
fn default_min_spread() -> i32 {
    2
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BookMakerRulesFile {
    pub markets: Vec<MakerMarket>,
}

#[derive(Debug, Clone)]
pub struct BookMakerConfig {
    pub config_file: PathBuf,
    /// Cents to skew quotes per contract of inventory.
    pub inventory_skew_cents_per_contract: i32,
    /// Cadence to re-poll the config file for mtime changes.
    pub config_refresh_interval: Duration,
}

impl BookMakerConfig {
    /// Build from env.
    /// - `PREDIGY_BOOK_MAKER_CONFIG` (path) — required (file
    ///   existence is what gates registration in the engine).
    /// - `PREDIGY_BOOK_MAKER_SKEW_CENTS_PER_CONTRACT` (i32,
    ///   default 1)
    /// - `PREDIGY_BOOK_MAKER_REFRESH_MS` (u64, default 30_000)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            inventory_skew_cents_per_contract: 1,
            config_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_SKEW_CENTS_PER_CONTRACT")
            && let Ok(n) = v.parse()
        {
            c.inventory_skew_cents_per_contract = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.config_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn config_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_BOOK_MAKER_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum QuoteSide {
    Bid, // YES Buy
    Ask, // YES Sell
}

impl QuoteSide {
    fn cid_tag(self) -> &'static str {
        match self {
            Self::Bid => "B",
            Self::Ask => "A",
        }
    }
    fn intent_action(self) -> IntentAction {
        match self {
            Self::Bid => IntentAction::Buy,
            Self::Ask => IntentAction::Sell,
        }
    }
}

/// Pure-function quote computation. Given a touch and the
/// strategy's current YES-equivalent inventory, return desired
/// (bid_cents, ask_cents) — or None to skip quoting (book too
/// tight, etc).
#[must_use]
pub fn compute_desired_quotes(
    yes_bid_cents: u8,
    yes_ask_cents: u8,
    inventory_contracts: i32,
    skew_per_contract: i32,
    min_spread_cents: i32,
) -> Option<(u8, u8)> {
    if yes_ask_cents <= yes_bid_cents {
        return None; // crossed/locked book
    }
    // Step inside by 1¢ each side (the maker's spread capture).
    let mut bid = i32::from(yes_bid_cents).saturating_add(1);
    let mut ask = i32::from(yes_ask_cents).saturating_sub(1);

    // Inventory skew: when long N, shift both quotes DOWN by
    // N × skew (less attractive to buy, more attractive to sell).
    // When short N, shift both UP. Linear approximation; Stoikov-
    // optimal is more nuanced.
    let shift = inventory_contracts.saturating_mul(skew_per_contract);
    bid = bid.saturating_sub(shift);
    ask = ask.saturating_sub(shift);

    // Clamp to legal Kalshi range [1, 99].
    let bid = bid.clamp(1, 99) as u8;
    let ask = ask.clamp(1, 99) as u8;

    if i32::from(ask) - i32::from(bid) < min_spread_cents {
        return None;
    }
    if ask <= bid {
        return None;
    }
    Some((bid, ask))
}

#[derive(Debug, Clone)]
struct ActiveOrderRecord {
    client_id: String,
    price_cents: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct CachedTouch {
    yes_bid_cents: u8,
    yes_ask_cents: u8,
}

#[derive(Debug)]
pub struct BookMakerStrategy {
    config: BookMakerConfig,
    markets: Vec<MakerMarket>,
    /// Reverse lookup: ticker → index into `markets`.
    ticker_to_idx: HashMap<String, usize>,
    /// Latest touch cache per ticker.
    touches: HashMap<String, CachedTouch>,
    /// In-memory mirror of active resting orders by (ticker,
    /// QuoteSide) → record. Refreshed from `Db::active_intents`
    /// on every BookUpdate that lands on a configured ticker.
    active_orders: HashMap<(String, QuoteSide), ActiveOrderRecord>,
    /// In-memory inventory cache: net YES-equivalent contracts
    /// per ticker (positive = long YES, negative = short).
    inventory: HashMap<String, i32>,
    last_config_refresh: Option<Instant>,
    pending_intents: Vec<Intent>,
    pending_cancels: Vec<String>,
}

impl BookMakerStrategy {
    pub fn new(config: BookMakerConfig) -> Self {
        Self {
            config,
            markets: Vec::new(),
            ticker_to_idx: HashMap::new(),
            touches: HashMap::new(),
            active_orders: HashMap::new(),
            inventory: HashMap::new(),
            last_config_refresh: None,
            pending_intents: Vec::new(),
            pending_cancels: Vec::new(),
        }
    }

    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    pub fn subscribed_tickers(&self) -> Vec<String> {
        self.ticker_to_idx.keys().cloned().collect()
    }

    fn reload_markets(&mut self) {
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.config_file.display(),
                    "book-maker: config not present yet"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
            Err(e) => {
                warn!(
                    path = %self.config.config_file.display(),
                    error = %e,
                    "book-maker: config read failed"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let parsed: BookMakerRulesFile = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    path = %self.config.config_file.display(),
                    error = %e,
                    "book-maker: config parse failed"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let mut idx = HashMap::with_capacity(parsed.markets.len());
        for (i, m) in parsed.markets.iter().enumerate() {
            idx.insert(m.ticker.clone(), i);
        }
        info!(
            n_markets = parsed.markets.len(),
            "book-maker: config loaded"
        );
        self.markets = parsed.markets;
        self.ticker_to_idx = idx;
        self.last_config_refresh = Some(Instant::now());
    }

    fn record_book(&mut self, market: &MarketTicker, book: &OrderBook) {
        let key = market.as_str().to_string();
        if !self.ticker_to_idx.contains_key(&key) {
            return;
        }
        // YES bid: best YES bid directly.
        // YES ask: 100 - best NO bid (Kalshi convention).
        let yes_bid = book.best_yes_bid().map(|(p, _)| p.cents()).unwrap_or(0);
        let yes_ask = book
            .best_no_bid()
            .and_then(|(p, _)| 100u8.checked_sub(p.cents()))
            .unwrap_or(0);
        self.touches.insert(
            key,
            CachedTouch {
                yes_bid_cents: yes_bid,
                yes_ask_cents: yes_ask,
            },
        );
    }

    /// Build the stable client_id for a given (ticker, side, price)
    /// quote. The price is in the cid so a re-quote at a new
    /// price produces a new cid (which the OMS treats as a fresh
    /// order, not an idempotent re-submit).
    fn build_cid(ticker: &str, side: QuoteSide, price_cents: u8) -> String {
        format!(
            "book-maker:{cid_t}:{tag}:{p:02}",
            cid_t = cid_safe_ticker(ticker),
            tag = side.cid_tag(),
            p = price_cents,
        )
    }

    /// For one configured market, emit any cancels for stale
    /// quotes and any new intents for missing or repriced quotes.
    fn evaluate_market(&mut self, market_idx: usize) {
        let m = &self.markets[market_idx];
        let ticker = m.ticker.clone();
        let touch = match self.touches.get(&ticker).copied() {
            Some(t) if t.yes_bid_cents > 0 && t.yes_ask_cents > 0 => t,
            _ => return,
        };
        let inv = self.inventory.get(&ticker).copied().unwrap_or(0);
        let Some((desired_bid, desired_ask)) = compute_desired_quotes(
            touch.yes_bid_cents,
            touch.yes_ask_cents,
            inv,
            self.config.inventory_skew_cents_per_contract,
            m.min_spread_cents,
        ) else {
            // Book too tight — cancel anything we have here.
            self.cancel_active_for_ticker(&ticker);
            return;
        };

        // Inventory cap: skip the side that would breach.
        let cap = m.max_inventory_contracts;
        let bid_allowed = inv + m.quote_size <= cap;
        let ask_allowed = -(inv - m.quote_size) <= cap;

        for (side, desired_price, allowed) in [
            (QuoteSide::Bid, desired_bid, bid_allowed),
            (QuoteSide::Ask, desired_ask, ask_allowed),
        ] {
            let key = (ticker.clone(), side);
            let existing = self.active_orders.get(&key).cloned();
            if !allowed {
                if let Some(rec) = existing {
                    debug!(
                        ticker,
                        side = ?side,
                        inventory = inv,
                        cap,
                        "book-maker: inventory cap; cancelling"
                    );
                    self.pending_cancels.push(rec.client_id);
                    self.active_orders.remove(&key);
                }
                continue;
            }
            // Cancel + repost if price differs.
            if let Some(rec) = &existing
                && rec.price_cents == i32::from(desired_price)
            {
                continue;
            }
            if let Some(rec) = existing {
                self.pending_cancels.push(rec.client_id);
            }
            // Build new Intent.
            let cid = Self::build_cid(&ticker, side, desired_price);
            let intent = Intent {
                client_id: cid.clone(),
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(&ticker),
                side: Side::Yes,
                action: side.intent_action(),
                price_cents: Some(i32::from(desired_price)),
                qty: m.quote_size,
                order_type: OrderType::Limit,
                tif: Tif::Gtc,
                reason: Some(format!(
                    "book-maker {ticker} {tag} @ {desired_price}c (touch {b}/{a}, inv={inv})",
                    tag = side.cid_tag(),
                    b = touch.yes_bid_cents,
                    a = touch.yes_ask_cents,
                )),
                post_only: true,
            };
            self.pending_intents.push(intent);
            self.active_orders.insert(
                key,
                ActiveOrderRecord {
                    client_id: cid,
                    price_cents: i32::from(desired_price),
                },
            );
        }
    }

    fn cancel_active_for_ticker(&mut self, ticker: &str) {
        let mut to_drop = Vec::new();
        for ((t, side), rec) in &self.active_orders {
            if t == ticker {
                self.pending_cancels.push(rec.client_id.clone());
                to_drop.push((t.clone(), *side));
            }
        }
        for k in to_drop {
            self.active_orders.remove(&k);
        }
    }

    /// Refresh `inventory` and `active_orders` from the DB. One
    /// query each, cheap. Called at most once per BookUpdate that
    /// lands on a configured ticker.
    async fn refresh_state_from_db(
        &mut self,
        db: &predigy_engine_core::Db,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Positions: map ticker → net YES-equivalent qty.
        let mut inv = HashMap::new();
        for p in db.open_positions(Some(STRATEGY_ID.0)).await? {
            // YES side: signed qty as-is. NO side: -qty (a long-NO
            // = short-YES exposure). The maker's quotes are on
            // YES, so YES-equivalent inventory drives skew.
            let signed = match p.side.as_str() {
                "yes" => p.current_qty,
                "no" => -p.current_qty,
                _ => 0,
            };
            *inv.entry(p.ticker).or_insert(0) += signed;
        }
        self.inventory = inv;

        // Active intents: rebuild active_orders from authoritative
        // DB state. This catches:
        //   - In-memory record drift after a process restart
        //   - Orders cancelled at the venue but not reflected in
        //     the strategy's state yet
        let mut orders = HashMap::new();
        for i in db.active_intents(Some(STRATEGY_ID.0)).await? {
            // Skip rows the strategy has already asked to cancel.
            // `Db::active_intents` returns anything non-terminal,
            // which includes `cancel_requested`. If we left those
            // in our active_orders view, the next BookUpdate
            // would see them as "live at price X", queue ANOTHER
            // cancel, and loop forever — especially for orders
            // that never got a venue_order_id (so the cancel-at-
            // venue path defers indefinitely). Treating
            // cancel_requested as already-gone aligns the
            // maker's view with reality.
            if i.status == "cancel_requested" {
                continue;
            }
            // Decode side from cid tag (more reliable than the
            // intents.action column for our specific cid format).
            let side = if i.client_id.contains(":B:") {
                QuoteSide::Bid
            } else if i.client_id.contains(":A:") {
                QuoteSide::Ask
            } else {
                continue;
            };
            orders.insert(
                (i.ticker.clone(), side),
                ActiveOrderRecord {
                    client_id: i.client_id,
                    price_cents: i.price_cents,
                },
            );
        }
        self.active_orders = orders;
        Ok(())
    }
}

#[async_trait]
impl Strategy for BookMakerStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Box::new(e)),
        };
        let parsed: BookMakerRulesFile = serde_json::from_slice(&raw)?;
        let mut tickers: Vec<MarketTicker> = parsed
            .markets
            .iter()
            .map(|m| MarketTicker::new(&m.ticker))
            .collect();
        tickers.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        tickers.dedup_by(|a, b| a.as_str() == b.as_str());
        Ok(tickers)
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        let needs_refresh = self
            .last_config_refresh
            .is_none_or(|t| t.elapsed() >= self.config.config_refresh_interval);
        if needs_refresh {
            self.reload_markets();
        }
        match ev {
            Event::BookUpdate { market, book } => {
                let key = market.as_str().to_string();
                let Some(&idx) = self.ticker_to_idx.get(&key) else {
                    return Ok(Vec::new());
                };
                self.record_book(market, book);
                self.refresh_state_from_db(&state.db).await?;
                self.evaluate_market(idx);
                Ok(std::mem::take(&mut self.pending_intents))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn drain_pending_cancels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_cancels)
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.config_refresh_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_steps_inside_touch() {
        let q = compute_desired_quotes(40, 60, 0, 1, 2).unwrap();
        assert_eq!(q, (41, 59));
    }

    #[test]
    fn skew_when_long_drops_both_quotes() {
        // Long 5 contracts, skew 1¢/contract → both shift down 5¢.
        let q = compute_desired_quotes(40, 60, 5, 1, 2).unwrap();
        assert_eq!(q, (36, 54));
    }

    #[test]
    fn skew_when_short_raises_both_quotes() {
        // Short 5 contracts → both shift up 5¢.
        let q = compute_desired_quotes(40, 60, -5, 1, 2).unwrap();
        assert_eq!(q, (46, 64));
    }

    #[test]
    fn refuses_when_book_too_tight() {
        // 50/51 spread — stepping inside gives 51/50 which is invalid.
        let q = compute_desired_quotes(50, 51, 0, 1, 2);
        assert!(q.is_none());
    }

    #[test]
    fn refuses_when_min_spread_violated() {
        // 40/43 → 41/42, spread = 1, below min_spread=2.
        let q = compute_desired_quotes(40, 43, 0, 1, 2);
        assert!(q.is_none());
    }

    #[test]
    fn clamps_to_legal_range() {
        // 1/99 → step inside to 2/98.
        let q = compute_desired_quotes(1, 99, 0, 1, 2).unwrap();
        assert_eq!(q, (2, 98));
        // Heavy long shifts down, but bid clamps at 1.
        let q = compute_desired_quotes(5, 90, 100, 1, 2);
        // shift = 100 → bid 6-100 < 1 → clamped to 1; ask
        // 89-100 < 1 → clamped to 1; spread 0 → None.
        assert!(q.is_none());
    }

    #[test]
    fn cid_is_stable_per_price() {
        let a = BookMakerStrategy::build_cid("KX-FOO", QuoteSide::Bid, 42);
        let b = BookMakerStrategy::build_cid("KX-FOO", QuoteSide::Bid, 42);
        assert_eq!(a, b);
        let c = BookMakerStrategy::build_cid("KX-FOO", QuoteSide::Bid, 43);
        assert_ne!(a, c);
        let d = BookMakerStrategy::build_cid("KX-FOO", QuoteSide::Ask, 42);
        assert_ne!(a, d);
    }

    #[test]
    fn cid_strips_dots_from_ticker() {
        // Kalshi V2 rejects cids containing dots — same gotcha
        // that bit the engine cutover (commit 0c05c40).
        let cid = BookMakerStrategy::build_cid("KXBRAZILINF-T4.30", QuoteSide::Bid, 50);
        assert!(!cid.contains('.'));
    }

    #[test]
    fn config_loads_default_qty_and_spread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book-maker.json");
        let rules = serde_json::json!({
            "markets": [{
                "ticker": "KXFOO-X",
                "max_inventory_contracts": 10
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = BookMakerStrategy::new(BookMakerConfig {
            config_file: path,
            inventory_skew_cents_per_contract: 1,
            config_refresh_interval: Duration::from_secs(30),
        });
        s.reload_markets();
        assert_eq!(s.market_count(), 1);
        assert_eq!(s.markets[0].quote_size, 1);
        assert_eq!(s.markets[0].min_spread_cents, 2);
    }
}
