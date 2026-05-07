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
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info};

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
    /// Maximum seconds an open position is allowed to sit before
    /// the strategy force-flats it. Latency entries are bets that
    /// the alert moves the market within minutes; if the move
    /// hasn't materialised by `max_hold_secs`, the alert was
    /// likely a false positive and we'd rather free up risk
    /// budget than hold indefinitely. `0` disables time-based
    /// exits.
    pub max_hold_secs: i64,
    /// Floor price (cents) for the wide-IOC force-flat. Latency
    /// has no book subscription, so the exit limit is set
    /// conservatively so any standing bid takes us. `1` cent is
    /// the venue-side floor.
    pub force_flat_floor_cents: i32,
    /// How often to refresh the position cache from Postgres.
    pub position_refresh_interval: Duration,
    /// Periodic timer for re-evaluating exit conditions.
    pub tick_interval: Duration,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            // Default: force-flat at 30 min. Operator can disable
            // by setting to 0 if they want to hold indefinitely.
            max_hold_secs: 30 * 60,
            force_flat_floor_cents: 1,
            position_refresh_interval: Duration::from_secs(60),
            tick_interval: Duration::from_secs(60),
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
        }
    }

    /// Load rules from a JSON file containing a top-level array.
    pub fn from_json_file(path: &Path) -> Result<Self, std::io::Error> {
        let bytes = std::fs::read(path)?;
        let rules: Vec<LatencyRule> = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::new(rules))
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
            // Idempotency: alert.id is unique per NWS alert; using
            // it in client_id means a duplicate fan-out (alert
            // edited by NWS) collapses cleanly via the OMS.
            let client_id = format!(
                "latency:{ticker}:{alert_short}:{idx}",
                ticker = cid_safe_ticker(&rule.kalshi_market),
                alert_short = alert.id.chars().take(20).collect::<String>(),
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
        if self.config.max_hold_secs <= 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let now_instant = std::time::Instant::now();
        for (key, pos) in self.positions.clone() {
            if pos.signed_qty == 0 {
                continue;
            }
            // Per-position cooldown so multiple Ticks within the
            // tick_interval don't re-fire the close intent. The
            // OMS cid would dedupe anyway but the cooldown saves
            // round trips.
            if let Some(last) = self.last_exit_at.get(&key)
                && now_instant.duration_since(*last) < self.config.tick_interval
            {
                continue;
            }
            let age_secs = (now_utc - pos.opened_at).num_seconds();
            if age_secs < self.config.max_hold_secs {
                continue;
            }
            // Construct the force-flat. Sell on the same leg we
            // hold; buy if we're short. Limit at the wide floor.
            let action = if pos.signed_qty > 0 {
                IntentAction::Sell
            } else {
                IntentAction::Buy
            };
            let limit_cents = self.config.force_flat_floor_cents.clamp(1, 99);
            let abs_qty = pos.signed_qty.unsigned_abs() as i32;
            // Position-key + open-day bucket for idempotency. The
            // open-day is stable across ticks so repeated triggers
            // collapse via the OMS.
            let day_bucket = pos.opened_at.timestamp() / 86_400;
            let side_tag = match pos.side {
                Side::Yes => "Y",
                Side::No => "N",
            };
            let ticker = match key.split_once(':') {
                Some((t, _)) => t,
                None => continue,
            };
            let client_id = format!(
                "latency-flat:{cid_ticker}:{side_tag}:{day:08x}",
                cid_ticker = cid_safe_ticker(ticker),
                day = day_bucket as u32,
            );
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
                    "latency-flat: held_{age_secs}s ≥ max_hold_{}s entry={}¢",
                    self.config.max_hold_secs, pos.avg_entry_cents
                )),
            };
            info!(
                ticker,
                side = ?pos.side,
                signed_qty = pos.signed_qty,
                age_secs,
                avg_entry = pos.avg_entry_cents,
                "latency: force-flat aging position"
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
                    return Ok(vec![intent]);
                }
                Ok(Vec::new())
            }
            Event::Tick => {
                // Phase 6.2 — Tick-driven force-flat for stale
                // positions. The strategy has no book access so
                // exits are time-based only.
                Ok(self.evaluate_force_flats(chrono::Utc::now()))
            }
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
        NwsAlertPayload {
            id: format!("urn:oid:test-{event}-{area}-{severity}"),
            event_type: event.into(),
            severity: severity.into(),
            urgency: "Immediate".into(),
            area_desc: area.into(),
            states: states.iter().map(|s| (*s).to_string()).collect(),
            effective: None,
            onset: None,
            expires: None,
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
            max_hold_secs: 1800,
            force_flat_floor_cents: 1,
            position_refresh_interval: Duration::from_secs(60),
            tick_interval: Duration::from_secs(60),
        }
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
