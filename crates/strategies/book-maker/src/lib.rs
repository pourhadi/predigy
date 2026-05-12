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

    // ── Adaptive same-day exit logic (2026-05-10) ──
    /// Lock-in profit: when an open position's unrealized P&L
    /// per contract (mark − entry for long, entry − mark for
    /// short) reaches this many cents, emit an IOC flatten at
    /// the touch instead of waiting for our own maker quote to
    /// be lifted. Caps the upside-to-downside ratio against
    /// adverse settlement.
    pub profit_take_cents: i32,
    /// Stop-loss: when unrealized P&L per contract reaches
    /// −stop_loss_cents, emit an IOC flatten. Caps the
    /// per-position loss before settlement dominates it.
    pub stop_loss_cents: i32,
    /// Halt new quotes when time-to-settle drops below this
    /// window (seconds). Existing acked orders stay alive but
    /// we don't repost them on the next re-quote cycle.
    pub pre_settle_halt_secs: i64,
    /// Force-flatten when time-to-settle drops below this
    /// window (seconds). Emits IOC sells/buys at the current
    /// touch to dump any held inventory before the game outcome
    /// dominates the P&L.
    pub pre_settle_flatten_secs: i64,
    /// Within this many hours of settle, the inventory skew
    /// per contract DOUBLES — encouraging earlier flattening
    /// as the settlement-risk-discount kicks in.
    pub skew_escalation_t_hours: f64,
    /// For sport tickers parsed to a START time, the strategy
    /// estimates settlement at start + this many hours (typical
    /// game length). Used only when `markets.close_time` is
    /// not populated in the DB (which is the common case at
    /// the moment for sport markets).
    pub estimated_game_duration_hours: f64,
    /// Per-ticker quote-refresh cooldown. Higher = quotes rest
    /// longer at venue (more fill probability) but track moving
    /// books less aggressively. Suspended inside the flatten
    /// window where every-tick aggression matters.
    pub quote_refresh_cooldown: Duration,
    /// If we emit this many consecutive IOC exits for the same
    /// unchanged position and the position does not change, pause
    /// exit attempts for that ticker. This prevents stale-touch /
    /// no-liquidity loops from spamming cancelled IOCs forever.
    pub exit_failure_threshold: u32,
    /// Cooldown applied after `exit_failure_threshold` unchanged
    /// exit attempts.
    pub exit_failure_cooldown: Duration,
}

