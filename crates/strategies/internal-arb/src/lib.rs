// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-internal-arb` — Kalshi-internal sum-to-1
//! arbitrage (Audit S3).
//!
//! ## Mechanism
//!
//! Mutually-exclusive Kalshi event families ("Trump wins NV" +
//! "Harris wins NV" + "Other") are constrained: the YES side of
//! every leg must sum to ≤ 1.0 minus the venue's per-contract
//! fees. When the touch quotes drift such that
//!
//!   Σ yes_ask_cents + Σ fee_cents ≤ 100 - min_edge_cents
//!
//! buying one YES contract on every leg locks in a guaranteed
//! profit at settlement (exactly one leg resolves to $1, the
//! others to $0; total cost was < $1, so the spread is profit).
//!
//! ## What this strategy does
//!
//! - Reads a JSON config of event families: each family is a
//!   list of mutually-exclusive Kalshi tickers.
//! - Subscribes to all listed tickers via
//!   `subscribed_markets()`.
//! - Maintains a per-ticker book cache from `Event::BookUpdate`.
//! - On every BookUpdate for a known family ticker, recomputes
//!   the family's combined YES-buy cost. If it clears the edge
//!   threshold, queues a `LegGroup` of YES-buys for atomic
//!   submission via `Oms::submit_group` (Audit I7).
//! - Per-family cooldown prevents re-firing on every book delta
//!   while the OMS is still working an open group.
//!
//! ## What this strategy doesn't do
//!
//! - **No event-family detection.** The list of families is
//!   operator-curated (or written by a future event-family
//!   curator). No auto-discovery here.
//! - **No NO-side arb.** The mirror "buy NO on every leg" arb
//!   exists when Σ no_ask > (n - 1) + edge — left for a
//!   follow-up if the YES path proves out.
//! - **No NO-basket arb execution.** Only the proven YES-basket
//!   path executes; the NO-basket mirror remains in
//!   `InternalArbDirection` for scanner/payoff tests but is not
//!   yet wired through `evaluate_family`.
//!
//! ## Partial-fill hedge-completion (added 2026-05-16)
//!
//! IOC leg groups do leg-out in production: one leg fills, the
//! sibling's ask moves before its IOC arrives at Kalshi → the
//! sibling cancels with zero fill, leaving the filled leg as a
//! full-size unhedged directional position. The 2026-05-15 audit
//! quantified this: 0 fully-hedged groups in 36h vs 23 partials.
//!
//! `refresh_exposure` now also runs `detect_partial_fills` over
//! the open-position snapshot it already fetches. Any ticker with
//! a one-legged YES exposure on a binary family — and no NO
//! offset and no sibling-YES — is queued for a hedge. The
//! `emit_pending_hedges` pass emits a single-leg
//! `Buy NO @ best_no_ask, qty=yes_qty, IOC` for each. YES + NO =
//! 100c at settlement, so this locks in
//! `yes_entry + no_ask - 100` per pair (typically 2-6c) instead
//! of carrying a ±50c directional bet to settlement.
//!
//! Multi-leg (≥3 ticker) families are NOT hedged in this version
//! — the simple "buy NO on the filled ticker" trick only
//! flattens binary baskets. Detection logs a warning and skips.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::db::{Db, OpenPosition};
use predigy_engine_core::events::Event;
use predigy_engine_core::intent::{
    Intent, IntentAction, LegGroup, OrderType, Tif, cid_safe_ticker,
};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("internal-arb");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventFamily {
    /// Human-readable id used for cooldown bookkeeping + log
    /// correlation. Operator-chosen; convention is the Kalshi
    /// event prefix (e.g. `"NVPRES-2026"`).
    pub family_id: String,
    /// All mutually-exclusive Kalshi tickers in this family.
    /// Their YES legs must sum to ≤ 100¢ at settlement.
    pub tickers: Vec<String>,
    /// Per-leg taker fee in cents. Default to 0; in practice
    /// Kalshi fees are price-dependent — the strategy applies
    /// `predigy_core::fees::taker_fee` per leg, this is an
    /// override / safety pad.
    #[serde(default)]
    pub extra_fee_padding_cents: u32,
    /// Whether the configured family is known to be exhaustive
    /// (exactly one YES settles). `mutually_exclusive=true` from
    /// Kalshi is necessary but not sufficient for scanner
    /// promotion; candidate configs must include proof here.
    #[serde(default)]
    pub exhaustive: bool,
    /// Human/auditable provenance for the exhaustiveness claim.
    /// Existing manually curated live configs may omit this; the
    /// candidate writer added by the scanner must not.
    #[serde(default)]
    pub proof: Option<String>,
    /// Enabled arb directions. The live strategy currently
    /// executes only the proven YES-basket path; the NO-basket
    /// mirror is represented for scanner/payoff tests and future
    /// explicit promotion.
    #[serde(default = "default_directions")]
    pub directions: Vec<InternalArbDirection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalArbDirection {
    YesBasket,
    NoBasket,
}

fn default_directions() -> Vec<InternalArbDirection> {
    vec![InternalArbDirection::YesBasket]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InternalArbRulesFile {
    pub families: Vec<EventFamily>,
}

#[derive(Debug, Clone)]
pub struct InternalArbConfig {
    pub config_file: PathBuf,
    /// Min after-fee edge to fire (cents, in the aggregate).
    pub min_edge_cents: i32,
    /// Contracts to buy per leg.
    pub size: u32,
    /// Per-family cooldown so we don't re-fire while an existing
    /// group is still working at the venue.
    pub cooldown: Duration,
    /// Cadence to re-poll the config file for mtime changes.
    pub config_refresh_interval: Duration,
}

impl InternalArbConfig {
    /// Build from env. `PREDIGY_INTERNAL_ARB_CONFIG` is required.
    /// All other knobs fall back to defaults.
    ///
    /// - `PREDIGY_INTERNAL_ARB_CONFIG` (path) — required
    /// - `PREDIGY_INTERNAL_ARB_MIN_EDGE_CENTS` (i32, default 2)
    /// - `PREDIGY_INTERNAL_ARB_SIZE` (u32, default 1)
    /// - `PREDIGY_INTERNAL_ARB_COOLDOWN_MS` (u64, default 60_000)
    /// - `PREDIGY_INTERNAL_ARB_REFRESH_MS` (u64, default 30_000)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            min_edge_cents: 2,
            size: 1,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        };
        if let Ok(v) = std::env::var("PREDIGY_INTERNAL_ARB_MIN_EDGE_CENTS")
            && let Ok(n) = v.parse()
        {
            c.min_edge_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_INTERNAL_ARB_SIZE")
            && let Ok(n) = v.parse()
        {
            c.size = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_INTERNAL_ARB_COOLDOWN_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.cooldown = Duration::from_millis(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_INTERNAL_ARB_REFRESH_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.config_refresh_interval = Duration::from_millis(n);
        }
        c
    }
}

#[must_use]
pub fn config_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_INTERNAL_ARB_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
struct CachedFamily {
    family_id: String,
    tickers: Vec<String>,
    extra_fee_padding_cents: u32,
    exhaustive: bool,
    proof: Option<String>,
    directions: Vec<InternalArbDirection>,
}

/// Pure evaluator input for one family leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternalArbLegTouch {
    pub yes_ask_cents: u8,
    pub yes_ask_qty: u32,
}

/// Pure evaluation result for buying one YES on every leg of an
/// exactly-one-YES family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalArbOpportunity {
    pub n_legs: usize,
    pub total_ask_cents: i32,
    pub per_unit_taker_fee_cents: i32,
    pub per_unit_padding_cents: i32,
    pub per_unit_cost_cents: i32,
    pub edge_cents: i32,
    pub max_touch_qty: u32,
}

