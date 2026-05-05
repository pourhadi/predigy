//! News-latency strategy: when an external feed event matches a
//! configured trigger, submit a pre-decided trade on the
//! corresponding Kalshi market.
//!
//! ## What "matching" means
//!
//! A [`LatencyRule`] is a tuple of:
//!
//! - filter on the alert (event-type substring, optional area-code
//!   substring, optional minimum severity);
//! - the Kalshi market + side to lift;
//! - a max price the strategy will pay.
//!
//! The first rule a fresh alert matches, fires once. Each rule has
//! an `armed` flag — after firing it disarms until manually rearmed
//! by the operator (or by a config reload). This is intentional:
//! news alerts often duplicate ("Tornado Warning" upgraded to
//! "Tornado Emergency"), and we'd rather fire once and check than
//! double-up on a position.
//!
//! ## What it doesn't do
//!
//! - **No modeling.** The probability shift is encoded in the
//!   `target_price_cents` per rule. If you want a model that maps
//!   alert intensity to a probability (e.g. tornado warnings are
//!   worth 8¢ in the corresponding weather market), implement it
//!   above this layer and recompute the rule list before reload.
//! - **No latency optimisation.** Pulling in `tracing` is fine for
//!   v1; the round-trip on the REST executor is already 30-50 ms,
//!   which dwarfs any logging overhead.

use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_ext_feeds::NwsAlert;
use serde::{Deserialize, Serialize};

/// Severity ladder Kalshi-style: higher number = more severe.
/// NWS spec values map to these tiers; "Unknown" sorts at zero.
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
    /// Substring matched against `NwsAlert::event_type`. e.g.
    /// `"Tornado"` matches both "Tornado Warning" and "Tornado
    /// Emergency". Case-sensitive — NWS values are stable Title
    /// Case.
    pub event_substring: String,
    /// Optional substring matched against `NwsAlert::area_desc`.
    /// `None` means "no substring filter." Use sparingly — NWS
    /// `area_desc` is human-readable text with inconsistent
    /// state-code suffixes; structural filtering should go through
    /// [`required_states`](Self::required_states) instead.
    #[serde(default)]
    pub area_substring: Option<String>,
    /// 2-letter state codes the alert must be in for this rule to
    /// fire. Empty = no state filter (rule fires on alerts in any
    /// state the operator subscribes to). Matches against
    /// `NwsAlert::states` via set-intersection: rule fires if at
    /// least one of `required_states` is in the alert's states.
    /// Reliable: every NWS alert has a UGC-derived state list.
    #[serde(default)]
    pub required_states: Vec<String>,
    /// Minimum severity that triggers this rule.
    pub min_severity: Severity,
    pub kalshi_market: MarketTicker,
    pub side: Side,
    pub action: Action,
    /// Maximum price (in cents) the strategy will pay. Acts as the
    /// IOC limit price; sets the upper bound on the trade's cost.
    pub max_price_cents: u8,
    /// Contracts per fire.
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct LatencyStrategy {
    rules: Vec<LatencyRuleState>,
}

#[derive(Debug, Clone)]
struct LatencyRuleState {
    rule: LatencyRule,
    armed: bool,
}

impl LatencyStrategy {
    #[must_use]
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

    /// Re-arm every rule. Used after the operator's reset or after
    /// a configurable time window.
    pub fn rearm_all(&mut self) {
        for r in &mut self.rules {
            r.armed = true;
        }
    }

