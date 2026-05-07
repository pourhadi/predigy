// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-wx-stat` — temperature-market alpha that
//! consumes `wx-stat-curator`'s output file directly, bypassing
//! the `rules` table round-trip used by `stat-trader`.
//!
//! ## Why a dedicated strategy
//!
//! Weather markets have a known information schedule: NWM/NBM
//! ensemble forecasts publish on a fixed cadence; as the forecast
//! horizon collapses individual quantile probabilities approach
//! deterministic. The curator (`bin/wx-stat-curator`) computes
//! calibrated `model_p` from the latest cycle and writes a JSON
//! rules array.
//!
//! Today those rules either flow through stat (after operator
//! merging into `rules` table) or never reach a strategy at all.
//! This module wires the curator output directly into a
//! supervised strategy:
//!
//! - mtime-watch the curator's JSON output file
//! - on change, parse + diff
//! - self-subscribe to added Kalshi tickers via the engine's
//!   self-subscribe path (same plumbing used by latency for held
//!   markets)
//! - on every BookUpdate for a known ticker, evaluate the
//!   alpha — `bet_p` after fees vs `min_edge_cents` — using the
//!   same Kelly sizing math as stat
//!
//! ## Why isolate from stat
//!
//! Separate `STRATEGY_ID` ("wx-stat") means:
//! - positions and PnL tracked separately from stat
//! - per-strategy kill switch fires independently
//! - operator can A/B compare wx-stat alpha vs stat alpha
//! - risk caps enforced per-strategy don't share budget
//!
//! ## What this strategy doesn't do
//!
//! - **No active exits.** Weather markets settle at the
//!   measured-temperature time; the model probability is already
//!   resolved at settlement. No TP/SL/trailing — we hold to
//!   resolution. (If empirical analysis later shows benefit from
//!   intra-day exits, we layer them on without disturbing the
//!   curator interface.)
//! - **No DB rule reads.** Source-of-truth is the JSON file.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("wx-stat");

/// One curator-produced rule. The JSON schema mirrors the existing
/// `wx-stat-curator` output so no curator change is needed for
/// the strategy to consume the same file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WxStatRule {
    /// Kalshi market ticker.
    pub kalshi_market: MarketTicker,
    /// Calibrated model probability that YES resolves true.
    /// `0 < model_p < 1`.
    pub model_p: f64,
    /// Side to bet when after-fee edge clears `min_edge_cents`.
    pub side: Side,
    /// Min after-fee per-contract edge to fire (cents).
    pub min_edge_cents: u32,
}

#[derive(Debug, Clone)]
pub struct WxStatConfig {
    /// Bankroll in cents — input to Kelly sizing.
    pub bankroll_cents: u64,
    /// Fractional Kelly modifier ∈ (0, 1]. Default half-Kelly
    /// (0.5) — the curator's `model_p` is calibrated against
    /// historical NBM error (`wx-stat-fit-calibration`), so the
    /// half-Kelly choice reflects calibration confidence rather
    /// than the quarter-Kelly de-risking the bare stat strategy
    /// uses for hand-coded rules.
    pub kelly_factor: f64,
    /// Hard cap on contracts per fire.
    pub max_size: u32,
    /// Per-market cooldown between fires.
    pub cooldown: Duration,
    /// Path to the curator's JSON rules output.
    pub rule_file: PathBuf,
    /// How often to mtime-poll the rule file.
    pub rule_refresh_interval: Duration,
}