impl InternalArbOpportunity {
    /// If exactly one YES settles to $1, package profit is
    /// `100¢ - cost`.
    #[must_use]
    pub fn yes_basket_settlement_profit_cents(&self) -> i32 {
        100 - self.per_unit_cost_cents
    }
}

/// Pure internal-arb YES-basket evaluator shared by the live
/// strategy and the read-only scanner. This assumes the caller has
/// already established that the legs are exhaustive; candidates
/// without that proof must remain scanner-only.
#[must_use]
pub fn evaluate_internal_yes_basket(
    legs: &[InternalArbLegTouch],
    size: u32,
    extra_fee_padding_cents: u32,
    min_edge_cents: i32,
) -> Option<InternalArbOpportunity> {
    if legs.len() < 2 || size == 0 {
        return None;
    }

    let mut total_ask = 0_i32;
    let mut total_fee = 0_i32;
    let mut max_touch_qty = u32::MAX;
    for leg in legs {
        if leg.yes_ask_cents == 0 || leg.yes_ask_qty == 0 {
            return None;
        }
        total_ask += i32::from(leg.yes_ask_cents);
        total_fee += per_unit_taker_fee_cents(leg.yes_ask_cents, size)?;
        max_touch_qty = max_touch_qty.min(leg.yes_ask_qty);
    }

    let per_unit_padding = i32::try_from(extra_fee_padding_cents)
        .ok()?
        .saturating_mul(i32::try_from(legs.len()).ok()?);
    let per_unit_cost = total_ask + total_fee + per_unit_padding;
    let edge_cents = 100 - per_unit_cost;
    if edge_cents < min_edge_cents {
        return None;
    }

    Some(InternalArbOpportunity {
        n_legs: legs.len(),
        total_ask_cents: total_ask,
        per_unit_taker_fee_cents: total_fee,
        per_unit_padding_cents: per_unit_padding,
        per_unit_cost_cents: per_unit_cost,
        edge_cents,
        max_touch_qty,
    })
}

fn per_unit_taker_fee_cents(price_cents: u8, size: u32) -> Option<i32> {
    let qty = predigy_core::price::Qty::new(size).ok()?;
    let price = predigy_core::price::Price::from_cents(price_cents).ok()?;
    let total_fee = i32::try_from(predigy_core::fees::taker_fee(price, qty)).ok()?;
    let size_i32 = i32::try_from(size).ok()?;
    if size_i32 == 0 {
        return None;
    }
    Some((total_fee + size_i32 - 1) / size_i32)
}

