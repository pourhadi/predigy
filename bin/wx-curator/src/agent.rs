//! Anthropic API client + rule synthesis.
//!
//! Calls `POST https://api.anthropic.com/v1/messages` with the
//! [`prompt::SYSTEM_PROMPT`] and a user message describing the
//! current market batch. Decodes the response, validates each
//! proposed rule, and emits `latency_trader::LatencyRule` ready
//! to be serialised.
//!
//! ## Cost shape
//!
//! Sonnet 4.6 input is ~\$3/MTok, output ~\$15/MTok. Our system
//! prompt is ~700 tokens, each market block ~50 tokens. A batch
//! of 30 markets → ~2200 tokens in, ~1500 tokens out → ~\$0.03
//! per call. A full Kalshi-wide scan is one or two batches.
//!
//! [`prompt::SYSTEM_PROMPT`]: crate::prompt::SYSTEM_PROMPT

use crate::kalshi_scan::WeatherMarket;
use crate::prompt::{SYSTEM_PROMPT, user_message};
use latency_trader::{LatencyRule, Severity};
use predigy_core::market::MarketTicker;
use predigy_core::side::{Action, Side};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

const ANTHROPIC_API: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MODEL: &str = "claude-sonnet-4-6";
const MAX_TOKENS: u32 = 8192;

#[derive(Debug, thiserror::Error)]
pub enum CuratorError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("anthropic api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("decode response: {0}")]
    Decode(String),
    #[error("missing ANTHROPIC_API_KEY env var")]
    MissingApiKey,
}

/// One curated rule with its supporting reasoning. The wire shape
/// matches what we instruct Claude to emit (see `prompt.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedRule {
    pub market_ticker: String,
    pub reasoning: String,
    pub event_substring: String,
    #[serde(default)]
    pub area_substring: Option<String>,
    pub min_severity: String,
    pub side: String,
    pub max_price_cents: u8,
    #[serde(default = "default_size")]
    pub size: u32,
}

fn default_size() -> u32 {
    1
}

impl CuratedRule {
    /// Apply an automated direction sanity-check on temperature
    /// markets. Returns an error reason if the (event, market
    /// suffix, side) combination is the obvious-wrong one — e.g.
    /// "Excessive Heat Warning + KXHIGH...-T80 + side=no" (heat
    /// makes high ≥ 80 MORE likely, so buy YES).
    ///
    /// We only flag the high-confidence cases; rules that don't
    /// match a known temperature pattern (hurricane, flood, etc.)
    /// pass through untouched.
    fn temperature_direction_check(&self) -> Result<(), String> {
        let m = self.market_ticker.as_str();
        let is_high_temp = m.starts_with("KXHIGH");
        let is_low_temp = m.starts_with("KXLOW");
        if !(is_high_temp || is_low_temp) {
            return Ok(());
        }
        // Ticker suffix: -B<n> ("below n") or -T<n> ("at-or-above n").
        // Find the LAST -B or -T followed by digits.
        let (tail, is_below) = match (m.rfind("-B"), m.rfind("-T")) {
            (Some(b), Some(t)) if b > t => (&m[b + 2..], true),
            (Some(b), None) => (&m[b + 2..], true),
            (Some(_) | None, Some(t)) => (&m[t + 2..], false),
            (None, None) => return Ok(()),
        };
        // Confirm the suffix is a number; otherwise it's something
        // we don't model (e.g. '-X' or letters).
        if !tail.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return Ok(());
        }

