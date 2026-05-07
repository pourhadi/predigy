// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-implication-arb` — settlement-time
//! multi-leg arbitrage on implication pairs (Audit S9).
//!
//! ## Mechanism
//!
//! When a child event implies a parent event (`child_yes ⊂
//! parent_yes` — every settlement state where the child resolves
//! YES has the parent also resolving YES), the prices must
//! satisfy `P(child) ≤ P(parent)`. When the touch quotes drift
//! such that
//!
//!   yes_bid(child) − yes_ask(parent) ≥ min_edge_cents
//!
//! a two-leg trade locks in guaranteed profit:
//!
//!   - **Buy 1 YES of parent at `yes_ask(parent)`**
//!   - **Sell 1 YES of child at `yes_bid(child)`**
//!     (= **Buy 1 NO of child at `100 − yes_bid(child)`**)
//!
//! The minimum-payoff scenario is "parent YES & child YES":
//! parent settles to +$1, child short pays out $1, net $0 in
//! settlement; cash leg yields `yes_bid(child) − yes_ask(parent)`
//! up front. The other allowed scenarios (parent YES & child NO;
//! parent NO & child NO) only add further profit at settlement.
//! `child YES & parent NO` is impossible by the implication
//! premise.
//!
//! ## What this strategy does
//!
//! - Reads a JSON config of implication pairs:
//!   `[{ "parent": "KX-A", "child": "KX-B" }, ...]`.
//! - Subscribes to all referenced tickers.
//! - On every BookUpdate, checks every pair this ticker is part
//!   of and queues a `LegGroup` of two intents (buy-YES-parent +
//!   buy-NO-child) when the edge clears the threshold.
//! - Per-pair cooldown so we don't re-fire while the OMS is
//!   working an open group.
//!
//! ## What this strategy doesn't do
//!
//! - **No correlation modeling.** Captures the strict-implication
//!   case only. Markets that are merely correlated (e.g. "Yankees
//!   beat Red Sox today" + "Yankees > 90 wins") need a Bayesian
//!   constraint solver — a follow-up.
//! - **No auto-discovery.** Operator authors the pair list. A
//!   future curator could detect implication pairs from Kalshi's
//!   event taxonomy.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{
    Intent, IntentAction, LegGroup, OrderType, Tif, cid_safe_ticker,
};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use predigy_engine_core::{ActiveIntent, OpenPosition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("implication-arb");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImplicationPair {
    /// Parent ticker — must resolve YES whenever child does.
    pub parent: String,
    /// Child ticker — strict subset of parent's YES-resolution
    /// set.
    pub child: String,
    /// Optional pair-id for cooldown bookkeeping + log
    /// correlation. Defaults to `"{parent}|{child}"`.
    #[serde(default)]
    pub pair_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImplicationArbRulesFile {
    pub pairs: Vec<ImplicationPair>,
}

#[derive(Debug, Clone)]
pub struct ImplicationArbConfig {
    pub config_file: PathBuf,
    /// Min after-fee edge to fire (cents).
    pub min_edge_cents: i32,
    /// Contracts to trade per leg.
    pub size: u32,
    pub max_size_per_group: u32,
    pub max_pair_notional_cents: i64,
    pub max_cluster_notional_cents: i64,
    pub max_touch_take_fraction: f64,
    pub cooldown: Duration,
    pub config_refresh_interval: Duration,
}

impl ImplicationArbConfig {
    /// Build from env. `PREDIGY_IMPLICATION_ARB_CONFIG` required.
    ///
    /// - `PREDIGY_IMPLICATION_ARB_CONFIG` (path) — required
    /// - `PREDIGY_IMPLICATION_ARB_MIN_EDGE_CENTS` (i32, default 2)
    /// - `PREDIGY_IMPLICATION_ARB_SIZE` (u32, default 1)
    /// - `PREDIGY_IMPLICATION_ARB_COOLDOWN_MS` (u64, default 60_000)
    /// - `PREDIGY_IMPLICATION_ARB_REFRESH_MS` (u64, default 30_000)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            min_edge_cents: 2,
            size: 1,
            max_size_per_group: 5,
            max_pair_notional_cents: 500,
            max_cluster_notional_cents: 1_000,
            max_touch_take_fraction: 0.20,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_MIN_EDGE_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_edge_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_SIZE")
            && let Ok(n) = v.parse()
        {
            c.size = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_MAX_SIZE_PER_GROUP")
            && let Ok(n) = v.parse()
        {
            c.max_size_per_group = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_MAX_PAIR_NOTIONAL_CENTS")
            && let Ok(n) = v.parse()
        {
            c.max_pair_notional_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_MAX_CLUSTER_NOTIONAL_CENTS")
            && let Ok(n) = v.parse()
        {
            c.max_cluster_notional_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_MAX_TOUCH_TAKE_FRACTION")
            && let Ok(n) = v.parse::<f64>()
            && n > 0.0
            && n <= 1.0
        {
            c.max_touch_take_fraction = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_COOLDOWN_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.cooldown = Duration::from_millis(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_IMPLICATION_ARB_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.config_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn config_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_IMPLICATION_ARB_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
struct CachedPair {
    pair_id: String,
    parent: String,
    child: String,
}

#[derive(Debug, Clone, Copy)]
struct CachedTouch {
    yes_bid_cents: u8,
    yes_ask_cents: u8,
    yes_bid_qty: u32,
    yes_ask_qty: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct InventoryLeg {
    qty: i32,
    notional_cents: i64,
}

#[derive(Debug, Clone, Default)]
struct ArbInventory {
    by_ticker_side: HashMap<(String, String), InventoryLeg>,
    by_cluster_notional: HashMap<String, i64>,
}

impl ArbInventory {
    fn from_rows(positions: Vec<OpenPosition>, active_intents: Vec<ActiveIntent>) -> Self {
        let mut inv = Self::default();
        for p in positions {
            let qty = p.current_qty.max(0);
            if qty == 0 {
                continue;
            }
            inv.add(&p.ticker, &p.side, qty, p.avg_entry_cents);
        }
        for i in active_intents {
            if i.action != "buy" {
                continue;
            }
            inv.add(&i.ticker, &i.side, i.qty.max(0), i.price_cents);
        }
        inv
    }

    fn add(&mut self, ticker: &str, side: &str, qty: i32, price_cents: i32) {
        if qty <= 0 {
            return;
        }
        let notional = i64::from(qty) * i64::from(price_cents.max(0));
        let leg = self
            .by_ticker_side
            .entry((ticker.to_string(), side.to_string()))
            .or_default();
        leg.qty += qty;
        leg.notional_cents += notional;
        *self
            .by_cluster_notional
            .entry(cluster_key(ticker))
            .or_default() += notional;
    }

    fn reserve_group(&mut self, group: &LegGroup) {
        for intent in &group.intents {
            if intent.action != IntentAction::Buy {
                continue;
            }
            let side = match intent.side {
                Side::Yes => "yes",
                Side::No => "no",
            };
            self.add(
                intent.market.as_str(),
                side,
                intent.qty,
                intent.price_cents.unwrap_or(0),
            );
        }
    }

    fn leg(&self, ticker: &str, side: &str) -> InventoryLeg {
        self.by_ticker_side
            .get(&(ticker.to_string(), side.to_string()))
            .copied()
            .unwrap_or_default()
    }

    fn cluster_notional(&self, ticker: &str) -> i64 {
        self.by_cluster_notional
            .get(&cluster_key(ticker))
            .copied()
            .unwrap_or(0)
    }
}

#[derive(Debug)]
pub struct ImplicationArbStrategy {
    config: ImplicationArbConfig,
    pairs: Vec<CachedPair>,
    /// Reverse index: ticker → list of pair indexes it appears
    /// in (as either parent or child).
    ticker_to_pairs: HashMap<String, Vec<usize>>,
    touches: HashMap<String, CachedTouch>,
    last_fire_at: HashMap<String, Instant>,
    last_config_refresh: Option<Instant>,
    pending_groups: Vec<LegGroup>,
}

impl ImplicationArbStrategy {
    pub fn new(config: ImplicationArbConfig) -> Self {
        Self {
            config,
            pairs: Vec::new(),
            ticker_to_pairs: HashMap::new(),
            touches: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_config_refresh: None,
            pending_groups: Vec::new(),
        }
    }

    pub fn pair_count(&self) -> usize {
        self.pairs.len()
    }

    fn reload_pairs(&mut self) {
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.config_file.display(),
                    "implication-arb: config not present yet"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
            Err(e) => {
                warn!(error = %e, "implication-arb: config read failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let parsed: ImplicationArbRulesFile = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "implication-arb: config parse failed");
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let mut pairs = Vec::with_capacity(parsed.pairs.len());
        let mut idx: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, p) in parsed.pairs.into_iter().enumerate() {
            if p.parent == p.child {
                warn!(
                    parent = p.parent,
                    "implication-arb: degenerate self-pair; skipping"
                );
                continue;
            }
            let pair_id = p
                .pair_id
                .clone()
                .unwrap_or_else(|| format!("{}|{}", p.parent, p.child));
            idx.entry(p.parent.clone()).or_default().push(i);
            idx.entry(p.child.clone()).or_default().push(i);
            pairs.push(CachedPair {
                pair_id,
                parent: p.parent,
                child: p.child,
            });
        }
        info!(
            n_pairs = pairs.len(),
            n_tickers = idx.len(),
            "implication-arb: config loaded"
        );
        self.pairs = pairs;
        self.ticker_to_pairs = idx;
        self.last_config_refresh = Some(Instant::now());
    }

    fn record_book(&mut self, market: &MarketTicker, book: &OrderBook) {
        // Snapshot all four touch quantities. yes_bid + yes_ask
        // are derived from the YES-side bid stack and the
        // complement-of-no-bid for ask.
        let key = market.as_str().to_string();
        let yes_bid = book.best_yes_bid().map(|(p, q)| (p.cents(), q));
        let no_bid = book.best_no_bid().map(|(p, q)| (p.cents(), q));
        let yes_ask = no_bid.and_then(|(c, q)| 100u8.checked_sub(c).map(|a| (a, q)));
        match (yes_bid, yes_ask) {
            (Some((yb, yb_qty)), Some((ya, ya_qty))) if yb > 0 && ya > 0 => {
                self.touches.insert(
                    key,
                    CachedTouch {
                        yes_bid_cents: yb,
                        yes_ask_cents: ya,
                        yes_bid_qty: yb_qty,
                        yes_ask_qty: ya_qty,
                    },
                );
            }
            _ => {
                self.touches.remove(&key);
            }
        }
    }

    fn evaluate_pair(
        &mut self,
        idx: usize,
        now: Instant,
        inventory: &ArbInventory,
    ) -> Option<LegGroup> {
        let pair = &self.pairs[idx];
        if let Some(&last) = self.last_fire_at.get(&pair.pair_id)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let parent_touch = self.touches.get(&pair.parent).copied()?;
        let child_touch = self.touches.get(&pair.child).copied()?;

        // Arb condition: yes_bid_child − yes_ask_parent ≥
        // min_edge + per-leg taker fees.
        let parent_ask = parent_touch.yes_ask_cents;
        let child_bid = child_touch.yes_bid_cents;
        let parent_qty = parent_touch.yes_ask_qty;
        let child_qty = child_touch.yes_bid_qty;

        let raw_edge_cents = i32::from(child_bid) - i32::from(parent_ask);
        if raw_edge_cents < self.config.min_edge_cents {
            return None;
        }
        // Fees: buying YES_parent at parent_ask, selling YES_child
        // at child_bid (= buying NO_child at 100 − child_bid). The
        // take fee is paid on each leg's contract price.
        let probe = predigy_core::price::Qty::new(self.config.size).ok()?;
        let parent_price = predigy_core::price::Price::from_cents(parent_ask).ok()?;
        let no_child_ask = 100u8.checked_sub(child_bid)?;
        if no_child_ask == 0 {
            return None;
        }
        let no_child_price = predigy_core::price::Price::from_cents(no_child_ask).ok()?;
        let parent_fee =
            i32::try_from(predigy_core::fees::taker_fee(parent_price, probe)).unwrap_or(i32::MAX);
        let child_fee =
            i32::try_from(predigy_core::fees::taker_fee(no_child_price, probe)).unwrap_or(i32::MAX);
        let size_i32 = i32::try_from(self.config.size).unwrap_or(0);
        if size_i32 == 0 {
            return None;
        }
        let per_unit_fee = (parent_fee + child_fee) / size_i32;
        let net_edge = raw_edge_cents - per_unit_fee;
        if net_edge < self.config.min_edge_cents {
            debug!(
                pair = pair.pair_id,
                raw_edge_cents,
                per_unit_fee,
                net_edge,
                "implication-arb: edge below threshold after fees"
            );
            return None;
        }

        let parent_leg = inventory.leg(&pair.parent, "yes");
        let child_leg = inventory.leg(&pair.child, "no");
        if parent_leg.qty != child_leg.qty {
            debug!(
                pair = pair.pair_id,
                parent_qty = parent_leg.qty,
                child_qty = child_leg.qty,
                "implication-arb: unbalanced package inventory blocks scaling"
            );
            return None;
        }

        let existing_packages = parent_leg.qty.max(0) as u32;
        let edge_target = self.edge_target_packages(net_edge);
        if existing_packages >= edge_target {
            debug!(
                pair = pair.pair_id,
                existing_packages, edge_target, "implication-arb: edge-tier package cap reached"
            );
            return None;
        }

        let unit_notional =
            i64::from(parent_ask) + i64::from(no_child_ask) + i64::from(per_unit_fee);
        let pair_notional = parent_leg.notional_cents + child_leg.notional_cents;
        let pair_cap_qty = remaining_cap_qty(
            self.config.max_pair_notional_cents,
            pair_notional,
            unit_notional,
        )?;
        let cluster_notional = inventory
            .cluster_notional(&pair.parent)
            .max(inventory.cluster_notional(&pair.child));
        let cluster_cap_qty = remaining_cap_qty(
            self.config.max_cluster_notional_cents,
            cluster_notional,
            unit_notional,
        )?;
        let liquidity_qty = ((f64::from(parent_qty.min(child_qty))
            * self.config.max_touch_take_fraction)
            .floor() as u32)
            .max(1);
        let size = self
            .config
            .max_size_per_group
            .max(self.config.size)
            .min(parent_qty)
            .min(child_qty)
            .min(liquidity_qty)
            .min(edge_target.saturating_sub(existing_packages))
            .min(pair_cap_qty)
            .min(cluster_cap_qty);
        if size == 0 {
            return None;
        }
        let qty = i32::try_from(size).ok()?;
        let ts_min = chrono::Utc::now().timestamp() as u32 / 60;

        // Buy 1 YES_parent at parent_ask.
        let parent_cid = format!(
            "implication-arb:{cid_t}:p:{ask:02}:{size:04}:{ts:08x}",
            cid_t = cid_safe_ticker(&pair.parent),
            ask = parent_ask,
            size = size,
            ts = ts_min,
        );
        // Buy 1 NO_child at no_child_ask. (= selling YES_child at
        // yes_bid_child.)
        let child_cid = format!(
            "implication-arb:{cid_t}:c:{ask:02}:{size:04}:{ts:08x}",
            cid_t = cid_safe_ticker(&pair.child),
            ask = no_child_ask,
            size = size,
            ts = ts_min,
        );
        let intents = vec![
            Intent {
                client_id: parent_cid,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(&pair.parent),
                side: Side::Yes,
                action: IntentAction::Buy,
                price_cents: Some(i32::from(parent_ask)),
                qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "implication-arb {} parent_leg edge={net_edge}c",
                    pair.pair_id
                )),
            },
            Intent {
                client_id: child_cid,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(&pair.child),
                side: Side::No,
                action: IntentAction::Buy,
                price_cents: Some(i32::from(no_child_ask)),
                qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "implication-arb {} child_leg edge={net_edge}c",
                    pair.pair_id
                )),
            },
        ];
        info!(
            pair = pair.pair_id,
            parent = pair.parent,
            child = pair.child,
            parent_ask,
            child_bid,
            net_edge,
            size,
            "implication-arb: pair arb — submitting leg group"
        );
        self.last_fire_at.insert(pair.pair_id.clone(), now);
        LegGroup::new(intents)
    }

    fn edge_target_packages(&self, net_edge_cents: i32) -> u32 {
        let target = if net_edge_cents >= 10 {
            5
        } else if net_edge_cents >= 5 {
            3
        } else {
            1
        };
        target
            .max(self.config.size)
            .min(self.config.max_size_per_group.max(1))
    }
}

fn remaining_cap_qty(cap_cents: i64, used_cents: i64, unit_cents: i64) -> Option<u32> {
    if cap_cents <= 0 {
        return Some(u32::MAX);
    }
    if unit_cents <= 0 || used_cents >= cap_cents {
        return None;
    }
    u32::try_from((cap_cents - used_cents) / unit_cents).ok()
}

fn cluster_key(ticker: &str) -> String {
    ticker
        .split_once("-T")
        .map(|(prefix, _)| prefix.to_string())
        .unwrap_or_else(|| {
            ticker
                .rsplit_once('-')
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or_else(|| ticker.to_string())
        })
}

#[async_trait]
impl Strategy for ImplicationArbStrategy {
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
        let parsed: ImplicationArbRulesFile = serde_json::from_slice(&raw)?;
        let mut tickers: Vec<MarketTicker> = parsed
            .pairs
            .iter()
            .flat_map(|p| [MarketTicker::new(&p.parent), MarketTicker::new(&p.child)])
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
            self.reload_pairs();
        }
        match ev {
            Event::BookUpdate { market, book } => {
                self.record_book(market, book);
                let key = market.as_str().to_string();
                let candidates = self.ticker_to_pairs.get(&key).cloned().unwrap_or_default();
                let mut inventory = ArbInventory::from_rows(
                    state.db.open_positions(Some(STRATEGY_ID.0)).await?,
                    state.db.active_intents(Some(STRATEGY_ID.0)).await?,
                );
                let now = Instant::now();
                for idx in candidates {
                    if let Some(group) = self.evaluate_pair(idx, now, &inventory) {
                        inventory.reserve_group(&group);
                        self.pending_groups.push(group);
                    }
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn drain_pending_groups(&mut self) -> Vec<LegGroup> {
        std::mem::take(&mut self.pending_groups)
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.config_refresh_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::price::Price;

    fn book(yes_bid: Option<u8>, no_bid: Option<u8>) -> OrderBook {
        let mut b = OrderBook::new("KX-T");
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

    fn cfg(path: PathBuf) -> ImplicationArbConfig {
        ImplicationArbConfig {
            config_file: path,
            min_edge_cents: 2,
            size: 1,
            max_size_per_group: 5,
            max_pair_notional_cents: 500,
            max_cluster_notional_cents: 1_000,
            max_touch_take_fraction: 0.20,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        }
    }

    fn write_pairs(path: &std::path::Path, pairs: &serde_json::Value) {
        std::fs::write(path, serde_json::to_string(pairs).unwrap()).unwrap();
    }

    fn base_strategy(path: PathBuf) -> ImplicationArbStrategy {
        let mut c = cfg(path);
        c.cooldown = Duration::from_secs(0);
        ImplicationArbStrategy::new(c)
    }

    #[test]
    fn fires_when_child_bid_exceeds_parent_ask_plus_edge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );

        let mut s = ImplicationArbStrategy::new(cfg(path));
        s.reload_pairs();
        // Parent: yes_ask 30 (no_bid 70).
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(20), Some(70)));
        // Child: yes_bid 40 (well above parent ask of 30 → 10¢
        // raw edge before fees).
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(40), Some(50)));

        let group = s
            .evaluate_pair(0, Instant::now(), &ArbInventory::default())
            .expect("fires");
        assert_eq!(group.intents.len(), 2);
        let parent_leg = &group.intents[0];
        assert_eq!(parent_leg.market.as_str(), "KX-P");
        assert_eq!(parent_leg.side, Side::Yes);
        assert_eq!(parent_leg.action, IntentAction::Buy);
        assert_eq!(parent_leg.price_cents, Some(30));
        let child_leg = &group.intents[1];
        assert_eq!(child_leg.market.as_str(), "KX-C");
        assert_eq!(child_leg.side, Side::No);
        assert_eq!(child_leg.action, IntentAction::Buy);
        // Buying NO at 100 - yes_bid_child = 100 - 40 = 60.
        assert_eq!(child_leg.price_cents, Some(60));
    }

    #[test]
    fn skips_when_child_bid_below_parent_ask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = ImplicationArbStrategy::new(cfg(path));
        s.reload_pairs();
        // Parent yes_ask 50 (no_bid 50). Child yes_bid 30 — no
        // edge.
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(40), Some(50)));
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(30), Some(60)));
        assert!(
            s.evaluate_pair(0, Instant::now(), &ArbInventory::default())
                .is_none()
        );
    }

    #[test]
    fn skips_when_a_leg_has_no_book() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = ImplicationArbStrategy::new(cfg(path));
        s.reload_pairs();
        // Only parent has a book.
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(20), Some(70)));
        assert!(
            s.evaluate_pair(0, Instant::now(), &ArbInventory::default())
                .is_none()
        );
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = ImplicationArbStrategy::new(cfg(path));
        s.reload_pairs();
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(20), Some(70)));
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(40), Some(50)));
        let now = Instant::now();
        assert!(s.evaluate_pair(0, now, &ArbInventory::default()).is_some());
        assert!(s.evaluate_pair(0, now, &ArbInventory::default()).is_none());
    }

    #[test]
    fn degenerate_self_pair_skipped_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [
                {"parent": "KX-A", "child": "KX-A"},
                {"parent": "KX-A", "child": "KX-B"}
            ]}),
        );
        let mut s = ImplicationArbStrategy::new(cfg(path));
        s.reload_pairs();
        assert_eq!(s.pair_count(), 1);
    }

    #[test]
    fn high_edge_scales_group_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = base_strategy(path);
        s.reload_pairs();
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(10), Some(80))); // parent ask 20
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(40), Some(50))); // child bid 40

        let group = s
            .evaluate_pair(0, Instant::now(), &ArbInventory::default())
            .expect("high edge fires");

        assert!(
            group.intents[0].qty > 1,
            "high edge should scale above one package"
        );
        assert_eq!(group.intents[0].qty, group.intents[1].qty);
    }

    #[test]
    fn unbalanced_inventory_blocks_scaling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = base_strategy(path);
        s.reload_pairs();
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(10), Some(80)));
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(40), Some(50)));
        let mut inv = ArbInventory::default();
        inv.add("KX-P", "yes", 1, 20);

        assert!(s.evaluate_pair(0, Instant::now(), &inv).is_none());
    }

    #[test]
    fn existing_edge_tier_inventory_blocks_more_packages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.json");
        write_pairs(
            &path,
            &serde_json::json!({"pairs": [{"parent": "KX-P", "child": "KX-C"}]}),
        );
        let mut s = base_strategy(path);
        s.reload_pairs();
        s.record_book(&MarketTicker::new("KX-P"), &book(Some(10), Some(80)));
        s.record_book(&MarketTicker::new("KX-C"), &book(Some(40), Some(50)));
        let mut inv = ArbInventory::default();
        inv.add("KX-P", "yes", 5, 20);
        inv.add("KX-C", "no", 5, 60);

        assert!(s.evaluate_pair(0, Instant::now(), &inv).is_none());
    }
}