impl WxStatConfig {
    /// Read tunables from the environment. The rule-file path is
    /// required (`PREDIGY_WX_STAT_RULE_FILE`); without it the
    /// engine declines to register the strategy. All other vars
    /// fall back to defaults.
    ///
    /// - `PREDIGY_WX_STAT_RULE_FILE` (path) — required
    /// - `PREDIGY_WX_STAT_BANKROLL_CENTS` (u64)
    /// - `PREDIGY_WX_STAT_KELLY_FACTOR` (f64)
    /// - `PREDIGY_WX_STAT_MAX_SIZE` (u32)
    /// - `PREDIGY_WX_STAT_COOLDOWN_MS` (u64)
    /// - `PREDIGY_WX_STAT_RULE_REFRESH_MS` (u64)
    #[must_use]
    pub fn from_env(rule_file: PathBuf) -> Self {
        let mut c = Self {
            bankroll_cents: 10_000,
            kelly_factor: 0.5,
            max_size: 5,
            cooldown: Duration::from_secs(60),
            rule_file,
            rule_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_WX_STAT_BANKROLL_CENTS")
            && let Ok(n) = v.parse()
        {
            c.bankroll_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_WX_STAT_KELLY_FACTOR")
            && let Ok(n) = v.parse::<f64>()
            && n > 0.0
            && n <= 1.0
        {
            c.kelly_factor = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_WX_STAT_MAX_SIZE")
            && let Ok(n) = v.parse()
        {
            c.max_size = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_WX_STAT_COOLDOWN_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.cooldown = Duration::from_millis(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_WX_STAT_RULE_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.rule_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

/// Pull rule-file path from env. `None` means the engine skips
/// registering this strategy.
#[must_use]
pub fn rule_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_WX_STAT_RULE_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
struct CachedRule {
    side: Side,
    model_p: f64,
    min_edge_cents: i32,
}

#[derive(Debug)]
pub struct WxStatStrategy {
    config: WxStatConfig,
    rules: HashMap<String, CachedRule>,
    last_fire_at: HashMap<String, Instant>,
    last_rule_mtime: Option<SystemTime>,
    last_rule_refresh: Option<Instant>,
    /// Tickers we've already self-subscribed to. Bounded growth
    /// (one per market the curator has ever proposed since boot).
    subscribed: HashSet<String>,
}

impl WxStatStrategy {
    pub fn new(config: WxStatConfig) -> Self {
        Self {
            config,
            rules: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_rule_mtime: None,
            last_rule_refresh: None,
            subscribed: HashSet::new(),
        }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Mtime-poll the rule file. On change, parse + diff. Returns
    /// the list of newly-added tickers; the caller is responsible
    /// for calling `state.subscribe_to_markets(...)` (split this
    /// way so the file-loading half is exercisable in unit tests
    /// without a `StrategyState`).
    fn reload_rules_from_disk(&mut self) -> Vec<MarketTicker> {
        let mtime = match std::fs::metadata(&self.config.rule_file).and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.rule_file.display(),
                    "wx-stat: rule file not present yet; skipping reload"
                );
                self.last_rule_refresh = Some(Instant::now());
                return Vec::new();
            }
            Err(e) => {
                warn!(
                    path = %self.config.rule_file.display(),
                    error = %e,
                    "wx-stat: rule file stat failed"
                );
                self.last_rule_refresh = Some(Instant::now());
                return Vec::new();
            }
        };
        if Some(mtime) == self.last_rule_mtime {
            self.last_rule_refresh = Some(Instant::now());
            return Vec::new();
        }
        let raw = match std::fs::read(&self.config.rule_file) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    path = %self.config.rule_file.display(),
                    error = %e,
                    "wx-stat: rule file read failed"
                );
                self.last_rule_mtime = Some(mtime);
                self.last_rule_refresh = Some(Instant::now());
                return Vec::new();
            }
        };
        let parsed: Vec<WxStatRule> = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    path = %self.config.rule_file.display(),
                    error = %e,
                    "wx-stat: rule file parse failed"
                );
                self.last_rule_mtime = Some(mtime);
                self.last_rule_refresh = Some(Instant::now());
                return Vec::new();
            }
        };
        let mut next: HashMap<String, CachedRule> = HashMap::with_capacity(parsed.len());
        for r in parsed {
            if !(0.01..=0.99).contains(&r.model_p) {
                debug!(
                    ticker = %r.kalshi_market.as_str(),
                    model_p = r.model_p,
                    "wx-stat: rule outside [0.01, 0.99]; skipping"
                );
                continue;
            }
            let key = r.kalshi_market.as_str().to_string();
            let min_edge = i32::try_from(r.min_edge_cents).unwrap_or(0);
            next.insert(
                key,
                CachedRule {
                    side: r.side,
                    model_p: r.model_p,
                    min_edge_cents: min_edge,
                },
            );
        }
        let n_loaded = next.len();
        let added: Vec<MarketTicker> = next
            .keys()
            .filter(|k| !self.subscribed.contains(k.as_str()))
            .map(|k| MarketTicker::new(k.clone()))
            .collect();
        for k in next.keys() {
            self.subscribed.insert(k.clone());
        }
        self.rules = next;
        self.last_rule_mtime = Some(mtime);
        self.last_rule_refresh = Some(Instant::now());
        info!(
            n_rules = n_loaded,
            n_newly_subscribed = added.len(),
            "wx-stat: rules reloaded from curator output"
        );
        added
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