impl BookMakerConfig {
    /// Build from env.
    /// - `PREDIGY_BOOK_MAKER_CONFIG` (path) — required (file
    ///   existence is what gates registration in the engine).
    /// - `PREDIGY_BOOK_MAKER_SKEW_CENTS_PER_CONTRACT` (i32,
    ///   default 1)
    /// - `PREDIGY_BOOK_MAKER_REFRESH_MS` (u64, default 30_000)
    /// - `PREDIGY_BOOK_MAKER_PROFIT_TAKE_CENTS` (i32, default 5)
    /// - `PREDIGY_BOOK_MAKER_STOP_LOSS_CENTS` (i32, default 8)
    /// - `PREDIGY_BOOK_MAKER_PRE_SETTLE_HALT_SECS` (i64, default 1800)
    /// - `PREDIGY_BOOK_MAKER_PRE_SETTLE_FLATTEN_SECS` (i64, default 600)
    /// - `PREDIGY_BOOK_MAKER_SKEW_ESCALATION_HOURS` (f64, default 2.0)
    /// - `PREDIGY_BOOK_MAKER_GAME_DURATION_HOURS` (f64, default 3.5)
    /// - `PREDIGY_BOOK_MAKER_QUOTE_REFRESH_SECS` (u64, default 10)
    /// - `PREDIGY_BOOK_MAKER_EXIT_FAILURE_THRESHOLD` (u32, default 5)
    /// - `PREDIGY_BOOK_MAKER_EXIT_FAILURE_COOLDOWN_SECS` (u64, default 600)
    #[must_use]
    pub fn from_env(config_file: PathBuf) -> Self {
        let mut c = Self {
            config_file,
            inventory_skew_cents_per_contract: 1,
            config_refresh_interval: Duration::from_secs(30),
            profit_take_cents: 5,
            stop_loss_cents: 8,
            pre_settle_halt_secs: 1800,
            pre_settle_flatten_secs: 600,
            skew_escalation_t_hours: 2.0,
            estimated_game_duration_hours: 3.5,
            quote_refresh_cooldown: Duration::from_secs(10),
            exit_failure_threshold: 5,
            exit_failure_cooldown: Duration::from_secs(600),
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
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_PROFIT_TAKE_CENTS")
            && let Ok(n) = v.parse()
        {
            c.profit_take_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_STOP_LOSS_CENTS")
            && let Ok(n) = v.parse()
        {
            c.stop_loss_cents = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_PRE_SETTLE_HALT_SECS")
            && let Ok(n) = v.parse()
        {
            c.pre_settle_halt_secs = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_PRE_SETTLE_FLATTEN_SECS")
            && let Ok(n) = v.parse()
        {
            c.pre_settle_flatten_secs = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_SKEW_ESCALATION_HOURS")
            && let Ok(n) = v.parse()
        {
            c.skew_escalation_t_hours = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_GAME_DURATION_HOURS")
            && let Ok(n) = v.parse()
        {
            c.estimated_game_duration_hours = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_QUOTE_REFRESH_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.quote_refresh_cooldown = Duration::from_secs(n);
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_EXIT_FAILURE_THRESHOLD")
            && let Ok(n) = v.parse::<u32>()
        {
            c.exit_failure_threshold = n;
        }
        if let Ok(v) = std::env::var("PREDIGY_BOOK_MAKER_EXIT_FAILURE_COOLDOWN_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            c.exit_failure_cooldown = Duration::from_secs(n);
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

/// Parse the estimated settlement time (unix seconds) from a
/// sport ticker that embeds the start time + date. Only MLB
/// (KXMLBGAME) tickers carry an explicit HHMM in the symbol;
/// for other formats we return None and the strategy operates
/// without time-aware exits on those.
///
/// Format: `KXMLBGAME-{YY}{MMM}{DD}{HHMM}{matchup}-{team}`
/// e.g.   `KXMLBGAME-26MAY111810LAACLE-LAA`
///                       ^^^^^^^^^^^^
///                       26 MAY 11 18:10  → May 11 2026 18:10 ET
///
/// Adds `estimated_game_duration_hours` to get a rough
/// settlement estimate. ET → UTC conversion uses a fixed −4
/// offset (EDT) — close enough for the strategy's purposes
/// (the deltas we care about are minutes-to-settle order of
/// magnitude, not exact UTC).
#[must_use]
pub fn parse_settlement_unix_from_ticker(
    ticker: &str,
    estimated_game_duration_hours: f64,
) -> Option<i64> {
    let prefix = "KXMLBGAME-";
    let rest = ticker.strip_prefix(prefix)?;
    // Need at least 11 chars: YY(2) + MMM(3) + DD(2) + HHMM(4).
    if rest.len() < 11 {
        return None;
    }
    let yy: i32 = rest[0..2].parse().ok()?;
    let mmm = &rest[2..5];
    let dd: u32 = rest[5..7].parse().ok()?;
    let hhmm: u32 = rest[7..11].parse().ok()?;
    let month: u32 = match mmm {
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
    let hour = hhmm / 100;
    let minute = hhmm % 100;
    if hour > 23 || minute > 59 {
        return None;
    }
    let year = 2000 + yy;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, dd)?;
    let naive = date.and_hms_opt(hour, minute, 0)?;
    // ET start → UTC. EDT is UTC-4. (We don't bother detecting
    // standard vs daylight; the strategy is robust to ±1h.)
    let start_utc = naive.and_utc().timestamp() + 4 * 3600;
    let settle_utc = start_utc + (estimated_game_duration_hours * 3600.0) as i64;
    Some(settle_utc)
}

/// One open position for this strategy, with the price basis
/// used for unrealized-P&L computation.
#[derive(Debug, Clone)]
struct OpenLeg {
    /// Signed contract count. + long YES, − long NO (which we
    /// represent as a negative YES exposure).
    signed_qty: i32,
    /// Average entry price in cents. For a long YES this is what
    /// we paid; for a long NO this is what we paid for NO (so
    /// our "YES-equivalent" cost basis is `100 − avg_entry`).
    avg_entry_cents: i32,
    /// Side string ("yes" | "no") — needed because mark
    /// computation differs.
    side: String,
}

type ExitPositionSignature = Vec<(String, i32, i32)>;

fn exit_position_signature(legs: &[OpenLeg]) -> ExitPositionSignature {
    let mut sig: ExitPositionSignature = legs
        .iter()
        .map(|l| (l.side.clone(), l.signed_qty, l.avg_entry_cents))
        .collect();
    sig.sort();
    sig
}

#[derive(Debug, Clone)]
struct ExitAttemptState {
    signature: ExitPositionSignature,
    consecutive_emits: u32,
    suppressed_until: Option<Instant>,
}

fn should_suppress_exit_attempt(
    state: &mut ExitAttemptState,
    signature: &ExitPositionSignature,
    now: Instant,
) -> bool {
    if state.signature != *signature {
        state.signature = signature.clone();
        state.consecutive_emits = 0;
        state.suppressed_until = None;
        return false;
    }
    if let Some(until) = state.suppressed_until {
        if until > now {
            return true;
        }
        // Keep the counter above threshold after the cooldown
        // expires. That turns the next unchanged-position exit
        // into a single liquidity probe; if it still does not
        // change the position, we immediately suppress again
        // instead of allowing another full threshold-sized burst.
        state.suppressed_until = None;
    }
    false
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
    /// Full open-leg cache (per ticker, per side) — used for
    /// per-position unrealized-P&L computation. Distinct from
    /// `inventory` (net qty) because NO positions don't combine
    /// trivially with YES positions for mark/PnL math.
    open_legs: HashMap<String, Vec<OpenLeg>>,
    /// Estimated settlement time per ticker (unix seconds). For
    /// MLB tickers we parse it directly from the ticker; for
    /// others we leave it None and run without time-aware
    /// behavior. Populated lazily.
    settle_unix: HashMap<String, Option<i64>>,
    /// Per-ticker cooldown for exit emissions. Without this the
    /// strategy emits an IOC exit on every BookUpdate that hits
    /// a ticker with an open underwater position — generating
    /// thousands of redundant emissions per minute. The OMS
    /// dedupes on cid (price+minute embedded), so most never
    /// reach the venue, but the wasted work + log spam is
    /// significant. Cooldown caps emissions to one per N
    /// seconds per ticker.
    last_exit_at: HashMap<String, Instant>,
    /// Per-ticker cooldown for quote re-emissions. Same idea as
    /// `last_exit_at` but for the maker's bid/ask churn. With
    /// 70+ live markets, sub-cent book micro-moves trigger
    /// constant cancel+repost cycles that blow through the OMS
    /// in-flight cap. This cooldown bounds re-quote frequency
    /// to one per N seconds per ticker.
    last_quote_at: HashMap<String, Instant>,
    /// Repeated IOC exit attempts against a stale local touch can
    /// return venue cancels forever when the real venue book has no
    /// executable liquidity. Track unchanged-position exit emissions
    /// and pause after a small burst.
    exit_attempts: HashMap<String, ExitAttemptState>,
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
            open_legs: HashMap::new(),
            settle_unix: HashMap::new(),
            last_exit_at: HashMap::new(),
            last_quote_at: HashMap::new(),
            exit_attempts: HashMap::new(),
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

    /// Estimated settlement unix time for a ticker. Tries DB
    /// `markets.close_time` first (populated by other components
    /// when available), then falls back to parsing the ticker
    /// string for sport markets that embed start time. Caches
    /// the result so we only do this work once per ticker.
    fn settle_unix_for(&mut self, ticker: &str) -> Option<i64> {
        if let Some(cached) = self.settle_unix.get(ticker) {
            return *cached;
        }
        let parsed =
            parse_settlement_unix_from_ticker(ticker, self.config.estimated_game_duration_hours);
        self.settle_unix.insert(ticker.to_string(), parsed);
        parsed
    }

    /// Inventory skew per contract — escalates as we approach
    /// settle. Within `skew_escalation_t_hours` of settle, the
    /// skew doubles (encouraging earlier flattening). When
    /// `secs_to_settle` is None (no estimate), uses the base
    /// configured skew unchanged.
    fn effective_skew_for(&self, secs_to_settle: Option<i64>) -> i32 {
        let base = self.config.inventory_skew_cents_per_contract;
        match secs_to_settle {
            Some(secs) if secs > 0 => {
                let hours = secs as f64 / 3600.0;
                if hours < self.config.skew_escalation_t_hours {
                    base.saturating_mul(2)
                } else {
                    base
                }
            }
            _ => base,
        }
    }

    /// Per-position exit decisions. For each open leg on this
    /// ticker, compute unrealized P&L and emit an IOC flatten
    /// if it crosses a threshold (profit-take, stop-loss, or
    /// pre-settle aggressive flatten).
    fn evaluate_exits(&mut self, ticker: &str, touch: &CachedTouch, secs_to_settle: Option<i64>) {
        let legs = match self.open_legs.get(ticker) {
            Some(legs) if !legs.is_empty() => legs.clone(),
            _ => return,
        };
        let signature = exit_position_signature(&legs);
        let now = Instant::now();
        let state = self
            .exit_attempts
            .entry(ticker.to_string())
            .or_insert_with(|| ExitAttemptState {
                signature: signature.clone(),
                consecutive_emits: 0,
                suppressed_until: None,
            });
        if should_suppress_exit_attempt(state, &signature, now) {
            return;
        }
        let in_flatten_window = secs_to_settle
            .map(|s| s >= 0 && s < self.config.pre_settle_flatten_secs)
            .unwrap_or(false);

        // Per-ticker cooldown. The OMS dedupes on cid (price +
        // minute embedded) so most repeat emissions never reach
        // the venue — but the strategy work + log spam is
        // wasteful. 3s default; suspended inside the flatten
        // window where we want every-tick aggression.
        let cooldown = if in_flatten_window {
            Duration::ZERO
        } else {
            Duration::from_secs(3)
        };
        if let Some(&last) = self.last_exit_at.get(ticker)
            && last.elapsed() < cooldown
        {
            return;
        }
        // Track that we considered an exit; even if no threshold
        // fires this tick we still want the cooldown applied
        // (otherwise we'd consider every BookUpdate forever).
        let mut emitted_any = false;

        for leg in legs {
            if leg.signed_qty == 0 {
                continue;
            }
            // Mark = price we'd unwind at on a market sell.
            // For long YES: sell into yes_bid.
            // For long NO (signed_qty > 0 with side="no"): we'd
            // sell NO → buy YES at ask → effective "mark" is
            // 100 − yes_ask (the NO bid we'd hit).
            // We unwind via IOC at the touch.
            let (mark, exit_action, exit_side, abs_qty) = if leg.side == "yes" {
                let abs = leg.signed_qty.unsigned_abs();
                if leg.signed_qty > 0 {
                    // Long YES — sell at yes_bid.
                    (
                        i32::from(touch.yes_bid_cents),
                        IntentAction::Sell,
                        Side::Yes,
                        abs,
                    )
                } else {
                    // Short YES — buy back at yes_ask.
                    (
                        i32::from(touch.yes_ask_cents),
                        IntentAction::Buy,
                        Side::Yes,
                        abs,
                    )
                }
            } else if leg.side == "no" && leg.signed_qty > 0 {
                // Long NO — sell at no_bid. no_bid = 100 - yes_ask.
                let no_bid = 100i32.saturating_sub(i32::from(touch.yes_ask_cents));
                (
                    no_bid,
                    IntentAction::Sell,
                    Side::No,
                    leg.signed_qty.unsigned_abs(),
                )
            } else {
                continue;
            };

            // Unrealized P&L per contract.
            let pnl_per = mark.saturating_sub(leg.avg_entry_cents);
            let profit_take = pnl_per >= self.config.profit_take_cents;
            let stop_loss = pnl_per <= -self.config.stop_loss_cents;

            let reason_tag = if in_flatten_window {
                "pre-settle"
            } else if profit_take {
                "tp"
            } else if stop_loss {
                "sl"
            } else {
                continue;
            };

            // IOC flatten at the current mark.
            let exit_price = i32::from(mark.clamp(1, 99) as u8);
            let minute = (chrono::Utc::now().timestamp() / 60) as u32;
            let cid = format!(
                "book-maker:{cid_t}:X:{tag}:{p:02}:{minute:08x}",
                cid_t = cid_safe_ticker(ticker),
                tag = reason_tag,
                p = exit_price,
            );
            let qty = match i32::try_from(abs_qty) {
                Ok(q) => q,
                Err(_) => continue,
            };
            if qty == 0 {
                continue;
            }

            info!(
                ticker,
                reason = reason_tag,
                signed_qty = leg.signed_qty,
                avg_entry = leg.avg_entry_cents,
                mark,
                pnl_per,
                secs_to_settle = ?secs_to_settle,
                "book-maker: emit exit"
            );

            self.pending_intents.push(Intent {
                client_id: cid,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(ticker),
                side: exit_side,
                action: exit_action,
                price_cents: Some(exit_price),
                qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "book-maker exit {tag}: entry={ec}c mark={mark}c pnl={pnl}c qty={qty}",
                    tag = reason_tag,
                    ec = leg.avg_entry_cents,
                    pnl = pnl_per,
                )),
                post_only: false,
            });
            emitted_any = true;
        }
        // Cooldown timestamp records "we evaluated this ticker
        // for exit," not just "we emitted." That way a ticker
        // whose position isn't threshold-tripping yet doesn't
        // get re-evaluated every BookUpdate either.
        if emitted_any {
            let state = self
                .exit_attempts
                .entry(ticker.to_string())
                .or_insert_with(|| ExitAttemptState {
                    signature,
                    consecutive_emits: 0,
                    suppressed_until: None,
                });
            state.consecutive_emits = state.consecutive_emits.saturating_add(1);
            if self.config.exit_failure_threshold > 0
                && state.consecutive_emits >= self.config.exit_failure_threshold
            {
                state.suppressed_until = Some(Instant::now() + self.config.exit_failure_cooldown);
                warn!(
                    ticker,
                    consecutive_exit_emits = state.consecutive_emits,
                    cooldown_secs = self.config.exit_failure_cooldown.as_secs(),
                    "book-maker: suppressing exits after repeated unchanged-position attempts"
                );
            }
        }
        self.last_exit_at.insert(ticker.to_string(), Instant::now());
    }

    /// For one configured market, emit any cancels for stale
    /// quotes and any new intents for missing or repriced quotes.
    fn evaluate_market(&mut self, market_idx: usize) {
        // Snapshot the config we need up-front so the mutable
        // borrows below (settle_unix_for, evaluate_exits, etc.)
        // don't fight an immutable borrow of self.markets[].
        let (ticker, quote_size, max_inventory, min_spread_cents) = {
            let m = &self.markets[market_idx];
            (
                m.ticker.clone(),
                m.quote_size,
                m.max_inventory_contracts,
                m.min_spread_cents,
            )
        };
        let touch = match self.touches.get(&ticker).copied() {
            Some(t) if t.yes_bid_cents > 0 && t.yes_ask_cents > 0 => t,
            _ => return,
        };

        // ── 1. P&L exits + time-to-settle flatten ──
        // These fire BEFORE we evaluate new quotes. If the strategy
        // is going to dump inventory this tick, we don't want to
        // re-post quotes that immediately get cancel-replaced.
        let now_unix = chrono::Utc::now().timestamp();
        let settle_unix = self.settle_unix_for(&ticker);
        let secs_to_settle = settle_unix.map(|t| t.saturating_sub(now_unix));
        self.evaluate_exits(&ticker, &touch, secs_to_settle);

        // ── 2. Quote suppression near settle ──
        // Inside the halt window, stop posting NEW quotes. The
        // exit logic above is the only thing that emits intents
        // for this ticker in this window.
        if let Some(secs) = secs_to_settle
            && secs >= 0
            && secs < self.config.pre_settle_halt_secs
        {
            debug!(
                ticker,
                secs_to_settle = secs,
                "book-maker: in halt window; suppressing new quotes"
            );
            self.cancel_active_for_ticker(&ticker);
            return;
        }

        // ── 3. Per-ticker quote cooldown ──
        // Sub-second book micro-moves trigger constant
        // cancel+repost churn across 70+ markets, blowing
        // through the OMS in-flight cap. Limit one re-quote
        // cycle per ticker per `quote_refresh_cooldown_secs`.
        // The longer the cooldown, the longer our quotes rest
        // at venue (more chance to be lifted by a taker) at
        // the cost of staler prices. Default 10s — captures
        // most fill opportunities while bounding churn.
        // Inside the flatten window we skip the cooldown so
        // urgent flatten attempts go through.
        let in_flatten = secs_to_settle
            .map(|s| s >= 0 && s < self.config.pre_settle_flatten_secs)
            .unwrap_or(false);
        if !in_flatten
            && let Some(&last) = self.last_quote_at.get(&ticker)
            && last.elapsed() < self.config.quote_refresh_cooldown
        {
            return;
        }
        self.last_quote_at.insert(ticker.clone(), Instant::now());

        // ── 4. Normal quote computation with dynamic skew ──
        let inv = self.inventory.get(&ticker).copied().unwrap_or(0);
        let effective_skew = self.effective_skew_for(secs_to_settle);
        let Some((desired_bid, desired_ask)) = compute_desired_quotes(
            touch.yes_bid_cents,
            touch.yes_ask_cents,
            inv,
            effective_skew,
            min_spread_cents,
        ) else {
            // Book too tight — cancel anything we have here.
            self.cancel_active_for_ticker(&ticker);
            return;
        };

        // Inventory cap: skip the side that would breach.
        let cap = max_inventory;
        let bid_allowed = inv + quote_size <= cap;
        let ask_allowed = -(inv - quote_size) <= cap;

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
                qty: quote_size,
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

    /// Refresh `inventory`, `open_legs`, and `active_orders`
    /// from the DB. Two queries, cheap. Called at most once per
    /// BookUpdate that lands on a configured ticker.
    async fn refresh_state_from_db(
        &mut self,
        db: &predigy_engine_core::Db,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Positions: map ticker → net YES-equivalent qty, AND
        // ticker → list of per-side legs (for P&L exits).
        let mut inv = HashMap::new();
        let mut legs: HashMap<String, Vec<OpenLeg>> = HashMap::new();
        for p in db.open_positions(Some(STRATEGY_ID.0)).await? {
            if p.current_qty == 0 {
                continue;
            }
            // YES side: signed qty as-is. NO side: -qty (a long-NO
            // = short-YES exposure). The maker's quotes are on
            // YES, so YES-equivalent inventory drives skew.
            let signed = match p.side.as_str() {
                "yes" => p.current_qty,
                "no" => -p.current_qty,
                _ => 0,
            };
            *inv.entry(p.ticker.clone()).or_insert(0) += signed;
            legs.entry(p.ticker).or_default().push(OpenLeg {
                signed_qty: p.current_qty,
                avg_entry_cents: p.avg_entry_cents,
                side: p.side,
            });
        }
        self.inventory = inv;
        self.open_legs = legs;
        let open_tickers = self
            .open_legs
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        self.exit_attempts
            .retain(|ticker, _| open_tickers.contains(ticker));

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
    fn parses_mlb_ticker_settlement_time() {
        // KXMLBGAME-26MAY111810LAACLE-LAA → May 11 2026 18:10 ET
        // 18:10 ET + 4h offset = 22:10 UTC start. Plus 3.5h game
        // duration = 01:40 UTC on May 12.
        let t = parse_settlement_unix_from_ticker("KXMLBGAME-26MAY111810LAACLE-LAA", 3.5).unwrap();
        // Sanity: should be within a sensible 2026 range.
        assert!(t > 1_777_000_000); // ~Mid Apr 2026
        assert!(t < 1_790_000_000); // ~Sep 2026

        // Different start time → different settle.
        let t2 = parse_settlement_unix_from_ticker("KXMLBGAME-26MAY112005AZTEX-AZ", 3.5).unwrap();
        assert!(t2 > t, "later-starting game settles later");
        // Same date 20:05 ET vs 18:10 ET = 1h55min later.
        assert!(t2 - t >= 6900 && t2 - t < 7200);
    }

    #[test]
    fn parser_rejects_non_mlb_tickers() {
        // NHL/NBA/other formats don't embed start time — parser
        // returns None and the strategy operates without
        // time-aware exits on those.
        assert!(parse_settlement_unix_from_ticker("KXNHLGAME-26MAY12ANAVGK-ANA", 3.5).is_none());
        assert!(parse_settlement_unix_from_ticker("KXNBASERIES-26MINSASR2-SAS", 3.5).is_none());
        assert!(parse_settlement_unix_from_ticker("KXPAYROLLS-26AUG-T20000", 3.5).is_none());
    }

    #[test]
    fn parser_rejects_garbage() {
        assert!(parse_settlement_unix_from_ticker("not-a-ticker", 3.5).is_none());
        assert!(parse_settlement_unix_from_ticker("KXMLBGAME-", 3.5).is_none());
        assert!(parse_settlement_unix_from_ticker("KXMLBGAME-26ZZZ111810XX-Y", 3.5).is_none());
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
            profit_take_cents: 5,
            stop_loss_cents: 8,
            pre_settle_halt_secs: 1800,
            pre_settle_flatten_secs: 600,
            skew_escalation_t_hours: 2.0,
            estimated_game_duration_hours: 3.5,
            quote_refresh_cooldown: Duration::from_secs(10),
            exit_failure_threshold: 5,
            exit_failure_cooldown: Duration::from_secs(600),
        });
        s.reload_markets();
        assert_eq!(s.market_count(), 1);
        assert_eq!(s.markets[0].quote_size, 1);
        assert_eq!(s.markets[0].min_spread_cents, 2);
    }

    #[test]
    fn exit_position_signature_is_order_stable() {
        let a = vec![
            OpenLeg {
                signed_qty: -1,
                avg_entry_cents: 49,
                side: "yes".to_string(),
            },
            OpenLeg {
                signed_qty: 2,
                avg_entry_cents: 35,
                side: "no".to_string(),
            },
        ];
        let b = vec![a[1].clone(), a[0].clone()];
        assert_eq!(exit_position_signature(&a), exit_position_signature(&b));
    }

    #[test]
    fn expired_exit_suppression_allows_only_one_probe() {
        let signature = vec![("yes".to_string(), -1, 49)];
        let now = Instant::now();
        let mut state = ExitAttemptState {
            signature: signature.clone(),
            consecutive_emits: 5,
            suppressed_until: Some(now - Duration::from_secs(1)),
        };

        assert!(!should_suppress_exit_attempt(&mut state, &signature, now));
        assert_eq!(state.consecutive_emits, 5);
        assert!(state.suppressed_until.is_none());

        state.suppressed_until = Some(now + Duration::from_secs(600));
        assert!(should_suppress_exit_attempt(&mut state, &signature, now));

        let changed = vec![("yes".to_string(), 0, 49)];
        assert!(!should_suppress_exit_attempt(&mut state, &changed, now));
        assert_eq!(state.consecutive_emits, 0);
        assert!(state.suppressed_until.is_none());
    }
}