/// Identify ticker positions left orphaned after a partial-fill
/// leg group: open YES qty with no offsetting NO position AND no
/// sibling YES in the same configured family. Binary families
/// only; 3+ leg families are skipped with a warning since their
/// hedge construction is non-trivial (the simple
/// "buy NO on the filled ticker" trick that works for 2-leg
/// families only flattens directional exposure when the family
/// is exactly two mutually-exclusive outcomes).
///
/// Pure function on the inputs so it's testable without a live
/// DB. Inputs are the strategy's cached family config and the
/// rows returned by `Db::open_positions`.
fn detect_partial_fills<'a, I>(
    families: &[CachedFamily],
    ticker_to_families: &HashMap<String, Vec<usize>>,
    positions: I,
) -> HashMap<String, PartialFillInfo>
where
    I: IntoIterator<Item = &'a OpenPosition>,
{
    let mut yes_open: HashMap<String, &OpenPosition> = HashMap::new();
    let mut no_open_tickers: HashSet<String> = HashSet::new();
    for p in positions {
        if p.current_qty == 0 {
            continue;
        }
        match p.side.as_str() {
            "yes" => {
                yes_open.insert(p.ticker.clone(), p);
            }
            "no" => {
                no_open_tickers.insert(p.ticker.clone());
            }
            _ => {}
        }
    }

    let mut partials = HashMap::new();
    for (ticker, yes_pos) in &yes_open {
        // Already hedged via a prior NO fill — nothing to do.
        if no_open_tickers.contains(ticker) {
            continue;
        }
        let Some(family_idxs) = ticker_to_families.get(ticker) else {
            continue;
        };
        for &fam_idx in family_idxs {
            let fam = &families[fam_idx];
            if fam.tickers.len() != 2 {
                warn!(
                    family = fam.family_id,
                    ticker = ticker,
                    n_tickers = fam.tickers.len(),
                    "internal-arb: partial-fill on non-binary family; hedge construction not yet supported"
                );
                continue;
            }
            let sibling_has_yes = fam
                .tickers
                .iter()
                .any(|t| t != ticker && yes_open.contains_key(t));
            if sibling_has_yes {
                // Both YES legs are open — the basket arb is
                // complete; nothing orphaned.
                continue;
            }
            partials.insert(
                ticker.clone(),
                PartialFillInfo {
                    family_id: fam.family_id.clone(),
                    yes_qty: yes_pos.current_qty,
                    yes_avg_entry_cents: yes_pos.avg_entry_cents,
                },
            );
        }
    }
    partials
}

#[derive(Debug)]
pub struct InternalArbStrategy {
    config: InternalArbConfig,
    families: Vec<CachedFamily>,
    /// Reverse index: ticker → indexes into `families`.
    ticker_to_families: HashMap<String, Vec<usize>>,
    /// Latest YES-ask in cents per ticker. Derived from
    /// `100 - best_no_bid` (the standard YES-ask shape on
    /// Kalshi). `None` when book is one-sided.
    yes_ask_cents: HashMap<String, u8>,
    /// Available qty at the YES touch per ticker (caps the leg
    /// size).
    yes_ask_qty: HashMap<String, u32>,
    last_fire_at: HashMap<String, Instant>,
    last_config_refresh: Option<Instant>,
    pending_groups: Vec<LegGroup>,
    /// 2026-05-09 anti-legging gate. Tickers where this strategy
    /// has either an open position (`current_qty != 0`) or an
    /// in-flight non-terminal intent. Rebuilt from
    /// `Db::open_positions` + `Db::active_intents` on every
    /// BookUpdate that lands on a configured ticker (matching the
    /// implication-arb inventory-refresh pattern). Any family
    /// whose tickers overlap this set is skipped in
    /// `evaluate_family`, preventing the "cheap leg lifts every
    /// minute, expensive leg never lifts, underdog YES exposure
    /// stacks" pathology that the audit found post-cap-raise.
    exposed_tickers: HashSet<String>,
    /// 2026-05-16 partial-fill hedge-completion. Latest NO-ask in
    /// cents per ticker, derived from `100 - best_yes_bid`. Used
    /// to price hedge orders when a leg group resolves with one
    /// YES leg filled and the sibling cancelled.
    no_ask_cents: HashMap<String, u8>,
    /// Available qty at the NO touch per ticker (caps the hedge
    /// size).
    no_ask_qty: HashMap<String, u32>,
    /// Tickers with a one-legged YES exposure detected on the
    /// most recent `refresh_exposure` — i.e. a partial-fill ghost
    /// that needs a NO-side hedge to lock in YES+NO = 100c.
    /// Cleared automatically once the YES position is offset by a
    /// NO-side fill (or settles). Only binary families are
    /// flagged; multi-leg partials are logged and skipped.
    partial_fill_positions: HashMap<String, PartialFillInfo>,
    /// Tickers with an in-flight `internal-arb-hedge:` intent.
    /// Rebuilt from `Db::active_intents` on every refresh so a
    /// cancelled IOC hedge can be retried on the next BookUpdate
    /// without double-submitting.
    hedge_in_flight: HashSet<String>,
}

/// One-legged exposure that needs a NO-side hedge to neutralize
/// the directional bet left behind by a partial-fill leg group.
#[derive(Debug, Clone)]
pub struct PartialFillInfo {
    pub family_id: String,
    pub yes_qty: i32,
    pub yes_avg_entry_cents: i32,
}

impl InternalArbStrategy {
    pub fn new(config: InternalArbConfig) -> Self {
        Self {
            config,
            families: Vec::new(),
            ticker_to_families: HashMap::new(),
            yes_ask_cents: HashMap::new(),
            yes_ask_qty: HashMap::new(),
            last_fire_at: HashMap::new(),
            last_config_refresh: None,
            pending_groups: Vec::new(),
            exposed_tickers: HashSet::new(),
            no_ask_cents: HashMap::new(),
            no_ask_qty: HashMap::new(),
            partial_fill_positions: HashMap::new(),
            hedge_in_flight: HashSet::new(),
        }
    }

    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    pub fn subscribed_tickers(&self) -> Vec<String> {
        self.ticker_to_families.keys().cloned().collect()
    }

