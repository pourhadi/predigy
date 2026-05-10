// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-latency` — race the Kalshi book on news /
//! data events. Implements [`predigy_engine_core::Strategy`].
//!
//! Logic preserved verbatim from `bin/latency-trader/src/strategy.rs`
//! (the legacy daemon being phased out). The thesis: when an
//! external feed event matches a configured trigger, lift a
//! pre-decided trade on the corresponding Kalshi market before
//! the market reprices.
//!
//! ## Rule semantics
//!
//! Each [`LatencyRule`] is a tuple of:
//! - filter on the alert (event-type substring, optional area-code
//!   substring, optional minimum severity, optional state list);
//! - the Kalshi market + side to lift;
//! - a max price the strategy will pay (IOC limit ceiling).
//!
//! The first rule a fresh alert matches fires once. Each rule has
//! an `armed` flag — after firing it disarms until manually rearmed
//! (config reload, operator action, or a future re-arm-on-cycle
//! policy). Intentional: NWS often duplicates alerts ("Tornado
//! Warning" upgraded to "Tornado Emergency"); single-fire is the
//! safer default.
//!
//! ## What this strategy doesn't do
//!
//! - **No modeling.** The probability shift is encoded in
//!   `target_price_cents` per rule. If you want a model that maps
//!   alert intensity to a probability, implement it ABOVE this
//!   layer and recompute the rule list before reload.
//! - **No latency optimisation.** Tracing overhead is dwarfed by
//!   REST submit latency (~200ms) — order of magnitudes worse than
//!   any tracing macro.
//!
//! ## Engine wiring
//!
//! The strategy declares `external_subscriptions() -> ["nws_alerts"]`
//! at registration. The engine's [`crate::external_feeds`]
//! dispatcher fans every `NwsAlertPayload` to this strategy via
//! `Event::External(ExternalEvent::NwsAlert(...))`. The strategy
//! has no Kalshi-side market subscriptions — it only fires on
//! external triggers and submits IOC orders that don't depend on
//! seeing the book first.

use async_trait::async_trait;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::predigy_core_compat::NwsAlertPayload;
use predigy_engine_core::events::{Event, ExternalEvent};
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif, cid_safe_ticker};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

pub const STRATEGY_ID: StrategyId = StrategyId("latency");

/// Severity ladder Kalshi-style: higher = more severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Unknown,
    Minor,
    Moderate,
    Severe,
    Extreme,
}

