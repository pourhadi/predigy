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
    /// `None` means "any area." NWS area codes look like
    /// `"Travis, TX; Hays, TX"`.
    pub area_substring: Option<String>,
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
        NwsAlert {
            id: "test".into(),
            event_type: event.into(),
            severity: severity.into(),
            urgency: "Immediate".into(),
            area_desc: area.into(),
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
