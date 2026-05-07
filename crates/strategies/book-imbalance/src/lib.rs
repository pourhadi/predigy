// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-book-imbalance` — order-book mean
//! reversion (Audit S4).
//!
//! ## Mechanism
//!
//! When a Kalshi market's touch shows a heavily one-sided stack
//! (e.g. yes_bid_qty = 100, no_bid_qty = 5) the dominant side
//! often indicates someone pushing price; the imbalance tends
//! to mean-revert over the next 1–5 minutes as the displayed
//! strength either gets unwound or absorbed.
//!
//! Strategy:
//! - Compute touch imbalance =
//!     (yes_bid_qty − no_bid_qty) / (yes_bid_qty + no_bid_qty)
//! - When |imbalance| ≥ threshold, fade the thick side: if the
//!   YES bid stack dominates, **buy NO at no_ask**; if the NO
//!   bid stack dominates, **buy YES at yes_ask**.
//! - IOC limit at the current ask. Active exits handled by the
//!   existing position-management infrastructure (TP/SL via
//!   the strategy's StatConfig-style fields, shipped separately
//!   in this audit round under A1/A3).
//!
//! ## What this strategy doesn't do
//!
//! - **No active mark-aware exits in v1.** The strategy fires
//!   entries; the OMS's session-flatten / kill-switch
//!   infrastructure handles forced exits. Layered TP/SL is a
//!   follow-up (mirror the stat-trader style if empirical
//!   results justify it).
//! - **No book-depth aggregation.** Decision is touch-only.
//!   Stack-of-stacks aggregation is more robust to noise but
//!   doesn't fundamentally change the signal — defer.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use predigy_engine_core::{ActiveIntent, OpenPosition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("book-imbalance");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImbalanceMarket {
    pub ticker: String,
    /// Per-market override of `imbalance_threshold` if set.
    /// Otherwise the global default applies.
    #[serde(default)]
    pub imbalance_threshold_override: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImbalanceRulesFile {
    pub markets: Vec<ImbalanceMarket>,
}

#[derive(Debug, Clone)]
pub struct ImbalanceConfig {
    pub config_file: PathBuf,
    /// Default imbalance threshold ∈ (0, 1]. 0.7 means the
    /// dominant side must be ≥ 0.7 of total touch qty for the
    /// signal to fire (5.7× ratio). Tighter (0.8) = cleaner
    /// signal but fewer fires.
    pub imbalance_threshold: f64,
    /// Min total touch qty before the strategy considers the
    /// signal credible — a 1-vs-0 imbalance is meaningless.
    pub min_total_qty: u32,
    /// Min after-fee per-contract edge to fire (cents). Same
    /// math as stat: ensures a crossed touch fee model leaves
    /// edge.
    pub min_edge_cents: i32,
    /// Max ask price to take — never fire above this floor of
    /// the price range (avoids buying $0.99 contracts).
    pub max_take_ask_cents: u8,
    /// Min ask price to take — never below 1¢.
    pub min_take_ask_cents: u8,
    /// Contracts per fire.
    pub size: u32,
    /// Maximum inventory on one side of one market. Dynamic signal sizing picks
    /// a target below this ceiling; this is the absolute per-market guardrail.
    pub max_contracts_per_market: u32,
    /// Maximum contracts in a single IOC entry/flatten attempt.
    pub max_size_per_fire: u32,
    /// Maximum fraction of displayed touch liquidity to consume.
    pub max_touch_take_fraction: f64,
    /// Per-market cooldown between fires.
    pub cooldown: Duration,
    pub config_refresh_interval: Duration,
}