    async fn subscribe_held_positions(
        &mut self,
        state: &StrategyState,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let added: Vec<MarketTicker> = state
            .db
            .open_positions(Some(STRATEGY_ID.0))
            .await?
            .into_iter()
            .map(|p| p.ticker)
            .filter(|ticker| self.subscribed.insert(ticker.clone()))
            .map(MarketTicker::new)
            .collect();
        if !added.is_empty() {
            state.subscribe_to_markets(added);
        }
        Ok(())
    }
}

#[async_trait]
impl Strategy for WxStatStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        let mut tickers: HashSet<String> = state
            .db
            .open_positions(Some(STRATEGY_ID.0))
            .await?
            .into_iter()
            .map(|p| p.ticker)
            .collect();
        if let Ok(raw) = std::fs::read(&self.config.rule_file)
            && let Ok(rules) = serde_json::from_slice::<Vec<WxStatRule>>(&raw)
        {
            tickers.extend(
                rules
                    .into_iter()
                    .map(|r| r.kalshi_market.as_str().to_string()),
            );
        }
        Ok(tickers.into_iter().map(MarketTicker::new).collect())
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // First-call reload + periodic mtime check.
        let needs_refresh = self
            .last_rule_refresh
            .is_none_or(|t| t.elapsed() >= self.config.rule_refresh_interval);
        if needs_refresh {
            let added = self.reload_rules_from_disk();
            if !added.is_empty() {
                state.subscribe_to_markets(added);
            }
            self.subscribe_held_positions(state).await?;
        }

        match ev {
            Event::BookUpdate { market, book } => {
                let now = Instant::now();
                if let Some(intent) = self.evaluate(market, book, now) {
                    return Ok(vec![intent]);
                }
                Ok(Vec::new())
            }
            Event::Tick => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.rule_refresh_interval)
    }
}

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
    config: &WxStatConfig,
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
    let client_id = format!(
        "wx-stat:{ticker}:{ask:02}:{size:04}:{ts:08x}",
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
            "wx-stat fire: model_p={:.3} ask={}c edge={:.1}c size={}",
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

    fn cfg(rule_file: PathBuf) -> WxStatConfig {
        WxStatConfig {
            bankroll_cents: 10_000,
            kelly_factor: 0.5,
            max_size: 5,
            cooldown: Duration::from_secs(60),
            rule_file,
            rule_refresh_interval: Duration::from_secs(30),
        }
    }

    #[test]
    fn build_intent_fires_on_clear_edge() {
        let rule = CachedRule {
            side: Side::Yes,
            model_p: 0.85,
            min_edge_cents: 3,
        };
        let cfg = cfg(PathBuf::from("/dev/null"));
        let market = MarketTicker::new("KX-WX");
        let intent = build_intent(&market, &rule, &cfg, 70, 100).expect("fires");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Buy);
        assert_eq!(intent.price_cents, Some(70));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(intent.client_id.starts_with("wx-stat:KX-WX:70:"));
    }

    #[test]
    fn build_intent_skips_when_edge_below_threshold() {
        let rule = CachedRule {
            side: Side::Yes,
            model_p: 0.55,
            min_edge_cents: 5,
        };
        let cfg = cfg(PathBuf::from("/dev/null"));
        let market = MarketTicker::new("KX-NEAR");
        // ask 50 → raw edge 5¢ minus fee ~ < 5; should skip.
        assert!(build_intent(&market, &rule, &cfg, 50, 100).is_none());
    }

    #[test]
    fn build_intent_skips_when_model_p_invalid() {
        let rule = CachedRule {
            side: Side::Yes,
            model_p: 0.005, // outside [0.01, 0.99]
            min_edge_cents: 1,
        };
        let cfg = cfg(PathBuf::from("/dev/null"));
        let market = MarketTicker::new("KX-INV");
        assert!(build_intent(&market, &rule, &cfg, 1, 100).is_none());
    }

    #[test]
    fn derive_ask_yes_uses_complement_of_no_bid() {
        let book = book_with_quotes(Some(40), Some(50));
        // YES ask = 100 - NO bid = 100 - 50 = 50
        let (ask, _) = derive_ask(&book, Side::Yes).unwrap();
        assert_eq!(ask, 50);
    }

    #[test]
    fn cooldown_blocks_repeat_fires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wx-stat-rules.json");
        let rules = serde_json::json!([{
            "kalshi_market": "KX-WX1",
            "model_p": 0.85,
            "side": "yes",
            "min_edge_cents": 3
        }]);
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = WxStatStrategy::new(cfg(path));

        // Force-load the rules (bypass the StrategyState plumbing
        // by stuffing them directly).
        s.rules.insert(
            "KX-WX1".into(),
            CachedRule {
                side: Side::Yes,
                model_p: 0.85,
                min_edge_cents: 3,
            },
        );

        let book = book_with_quotes(Some(30), Some(30)); // YES ask 70
        let market = MarketTicker::new("KX-WX1");
        let now = Instant::now();
        let first = s.evaluate(&market, &book, now);
        assert!(first.is_some(), "first fire should pass");
        // Second fire within cooldown.
        let second = s.evaluate(&market, &book, now);
        assert!(second.is_none(), "second fire should be cooldown-blocked");
    }

    #[test]
    fn rule_reload_picks_up_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wx-stat-rules.json");
        std::fs::write(&path, "[]").unwrap();

        let mut s = WxStatStrategy::new(cfg(path.clone()));
        let added = s.reload_rules_from_disk();
        assert!(added.is_empty());
        assert_eq!(s.rule_count(), 0);

        // Write rules. The mtime granularity on some filesystems
        // is ms-coarse so sleep briefly to force a distinct
        // mtime, then write.
        std::thread::sleep(Duration::from_millis(20));
        let rules = serde_json::json!([{
            "kalshi_market": "KX-NEW",
            "model_p": 0.7,
            "side": "yes",
            "min_edge_cents": 2
        }]);
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let added = s.reload_rules_from_disk();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].as_str(), "KX-NEW");
        assert_eq!(s.rule_count(), 1);

        // Re-running without changes returns empty.
        let added = s.reload_rules_from_disk();
        assert!(added.is_empty());
    }

    #[test]
    fn rule_reload_skips_invalid_model_p() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wx-stat-rules.json");
        let rules = serde_json::json!([
            { "kalshi_market": "KX-OK",  "model_p": 0.7,   "side": "yes", "min_edge_cents": 2 },
            { "kalshi_market": "KX-BAD", "model_p": 1.5,   "side": "yes", "min_edge_cents": 2 },
            { "kalshi_market": "KX-LOW", "model_p": 0.001, "side": "no",  "min_edge_cents": 2 },
        ]);
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = WxStatStrategy::new(cfg(path));
        let added = s.reload_rules_from_disk();
        let names: Vec<_> = added.iter().map(|m| m.as_str().to_string()).collect();
        assert_eq!(names, vec!["KX-OK".to_string()]);
        assert_eq!(s.rule_count(), 1);
    }
}