    /// Re-arm a single rule by index.
    pub fn rearm(&mut self, idx: usize) -> bool {
        if let Some(r) = self.rules.get_mut(idx) {
            r.armed = true;
            true
        } else {
            false
        }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Try to fire a rule against `alert`. Returns the resulting
    /// [`Intent`] (and the index of the rule that fired) on a
    /// match; `None` otherwise. Disarms the matched rule.
    pub fn evaluate(&mut self, alert: &NwsAlert) -> Option<(usize, Intent)> {
        let alert_severity = Severity::from_str(&alert.severity);
        for (idx, state) in self.rules.iter_mut().enumerate() {
            if !state.armed {
                continue;
            }
            let rule = &state.rule;
            if !alert.event_type.contains(&rule.event_substring) {
                continue;
            }
            // Required-state set intersection: rule fires if any
            // of its `required_states` is in the alert's states.
            // Empty `required_states` = no filter.
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
            // Match — build the intent.
            let Ok(price) = Price::from_cents(rule.max_price_cents) else {
                continue;
            };
            let Ok(qty) = Qty::new(rule.size) else {
                continue;
            };
            let intent = Intent::limit(
                rule.kalshi_market.clone(),
                rule.side,
                rule.action,
                price,
                qty,
            )
            .with_tif(TimeInForce::Ioc);
            state.armed = false;
            return Some((idx, intent));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(event: &str, area: &str, severity: &str) -> NwsAlert {
        alert_in(event, area, severity, &["TX"])
    }

    fn alert_in(event: &str, area: &str, severity: &str, states: &[&str]) -> NwsAlert {
        NwsAlert {
            id: "test".into(),
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
            kalshi_market: MarketTicker::new("WX-TX"),
            side: Side::Yes,
            action: Action::Buy,
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
        assert_eq!(intent.tif, TimeInForce::Ioc);
        assert_eq!(intent.price.cents(), 50);
        assert_eq!(intent.qty.get(), 10);
    }

    #[test]
    fn disarms_after_first_match() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let _ = s
            .evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
            .expect("first fires");
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn rearm_brings_rule_back() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        let _ = s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"));
        assert!(s.rearm(0));
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
                .is_some()
        );
    }

    #[test]
    fn area_filter_blocks_non_matching_alert() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", Some("TX"), Severity::Moderate)]);
        // Alert area is California — doesn't contain "TX".
        assert!(
            s.evaluate(&alert("Tornado Warning", "Sonoma, CA", "Severe"))
                .is_none()
        );
    }

    #[test]
    fn required_states_blocks_alert_in_other_state() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.required_states = vec!["TX".into()];
        let mut s = LatencyStrategy::new(vec![r]);
        // Alert is in IL (Severe Thunderstorm), our rule wants TX.
        assert!(
            s.evaluate(&alert_in(
                "Tornado Warning",
                "Macoupin, IL; Madison, IL",
                "Severe",
                &["IL"]
            ))
            .is_none()
        );
    }

    #[test]
    fn required_states_fires_when_alert_overlaps() {
        let mut r = rule("Tornado", None, Severity::Moderate);
        r.required_states = vec!["TX".into(), "OK".into()];
        let mut s = LatencyStrategy::new(vec![r]);
        // Multi-state alert spanning TX and KS — TX overlaps the
        // rule's required set.
        assert!(
            s.evaluate(&alert_in(
                "Tornado Warning",
                "Travis, TX; Bell, TX; Cherokee, KS",
                "Severe",
                &["TX", "KS"]
            ))
            .is_some()
        );
    }

    #[test]
    fn required_states_empty_means_no_filter() {
        // Default behaviour: rules built via `rule()` have empty
        // required_states; an alert in any state should fire.
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Moderate)]);
        assert!(
            s.evaluate(&alert_in("Tornado Warning", "Polk, IA", "Severe", &["IA"]))
                .is_some()
        );
    }

    #[test]
    fn min_severity_blocks_under_threshold_alert() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Severe)]);
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Moderate"))
                .is_none()
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let r1 = LatencyRule {
            event_substring: "Tornado".into(),
            area_substring: None,
            required_states: Vec::new(),
            min_severity: Severity::Moderate,
            kalshi_market: MarketTicker::new("FIRST"),
            side: Side::Yes,
            action: Action::Buy,
            max_price_cents: 30,
            size: 5,
        };
        let r2 = LatencyRule {
            event_substring: "Tornado".into(),
            area_substring: None,
            required_states: Vec::new(),
            min_severity: Severity::Moderate,
            kalshi_market: MarketTicker::new("SECOND"),
            side: Side::Yes,
            action: Action::Buy,
            max_price_cents: 50,
            size: 10,
        };
        let mut s = LatencyStrategy::new(vec![r1, r2]);
        let (idx, intent) = s
            .evaluate(&alert("Tornado Warning", "Travis, TX", "Severe"))
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(intent.market.as_str(), "FIRST");
    }

    #[test]
    fn severity_ordering_is_intuitive() {
        assert!(Severity::Extreme > Severity::Severe);
        assert!(Severity::Severe > Severity::Moderate);
        assert!(Severity::Moderate > Severity::Minor);
        assert!(Severity::Minor > Severity::Unknown);
    }

    #[test]
    fn unknown_severity_sorts_below_known() {
        let mut s = LatencyStrategy::new(vec![rule("Tornado", None, Severity::Minor)]);
        // "Unknown" alert below Minor → no match.
        assert!(
            s.evaluate(&alert("Tornado Warning", "Travis, TX", "Whatever"))
                .is_none()
        );
    }
}
