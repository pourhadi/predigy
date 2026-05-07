// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-news-trader` — Audit S5: news-event
//! semantic latency expansion via decoupled classifier.
//!
//! ## Mechanism
//!
//! Rather than coupling a specific text classifier (Claude,
//! custom ML, manual operator) into the engine, this strategy
//! reads **pre-classified news items** from an operator-managed
//! JSONL file. Any upstream pipeline that produces classified
//! items can feed it: a Claude-based classifier service, a
//! Python script with sklearn, a manual operator drop, or a
//! webhook from a paid news vendor.
//!
//! The JSONL schema is a sequence of:
//!
//! ```json
//! {"item_id": "twitter-1234", "ticker": "KX-XYZ",
//!  "side": "yes", "action": "buy", "max_price_cents": 60,
//!  "size": 5, "source": "twitter:@bigaccount",
//!  "headline": "...", "classified_at": "2026-05-07T12:34:56Z",
//!  "confidence": 0.85}
//! ```
//!
//! Strategy behavior:
//! - On each Tick, mtime-poll the JSONL file. If the file has
//!   new lines (more lines than the previous tick), parse + fire
//!   the new items.
//! - Each item's `item_id` is the dedup key — even if the
//!   strategy restarts and re-reads the file from scratch, every
//!   item is processed at most once via an in-memory
//!   `HashSet<item_id>`. The OMS layer also dedupes on
//!   `client_id` so the system is doubly idempotent.
//! - Items below `min_confidence` are skipped. Items with
//!   `max_price_cents` outside `[min_take_ask, max_take_ask]`
//!   are skipped.
//! - The strategy submits an IOC limit at `max_price_cents` for
//!   `size` contracts on the named `(ticker, side)`.
//!
//! ## What this strategy doesn't do
//!
//! - **No classifier.** The classifier is a separate process
//!   entirely; this strategy trusts its output (with bounds
//!   checks). Decoupling lets the operator iterate on
//!   classifier choices without touching the engine.
//! - **No active mark-aware exits.** The OMS's session-flatten +
//!   kill-switch handle forced flats. Layered TP/SL is a
//!   follow-up.
//! - **No dynamic market subscription.** The strategy submits
//!   intents directly; the engine's existing market-data router
//!   wires per-strategy book subscriptions only for static
//!   `subscribed_markets`. News-trader doesn't need book updates
//!   to fire (max_price_cents in the classified item is the
//!   ceiling).

use async_trait::async_trait;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("news-trader");

/// One classified-news item. Schema is operator-controlled —
/// the upstream classifier produces these.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClassifiedNewsItem {
    /// Stable id from the classifier (e.g. `"twitter-1234"`).
    /// The strategy dedupes on this. Reused ids are a no-op.
    pub item_id: String,
    pub ticker: String,
    pub side: Side,
    /// `"buy"` or `"sell"`.
    pub action: String,
    /// IOC limit price. The strategy never pays above this.
    pub max_price_cents: u8,
    pub size: u32,
    pub source: String,
    /// Optional: original headline / text the classifier saw.
    /// Logged + persisted in the intent's `reason` for forensic
    /// review, never used for matching.
    #[serde(default)]
    pub headline: Option<String>,
    /// ISO-8601. Logged for audit; not used for filtering.
    #[serde(default)]
    pub classified_at: Option<String>,
    /// Classifier's confidence ∈ `[0.0, 1.0]`. Items below
    /// `min_confidence` are skipped.
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct NewsTraderConfig {
    /// Path to the JSONL of classified news items. Append-only;
    /// the upstream classifier appends new items.
    pub items_file: PathBuf,
    /// Minimum classifier confidence to fire on an item. `0.0`
    /// disables the gate.
    pub min_confidence: f64,
    /// Min IOC limit cents — never fire below this.
    pub min_take_ask_cents: u8,
    /// Max IOC limit cents — never fire above this.
    pub max_take_ask_cents: u8,
    /// File-poll cadence.
    pub refresh_interval: Duration,
}