        let ev = self.event_substring.as_str();
        let warm_alert = ev.contains("Heat") || ev.contains("Excessive");
        let cold_alert = ev.contains("Freeze") || ev.contains("Frost");
        if !(warm_alert || cold_alert) {
            return Ok(());
        }
        // Determine which side the alert SHOULD push us to buy.
        // Both `KXHIGH-T<n>` ("high ≥ n") and `KXLOW-T<n>` ("low ≥ n")
        // share the same logic: warm alerts push the temp UP, so
        // they make `≥ n` more likely (buy YES) and `< n` less
        // likely (buy NO).
        let warm_buy_yes = !is_below;
        let expected = match (warm_alert, warm_buy_yes) {
            (true, true) | (false, false) => "yes",
            (true, false) | (false, true) => "no",
        };
        let actual = self.side.to_lowercase();
        if actual != expected {
            return Err(format!(
                "temperature direction inverted: alert={ev}, market={m}, got side={actual}, expected={expected}"
            ));
        }
        Ok(())
    }

    /// Convert to a `latency_trader::LatencyRule`. Validates the
    /// fields; returns `Err(reason)` if invalid (e.g. price out of
    /// range, unknown severity, unknown side, or temperature
    /// direction inverted).
    pub fn into_rule(self) -> Result<LatencyRule, String> {
        self.temperature_direction_check()?;
        let min_severity = match self.min_severity.as_str() {
            "Unknown" => Severity::Unknown,
            "Minor" => Severity::Minor,
            "Moderate" => Severity::Moderate,
            "Severe" => Severity::Severe,
            "Extreme" => Severity::Extreme,
            other => return Err(format!("unknown severity {other:?}")),
        };
        let side = match self.side.as_str() {
            "yes" | "Yes" | "YES" => Side::Yes,
            "no" | "No" | "NO" => Side::No,
            other => return Err(format!("unknown side {other:?}")),
        };
        if !(1..=99).contains(&self.max_price_cents) {
            return Err(format!(
                "max_price_cents {} out of 1..=99",
                self.max_price_cents
            ));
        }
        if self.size == 0 || self.size > 100 {
            return Err(format!("size {} out of 1..=100", self.size));
        }
        if self.event_substring.is_empty() {
            return Err("event_substring empty".into());
        }
        Ok(LatencyRule {
            event_substring: self.event_substring,
            area_substring: self.area_substring,
            min_severity,
            kalshi_market: MarketTicker::new(&self.market_ticker),
            side,
            action: Action::Buy,
            max_price_cents: self.max_price_cents,
            size: self.size,
        })
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'static str,
    max_tokens: u32,
    system: &'static str,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Call Claude on a batch of markets. Returns the proposed rules.
/// Caller is expected to call `into_rule()` on each to validate
/// and convert.
pub async fn propose_rules(markets: &[WeatherMarket]) -> Result<Vec<CuratedRule>, CuratorError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| CuratorError::MissingApiKey)?;
    let user = user_message(markets);
    let body = AnthropicRequest {
        model: MODEL,
        max_tokens: MAX_TOKENS,
        system: SYSTEM_PROMPT,
        messages: vec![AnthropicMessage {
            role: "user",
            content: &user,
        }],
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_mins(2))
        .build()?;
    let resp = client
        .post(ANTHROPIC_API)
        .header("x-api-key", &api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(CuratorError::Api {
            status: status.as_u16(),
            body: text,
        });
    }
    let parsed: AnthropicResponse = serde_json::from_str(&text)
        .map_err(|e| CuratorError::Decode(format!("envelope: {e} — body: {text}")))?;
    if let Some(u) = parsed.usage {
        info!(
            in_tokens = u.input_tokens,
            out_tokens = u.output_tokens,
            "anthropic usage"
        );
    }
    let raw = parsed
        .content
        .into_iter()
        .find(|c| c.kind == "text")
        .and_then(|c| c.text)
        .ok_or_else(|| CuratorError::Decode("no text block in response".into()))?;
    let cleaned = strip_markdown_fences(&raw);
    let rules: Vec<CuratedRule> = serde_json::from_str(cleaned)
        .map_err(|e| CuratorError::Decode(format!("rules array: {e} — raw: {raw}")))?;
    if rules.is_empty() {
        warn!("model returned 0 rules for {} markets", markets.len());
    } else {
        info!(proposed = rules.len(), of = markets.len(), "rules proposed");
    }
    Ok(rules)
}

