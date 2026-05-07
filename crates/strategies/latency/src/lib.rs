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
use predigy_engine_core::events::{Event, ExternalEvent};
use predigy_engine_core::events::predigy_core_compat::NwsAlertPayload;
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::info;

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

#[derive(Debug)]
pub struct LatencyStrategy {
    rules: Vec<LatencyRuleState>,
}

impl LatencyStrategy {
    pub fn new(rules: Vec<LatencyRule>) -> Self {
        Self {
            rules: rules
                .into_iter()
                .map(|r| LatencyRuleState {
                    rule: r,
                    armed: true,
                })
                .collect(),
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
                ticker = rule.kalshi_market,
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
        _state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        if let Event::External(ExternalEvent::NwsAlert(alert)) = ev {
            if let Some((_idx, intent)) = self.evaluate(alert) {
                return Ok(vec![intent]);
            }
        }
        Ok(Vec::new())
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
            s.evaluate(&alert_in("Tornado Warning", "Travis, TX", "Severe", &["TX"]))
                .is_none()
        );
    }

    #[test]
    fn required_states_filter_admits_intersect() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.required_states = vec!["OK".into(), "TX".into()];
        let mut s = LatencyStrategy::new(vec![r]);
        assert!(
            s.evaluate(&alert_in("Tornado Warning", "Travis, TX", "Severe", &["TX"]))
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
}