impl Severity {
    fn from_str(s: &str) -> Self {
        match s {
            "Extreme" => Self::Extreme,
            "Severe" => Self::Severe,
            "Moderate" => Self::Moderate,
            "Minor" => Self::Minor,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyRule {
    /// Substring matched against `event_type`. Case-sensitive — NWS
    /// values are stable Title Case.
    pub event_substring: String,
    /// Optional substring matched against `area_desc`.
    #[serde(default)]
    pub area_substring: Option<String>,
    /// 2-letter state codes the alert must be in. Empty = no
    /// filter. Matches via set-intersection.
    #[serde(default)]
    pub required_states: Vec<String>,
    pub min_severity: Severity,
    pub kalshi_market: String,
    /// `"yes"` or `"no"` — JSON-friendly string form. Mapped to
    /// `Side` at evaluation time.
    pub side: String,
    /// `"buy"` or `"sell"`.
    pub action: String,
    /// IOC limit ceiling, cents.
    pub max_price_cents: u8,
    pub size: u32,
}

#[derive(Debug, Clone)]
struct LatencyRuleState {
    rule: LatencyRule,
    armed: bool,
}

/// Phase 6.2 — strategy-level config that's not per-rule.
/// Loaded separately from the rule JSON.
#[derive(Debug, Clone)]
pub struct LatencyConfig {
    /// **Audit A5** — tiered force-flat thresholds (seconds).
    ///
    /// Tier 1 (`tier1_secs`): light TP. Once the position has
    /// run this long, exit on any positive PnL using the cached
    /// mark from the book. Default 5 min.
    ///
    /// Tier 2 (`tier2_secs`): mark-aware force-flat at the
    /// current bid (mark-aware unwind, no profit gate). Default
    /// 15 min.
    ///
    /// Tier 3 (`max_hold_secs`): wide IOC regardless of mark. Long
    /// positions sell at `force_flat_floor_cents`; short positions buy to
    /// cover at `100 - force_flat_floor_cents`. Default 30 min.
    ///
    /// `0` on any tier disables that tier.
    pub tier1_secs: i64,
    pub tier2_secs: i64,
    pub max_hold_secs: i64,
    /// Floor price (cents) for the tier-3 wide-IOC force-flat.
    /// Set conservatively (1¢) so any standing bid takes us.
    pub force_flat_floor_cents: i32,
    /// How often to refresh the position cache from Postgres.
    pub position_refresh_interval: Duration,
    /// Periodic timer for re-evaluating exit conditions.
    pub tick_interval: Duration,
    /// Maximum age for a latency entry trigger. Active NWS alerts can remain in
    /// the feed long after issuance; old active alerts are stale for a latency
    /// strategy and must not re-fire after engine restarts.
    pub max_alert_age_secs: i64,
}

impl LatencyConfig {
    /// Audit B2 + B3 — env-var overrides:
    /// - `PREDIGY_LATENCY_TIER1_SECS` / `TIER2_SECS` /
    ///   `MAX_HOLD_SECS` (i64; tier thresholds)
    /// - `PREDIGY_LATENCY_FORCE_FLAT_FLOOR_CENTS` (i32)
    /// - `PREDIGY_LATENCY_TICK_INTERVAL_MS` (u64)
    #[must_use]
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_TIER1_SECS") {
            if let Ok(n) = v.parse() {
                c.tier1_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_TIER2_SECS") {
            if let Ok(n) = v.parse() {
                c.tier2_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_MAX_HOLD_SECS") {
            if let Ok(n) = v.parse() {
                c.max_hold_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_FORCE_FLAT_FLOOR_CENTS") {
            if let Ok(n) = v.parse() {
                c.force_flat_floor_cents = n;
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_TICK_INTERVAL_MS") {
            if let Ok(n) = v.parse::<u64>() {
                c.tick_interval = Duration::from_millis(n);
            }
        }
        if let Ok(v) = std::env::var("PREDIGY_LATENCY_MAX_ALERT_AGE_SECS") {
            if let Ok(n) = v.parse::<i64>() {
                c.max_alert_age_secs = n;
            }
        }
        c
    }
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            // A5 tiers: 5/15/30 min. Tunable per env var below.
            tier1_secs: 5 * 60,
            tier2_secs: 15 * 60,
            max_hold_secs: 30 * 60,
            force_flat_floor_cents: 1,
            position_refresh_interval: Duration::from_secs(60),
            tick_interval: Duration::from_secs(60),
            max_alert_age_secs: 10 * 60,
        }
    }
}

/// Phase 6.2 — open-position snapshot. Refreshed from Postgres
/// on the configured cadence; stale up to one
/// `position_refresh_interval`.
#[derive(Debug, Clone)]
struct CachedPosition {
    side: Side,
    /// Signed: positive = long.
    signed_qty: i32,
    avg_entry_cents: i32,
    opened_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct LatencyStrategy {
    config: LatencyConfig,
    rules: Vec<LatencyRuleState>,
    /// Phase 6.2 — open positions, keyed by `"{ticker}:{side_tag}"`.
    positions: HashMap<String, CachedPosition>,
    /// Phase 6.2 — per-position exit cooldown.
    last_exit_at: HashMap<String, std::time::Instant>,
    last_position_refresh: Option<std::time::Instant>,
    /// **Audit A5** — latest known book mark per ticker, populated
    /// from `Event::BookUpdate` on markets we're holding (the
    /// strategy self-subscribes after firing an entry).
    /// `(yes_bid_cents, no_bid_cents)` — derive YES/NO marks
    /// from these.
    book_marks: HashMap<String, (Option<u8>, Option<u8>)>,
    /// Tickers we've requested subscriptions for. Bounded growth
    /// (one entry per market we've ever held).
    subscribed: std::collections::HashSet<String>,
}

impl LatencyStrategy {
    pub fn new(rules: Vec<LatencyRule>) -> Self {
        Self::with_config(LatencyConfig::default(), rules)
    }

    pub fn with_config(config: LatencyConfig, rules: Vec<LatencyRule>) -> Self {
        Self {
            config,
            rules: rules
                .into_iter()
                .map(|r| LatencyRuleState {
                    rule: r,
                    armed: true,
                })
                .collect(),
            positions: HashMap::new(),
            last_exit_at: HashMap::new(),
            last_position_refresh: None,
            book_marks: HashMap::new(),
            subscribed: std::collections::HashSet::new(),
        }
    }

    /// Load rules from a JSON file containing a top-level array.
    /// Strategy-level config is taken from the environment via
    /// `LatencyConfig::from_env()`; rule-level config is in the
    /// JSON file (per-rule market, side, action, severity, etc.).
    pub fn from_json_file(path: &Path) -> Result<Self, std::io::Error> {
        let bytes = std::fs::read(path)?;
        let rules: Vec<LatencyRule> = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::with_config(LatencyConfig::from_env(), rules))
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn rearm_all(&mut self) {
        for r in &mut self.rules {
            r.armed = true;
        }
    }

    fn evaluate(&mut self, alert: &NwsAlertPayload) -> Option<(usize, Intent)> {
        if !alert_is_fresh(alert, chrono::Utc::now(), self.config.max_alert_age_secs) {
            debug!(alert_id = %alert.id, event = %alert.event_type, "latency: stale alert skipped");
            return None;
        }
        let alert_severity = Severity::from_str(&alert.severity);
        for (idx, state) in self.rules.iter_mut().enumerate() {
            if !state.armed {
                continue;
            }
            let rule = &state.rule;
            if !alert.event_type.contains(&rule.event_substring) {
                continue;
            }
            if !rule.required_states.is_empty()
                && !rule
                    .required_states
                    .iter()
                    .any(|s| alert.states.iter().any(|a| a == s))
            {
                continue;
            }
            if let Some(area_filter) = &rule.area_substring
                && !alert.area_desc.contains(area_filter)
            {
                continue;
            }
            if alert_severity < rule.min_severity {
                continue;
            }

            let side = match rule.side.as_str() {
                "yes" => Side::Yes,
                "no" => Side::No,
                _ => continue,
            };
            let action = match rule.action.as_str() {
                "buy" => IntentAction::Buy,
                "sell" => IntentAction::Sell,
                _ => continue,
            };
            if rule.max_price_cents == 0 || rule.max_price_cents >= 100 {
                continue;
            }
            let qty = match i32::try_from(rule.size) {
                Ok(q) if q > 0 => q,
                _ => continue,
            };
            // Idempotency: hash the full NWS alert id, rule index, and ticker.
            // Prefix truncation is unsafe because many NWS ids share the same
            // `urn:oid:` prefix; the hash stays stable across restarts while
            // remaining Kalshi-cid safe.
            let alert_hash = alert_hash(&alert.id, idx, &rule.kalshi_market);
            let client_id = format!(
                "latency:{ticker}:{alert_hash}:{idx}",
                ticker = cid_safe_ticker(&rule.kalshi_market),
            );
            let intent = Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(&rule.kalshi_market),
                side,
                action,
                price_cents: Some(i32::from(rule.max_price_cents)),
                qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "latency: rule[{idx}] event={:?} severity={:?}",
                    rule.event_substring, rule.min_severity
                )),
                post_only: false,
            };
            state.armed = false;
            info!(
                rule_idx = idx,
                ticker = %rule.kalshi_market,
                alert_id = %alert.id,
                event = %alert.event_type,
                severity = %alert.severity,
                "latency: rule fired"
            );
            return Some((idx, intent));
        }
        None
    }

    /// Phase 6.2 — refresh the open-position cache from Postgres.
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
        self.positions = next;
        self.last_position_refresh = Some(std::time::Instant::now());
        debug!(n_positions = n, "latency: position cache refreshed");
        Ok(())
    }