impl NewsTraderConfig {
    /// `PREDIGY_NEWS_TRADER_ITEMS_FILE` is required.
    ///
    /// - `..._ITEMS_FILE` (path) — required
    /// - `..._MIN_CONFIDENCE` (f64, default 0.0)
    /// - `..._MIN_TAKE_ASK_CENTS` (u8, default 5)
    /// - `..._MAX_TAKE_ASK_CENTS` (u8, default 95)
    /// - `..._REFRESH_MS` (u64, default 5_000 — 5s polling)
    #[must_use]
    pub fn from_env(items_file: PathBuf) -> Self {
        let mut c = Self {
            items_file,
            min_confidence: 0.0,
            min_take_ask_cents: 5,
            max_take_ask_cents: 95,
            refresh_interval: Duration::from_secs(5),
        };
        if let Ok(v) = std::env::var("PREDIGY_NEWS_TRADER_MIN_CONFIDENCE")
            && let Ok(n) = v.parse::<f64>()
            && (0.0..=1.0).contains(&n)
        {
            c.min_confidence = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_NEWS_TRADER_MIN_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_NEWS_TRADER_MAX_TAKE_ASK_CENTS")
            && let Ok(n) = v.parse()
        {
            c.max_take_ask_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_NEWS_TRADER_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn items_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_NEWS_TRADER_ITEMS_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug)]
pub struct NewsTraderStrategy {
    config: NewsTraderConfig,
    /// Items processed (dedup key set). Bounded by total items
    /// the upstream has produced since strategy start. For
    /// long-running deployments we'd evict aged entries; v1
    /// keeps it simple.
    seen: HashSet<String>,
    last_refresh: Option<Instant>,
}

impl NewsTraderStrategy {
    pub fn new(config: NewsTraderConfig) -> Self {
        Self {
            config,
            seen: HashSet::new(),
            last_refresh: None,
        }
    }

    pub fn seen_count(&self) -> usize {
        self.seen.len()
    }

    /// Read the items file, return any items whose item_id is
    /// not in `seen`. Marks each returned item as seen.
    fn drain_new_items(&mut self) -> Vec<ClassifiedNewsItem> {
        let raw = match std::fs::read_to_string(&self.config.items_file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.items_file.display(),
                    "news-trader: items file not present yet"
                );
                self.last_refresh = Some(Instant::now());
                return Vec::new();
            }
            Err(e) => {
                warn!(error = %e, "news-trader: items file read failed");
                self.last_refresh = Some(Instant::now());
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (lineno, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let item: ClassifiedNewsItem = match serde_json::from_str(trimmed) {
                Ok(i) => i,
                Err(e) => {
                    warn!(
                        line_no = lineno + 1,
                        error = %e,
                        "news-trader: malformed item line; skipping"
                    );
                    continue;
                }
            };
            if self.seen.contains(&item.item_id) {
                continue;
            }
            self.seen.insert(item.item_id.clone());
            out.push(item);
        }
        self.last_refresh = Some(Instant::now());
        out
    }

    fn build_intent(&self, item: &ClassifiedNewsItem) -> Option<Intent> {
        // Confidence gate.
        if let Some(conf) = item.confidence
            && conf < self.config.min_confidence
        {
            debug!(
                item_id = item.item_id,
                conf,
                min = self.config.min_confidence,
                "news-trader: item below min_confidence"
            );
            return None;
        }
        // Price-rail gate.
        if item.max_price_cents < self.config.min_take_ask_cents
            || item.max_price_cents > self.config.max_take_ask_cents
        {
            debug!(
                item_id = item.item_id,
                max_price_cents = item.max_price_cents,
                "news-trader: item price outside [min, max] take floor"
            );
            return None;
        }
        if item.size == 0 {
            return None;
        }
        let action = match item.action.as_str() {
            "buy" => IntentAction::Buy,
            "sell" => IntentAction::Sell,
            other => {
                warn!(
                    item_id = item.item_id,
                    action = other,
                    "news-trader: unknown action; skip"
                );
                return None;
            }
        };
        let qty = i32::try_from(item.size).ok()?;
        // The item_id is the dedup key all the way down. cid
        // includes both the strategy prefix and a 20-char
        // truncation of item_id so the OMS row is operator-
        // greppable.
        let item_short: String = item.item_id.chars().take(20).collect();
        let client_id = format!(
            "news-trader:{cid_t}:{item_short}",
            cid_t = cid_safe_ticker(&item.ticker),
            item_short = item_short,
        );
        let intent = Intent {
            client_id,
            strategy: STRATEGY_ID.0,
            market: MarketTicker::new(&item.ticker),
            side: item.side,
            action,
            price_cents: Some(i32::from(item.max_price_cents)),
            qty,
            order_type: OrderType::Limit,
            tif: Tif::Ioc,
            reason: Some(format!(
                "news-trader: source={} confidence={:?} headline={:?}",
                item.source, item.confidence, item.headline
            )),
        };
        info!(
            item_id = item.item_id,
            source = item.source,
            ticker = item.ticker,
            side = ?item.side,
            max_price_cents = item.max_price_cents,
            size = item.size,
            "news-trader: firing classified-news intent"
        );
        Some(intent)
    }
}

#[async_trait]
impl Strategy for NewsTraderStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        // No book subscriptions — the strategy submits IOC limits
        // at the classifier-supplied price ceiling. Book state
        // doesn't gate firing.
        Ok(Vec::new())
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        _state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // First-event + tick refresh.
        let needs_refresh = self
            .last_refresh
            .is_none_or(|t| t.elapsed() >= self.config.refresh_interval);
        match ev {
            Event::Tick if needs_refresh => {}
            Event::External(_) => {
                // External events go to the file feed by
                // separate channel; ignore here.
                return Ok(Vec::new());
            }
            _ if needs_refresh => {}
            _ => return Ok(Vec::new()),
        }
        let items = self.drain_new_items();
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let Some(intent) = self.build_intent(&item) {
                out.push(intent);
            }
        }
        Ok(out)
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.refresh_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(items_file: PathBuf) -> NewsTraderConfig {
        NewsTraderConfig {
            items_file,
            min_confidence: 0.0,
            min_take_ask_cents: 5,
            max_take_ask_cents: 95,
            refresh_interval: Duration::from_millis(50),
        }
    }