    /// Rebuild exposure + partial-fill state from current open
    /// positions + in-flight intents. Two small DB reads. Called
    /// from `on_event` on every BookUpdate that lands on a
    /// configured ticker.
    async fn refresh_exposure(
        &mut self,
        db: &Db,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let positions = db.open_positions(Some(STRATEGY_ID.0)).await?;
        let active = db.active_intents(Some(STRATEGY_ID.0)).await?;

        let mut set = HashSet::new();
        for p in &positions {
            if p.current_qty != 0 {
                set.insert(p.ticker.clone());
            }
        }
        for i in &active {
            set.insert(i.ticker.clone());
        }
        if set != self.exposed_tickers {
            info!(
                n_exposed_tickers = set.len(),
                prior = self.exposed_tickers.len(),
                "internal-arb: exposure set refreshed"
            );
        }
        self.exposed_tickers = set;

        // Partial-fill detection on the same data we just fetched
        // (no extra DB round trip).
        let partials =
            detect_partial_fills(&self.families, &self.ticker_to_families, positions.iter());
        let hedge_in_flight: HashSet<String> = active
            .iter()
            .filter(|i| i.client_id.starts_with("internal-arb-hedge:"))
            .map(|i| i.ticker.clone())
            .collect();
        if partials.len() != self.partial_fill_positions.len()
            || hedge_in_flight.len() != self.hedge_in_flight.len()
        {
            info!(
                n_partial_fills = partials.len(),
                n_hedge_in_flight = hedge_in_flight.len(),
                "internal-arb: partial-fill state updated"
            );
        }
        self.partial_fill_positions = partials;
        self.hedge_in_flight = hedge_in_flight;
        Ok(())
    }

