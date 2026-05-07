// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-variance-fade` — fade rapid price moves
//! on stable-information markets (Audit S8).
//!
//! ## Mechanism
//!
//! The strategy maintains a per-ticker rolling window of mid-
//! price observations. Each `Event::BookUpdate` snapshots the
//! current mid (= `(yes_bid + (100 - no_bid)) / 2`) into the
//! window, evicting samples older than `window_secs`. When the
//! latest mid has moved more than `move_threshold_cents` away
//! from the median of the window AND the rolling realized
//! variance hasn't seen a comparable move recently, the
//! strategy classifies it as a vol-spike and fades the move:
//! buy the side that has gotten cheap.
//!
//! ## Why "stable-information markets"
//!
//! On a market where new information legitimately drives price
//! (e.g. an earnings beat moves the line), fading the move just
//! buys into a trend. The strategy is intended for markets the
//! operator believes are mostly information-stable on short
//! horizons — temperature markets near settlement, sports lines
//! in low-news periods, year-end forecast markets, etc. The
//! operator picks the universe in the config file.
//!
//! ## What this strategy doesn't do
//!
//! - **No news suppression.** If a real news event drives the
//!   move the strategy will fade it incorrectly. A future
//!   integration with the news-classifier (Audit S5) will gate
//!   on a "no high-impact news" signal — for now the operator's
//!   universe choice is the only safeguard.
//! - **No active mark-aware exits.** The OMS's session-flatten
//!   + kill-switch handle forced flats. Layered TP/SL is a
//!   follow-up.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("variance-fade");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VarianceFadeMarket {
    pub ticker: String,
    /// Per-market move-threshold override in cents.
    #[serde(default)]
    pub move_threshold_cents_override: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VarianceFadeRulesFile {
    pub markets: Vec<VarianceFadeMarket>,
}

#[derive(Debug, Clone)]
pub struct VarianceFadeConfig {
    pub config_file: PathBuf,
    /// Rolling window length for the median-of-mids reference.
    pub window: Duration,
    /// Move threshold (cents) — the latest mid must be at least
    /// this far from the window median to count as a spike.
    pub move_threshold_cents: u8,
    /// Min number of observations required in the window before
    /// the strategy will fire. Prevents firing on freshly-
    /// subscribed markets with thin history.
    pub min_observations: usize,
    /// Min price floor — never buy when ask < this (the rails).
    pub min_take_ask_cents: u8,
    /// Max price ceiling — never buy above this.
    pub max_take_ask_cents: u8,
    pub size: u32,
    pub cooldown: Duration,
    pub config_refresh_interval: Duration,
}