    fn write_items(path: &std::path::Path, items: &[serde_json::Value]) {
        let mut s = String::new();
        for it in items {
            s.push_str(&serde_json::to_string(it).unwrap());
            s.push('\n');
        }
        std::fs::write(path, s).unwrap();
    }

    fn append_items(path: &std::path::Path, items: &[serde_json::Value]) {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        for it in items {
            f.write_all(serde_json::to_string(it).unwrap().as_bytes())
                .unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn drain_returns_new_items_skips_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        write_items(
            &path,
            &[
                serde_json::json!({
                    "item_id": "n1", "ticker": "KX-A", "side": "yes",
                    "action": "buy", "max_price_cents": 60, "size": 1,
                    "source": "test", "confidence": 0.9
                }),
                serde_json::json!({
                    "item_id": "n2", "ticker": "KX-B", "side": "no",
                    "action": "buy", "max_price_cents": 40, "size": 2,
                    "source": "test", "confidence": 0.7
                }),
            ],
        );
        let mut s = NewsTraderStrategy::new(cfg(path.clone()));
        let first = s.drain_new_items();
        assert_eq!(first.len(), 2);
        // Second drain returns nothing — both are seen.
        let second = s.drain_new_items();
        assert!(second.is_empty());

        // Append a third item.
        append_items(
            &path,
            &[serde_json::json!({
                "item_id": "n3", "ticker": "KX-C", "side": "yes",
                "action": "buy", "max_price_cents": 70, "size": 1,
                "source": "test"
            })],
        );
        let third = s.drain_new_items();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].item_id, "n3");
    }

    #[test]
    fn build_intent_passes_clean_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(&path, "").unwrap();
        let s = NewsTraderStrategy::new(cfg(path));
        let item = ClassifiedNewsItem {
            item_id: "n1".into(),
            ticker: "KX-NEWS".into(),
            side: Side::Yes,
            action: "buy".into(),
            max_price_cents: 60,
            size: 5,
            source: "test".into(),
            headline: Some("Big news".into()),
            classified_at: None,
            confidence: Some(0.95),
        };
        let intent = s.build_intent(&item).expect("fires");
        assert_eq!(intent.market.as_str(), "KX-NEWS");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Buy);
        assert_eq!(intent.qty, 5);
        assert_eq!(intent.price_cents, Some(60));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(intent.client_id.starts_with("news-trader:KX-NEWS:"));
    }

    #[test]
    fn build_intent_skips_below_min_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut c = cfg(path);
        c.min_confidence = 0.8;
        let s = NewsTraderStrategy::new(c);
        let item = ClassifiedNewsItem {
            item_id: "n1".into(),
            ticker: "KX-A".into(),
            side: Side::Yes,
            action: "buy".into(),
            max_price_cents: 50,
            size: 1,
            source: "test".into(),
            headline: None,
            classified_at: None,
            confidence: Some(0.5),
        };
        assert!(s.build_intent(&item).is_none());
    }

    #[test]
    fn build_intent_skips_outside_take_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(&path, "").unwrap();
        let s = NewsTraderStrategy::new(cfg(path));
        // max_price 99 — above 95 ceiling.
        let item = ClassifiedNewsItem {
            item_id: "n1".into(),
            ticker: "KX-A".into(),
            side: Side::Yes,
            action: "buy".into(),
            max_price_cents: 99,
            size: 1,
            source: "test".into(),
            headline: None,
            classified_at: None,
            confidence: None,
        };
        assert!(s.build_intent(&item).is_none());
    }

    #[test]
    fn build_intent_skips_invalid_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(&path, "").unwrap();
        let s = NewsTraderStrategy::new(cfg(path));
        let item = ClassifiedNewsItem {
            item_id: "n1".into(),
            ticker: "KX-A".into(),
            side: Side::Yes,
            action: "transmute".into(),
            max_price_cents: 50,
            size: 1,
            source: "test".into(),
            headline: None,
            classified_at: None,
            confidence: None,
        };
        assert!(s.build_intent(&item).is_none());
    }

    #[test]
    fn drain_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(
            &path,
            r#"{"item_id":"good","ticker":"KX-A","side":"yes","action":"buy","max_price_cents":50,"size":1,"source":"t"}
malformed-no-json
{"item_id":"good2","ticker":"KX-B","side":"no","action":"buy","max_price_cents":40,"size":2,"source":"t"}
"#,
        )
        .unwrap();
        let mut s = NewsTraderStrategy::new(cfg(path));
        let items = s.drain_new_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_id, "good");
        assert_eq!(items[1].item_id, "good2");
    }

    #[test]
    fn drain_handles_blank_lines_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(
            &path,
            r#"
# this is a comment
{"item_id":"n1","ticker":"KX-A","side":"yes","action":"buy","max_price_cents":50,"size":1,"source":"t"}

# another comment
"#,
        )
        .unwrap();
        let mut s = NewsTraderStrategy::new(cfg(path));
        let items = s.drain_new_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "n1");
    }

    #[test]
    fn item_id_used_as_cid_dedup_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        std::fs::write(&path, "").unwrap();
        let s = NewsTraderStrategy::new(cfg(path));
        let item = ClassifiedNewsItem {
            item_id: "twitter-9876543210abcdef".into(),
            ticker: "KX.WITH.PERIODS".into(),
            side: Side::Yes,
            action: "buy".into(),
            max_price_cents: 50,
            size: 1,
            source: "test".into(),
            headline: None,
            classified_at: None,
            confidence: None,
        };
        let intent = s.build_intent(&item).unwrap();
        // cid_safe_ticker strips periods.
        assert!(!intent.client_id.contains('.'));
        // First 20 chars of item_id appear in cid.
        assert!(intent.client_id.contains("twitter-9876543210ab"));
    }
}