    fn reload_families(&mut self) {
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %self.config.config_file.display(),
                    "internal-arb: config not present yet"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
            Err(e) => {
                warn!(
                    path = %self.config.config_file.display(),
                    error = %e,
                    "internal-arb: config read failed"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let parsed: InternalArbRulesFile = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    path = %self.config.config_file.display(),
                    error = %e,
                    "internal-arb: config parse failed"
                );
                self.last_config_refresh = Some(Instant::now());
                return;
            }
        };
        let mut families = Vec::with_capacity(parsed.families.len());
        let mut idx = HashMap::new();
        for f in parsed.families {
            if f.tickers.len() < 2 {
                warn!(
                    family = f.family_id,
                    n_tickers = f.tickers.len(),
                    "internal-arb: family with fewer than 2 tickers; skipping"
                );
                continue;
            }
            let family_idx = families.len();
            for t in &f.tickers {
                idx.entry(t.clone())
                    .or_insert_with(Vec::new)
                    .push(family_idx);
            }
            families.push(CachedFamily {
                family_id: f.family_id,
                tickers: f.tickers,
                extra_fee_padding_cents: f.extra_fee_padding_cents,
                exhaustive: f.exhaustive,
                proof: f.proof,
                directions: f.directions,
            });
        }
        info!(
            n_families = families.len(),
            n_tickers = idx.len(),
            "internal-arb: config loaded"
        );
        self.families = families;
        self.ticker_to_families = idx;
        self.last_config_refresh = Some(Instant::now());
    }

    fn record_book(&mut self, market: &MarketTicker, book: &OrderBook) {
        // Standard Kalshi YES-ask = 100 - best_no_bid.
        let key = market.as_str().to_string();
        let yes_ask = book
            .best_no_bid()
            .and_then(|(p, _)| 100u8.checked_sub(p.cents()));
        let yes_ask_qty = book.best_no_bid().map(|(_, q)| q).unwrap_or(0);
        match yes_ask {
            Some(c) if c > 0 => {
                self.yes_ask_cents.insert(key.clone(), c);
                self.yes_ask_qty.insert(key.clone(), yes_ask_qty);
            }
            _ => {
                self.yes_ask_cents.remove(&key);
                self.yes_ask_qty.remove(&key);
            }
        }
        // NO-ask = 100 - best_yes_bid (mirror of YES-ask derivation).
        // Used to price hedge orders for partial-fill recovery.
        let no_ask = book
            .best_yes_bid()
            .and_then(|(p, _)| 100u8.checked_sub(p.cents()));
        let no_ask_qty = book.best_yes_bid().map(|(_, q)| q).unwrap_or(0);
        match no_ask {
            Some(c) if c > 0 => {
                self.no_ask_cents.insert(key.clone(), c);
                self.no_ask_qty.insert(key, no_ask_qty);
            }
            _ => {
                self.no_ask_cents.remove(&key);
                self.no_ask_qty.remove(&key);
            }
        }
    }

    /// Build hedge intents for every partial-fill ticker whose
    /// book currently shows enough NO-side liquidity to cover the
    /// open YES qty and whose hedge isn't already in flight.
    ///
    /// Each hedge is a single-leg IOC `Buy NO` on the same ticker
    /// as the orphaned YES position. YES + NO = 100c at
    /// settlement, so the strategy locks in a known
    /// `yes_entry + no_ask - 100` loss per pair (typically the
    /// bid/ask spread + the price drift since the leg-out)
    /// instead of a ±50c directional outcome.
    fn emit_pending_hedges(&mut self) -> Vec<Intent> {
        let mut out = Vec::new();
        for (ticker, info) in &self.partial_fill_positions {
            if self.hedge_in_flight.contains(ticker) {
                continue;
            }
            let Some(&no_ask) = self.no_ask_cents.get(ticker) else {
                continue;
            };
            let book_qty = self.no_ask_qty.get(ticker).copied().unwrap_or(0);
            let needed = u32::try_from(info.yes_qty.abs()).unwrap_or(0);
            if needed == 0 || book_qty < needed {
                continue;
            }

            let ts_sec = chrono::Utc::now().timestamp() as u32;
            let client_id = format!(
                "internal-arb-hedge:{cid_ticker}:{ask:02}:{size:04}:{ts:08x}",
                cid_ticker = cid_safe_ticker(ticker),
                ask = no_ask,
                size = needed,
                ts = ts_sec,
            );
            let lock_in_cents = info.yes_avg_entry_cents + i32::from(no_ask);
            info!(
                family = info.family_id,
                ticker = ticker,
                yes_qty = info.yes_qty,
                yes_avg_entry_cents = info.yes_avg_entry_cents,
                no_ask_cents = no_ask,
                lock_in_total_cents = lock_in_cents,
                "internal-arb: emitting partial-fill hedge"
            );
            out.push(Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(ticker),
                side: Side::No,
                action: IntentAction::Buy,
                price_cents: Some(i32::from(no_ask)),
                qty: i32::try_from(needed).unwrap_or(i32::MAX),
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "internal-arb-hedge[{family}]: yes_qty={yq} yes_entry={ye}c no_ask={na}c lock_in={li}c",
                    family = info.family_id,
                    yq = info.yes_qty,
                    ye = info.yes_avg_entry_cents,
                    na = no_ask,
                    li = lock_in_cents,
                )),
                post_only: false,
            });
            // Reserve in-memory so a second BookUpdate in the same
            // tick can't double-emit before the engine persists the
            // intent. The next `refresh_exposure` will overwrite
            // this from authoritative DB state.
            self.hedge_in_flight.insert(ticker.clone());
        }
        out
    }

    fn evaluate_family(&mut self, family_idx: usize, now: Instant) -> Option<LegGroup> {
        let family = &self.families[family_idx];
        if let Some(&last) = self.last_fire_at.get(&family.family_id)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        if !family.directions.contains(&InternalArbDirection::YesBasket) {
            return None;
        }
        // Anti-legging gate. If any leg of this family already has
        // open exposure or an in-flight intent, refuse to fire.
        // Otherwise we'd compound legging — the cheap leg lifts
        // again, the expensive leg fails again, and we accumulate
        // naked underdog YES contracts. See `exposed_tickers` doc
        // for the failure mode.
        if let Some(exposed) = family
            .tickers
            .iter()
            .find(|t| self.exposed_tickers.contains(*t))
        {
            debug!(
                family = family.family_id,
                exposed_ticker = exposed,
                "internal-arb: skip family (existing leg exposure)"
            );
            return None;
        }

        // Need YES-ask quotes for every leg.
        let mut leg_asks: Vec<(String, u8)> = Vec::with_capacity(family.tickers.len());
        let mut touches = Vec::with_capacity(family.tickers.len());
        for t in &family.tickers {
            let &ask_cents = self.yes_ask_cents.get(t)?;
            let qty = *self.yes_ask_qty.get(t).unwrap_or(&0);
            leg_asks.push((t.clone(), ask_cents));
            touches.push(InternalArbLegTouch {
                yes_ask_cents: ask_cents,
                yes_ask_qty: qty,
            });
        }

        let opp = evaluate_internal_yes_basket(
            &touches,
            self.config.size,
            family.extra_fee_padding_cents,
            self.config.min_edge_cents,
        )?;
        debug!(
            family = family.family_id,
            exhaustive = family.exhaustive,
            proof_present = family.proof.as_ref().is_some_and(|p| !p.trim().is_empty()),
            total_ask = opp.total_ask_cents,
            total_taker_fee = opp.per_unit_taker_fee_cents,
            edge_cents = opp.edge_cents,
            "internal-arb: yes-basket evaluated"
        );

        // Cap the size by the touch's available qty at every leg.
        let size = self.config.size.min(opp.max_touch_qty);
        if size == 0 {
            return None;
        }
        let qty = i32::try_from(size).ok()?;
        // Build LegGroup of YES-buy intents at each leg's ask.
        let mut intents = Vec::with_capacity(leg_asks.len());
        let ts_min = chrono::Utc::now().timestamp() as u32 / 60;
        for (ticker, ask_cents) in leg_asks {
            let client_id = format!(
                "internal-arb:{cid_ticker}:{ask:02}:{size:04}:{ts:08x}",
                cid_ticker = cid_safe_ticker(&ticker),
                ask = ask_cents,
                size = size,
                ts = ts_min,
            );
            intents.push(Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(&ticker),
                side: Side::Yes,
                action: IntentAction::Buy,
                price_cents: Some(i32::from(ask_cents)),
                qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "internal-arb {family}: total_ask={total_ask}c fee={fee}c edge={edge}c",
                    family = family.family_id,
                    total_ask = opp.total_ask_cents,
                    fee = opp.per_unit_taker_fee_cents,
                    edge = opp.edge_cents,
                )),
                post_only: false,
            });
        }
        info!(
            family = family.family_id,
            n_legs = intents.len(),
            edge_cents = opp.edge_cents,
            size,
            "internal-arb: arb opportunity — submitting leg group"
        );
        self.last_fire_at.insert(family.family_id.clone(), now);
        LegGroup::new(intents)
    }
}