impl VarianceFadeConfig {
    /// Build from env. `PREDIGY_VARIANCE_FADE_CONFIG` required.
    ///
    /// - `..._CONFIG` (path) — required
    /// - `..._WINDOW_SECS` (u64, default 600 = 10 min)
    /// - `..._MOVE_THRESHOLD_CENTS` (u8, default 8)
    /// - `..._MIN_OBSERVATIONS` (usize, default 30)
    /// - `..._MIN_TAKE_ASK_CENTS` (u8, default 5)
    /// - `..._MAX_TAKE_ASK_CENTS` (u8, default 95)
    /// - `..._SIZE` (u32, default 1)
    /// - `..._COOLDOWN_MS` (u64, default 60_000)
    /// - `..._REFRESH_MS` (u64, default 30_000)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            window: Duration::from_secs(600),
            move_threshold_cents: 8,
            min_observations: 30,
            min_take_ask_cents: 5,
            max_take_ask_cents: 95,
            size: 1,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_WINDOW_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.window = Duration::from_secs(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_MOVE_THRESHOLD_CENTS")
            && let Ok(n) = v.parse()
        {
            c.move_threshold_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_MIN_OBSERVATIONS")
            && let Ok(n) = v.parse()
        {
            c.min_observations = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_MIN_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_MAX_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.max_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_SIZE")
            && let Ok(n) = v.parse()
        {
            c.size = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_COOLDOWN_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.cooldown = Duration::from_millis(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_VARIANCE_FADE_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.config_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn config_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_VARIANCE_FADE_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
struct CachedMarket {
    move_threshold_cents: u8,
}

#[derive(Debug, Clone, Copy)]
struct PriceObs {
    at: Instant,
    yes_mid_cents: u8,
}

#[derive(Debug)]
pub struct VarianceFadeStrategy {
    config: VarianceFadeConfig,
    markets: HashMap<String, CachedMarket>,
    /// Rolling window of `(at, yes_mid)` per ticker.
    history: HashMap<String, VecDeque<PriceObs>>,
    last_fire_at: HashMap<String, Instant>,
    last_config_refresh: Option<Instant>,
}

impl VarianceFadeStrategy {
    pub fn new(config: VarianceFadeConfig) -> Self {
        Self {
            config,
            markets: HashMap::new(),
            history: HashMap::new(),
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
                    "variance-fade: config not present yet"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
            Err(e) => {
                warn!(error = %e, "variance-fade: config read failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let parsed: VarianceFadeRulesFile = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "variance-fade: config parse failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let mut next: HashMap<String, CachedMarket> = HashMap::with_capacity(parsed.markets.len());
        for m in parsed.markets {
            let move_threshold_cents = m
                .move_threshold_cents_override
                .unwrap_or(self.config.move_threshold_cents);
            next.insert(
                m.ticker,
                CachedMarket {
                    move_threshold_cents,
                },
            );
        }
        info!(n_markets = next.len(), "variance-fade: config loaded");
        self.markets = next;
        self.last_config_refresh = Some(Instant::now());
    }

    /// Snapshot the current YES-mid into the rolling window. Mid
    /// is defined as `(yes_bid + (100 - no_bid)) / 2` when both
    /// sides exist, else the single side. Returns `None` when
    /// the touch can't produce a credible mid.
    fn record_mid(&mut self, ticker: &str, book: &OrderBook, now: Instant) -> Option<u8> {
        let yes_bid = book.best_yes_bid().map(|(p, _)| p.cents());
        let no_bid = book.best_no_bid().map(|(p, _)| p.cents());
        let mid = match (yes_bid, no_bid) {
            (Some(yb), Some(nb)) => {
                // 100 - no_bid = yes_ask. Mid is the midpoint of
                // yes_bid and yes_ask.
                let yes_ask = 100u8.checked_sub(nb)?;
                Some(u16::from(yb).midpoint(u16::from(yes_ask)))
            }
            (Some(yb), None) => Some(u16::from(yb)),
            (None, Some(nb)) => 100u8.checked_sub(nb).map(u16::from),
            (None, None) => None,
        }?;
        let mid_u8: u8 = mid.try_into().ok()?;
        let entry = self
            .history
            .entry(ticker.to_string())
            .or_insert_with(VecDeque::new);
        entry.push_back(PriceObs {
            at: now,
            yes_mid_cents: mid_u8,
        });
        // Evict samples older than the window.
        while let Some(front) = entry.front() {
            if now.duration_since(front.at) > self.config.window {
                entry.pop_front();
            } else {
                break;
            }
        }
        Some(mid_u8)
    }

    fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Option<Intent> {
        let key = market.as_str().to_string();
        let entry = self.markets.get(&key)?;
        let move_threshold = entry.move_threshold_cents;
        let current_mid = self.record_mid(&key, book, now)?;
        if let Some(&last) = self.last_fire_at.get(&key)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let history = self.history.get(&key)?;
        if history.len() < self.config.min_observations {
            return None;
        }
        // Median of the window — robust to one-tick noise.
        let mut samples: Vec<u8> = history.iter().map(|p| p.yes_mid_cents).collect();
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        let move_cents = i32::from(current_mid) - i32::from(median);
        if move_cents.unsigned_abs() < u32::from(move_threshold) {
            return None;
        }
        // Direction: positive move (current mid above median) →
        // YES has gotten expensive → fade by buying NO. Negative
        // move → buy YES.
        let (side, side_tag) = if move_cents > 0 {
            (Side::No, "N")
        } else {
            (Side::Yes, "Y")
        };
        let opposite_bid = match side {
            Side::Yes => book.best_no_bid()?.0.cents(),
            Side::No => book.best_yes_bid()?.0.cents(),
        };
        let ask_cents = 100u8.checked_sub(opposite_bid)?;
        if ask_cents < self.config.min_take_ask_cents || ask_cents > self.config.max_take_ask_cents
        {
            debug!(
                ticker = %key,
                ask_cents,
                "variance-fade: ask outside [min, max] take floor; skip"
            );
            return None;
        }
        let qty = i32::try_from(self.config.size).ok()?;
        let ts_min = chrono::Utc::now().timestamp() as u32 / 60;
        let client_id = format!(
            "variance-fade:{cid_t}:{side_tag}:{ask:02}:{size:04}:{ts:08x}",
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
                "variance-fade: mid={current_mid}c median={median}c move={move_cents}c samples={}",
                samples.len()
            )),
        };
        info!(
            ticker = %key,
            current_mid,
            median,
            move_cents,
            side = ?side,
            ask_cents,
            "variance-fade: firing fade"
        );
        self.last_fire_at.insert(key, now);
        Some(intent)
    }
}

#[async_trait]
impl Strategy for VarianceFadeStrategy {
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
        let parsed: VarianceFadeRulesFile = serde_json::from_slice(&raw)?;
        Ok(parsed
            .markets
            .into_iter()
            .map(|m| MarketTicker::new(&m.ticker))
            .collect())
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        _state: &mut StrategyState,
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
                if let Some(intent) = self.evaluate(market, book, now) {
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

    fn book_with_mid(yes_bid: u8, no_bid: u8) -> OrderBook {
        let mut b = OrderBook::new("KX-T");
        let snap = predigy_book::Snapshot {
            seq: 1,
            yes_bids: vec![(Price::from_cents(yes_bid).unwrap(), 100)],
            no_bids: vec![(Price::from_cents(no_bid).unwrap(), 100)],
        };
        b.apply_snapshot(snap);
        b
    }

    fn cfg(path: PathBuf) -> VarianceFadeConfig {
        VarianceFadeConfig {
            config_file: path,
            window: Duration::from_secs(600),
            move_threshold_cents: 8,
            min_observations: 5,
            min_take_ask_cents: 5,
            max_take_ask_cents: 95,
            size: 1,
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

    #[test]
    fn fades_upward_move_by_buying_no() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        // Seed the window with 10 observations centered around mid=50.
        for i in 0..10 {
            let book = book_with_mid(48, 50); // mid = (48 + 50) / 2 = 49
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        // Now spike to mid = 60: yes_bid 60, no_bid 38 → mid =
        // (60 + 62)/2 = 61.
        let spike_book = book_with_mid(60, 38);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &spike_book,
                now + Duration::from_secs(20),
            )
            .expect("fades the upward spike");
        assert_eq!(intent.side, Side::No);
        // ask = 100 - yes_bid = 100 - 60 = 40.
        assert_eq!(intent.price_cents, Some(40));
    }

    #[test]
    fn fades_downward_move_by_buying_yes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        for i in 0..10 {
            // mid around 50.
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        // Spike DOWN to mid=30: yes_bid 28, no_bid 68 → ask_yes =
        // 100 - 68 = 32, mid = (28+32)/2 = 30.
        let spike_book = book_with_mid(28, 68);
        let intent = s
            .evaluate(
                &MarketTicker::new("KX-T"),
                &spike_book,
                now + Duration::from_secs(20),
            )
            .expect("fades the downward spike");
        assert_eq!(intent.side, Side::Yes);
        // ask = 100 - no_bid = 100 - 68 = 32.
        assert_eq!(intent.price_cents, Some(32));
    }

    #[test]
    fn skips_small_move() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        for i in 0..10 {
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        // Move to mid=53 (only 4¢ above median 49 — below 8¢ threshold).
        let small_book = book_with_mid(52, 46);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &small_book,
                now + Duration::from_secs(20)
            )
            .is_none()
        );
    }

    #[test]
    fn skips_when_below_min_observations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        // Only 3 observations, threshold needs 5.
        for i in 0..3 {
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        let spike_book = book_with_mid(60, 38);
        assert!(
            s.evaluate(&MarketTicker::new("KX-T"), &spike_book, now)
                .is_none()
        );
    }

    #[test]
    fn evicts_observations_older_than_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut c = cfg(path);
        c.window = Duration::from_secs(10);
        let mut s = VarianceFadeStrategy::new(c);
        s.reload_markets();
        let now = Instant::now();
        // Push 5 old obs and 5 fresh obs — old ones evict.
        for i in 0..5 {
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        // Advance time past the window.
        let later = now + Duration::from_secs(100);
        for i in 0..5 {
            let book = book_with_mid(60, 38);
            let _ = s.record_mid("KX-T", &book, later + Duration::from_secs(i));
        }
        // History should only have the fresh 5 (old 5 evicted).
        assert_eq!(s.history.get("KX-T").map(|d| d.len()), Some(5));
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-T"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        for i in 0..10 {
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-T", &book, now + Duration::from_secs(i));
        }
        let spike_book = book_with_mid(60, 38);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &spike_book,
                now + Duration::from_secs(20)
            )
            .is_some()
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-T"),
                &spike_book,
                now + Duration::from_secs(21)
            )
            .is_none()
        );
    }

    #[test]
    fn skips_market_not_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        write_markets(&path, &["KX-A"]);
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let book = book_with_mid(60, 38);
        assert!(
            s.evaluate(&MarketTicker::new("KX-OTHER"), &book, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn per_market_threshold_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markets.json");
        let body = serde_json::json!({
            "markets": [
                {"ticker": "KX-LOOSE", "move_threshold_cents_override": 4},
                {"ticker": "KX-TIGHT", "move_threshold_cents_override": 20}
            ]
        });
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        let mut s = VarianceFadeStrategy::new(cfg(path));
        s.reload_markets();
        let now = Instant::now();
        // Seed median 49 on both.
        for i in 0..10 {
            let book = book_with_mid(48, 50);
            let _ = s.record_mid("KX-LOOSE", &book, now + Duration::from_secs(i));
            let _ = s.record_mid("KX-TIGHT", &book, now + Duration::from_secs(i));
        }
        // Move to mid 55 — 6¢ above median.
        let book = book_with_mid(54, 44);
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-LOOSE"),
                &book,
                now + Duration::from_secs(20)
            )
            .is_some()
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("KX-TIGHT"),
                &book,
                now + Duration::from_secs(20)
            )
            .is_none()
        );
    }
}