    /// Phase 6.2 — emit force-flat IOCs for any position older
    /// than `max_hold_secs`. Latency has no book subscription so
    /// the limit price is set conservatively to
    /// `force_flat_floor_cents` (default 1¢) — any standing bid
    /// takes us. Returns 0..N closing intents (one per stale
    /// position).
    fn evaluate_force_flats(&mut self, now_utc: chrono::DateTime<chrono::Utc>) -> Vec<Intent> {
        let mut out = Vec::new();
        let now_instant = std::time::Instant::now();
        for (key, pos) in self.positions.clone() {
            if pos.signed_qty == 0 {
                continue;
            }
            if let Some(last) = self.last_exit_at.get(&key)
                && now_instant.duration_since(*last) < self.config.tick_interval
            {
                continue;
            }
            let age_secs = (now_utc - pos.opened_at).num_seconds();
            let ticker = match key.split_once(':') {
                Some((t, _)) => t,
                None => continue,
            };

            // **A5 — tiered exit selection**:
            //   tier1 (>=tier1_secs): TP-only, mark-aware, fires
            //                         only when PnL > 0.
            //   tier2 (>=tier2_secs): mark-aware force-flat at
            //                         current bid (no profit gate).
            //   tier3 (>=max_hold_secs): wide IOC at floor (1¢).
            //
            // Determine which tier to fire (highest applicable).
            // Books for held positions arrive via the
            // self-subscribe path; the latest mark is in
            // book_marks. Without a mark we can still fire
            // tier3.
            let book_yes = self.book_marks.get(ticker).map(|t| t.0).unwrap_or(None);
            let book_no = self.book_marks.get(ticker).map(|t| t.1).unwrap_or(None);
            let mark_cents: Option<i32> = match (pos.side, pos.signed_qty.is_positive()) {
                (Side::Yes, true) => book_yes.map(i32::from),
                (Side::No, true) => book_no.map(i32::from),
                (Side::Yes, false) => book_no.map(|c| 100 - i32::from(c)),
                (Side::No, false) => book_yes.map(|c| 100 - i32::from(c)),
            };
            let pnl_per = mark_cents.map(|m| {
                if pos.signed_qty > 0 {
                    m - pos.avg_entry_cents
                } else {
                    pos.avg_entry_cents - m
                }
            });

            let tier3_active =
                self.config.max_hold_secs > 0 && age_secs >= self.config.max_hold_secs;
            let tier2_active = self.config.tier2_secs > 0 && age_secs >= self.config.tier2_secs;
            let tier1_active = self.config.tier1_secs > 0
                && age_secs >= self.config.tier1_secs
                && pnl_per.unwrap_or(0) > 0;

            let (tier_tag, limit_cents) = if tier3_active {
                (
                    "t3",
                    tier3_limit_cents(pos.signed_qty, self.config.force_flat_floor_cents),
                )
            } else if tier2_active {
                // Mark-aware force-flat. If we don't have a mark
                // yet, defer to tier3 — wait until the position
                // ages further.
                match mark_cents {
                    Some(m) => ("t2", m.clamp(1, 99)),
                    None => continue,
                }
            } else if tier1_active {
                // Light TP. Mark is required (we already filtered
                // on pnl_per > 0 which requires a mark).
                let m = mark_cents.expect("pnl_per filter implies mark");
                ("t1", m.clamp(1, 99))
            } else {
                continue;
            };

            let action = if pos.signed_qty > 0 {
                IntentAction::Sell
            } else {
                IntentAction::Buy
            };
            let abs_qty = pos.signed_qty.unsigned_abs() as i32;
            let day_bucket = pos.opened_at.timestamp() / 86_400;
            let side_tag = match pos.side {
                Side::Yes => "Y",
                Side::No => "N",
            };
            let client_id = format!(
                "latency-flat:{cid_ticker}:{side_tag}:{tier}:{day:08x}",
                cid_ticker = cid_safe_ticker(ticker),
                tier = tier_tag,
                day = day_bucket as u32,
            );
            let pnl_str = pnl_per
                .map(|p| format!("pnl={p}¢"))
                .unwrap_or_else(|| "no_mark".to_string());
            let intent = Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: MarketTicker::new(ticker),
                side: pos.side,
                action,
                price_cents: Some(limit_cents),
                qty: abs_qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "latency-flat:{tier_tag} held_{age_secs}s entry={}¢ limit={}¢ {pnl_str}",
                    pos.avg_entry_cents, limit_cents
                )),
                post_only: false,
            };
            info!(
                ticker,
                side = ?pos.side,
                signed_qty = pos.signed_qty,
                age_secs,
                avg_entry = pos.avg_entry_cents,
                tier = tier_tag,
                limit_cents,
                "latency: tiered force-flat firing"
            );
            self.last_exit_at.insert(key, now_instant);
            out.push(intent);
        }
        out
    }
}