impl ImbalanceConfig {
    /// Build from env. Required: `PREDIGY_BOOK_IMBALANCE_CONFIG`.
    ///
    /// - `..._CONFIG` (path) — required
    /// - `..._THRESHOLD` (f64, default 0.7)
    /// - `..._MIN_TOTAL_QTY` (u32, default 50)
    /// - `..._MIN_EDGE_CENTS` (i32, default 1)
    /// - `..._MAX_TAKE_ASK_CENTS` (u8, default 90)
    /// - `..._MIN_TAKE_ASK_CENTS` (u8, default 5)
    /// - `..._SIZE` (u32, default 1)
    /// - `..._COOLDOWN_MS` (u64, default 60_000)
    /// - `..._REFRESH_MS` (u64, default 30_000)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            imbalance_threshold: 0.7,
            min_total_qty: 50,
            min_edge_cents: 1,
            max_take_ask_cents: 90,
            min_take_ask_cents: 5,
            size: 1,
            max_contracts_per_market: 5,
            max_size_per_fire: 3,
            max_touch_take_fraction: 0.20,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_THRESHOLD")
            && let Ok(n) = v.parse::<f64>()
            && n > 0.0
            && n <= 1.0
        {
            c.imbalance_threshold = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MIN_TOTAL_QTY")
            && let Ok(n) = v.parse()
        {
            c.min_total_qty = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MIN_EDGE_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_edge_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MAX_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.max_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MIN_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_SIZE")
            && let Ok(n) = v.parse()
        {
            c.size = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MAX_CONTRACTS_PER_MARKET")
            && let Ok(n) = v.parse()
        {
            c.max_contracts_per_market = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MAX_SIZE_PER_FIRE")
            && let Ok(n) = v.parse()
        {
            c.max_size_per_fire = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_MAX_TOUCH_TAKE_FRACTION")
            && let Ok(n) = v.parse::<f64>()
            && n > 0.0
            && n <= 1.0
        {
            c.max_touch_take_fraction = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_COOLDOWN_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.cooldown = Duration::from_millis(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_IMBALANCE_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.config_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn config_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_BOOK_IMBALANCE_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
struct CachedMarket {
    threshold: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct TickerInventory {
    yes_qty: i32,
    no_qty: i32,
    active_orders: i32,
}

#[derive(Debug, Clone, Default)]
struct ImbalanceInventory {
    by_ticker: HashMap<String, TickerInventory>,
}

impl ImbalanceInventory {
    fn from_rows(positions: Vec<OpenPosition>, active_intents: Vec<ActiveIntent>) -> Self {
        let mut by_ticker: HashMap<String, TickerInventory> = HashMap::new();
        for p in positions {
            let entry = by_ticker.entry(p.ticker).or_default();
            match p.side.as_str() {
                "yes" => entry.yes_qty += p.current_qty.max(0),
                "no" => entry.no_qty += p.current_qty.max(0),
                _ => {}
            }
        }
        for i in active_intents {
            let entry = by_ticker.entry(i.ticker).or_default();
            entry.active_orders += i.qty.max(1);
        }
        Self { by_ticker }
    }

    fn ticker(&self, ticker: &str) -> TickerInventory {
        self.by_ticker.get(ticker).copied().unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct ImbalanceStrategy {
    config: ImbalanceConfig,
    markets: HashMap<String, CachedMarket>,
    last_fire_at: HashMap<String, Instant>,
    last_config_refresh: Option<Instant>,
}

impl ImbalanceStrategy {
    pub fn new(config: ImbalanceConfig) -> Self {
        Self {
            config,
            markets: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_config_refresh: None,
        }
    }

    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    fn reload_markets(&mut self) {
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.config_file.display(),
                    "book-imbalance: config not present yet"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
            Err(e) => {
                warn!(error = %e, "book-imbalance: config read failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let parsed: ImbalanceRulesFile = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "book-imbalance: config parse failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let mut next: HashMap<String, CachedMarket> = HashMap::with_capacity(parsed.markets.len());
        for m in parsed.markets {
            let threshold = m
                .imbalance_threshold_override
                .filter(|t| *t > 0.0 && *t <= 1.0)
                .unwrap_or(self.config.imbalance_threshold);
            next.insert(m.ticker, CachedMarket { threshold });
        }
        info!(n_markets = next.len(), "book-imbalance: config loaded");
        self.markets = next;
        self.last_config_refresh = Some(Instant::now());
    }

    fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
        inventory: &ImbalanceInventory,
    ) -> Option<Intent> {
        let key = market.as_str().to_string();
        let entry = self.markets.get(&key)?;
        let threshold = entry.threshold;
        if let Some(&last) = self.last_fire_at.get(&key)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let yes_bid = book.best_yes_bid();
        let no_bid = book.best_no_bid();
        let (Some((_yes_p, yes_qty)), Some((_no_p, no_qty))) = (yes_bid, no_bid) else {
            return None;
        };
        let total = yes_qty + no_qty;
        if total < self.config.min_total_qty {
            return None;
        }
        let imbalance = (i64::from(yes_qty) - i64::from(no_qty)) as f64 / f64::from(total);
        if imbalance.abs() < threshold {
            return None;
        }
        // Fade the dominant side. yes_bid stack dominant
        // (imbalance > 0) → buy NO. no_bid stack dominant
        // (imbalance < 0) → buy YES.
        let (side, side_tag) = if imbalance > 0.0 {
            (Side::No, "N")
        } else {
            (Side::Yes, "Y")
        };
        let inv = inventory.ticker(&key);
        if inv.active_orders > 0 {
            debug!(ticker = %key, active_orders = inv.active_orders, "book-imbalance: active order present; skip");
            return None;
        }
        let (same_qty, opposite_qty, opposite_side, opposite_bid_cents) = match side {
            Side::Yes => (inv.yes_qty, inv.no_qty, Side::No, no_bid?.0.cents()),
            Side::No => (inv.no_qty, inv.yes_qty, Side::Yes, yes_bid?.0.cents()),
        };
        if opposite_qty > 0 {
            return self.flatten_opposite(
                &key,
                market,
                opposite_side,
                opposite_qty,
                opposite_bid_cents,
                now,
            );
        }
        // ask = 100 - opposite-side bid.
        let opposite_bid_cents = match side {
            Side::Yes => no_bid?.0.cents(),
            Side::No => yes_bid?.0.cents(),
        };
        let ask_cents = 100u8.checked_sub(opposite_bid_cents)?;
        if ask_cents < self.config.min_take_ask_cents || ask_cents > self.config.max_take_ask_cents
        {
            debug!(
                ticker = %key,
                ask_cents,
                "book-imbalance: ask outside [min,max] take floor; skip"
            );
            return None;
        }
        // Edge sanity: a fade trade's "edge" is the implied
        // mean-reversion. We don't have a model_p; use a simple
        // proxy: imbalance × max_revert_cents, where the operator
        // tunes max_revert via the threshold. The min_edge_cents
        // gate ensures fees don't eat the move; with IOC limit at
        // the touch, the worst-case is a single tick's adverse
        // fill.
        let probe = predigy_core::price::Qty::new(self.config.size).ok()?;
        let p = predigy_core::price::Price::from_cents(ask_cents).ok()?;
        let fee = i32::try_from(predigy_core::fees::taker_fee(p, probe)).unwrap_or(i32::MAX);
        // Crude edge floor: fee must be < expected revert.
        // Operator can tighten via min_edge_cents.
        if fee > self.config.min_edge_cents {
            debug!(
                ticker = %key,
                fee,
                min_edge = self.config.min_edge_cents,
                "book-imbalance: fee exceeds min_edge; skip"
            );
            return None;
        }
        let dynamic_target = self.dynamic_target_contracts(imbalance.abs(), total);
        let remaining_capacity = i32::try_from(dynamic_target).ok()?.saturating_sub(same_qty);
        if remaining_capacity <= 0 {
            debug!(ticker = %key, same_qty, dynamic_target, "book-imbalance: dynamic inventory cap reached");
            return None;
        }
        let touch_qty = match side {
            Side::Yes => no_bid?.1,
            Side::No => yes_bid?.1,
        };
        let liquidity_qty =
            ((f64::from(touch_qty) * self.config.max_touch_take_fraction).floor() as i32).max(1);
        let per_fire = self.config.max_size_per_fire.max(self.config.size).max(1);
        let qty = remaining_capacity
            .min(liquidity_qty)
            .min(i32::try_from(per_fire).ok()?)
            .max(1);
        let ts_min = chrono::Utc::now().timestamp() as u32 / 60;
        let client_id = format!(
            "book-imbalance:{cid_t}:{side_tag}:{ask:02}:{size:04}:{ts:08x}",
            cid_t = cid_safe_ticker(&key),
            side_tag = side_tag,
            ask = ask_cents,
            size = self.config.size,
            ts = ts_min,
        );
        let intent = Intent {
            client_id,
            strategy: STRATEGY_ID.0,
            market: market.clone(),
            side,
            action: IntentAction::Buy,
            price_cents: Some(i32::from(ask_cents)),
            qty,
            order_type: OrderType::Limit,
            tif: Tif::Ioc,
            reason: Some(format!(
                "book-imbalance fade: imbalance={:.2} yes_qty={} no_qty={}",
                imbalance, yes_qty, no_qty
            )),
        };
        info!(
            ticker = %key,
            imbalance = format!("{:.3}", imbalance),
            side = ?side,
            ask_cents,
            "book-imbalance: firing fade"
        );
        self.last_fire_at.insert(key, now);
        Some(intent)
    }

    fn dynamic_target_contracts(&self, imbalance_abs: f64, total_touch_qty: u32) -> u32 {
        let tier = if imbalance_abs >= 0.90 && total_touch_qty >= self.config.min_total_qty * 4 {
            5
        } else if imbalance_abs >= 0.80 && total_touch_qty >= self.config.min_total_qty * 2 {
            3
        } else {
            1
        };
        tier.min(self.config.max_contracts_per_market.max(1))
    }

    fn flatten_opposite(
        &mut self,
        key: &str,
        market: &MarketTicker,
        side: Side,
        qty_available: i32,
        bid_cents: u8,
        now: Instant,
    ) -> Option<Intent> {
        if bid_cents == 0 || bid_cents >= 100 {
            return None;
        }
        let qty = qty_available
            .min(i32::try_from(self.config.max_size_per_fire.max(1)).ok()?)
            .max(1);
        let side_tag = match side {
            Side::Yes => "Y",
            Side::No => "N",
        };
        let ts_min = chrono::Utc::now().timestamp() as u32 / 60;
        let client_id = format!(
            "bi-flat:{suffix}:{side_tag}:{bid:02}:{qty:02}:{ts:08x}",
            suffix = short_ticker_hash(key),
            bid = bid_cents,
            ts = ts_min,
        );
        self.last_fire_at.insert(key.to_string(), now);
        Some(Intent {
            client_id,
            strategy: STRATEGY_ID.0,
            market: market.clone(),
            side,
            action: IntentAction::Sell,
            price_cents: Some(i32::from(bid_cents)),
            qty,
            order_type: OrderType::Limit,
            tif: Tif::Ioc,
            reason: Some("book-imbalance flatten before reversal".into()),
        })
    }
}

fn short_ticker_hash(ticker: &str) -> String {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in ticker.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[async_trait]
impl Strategy for ImbalanceStrategy {
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
        let parsed: ImbalanceRulesFile = serde_json::from_slice(&raw)?;
        Ok(parsed
            .markets
            .into_iter()
            .map(|m| MarketTicker::new(&m.ticker))
            .collect())
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
                let now = Instant::now();
                let inventory = ImbalanceInventory::from_rows(
                    state.db.open_positions(Some(STRATEGY_ID.0)).await?,
                    state.db.active_intents(Some(STRATEGY_ID.0)).await?,
                );
                if let Some(intent) = self.evaluate(market, book, now, &inventory) {
                    return Ok(vec![intent]);
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.config_refresh_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::price::Price;

    fn book_with_stacks(yes_bid: u8, yes_qty: u32, no_bid: u8, no_qty: u32) -> OrderBook {
        let mut b = OrderBook::new("KX-T");
        let snap = predigy_book::Snapshot {
            seq: 1,
            yes_bids: vec![(Price::from_cents(yes_bid).unwrap(), yes_qty)],
            no_bids: vec![(Price::from_cents(no_bid).unwrap(), no_qty)],
        };
        b.apply_snapshot(snap);
        b
    }

    fn cfg(path: PathBuf) -> ImbalanceConfig {
        ImbalanceConfig {
            config_file: path,
            imbalance_threshold: 0.7,
            min_total_qty: 50,
            min_edge_cents: 5,
            max_take_ask_cents: 90,
            min_take_ask_cents: 5,
            size: 1,
            max_contracts_per_market: 5,
            max_size_per_fire: 3,
            max_touch_take_fraction: 0.20,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        }
    }

    fn write_markets(path: &std::path::Path, tickers: &[&str]) {
        let body = serde_json::json!({
            "markets": tickers.iter().map(|t| serde_json::json!({"ticker": t})).collect::<Vec<_>>()
        });
        std::fs::write(path, serde_json::to_string(&body).unwrap()).unwrap();
    }

    fn inventory(
        ticker: &str,
        yes_qty: i32,
        no_qty: i32,
        active_orders: i32,
    ) -> ImbalanceInventory {
        let mut by_ticker = HashMap::new();
        by_ticker.insert(
            ticker.to_string(),
            TickerInventory {
                yes_qty,
                no_qty,
                active_orders,
            },
        );
        ImbalanceInventory { by_ticker }
    }

    #[test]
    fn fades_dominant_yes_bid_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // yes_bid 100 qty, no_bid 5 qty → imbalance = 0.905 > 0.7.
        let book = book_with_stacks(40, 100, 50, 5);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .expect("fades the YES bid stack → buy NO");
        assert_eq!(intent.side, Side::No);
        assert_eq!(intent.action, IntentAction::Buy);
        // ask = 100 - yes_bid = 60.
        assert_eq!(intent.price_cents, Some(60));
    }

    #[test]
    fn fades_dominant_no_bid_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // no_bid 100 qty, yes_bid 5 qty → imbalance = -0.905.
        let book = book_with_stacks(40, 5, 50, 100);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .expect("fades the NO bid stack → buy YES");
        assert_eq!(intent.side, Side::Yes);
        // ask = 100 - no_bid = 50.
        assert_eq!(intent.price_cents, Some(50));
    }

    #[test]
    fn skips_balanced_book() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // Balanced 50/50.
        let book = book_with_stacks(40, 50, 50, 50);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn skips_when_total_qty_below_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // Tiny stacks: 5 vs 1 → high ratio but tiny total (6 < 50).
        let book = book_with_stacks(40, 5, 50, 1);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn skips_when_market_not_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-A"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        let book = book_with_stacks(40, 100, 50, 5);
        // Different ticker.
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-OTHER"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        let book = book_with_stacks(40, 100, 50, 5);
        let now = Instant::now();
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                now,
                &ImbalanceInventory::default(),
            )
            .is_some()
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                now,
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn skips_when_ask_above_max_take() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // YES bid = 95, so NO ask = 100-95 = 5; if YES dominant we
        // buy NO @ 5 — fine. We need a case where the take ask
        // exceeds max_take_ask_cents (90). YES_bid 5 → NO ask
        // would be 95. Make NO dominant so we buy YES at
        // 100 - no_bid. With no_bid 5, yes_ask = 95 → above 90.
        let book = book_with_stacks(2, 5, 5, 100);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn per_market_threshold_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        let body = serde_json::json!({
            "markets": [
                {"ticker": "KX-EASY", "imbalance_threshold_override": 0.4},
                {"ticker": "KX-STRICT", "imbalance_threshold_override": 0.95}
            ]
        });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        // Imbalance = (60 - 30)/(60+30) = 0.333. Below 0.4 → fail
        // even on EASY. (Sanity for the override path — we just
        // want to confirm the override is respected as the
        // threshold.)
        let book = book_with_stacks(40, 60, 50, 30);
        // EASY threshold 0.4 — 0.333 still below; no fire.
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-EASY"),
                &book,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_none()
        );
        // Now imbalance = (80 - 20)/100 = 0.6. Easy at 0.4 fires;
        // strict at 0.95 doesn't.
        let book2 = book_with_stacks(40, 80, 50, 20);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-EASY"),
                &book2,
                Instant::now(),
                &ImbalanceInventory::default(),
            )
            .is_some()
        );
        // Reset cooldown by waiting on STRICT.
        let book3 = book_with_stacks(40, 80, 50, 20);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-STRICT"),
                &book3,
                Instant::now() + Duration::from_secs(120),
                &ImbalanceInventory::default(),
            )
            .is_none(),
            "STRICT 0.95 threshold should reject 0.6 imbalance"
        );
    }

    #[test]
    fn same_side_inventory_can_scale_under_dynamic_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut c = cfg(path);
        c.cooldown = Duration::from_secs(0);
        let mut s = ImbalanceStrategy::new(c);
        s.reload_markets();
        let book = book_with_stacks(40, 250, 50, 5);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &inventory("KX-T", 0, 1, 0),
            )
            .expect("same-side NO inventory should scale");
        assert_eq!(intent.side, Side::No);
        assert_eq!(intent.action, IntentAction::Buy);
        assert!(intent.qty >= 1);
    }

    #[test]
    fn opposite_inventory_flattens_before_reversal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        let book = book_with_stacks(40, 250, 50, 5);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &inventory("KX-T", 2, 0, 0),
            )
            .expect("opposite YES inventory should be flattened");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.price_cents, Some(40));
    }

    #[test]
    fn active_order_blocks_new_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        let book = book_with_stacks(40, 250, 50, 5);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &book,
                Instant::now(),
                &inventory("KX-T", 0, 0, 1),
            )
            .is_none()
        );
    }

    #[test]
    fn flatten_client_id_stays_short_enough_for_kalshi() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KXVOTEHUBTRUMPUPDOWN-26MAY07"]);
        let mut s = ImbalanceStrategy::new(cfg(path));
        s.reload_markets();
        let intent = s
            .flatten_opposite(
                "KXVOTEHUBTRUMPUPDOWN-26MAY07",
                &MarketTicker::new("KXVOTEHUBTRUMPUPDOWN-26MAY07"),
                Side::Yes,
                10,
                80,
                Instant::now(),
            )
            .unwrap();
        assert!(intent.client_id.len() <= 64, "{}", intent.client_id);
        assert!(intent.client_id.starts_with("bi-flat:"));
    }
}