/// Models occasionally wrap their JSON in ```json ... ``` despite
/// being asked not to. Strip a single leading/trailing fence if
/// present rather than failing the parse.
fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_rule(price: u8, severity: &str, side: &str) -> CuratedRule {
        CuratedRule {
            market_ticker: "KX-T".into(),
            reasoning: "test".into(),
            event_substring: "Tornado".into(),
            area_substring: Some("TX".into()),
            min_severity: severity.into(),
            side: side.into(),
            max_price_cents: price,
            size: 1,
        }
    }

    #[test]
    fn into_rule_round_trips_a_valid_proposal() {
        let r = raw_rule(35, "Severe", "yes").into_rule().expect("valid");
        assert_eq!(r.max_price_cents, 35);
        assert_eq!(r.kalshi_market.as_str(), "KX-T");
        assert_eq!(r.min_severity, Severity::Severe);
        assert_eq!(r.side, Side::Yes);
    }

    #[test]
    fn rejects_out_of_range_price() {
        assert!(raw_rule(0, "Severe", "yes").into_rule().is_err());
        assert!(raw_rule(100, "Severe", "yes").into_rule().is_err());
    }

    #[test]
    fn rejects_unknown_severity() {
        assert!(raw_rule(35, "Catastrophic", "yes").into_rule().is_err());
    }

    #[test]
    fn rejects_unknown_side() {
        assert!(raw_rule(35, "Severe", "maybe").into_rule().is_err());
    }

    fn temp_rule(market: &str, event: &str, side: &str) -> CuratedRule {
        CuratedRule {
            market_ticker: market.into(),
            reasoning: "test".into(),
            event_substring: event.into(),
            area_substring: Some("TX".into()),
            min_severity: "Severe".into(),
            side: side.into(),
            max_price_cents: 30,
            size: 1,
        }
    }

    #[test]
    fn temperature_direction_catches_heat_on_high_below_with_yes() {
        // "Austin high < 88" + Excessive Heat + side=yes is wrong:
        // heat makes "high < 88" LESS likely; should buy NO.
        let r = temp_rule("KXHIGHTAUS-26MAY05-B88", "Excessive Heat Warning", "yes");
        assert!(r.into_rule().is_err());
    }

    #[test]
    fn temperature_direction_passes_heat_on_high_above_with_yes() {
        // "Austin high ≥ 88" + Excessive Heat + side=yes is right.
        let r = temp_rule("KXHIGHTAUS-26MAY05-T88", "Excessive Heat Warning", "yes");
        assert!(r.into_rule().is_ok());
    }

    #[test]
    fn temperature_direction_passes_freeze_on_low_above_with_no() {
        // "Boston low ≥ 51" + Freeze Warning + side=no is right
        // (freeze makes low LESS likely to clear 51).
        let r = temp_rule("KXLOWTBOS-26MAY05-T51", "Freeze Warning", "no");
        assert!(r.into_rule().is_ok());
    }

    #[test]
    fn temperature_direction_catches_freeze_on_low_below_with_no() {
        // "Boston low < 51" + Freeze + side=no is wrong: freeze
        // makes low MORE likely to be below 51, so buy YES.
        let r = temp_rule("KXLOWTBOS-26MAY05-B51.5", "Freeze Warning", "no");
        assert!(r.into_rule().is_err());
    }

    #[test]
    fn temperature_check_skips_non_temp_markets() {
        // Hurricane market shouldn't be touched by temp logic
        // even though it's in our weather set.
        let r = temp_rule("KXHURMIA-26", "Hurricane Warning", "yes");
        assert!(r.into_rule().is_ok());
    }

    #[test]
    fn strips_markdown_fence_when_present() {
        let s = "```json\n[]\n```";
        assert_eq!(strip_markdown_fences(s), "[]");
    }

    #[test]
    fn no_fence_passthrough() {
        let s = "[]";
        assert_eq!(strip_markdown_fences(s), "[]");
    }
}