fn position_key(ticker: &str, side: Side) -> String {
    let tag = match side {
        Side::Yes => 'y',
        Side::No => 'n',
    };
    format!("{ticker}:{tag}")
}

fn tier3_limit_cents(signed_qty: i32, force_flat_floor_cents: i32) -> i32 {
    let floor = force_flat_floor_cents.clamp(1, 99);
    if signed_qty > 0 { floor } else { 100 - floor }
}

fn alert_hash(alert_id: &str, rule_idx: usize, ticker: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(alert_id.as_bytes());
    hasher.update(b"|");
    hasher.update(rule_idx.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(ticker.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().fold(String::new(), |mut out, b| {
        let _ = write!(&mut out, "{b:02x}");
        out
    })
}

fn alert_is_fresh(
    alert: &NwsAlertPayload,
    now: chrono::DateTime<chrono::Utc>,
    max_age_secs: i64,
) -> bool {
    if max_age_secs <= 0 {
        return false;
    }
    if let Some(expires) = alert.expires.as_deref().and_then(parse_rfc3339_utc)
        && expires <= now
    {
        return false;
    }
    let Some(started_at) = alert
        .onset
        .as_deref()
        .and_then(parse_rfc3339_utc)
        .or_else(|| alert.effective.as_deref().and_then(parse_rfc3339_utc))
    else {
        warn!(alert_id = %alert.id, "latency: alert missing onset/effective; refusing entry");
        return false;
    };
    let age_secs = now.signed_duration_since(started_at).num_seconds();
    (0..=max_age_secs).contains(&age_secs)
}

fn parse_rfc3339_utc(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[async_trait]
impl Strategy for LatencyStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        // No book subscriptions — the strategy lifts at the rule's
        // configured price ceiling regardless of book state. Any
        // book updates arriving here would be ignored anyway.
        Ok(Vec::new())
    }

    fn external_subscriptions(&self) -> Vec<&'static str> {
        vec!["nws_alerts"]
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // Phase 6.2 — refresh the position cache periodically so
        // tick-driven force-flats see fresh state.
        let needs_refresh = self
            .last_position_refresh
            .is_none_or(|t| t.elapsed() >= self.config.position_refresh_interval);
        if needs_refresh {
            self.refresh_positions(state).await?;
        }

        match ev {
            Event::External(ExternalEvent::NwsAlert(alert)) => {
                if let Some((_idx, intent)) = self.evaluate(alert) {
                    // **Audit A5** — self-subscribe to this market
                    // so subsequent BookUpdates feed
                    // `book_marks` and tier-1/2 force-flats can
                    // exit at mark.
                    let ticker = intent.market.as_str().to_string();
                    if self.subscribed.insert(ticker.clone()) {
                        state.subscribe_to_markets(vec![intent.market.clone()]);
                    }
                    return Ok(vec![intent]);
                }
                Ok(Vec::new())
            }
            Event::BookUpdate { market, book } => {
                // **Audit A5** — cache best bids so tiered
                // force-flats can compute mark-aware exits. We
                // record (yes_bid, no_bid); YES/NO marks for any
                // direction are derived in
                // `evaluate_force_flats`.
                let yes_bid = book.best_yes_bid().map(|(p, _)| p.cents());
                let no_bid = book.best_no_bid().map(|(p, _)| p.cents());
                self.book_marks
                    .insert(market.as_str().to_string(), (yes_bid, no_bid));
                Ok(Vec::new())
            }
            Event::Tick => Ok(self.evaluate_force_flats(chrono::Utc::now())),
            _ => Ok(Vec::new()),
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.config.tick_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(event: &str, area: &str, severity: &str) -> NwsAlertPayload {
        alert_in(event, area, severity, &["TX"])
    }

    fn alert_in(event: &str, area: &str, severity: &str, states: &[&str]) -> NwsAlertPayload {
        let now = chrono::Utc::now();
        NwsAlertPayload {
            id: format!("urn:oid:test-{event}-{area}-{severity}"),
            event_type: event.into(),
            severity: severity.into(),
            urgency: "Immediate".into(),
            area_desc: area.into(),
            states: states.iter().map(|s| (*s).to_string()).collect(),
            effective: Some((now - chrono::Duration::seconds(30)).to_rfc3339()),
            onset: Some((now - chrono::Duration::seconds(30)).to_rfc3339()),
            expires: Some((now + chrono::Duration::hours(1)).to_rfc3339()),
            headline: None,
        }
    }

    fn rule(event: &str, area: Option<&str>, sev: Severity) -> LatencyRule {
        LatencyRule {
            event_substring: event.into(),
            area_substring: area.map(String::from),
            required_states: Vec::new(),
            min_severity: sev,
            kalshi_market: "WX-TX".into(),
            side: "yes".into(),
            action: "buy".into(),
            max_price_cents: 50,
            size: 10,
        }
    }

    #[test]
    fn fires_on_substring_match() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let result = s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"));
        let (idx, intent) = result.expect("matched");
        assert_eq!(idx, 0);
        assert_eq!(intent.market.as_str(), "WX-TX");
        assert_eq!(intent.tif, Tif::Ioc);
        assert_eq!(intent.price_cents, Some(50));
        assert_eq!(intent.qty, 10);
        assert_eq!(intent.action, IntentAction::Buy);
        assert_eq!(intent.side, Side::Yes);
    }

    #[test]
    fn disarms_after_first_match() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
            .expect("first fires");
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn no_fire_below_severity() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Severe)]);
        // Moderate < Severe.
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Moderate"))
                .is_none()
        );
    }

    #[test]
    fn area_substring_filter_blocks() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", Some("Hays"), Severity::Moderate)]);
        // Area is "Travis", filter wants "Hays".
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn area_substring_filter_passes() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", Some("Travis"), Severity::Moderate)]);
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_some()
        );
    }

    #[test]
    fn required_states_filter_excludes_other_state_alerts() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.required_states = vec!["OK".into()];
        let mut s = LatencyStrategy::new(vec![r]);
        // Alert is in TX, rule requires OK.
        assert!(
            s.evaluate(&alert_in(
                "Tornado Warning",
                "Travis, TX",
                "Severe",
                &["TX"]
            ))
            .is_none()
        );
    }

    #[test]
    fn required_states_filter_admits_intersect() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.required_states = vec!["OK".into(), "TX".into()];
        let mut s = LatencyStrategy::new(vec![r]);
        assert!(
            s.evaluate(&alert_in(
                "Tornado Warning",
                "Travis, TX",
                "Severe",
                &["TX"]
            ))
            .is_some()
        );
    }

    #[test]
    fn rearm_all_re_enables() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
            .expect("first");
        s.rearm_all();
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_some()
        );
    }

    #[test]
    fn invalid_side_or_action_skips_rule() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.side = "bogus".into();
        let mut s = LatencyStrategy::new(vec![r]);
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn out_of_range_price_skips() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.max_price_cents = 100;
        let mut s = LatencyStrategy::new(vec![r]);
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn from_json_file_loads_rules() {
        let json = r#"[
            {
                "event_substring": "Tornado",
                "min_severity": "Moderate",
                "kalshi_market": "WX-TX",
                "side": "yes",
                "action": "buy",
                "max_price_cents": 50,
                "size": 10
            }
        ]"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.json");
        std::fs::write(&path, json).unwrap();
        let s = LatencyStrategy::from_json_file(&path).unwrap();
        assert_eq!(s.rule_count(), 1);
    }

    // ─── Phase 6.2 force-flat tests ──────────────────────────

    fn cached_position(
        side: Side,
        signed_qty: i32,
        avg_entry: i32,
        opened_at: chrono::DateTime<chrono::Utc>,
    ) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents: avg_entry,
            opened_at,
        }
    }

    fn cfg() -> LatencyConfig {
        LatencyConfig {
            tier1_secs: 5 * 60,
            tier2_secs: 15 * 60,
            max_hold_secs: 1800,
            force_flat_floor_cents: 1,
            position_refresh_interval: Duration::from_secs(60),
            tick_interval: Duration::from_secs(60),
            max_alert_age_secs: 10 * 60,
        }
    }

    #[test]
    fn stale_alert_does_not_fire() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let mut a = alert("Tornado Warning", "Travis, TX", "Severe");
        let old = chrono::Utc::now() - chrono::Duration::seconds(3600);
        a.onset = Some(old.to_rfc3339());
        a.effective = Some(old.to_rfc3339());
        assert!(s.evaluate(&a).is_none());
    }

    #[test]
    fn expired_alert_does_not_fire() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let mut a = alert("Tornado Warning", "Travis, TX", "Severe");
        a.expires = Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        assert!(s.evaluate(&a).is_none());
    }

    #[test]
    fn missing_alert_time_does_not_fire() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let mut a = alert("Tornado Warning", "Travis, TX", "Severe");
        a.onset = None;
        a.effective = None;
        assert!(s.evaluate(&a).is_none());
    }

    #[test]
    fn alert_hash_uses_full_id() {
        let a = alert_hash("urn:oid:shared-prefix-A", 1, "WX-TX");
        let b = alert_hash("urn:oid:shared-prefix-B", 1, "WX-TX");
        assert_ne!(a, b);
        assert_eq!(a, alert_hash("urn:oid:shared-prefix-A", 1, "WX-TX"));
    }

    #[test]
    fn force_flat_fires_for_aged_long_yes() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        // Position opened 2h ago, max_hold is 30 min.
        let opened = now - chrono::Duration::seconds(7200);
        s.positions.insert(
            position_key("WX-A", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        let intent = &intents[0];
        assert_eq!(intent.market.as_str(), "WX-A");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 5);
        assert_eq!(intent.price_cents, Some(1));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(intent.client_id.starts_with("latency-flat:WX-A:Y:"));
    }

    #[test]
    fn tier3_short_buys_to_cover_with_wide_ceiling() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(7200);
        s.positions.insert(
            position_key("WX-SHORT", Side::Yes),
            cached_position(Side::Yes, -3, 80, opened),
        );

        let intents = s.evaluate_force_flats(now);

        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].action, IntentAction::Buy);
        assert_eq!(intents[0].price_cents, Some(99));
        assert!(intents[0].client_id.contains(":t3:"));
    }

    #[test]
    fn force_flat_skips_recent_position() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        // Just opened — well under max_hold.
        let opened = now - chrono::Duration::seconds(60);
        s.positions.insert(
            position_key("WX-B", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        assert!(s.evaluate_force_flats(now).is_empty());
    }

    #[test]
    fn force_flat_handles_long_no() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(7200);
        s.positions.insert(
            position_key("WX-C", Side::No),
            cached_position(Side::No, 4, 30, opened),
        );
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].side, Side::No);
        assert_eq!(intents[0].action, IntentAction::Sell);
        assert_eq!(intents[0].price_cents, Some(1));
    }

    #[test]
    fn force_flat_disabled_when_max_hold_zero() {
        let mut cfg_off = cfg();
        cfg_off.max_hold_secs = 0;
        let mut s = LatencyStrategy::with_config(cfg_off, Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(7200);
        s.positions.insert(
            position_key("WX-D", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        assert!(s.evaluate_force_flats(now).is_empty());
    }

    #[test]
    fn force_flat_cooldown_blocks_repeat() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(7200);
        s.positions.insert(
            position_key("WX-E", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        let first = s.evaluate_force_flats(now);
        assert_eq!(first.len(), 1);
        // Immediate repeat: cooldown blocks it.
        let second = s.evaluate_force_flats(now);
        assert!(second.is_empty());
    }

    // ─── A5 tiered force-flat tests ─────────────────────────

    #[test]
    fn tier1_takes_profit_with_positive_pnl_mark() {
        // Position aged 6 min (>tier1=5min, <tier2=15min).
        // YES long @ 50¢, mark = 60¢, PnL +10¢ → tier1 fires
        // limit at 60¢.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(360);
        s.positions.insert(
            position_key("WX-T1", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        s.book_marks.insert("WX-T1".into(), (Some(60), Some(40)));
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].price_cents, Some(60));
        assert!(intents[0].client_id.contains(":t1:"));
    }

    #[test]
    fn tier1_skips_when_pnl_negative() {
        // Position aged 6 min, mark 40¢ < entry 50¢. Tier1 needs
        // PnL > 0 → no fire. Below tier2 too → no fire.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(360);
        s.positions.insert(
            position_key("WX-T1N", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        s.book_marks.insert("WX-T1N".into(), (Some(40), Some(60)));
        assert!(s.evaluate_force_flats(now).is_empty());
    }

    #[test]
    fn tier2_force_flats_at_mark_regardless_of_pnl() {
        // Position aged 16 min (>tier2=15min, <tier3=30min).
        // No-profit gate at tier2. Mark at 30¢ (loss vs 50¢
        // entry) → tier2 fires limit 30¢.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(960);
        s.positions.insert(
            position_key("WX-T2", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        s.book_marks.insert("WX-T2".into(), (Some(30), Some(70)));
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].price_cents, Some(30));
        assert!(intents[0].client_id.contains(":t2:"));
    }

    #[test]
    fn tier2_skips_without_book_mark_defers_to_tier3() {
        // Aged 16 min, no book mark. Tier2 requires mark — skip.
        // Below tier3 — skip too.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(960);
        s.positions.insert(
            position_key("WX-T2N", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        assert!(s.evaluate_force_flats(now).is_empty());
    }

    #[test]
    fn tier3_wide_floor_fires_without_mark() {
        // Aged 31 min. No book mark → tier3 fires at floor 1¢.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(1860);
        s.positions.insert(
            position_key("WX-T3", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].price_cents, Some(1));
        assert!(intents[0].client_id.contains(":t3:"));
    }

    #[test]
    fn tier3_overrides_tier2_at_max_hold() {
        // Aged 31 min, with book mark. Tier3 must take priority
        // over tier2 because tier3 is the hard floor.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(1860);
        s.positions.insert(
            position_key("WX-T3M", Side::Yes),
            cached_position(Side::Yes, 5, 50, opened),
        );
        s.book_marks.insert("WX-T3M".into(), (Some(40), Some(60)));
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].price_cents, Some(1));
        assert!(intents[0].client_id.contains(":t3:"));
    }

    #[test]
    fn no_long_handles_mark_via_complement() {
        // NO long: signed_qty +4, side=No, entry=30¢. Aged
        // tier2. YES bid is 70 → NO bid stored as 30. Mark for
        // NO long should equal NO bid = 30 → exit at 30¢ even
        // though entry was also 30¢.
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let opened = now - chrono::Duration::seconds(960);
        s.positions.insert(
            position_key("WX-NO", Side::No),
            cached_position(Side::No, 4, 30, opened),
        );
        s.book_marks.insert("WX-NO".into(), (Some(70), Some(30)));
        let intents = s.evaluate_force_flats(now);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].side, Side::No);
        assert_eq!(intents[0].price_cents, Some(30));
        assert!(intents[0].client_id.contains(":t2:"));
    }

    #[test]
    fn force_flat_emits_one_per_aging_position() {
        let mut s = LatencyStrategy::with_config(cfg(), Vec::new());
        let now = chrono::Utc::now();
        let aged = now - chrono::Duration::seconds(7200);
        let fresh = now - chrono::Duration::seconds(60);
        s.positions.insert(
            position_key("WX-F", Side::Yes),
            cached_position(Side::Yes, 5, 50, aged),
        );
        s.positions.insert(
            position_key("WX-G", Side::Yes),
            cached_position(Side::Yes, 5, 50, fresh),
        );
        let intents = s.evaluate_force_flats(now);
        assert_eq!(
            intents.len(),
            1,
            "only the aged position should produce a flat"
        );
        assert_eq!(intents[0].market.as_str(), "WX-F");
    }
}