#[async_trait]
impl Strategy for InternalArbStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        // First-load happens here so the engine knows what to
        // subscribe to. Note `&self` is read-only — we can't
        // populate self.families here. Instead we read the file
        // directly and return the ticker set; on_event then does
        // the canonical reload-and-cache.
        let raw = match std::fs::read(&self.config.config_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Box::new(e)),
        };
        let parsed: InternalArbRulesFile = serde_json::from_slice(&raw)?;
        let mut tickers: Vec<MarketTicker> = parsed
            .families
            .iter()
            .flat_map(|f| f.tickers.iter().map(|t| MarketTicker::new(t)))
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
            self.reload_families();
        }
        match ev {
            Event::BookUpdate { market, book } => {
                self.record_book(market, book);
                let key = market.as_str().to_string();
                let candidate_indexes = self
                    .ticker_to_families
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                if candidate_indexes.is_empty() {
                    return Ok(Vec::new());
                }
                // Refresh exposure on every BookUpdate that lands
                // on a configured ticker. Two small DB reads — the
                // legging pathology we're guarding against fires
                // every minute, so 30s-cached exposure data is too
                // stale (we'd legged a second naked contract before
                // the cache caught up). The implication-arb
                // strategy uses the same per-event inventory
                // refresh pattern.
                self.refresh_exposure(&state.db).await?;
                // Hedge any partial fills we observed on this
                // refresh BEFORE evaluating new arb opportunities.
                // This ensures the orphaned YES leg gets a NO-side
                // companion the next time the OMS submits, locking
                // YES+NO=100c instead of riding a ±50c directional
                // bet to settlement.
                let hedges = self.emit_pending_hedges();
                let now = Instant::now();
                for idx in candidate_indexes {
                    if let Some(group) = self.evaluate_family(idx, now) {
                        // Reserve the legs we're about to submit so
                        // a second BookUpdate in the same loop tick
                        // can't double-fire on the same family.
                        for intent in &group.intents {
                            self.exposed_tickers
                                .insert(intent.market.as_str().to_string());
                        }
                        self.pending_groups.push(group);
                    }
                }
                Ok(hedges)
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

    fn book_with_yes_ask(yes_ask_cents: u8, qty: u32) -> OrderBook {
        // YES ask = 100 - NO bid → set NO bid to 100 - yes_ask.
        let no_bid_cents = 100 - yes_ask_cents;
        let mut b = OrderBook::new("KX-A");
        let snap = predigy_book::Snapshot {
            seq: 1,
            yes_bids: Vec::new(),
            no_bids: vec![(Price::from_cents(no_bid_cents).unwrap(), qty)],
        };
        b.apply_snapshot(snap);
        b
    }

    fn cfg(path: PathBuf) -> InternalArbConfig {
        InternalArbConfig {
            config_file: path,
            min_edge_cents: 2,
            size: 1,
            cooldown: Duration::from_secs(60),
            config_refresh_interval: Duration::from_secs(30),
        }
    }

    #[test]
    fn pure_yes_basket_evaluator_has_nonnegative_payoff_for_exhaustive_family() {
        let legs = [
            InternalArbLegTouch {
                yes_ask_cents: 20,
                yes_ask_qty: 10,
            },
            InternalArbLegTouch {
                yes_ask_cents: 30,
                yes_ask_qty: 7,
            },
            InternalArbLegTouch {
                yes_ask_cents: 35,
                yes_ask_qty: 8,
            },
        ];
        let opp = evaluate_internal_yes_basket(&legs, 1, 0, 2).expect("edge clears");
        assert_eq!(opp.n_legs, 3);
        assert_eq!(opp.max_touch_qty, 7);
        assert_eq!(opp.yes_basket_settlement_profit_cents(), opp.edge_cents);
        assert!(opp.yes_basket_settlement_profit_cents() >= 0);
    }

    #[test]
    fn pure_yes_basket_evaluator_rejects_non_edge() {
        let legs = [
            InternalArbLegTouch {
                yes_ask_cents: 50,
                yes_ask_qty: 10,
            },
            InternalArbLegTouch {
                yes_ask_cents: 50,
                yes_ask_qty: 10,
            },
        ];
        assert!(evaluate_internal_yes_basket(&legs, 1, 0, 2).is_none());
    }

    #[test]
    fn config_accepts_exhaustiveness_proof_and_directions() {
        let rules: InternalArbRulesFile = serde_json::from_value(serde_json::json!({
            "families": [{
                "family_id": "PROVEN",
                "tickers": ["KX-A", "KX-B"],
                "exhaustive": true,
                "proof": "two-outcome game; one winner",
                "directions": ["yes_basket", "no_basket"]
            }]
        }))
        .unwrap();
        let fam = &rules.families[0];
        assert!(fam.exhaustive);
        assert_eq!(fam.proof.as_deref(), Some("two-outcome game; one winner"));
        assert_eq!(fam.directions.len(), 2);
        assert!(fam.directions.contains(&InternalArbDirection::YesBasket));
        assert!(fam.directions.contains(&InternalArbDirection::NoBasket));
    }

    #[test]
    fn fires_when_total_ask_clears_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [{
                "family_id": "TEST-FAM",
                "tickers": ["KX-A", "KX-B", "KX-C"]
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        assert_eq!(s.family_count(), 1);

        // Three legs, each at 30¢ ask → total 90¢ → ~10¢ edge before fees.
        s.record_book(&MarketTicker::new("KX-A"), &book_with_yes_ask(30, 100));
        s.record_book(&MarketTicker::new("KX-B"), &book_with_yes_ask(30, 100));
        s.record_book(&MarketTicker::new("KX-C"), &book_with_yes_ask(30, 100));

        let group = s.evaluate_family(0, Instant::now()).expect("fires");
        assert_eq!(group.intents.len(), 3);
        for intent in &group.intents {
            assert_eq!(intent.side, Side::Yes);
            assert_eq!(intent.action, IntentAction::Buy);
            assert_eq!(intent.qty, 1);
            assert_eq!(intent.price_cents, Some(30));
            assert!(intent.client_id.starts_with("internal-arb:"));
        }
    }

    #[test]
    fn skips_when_total_ask_too_high() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [{
                "family_id": "NO-EDGE",
                "tickers": ["KX-X", "KX-Y"]
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        // 50 + 50 = 100¢ — no edge at all.
        s.record_book(&MarketTicker::new("KX-X"), &book_with_yes_ask(50, 100));
        s.record_book(&MarketTicker::new("KX-Y"), &book_with_yes_ask(50, 100));
        assert!(s.evaluate_family(0, Instant::now()).is_none());
    }

    #[test]
    fn skips_when_a_leg_has_no_book() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [{
                "family_id": "MISSING-LEG",
                "tickers": ["KX-P", "KX-Q"]
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        s.record_book(&MarketTicker::new("KX-P"), &book_with_yes_ask(20, 100));
        // KX-Q never has a book.
        assert!(s.evaluate_family(0, Instant::now()).is_none());
    }

    #[test]
    fn skips_family_when_any_leg_has_existing_exposure() {
        // 2026-05-09 anti-legging gate. After a prior cycle's
        // cheap leg filled and expensive leg cancelled, the
        // family has unbalanced exposure. We must not re-fire
        // the same family — that's how naked underdog YES
        // contracts stack up.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [{
                "family_id": "LEGGED",
                "tickers": ["KX-CHEAP", "KX-EXPENSIVE"]
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        // Edge clears comfortably (5¢ + 30¢ = 35¢ for $1 settle).
        s.record_book(&MarketTicker::new("KX-CHEAP"), &book_with_yes_ask(5, 100));
        s.record_book(
            &MarketTicker::new("KX-EXPENSIVE"),
            &book_with_yes_ask(30, 100),
        );
        // Without exposure: fires.
        let group = s
            .evaluate_family(0, Instant::now())
            .expect("baseline fires");
        assert_eq!(group.intents.len(), 2);

        // Now mark the cheap leg as already exposed (the leftover
        // from a prior partial-fill cycle) and confirm we refuse.
        let mut s = InternalArbStrategy::new(cfg(s.config.config_file.clone()));
        s.reload_families();
        s.record_book(&MarketTicker::new("KX-CHEAP"), &book_with_yes_ask(5, 100));
        s.record_book(
            &MarketTicker::new("KX-EXPENSIVE"),
            &book_with_yes_ask(30, 100),
        );
        s.exposed_tickers.insert("KX-CHEAP".to_string());
        assert!(
            s.evaluate_family(0, Instant::now()).is_none(),
            "should refuse to re-fire family with existing leg exposure"
        );
    }

    #[test]
    fn cooldown_blocks_repeat_within_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [{
                "family_id": "COOL",
                "tickers": ["KX-1", "KX-2"]
            }]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();

        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        s.record_book(&MarketTicker::new("KX-1"), &book_with_yes_ask(30, 100));
        s.record_book(&MarketTicker::new("KX-2"), &book_with_yes_ask(30, 100));

        let now = Instant::now();
        assert!(s.evaluate_family(0, now).is_some());
        // Second eval at the same instant — cooldown blocks.
        assert!(s.evaluate_family(0, now).is_none());
    }

    #[test]
    fn family_with_only_one_ticker_skipped_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [
                { "family_id": "VALID", "tickers": ["KX-A", "KX-B"] },
                { "family_id": "BAD",   "tickers": ["KX-Z"] }
            ]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        assert_eq!(s.family_count(), 1);
        assert_eq!(s.families[0].family_id, "VALID");
    }

    #[test]
    fn ticker_index_uses_post_skip_family_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [
                { "family_id": "BAD",   "tickers": ["KX-Z"] },
                { "family_id": "VALID", "tickers": ["KX-A", "KX-B"] }
            ]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        assert_eq!(s.family_count(), 1);
        assert_eq!(s.ticker_to_families.get("KX-A"), Some(&vec![0]));
    }

    #[test]
    fn ticker_to_families_index_built_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        let rules = serde_json::json!({
            "families": [
                { "family_id": "FAM1", "tickers": ["KX-A", "KX-B"] },
                { "family_id": "FAM2", "tickers": ["KX-A", "KX-C"] }
            ]
        });
        std::fs::write(&path, serde_json::to_string(&rules).unwrap()).unwrap();
        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        // KX-A appears in both families.
        let kx_a = s.ticker_to_families.get("KX-A").unwrap();
        assert_eq!(kx_a.len(), 2);
        let kx_b = s.ticker_to_families.get("KX-B").unwrap();
        assert_eq!(kx_b.len(), 1);
    }

    // ─── Partial-fill hedge-completion tests ────────────────

    fn mk_open_pos(ticker: &str, side: &str, qty: i32, entry: i32) -> OpenPosition {
        OpenPosition {
            strategy: STRATEGY_ID.0.to_string(),
            ticker: ticker.to_string(),
            side: side.to_string(),
            current_qty: qty,
            avg_entry_cents: entry,
            realized_pnl_cents: 0,
            fees_paid_cents: 0,
            opened_at: chrono::Utc::now(),
            last_fill_at: None,
        }
    }

    fn loaded_strategy(families: serde_json::Value) -> InternalArbStrategy {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("families.json");
        std::fs::write(&path, serde_json::to_string(&families).unwrap()).unwrap();
        let mut s = InternalArbStrategy::new(cfg(path));
        s.reload_families();
        // Keep tempdir alive for the duration of the strategy by
        // leaking it — the strategy holds the path but never
        // re-reads it after the initial load in these tests.
        std::mem::forget(dir);
        s
    }

    #[test]
    fn detect_partial_fill_flags_one_legged_yes() {
        let s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        let positions = vec![mk_open_pos("KX-A", "yes", 1, 46)];
        let partials = detect_partial_fills(&s.families, &s.ticker_to_families, positions.iter());
        assert_eq!(partials.len(), 1);
        let info = partials.get("KX-A").expect("partial flagged");
        assert_eq!(info.family_id, "GAME");
        assert_eq!(info.yes_qty, 1);
        assert_eq!(info.yes_avg_entry_cents, 46);
    }

    #[test]
    fn detect_partial_fill_ignores_fully_hedged_basket() {
        // Both YES legs filled — the basket arb is complete, no orphan.
        let s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        let positions = vec![
            mk_open_pos("KX-A", "yes", 1, 46),
            mk_open_pos("KX-B", "yes", 1, 51),
        ];
        let partials = detect_partial_fills(&s.families, &s.ticker_to_families, positions.iter());
        assert!(partials.is_empty());
    }

    #[test]
    fn detect_partial_fill_ignores_ticker_with_existing_no_hedge() {
        // YES filled, NO already filled on the same ticker — the
        // hedge already completed; nothing to do.
        let s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        let positions = vec![
            mk_open_pos("KX-A", "yes", 1, 46),
            mk_open_pos("KX-A", "no", 1, 56),
        ];
        let partials = detect_partial_fills(&s.families, &s.ticker_to_families, positions.iter());
        assert!(partials.is_empty());
    }

    #[test]
    fn detect_partial_fill_skips_non_binary_family() {
        // 3-team family — hedge construction is non-trivial for
        // multi-leg baskets; the detector skips with a warning.
        let s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "TRIPLE", "tickers": ["KX-A", "KX-B", "KX-C"] }
            ]
        }));
        let positions = vec![mk_open_pos("KX-A", "yes", 1, 33)];
        let partials = detect_partial_fills(&s.families, &s.ticker_to_families, positions.iter());
        assert!(partials.is_empty());
    }

    #[test]
    fn detect_partial_fill_ignores_unconfigured_ticker() {
        // Open position on a ticker that isn't in any configured
        // family — outside this strategy's scope.
        let s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        let positions = vec![mk_open_pos("KX-Z", "yes", 1, 50)];
        let partials = detect_partial_fills(&s.families, &s.ticker_to_families, positions.iter());
        assert!(partials.is_empty());
    }

    fn book_with_both_sides(yes_ask_cents: u8, no_ask_cents: u8, qty: u32) -> OrderBook {
        // YES ask = 100 - NO bid → NO bid at (100 - yes_ask).
        // NO  ask = 100 - YES bid → YES bid at (100 - no_ask).
        let mut b = OrderBook::new("KX-A");
        let snap = predigy_book::Snapshot {
            seq: 1,
            yes_bids: vec![(Price::from_cents(100 - no_ask_cents).unwrap(), qty)],
            no_bids: vec![(Price::from_cents(100 - yes_ask_cents).unwrap(), qty)],
        };
        b.apply_snapshot(snap);
        b
    }

    #[test]
    fn emit_pending_hedges_builds_no_buy_for_partial_fill() {
        let mut s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        // Book has both sides quoted: YES ask 44, NO ask 58.
        s.record_book(&MarketTicker::new("KX-A"), &book_with_both_sides(44, 58, 5));
        s.partial_fill_positions.insert(
            "KX-A".to_string(),
            PartialFillInfo {
                family_id: "GAME".to_string(),
                yes_qty: 1,
                yes_avg_entry_cents: 46,
            },
        );
        let intents = s.emit_pending_hedges();
        assert_eq!(intents.len(), 1);
        let hedge = &intents[0];
        assert_eq!(hedge.market.as_str(), "KX-A");
        assert_eq!(hedge.side, Side::No);
        assert_eq!(hedge.action, IntentAction::Buy);
        assert_eq!(hedge.price_cents, Some(58));
        assert_eq!(hedge.qty, 1);
        assert_eq!(hedge.tif, Tif::Ioc);
        assert!(hedge.client_id.starts_with("internal-arb-hedge:"));
        // Defensive in-memory reservation set so a second emit in
        // the same tick is a no-op.
        assert!(s.hedge_in_flight.contains("KX-A"));
        assert!(s.emit_pending_hedges().is_empty());
    }

    #[test]
    fn emit_pending_hedges_skips_when_hedge_already_in_flight() {
        let mut s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        s.record_book(&MarketTicker::new("KX-A"), &book_with_both_sides(44, 58, 5));
        s.partial_fill_positions.insert(
            "KX-A".to_string(),
            PartialFillInfo {
                family_id: "GAME".to_string(),
                yes_qty: 1,
                yes_avg_entry_cents: 46,
            },
        );
        s.hedge_in_flight.insert("KX-A".to_string());
        assert!(s.emit_pending_hedges().is_empty());
    }

    #[test]
    fn emit_pending_hedges_skips_when_no_side_lacks_liquidity() {
        let mut s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        // NO-side qty is 0 → not enough to cover the YES qty=3 hedge.
        s.record_book(&MarketTicker::new("KX-A"), &book_with_both_sides(44, 58, 0));
        s.partial_fill_positions.insert(
            "KX-A".to_string(),
            PartialFillInfo {
                family_id: "GAME".to_string(),
                yes_qty: 3,
                yes_avg_entry_cents: 46,
            },
        );
        assert!(s.emit_pending_hedges().is_empty());
        // Must NOT pre-reserve when we couldn't actually emit.
        assert!(!s.hedge_in_flight.contains("KX-A"));
    }

    #[test]
    fn record_book_tracks_both_yes_and_no_asks() {
        let mut s = loaded_strategy(serde_json::json!({
            "families": [
                { "family_id": "GAME", "tickers": ["KX-A", "KX-B"] }
            ]
        }));
        s.record_book(&MarketTicker::new("KX-A"), &book_with_both_sides(42, 60, 7));
        assert_eq!(s.yes_ask_cents.get("KX-A"), Some(&42));
        assert_eq!(s.yes_ask_qty.get("KX-A"), Some(&7));
        assert_eq!(s.no_ask_cents.get("KX-A"), Some(&60));
        assert_eq!(s.no_ask_qty.get("KX-A"), Some(&7));
    }
}
