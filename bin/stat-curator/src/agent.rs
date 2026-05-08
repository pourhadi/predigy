//! Anthropic API client + `StatRule` synthesis.
//!
//! Calls `POST https://api.anthropic.com/v1/messages` with
//! [`prompt::SYSTEM_PROMPT`].  Decodes the response, validates each
//! proposed rule (probability range, edge gap, side direction),
//! emits `stat_trader::StatRule` ready for serialisation.
//!
//! ## Cost shape
//!
//! Sonnet 4.6 input ~$3/MTok, output ~$15/MTok.  System prompt
//! ~900 tokens, each market block ~80 tokens.  Batch of 25 markets
//! → ~2900 tokens in, ~600 tokens out → ~$0.02 per call.  Daily
//! scan with 4-8 batches → ~$0.10-0.20/day.
//!
//! [`prompt::SYSTEM_PROMPT`]: crate::prompt::SYSTEM_PROMPT

use crate::kalshi_scan::StatMarket;
use crate::prompt::{SYSTEM_PROMPT, user_message};
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use serde::{Deserialize, Serialize};
use stat_trader::StatRule;
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

/// One curated rule with its supporting reasoning.  The wire shape
/// matches what we instruct Claude to emit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedStatRule {
    pub kalshi_ticker: String,
    pub model_probability: f64,
    pub reasoning: String,
    pub confidence: String,
    pub min_edge_cents: u32,
}

impl CuratedStatRule {
    /// Validate + convert to a `StatRule`, choosing the trade side
    /// (Yes / No) based on which direction the model probability
    /// implies.  Returns the converted rule plus the chosen side
    /// for audit logging.
    ///
    /// `yes_ask_cents` and `no_ask_cents` come from the matching
    /// `StatMarket` at scan time; we use them to verify a real
    /// edge gap before committing the rule.
    pub fn into_rule(
        self,
        yes_ask_cents: u8,
        no_ask_cents: u8,
    ) -> Result<(StatRule, Side, String), String> {
        // 1. Probability range.
        if !(0.05..=0.95).contains(&self.model_probability) {
            return Err(format!(
                "model_probability {:.3} outside [0.05, 0.95]",
                self.model_probability
            ));
        }
        // 2. Confidence level.
        let conf = self.confidence.to_lowercase();
        if conf != "high" && conf != "medium" {
            return Err(format!("confidence {:?} not high/medium", self.confidence));
        }
        // 3. Min edge in (1, 25).  Larger values mean over-conservatism
        // that produces no fires; smaller values risk no-edge trades.
        if !(1..=25).contains(&self.min_edge_cents) {
            return Err(format!(
                "min_edge_cents {} outside [1, 25]",
                self.min_edge_cents
            ));
        }
        // 4. Determine side from probability vs ask.  Buy YES iff
        // model_p × 100 - yes_ask_cents >= min_edge_cents.  Buy NO
        // iff (1 - model_p) × 100 - no_ask_cents >= min_edge_cents.
        let p_cents_yes = (self.model_probability * 100.0).round() as i32;
        let p_cents_no = 100 - p_cents_yes;
        let yes_edge = p_cents_yes - i32::from(yes_ask_cents);
        let no_edge = p_cents_no - i32::from(no_ask_cents);
        let edge_threshold = i32::try_from(self.min_edge_cents).unwrap_or(i32::MAX);
        let (side, edge) = if yes_edge >= edge_threshold && yes_edge >= no_edge {
            (Side::Yes, yes_edge)
        } else if no_edge >= edge_threshold {
            (Side::No, no_edge)
        } else {
            return Err(format!(
                "no edge: yes={p_cents_yes}-{yes_ask_cents}={yes_edge}, \
                 no={p_cents_no}-{no_ask_cents}={no_edge}, threshold={}",
                self.min_edge_cents
            ));
        };
        // 5. Build the StatRule.  Defensive ticker shape check: a
        // hallucinated ticker would fail elsewhere but we'd rather
        // catch it here.  Empty / whitespace-only strings are the
        // only thing that obviously doesn't make sense at this
        // layer; Kalshi format validation lives in the runtime.
        if self.kalshi_ticker.trim().is_empty() {
            return Err("kalshi_ticker is empty".into());
        }
        let market = MarketTicker::new(&self.kalshi_ticker);
        let rule = StatRule {
            kalshi_market: market,
            model_p: self.model_probability,
            side,
            min_edge_cents: self.min_edge_cents,
            settlement_date: None,
            generated_at_utc: None,
        };
        let audit = format!(
            "{} {:?} model_p={:.3} edge={} cents conf={}",
            self.kalshi_ticker, side, self.model_probability, edge, self.confidence
        );
        Ok((rule, side, audit))
    }
}

#[derive(Debug, Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
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

/// Call Claude on a batch of markets.  Returns the proposed rules.
/// Caller is expected to call `into_rule()` on each to validate
/// and convert.
pub async fn propose_rules(markets: &[StatMarket]) -> Result<Vec<CuratedStatRule>, CuratorError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| CuratorError::MissingApiKey)?;
    let user = user_message(markets);
    // Sonnet 4.6 doesn't support assistant-message prefill, so we
    // can't force a `[` start that way.  Instead the parser-side
    // `extract_json_array` locates the JSON array even if Claude
    // prepends analysis prose, which it does ~50% of the time on
    // the kind of probability-calibration task this curator runs.
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
    let cleaned = extract_json_array(strip_markdown_fences(&raw))
        .ok_or_else(|| CuratorError::Decode(format!("no JSON array found in: {raw}")))?;
    let rules: Vec<CuratedStatRule> = serde_json::from_str(cleaned)
        .map_err(|e| CuratorError::Decode(format!("rules array: {e} — raw: {raw}")))?;
    if rules.is_empty() {
        warn!("model returned 0 rules for {} markets", markets.len());
    } else {
        info!(proposed = rules.len(), of = markets.len(), "rules proposed");
    }
    Ok(rules)
}

/// Models occasionally wrap their JSON in ```json ... ``` despite
/// being asked not to.  Strip a single leading/trailing fence if
/// present rather than failing the parse.
fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Extract the outermost JSON array `[...]` from a string that may
/// contain leading prose / markdown analysis.  Sonnet 4.6 in
/// extended-reasoning mode often produces narrative analysis
/// before the requested JSON despite explicit instructions to
/// output JSON only.  Rather than fight the model, we just locate
/// the first `[` and the matching closing `]` (depth-balanced).
///
/// Returns None if no valid JSON array shape is found.  Inside
/// strings (`"..."`) brackets are ignored to handle JSON-encoded
/// arrays that contain bracket characters in field values.
fn extract_json_array(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'[')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' if !in_string => depth += 1,
            b']' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_rule(p: f64, edge: u32, conf: &str) -> CuratedStatRule {
        CuratedStatRule {
            kalshi_ticker: "KX-T".into(),
            model_probability: p,
            reasoning: "test".into(),
            confidence: conf.into(),
            min_edge_cents: edge,
        }
    }

    #[test]
    fn into_rule_emits_yes_when_model_above_yes_ask() {
        // Model says 0.62 (62¢), yes_ask=55, no_ask=47.
        // Yes edge = 62 - 55 = 7.  No edge = 38 - 47 = -9.
        // With edge threshold 4, picks YES.
        let r = raw_rule(0.62, 4, "high");
        let (rule, side, _audit) = r.into_rule(55, 47).unwrap();
        assert_eq!(side, Side::Yes);
        assert!((rule.model_p - 0.62).abs() < 1e-9);
        assert_eq!(rule.min_edge_cents, 4);
    }

    #[test]
    fn into_rule_emits_no_when_model_below_yes_price_implies_no_edge() {
        // Model says 0.30 (30¢), yes_ask=55, no_ask=47.
        // Yes edge = 30 - 55 = -25.  No edge = 70 - 47 = 23.
        // With threshold 4, picks NO.
        let r = raw_rule(0.30, 4, "high");
        let (rule, side, _audit) = r.into_rule(55, 47).unwrap();
        assert_eq!(side, Side::No);
        assert!((rule.model_p - 0.30).abs() < 1e-9);
    }

    #[test]
    fn rejects_no_edge() {
        // Model says 0.50, both sides at 47/47.  Neither side
        // clears the 4-cent threshold.
        let r = raw_rule(0.50, 4, "high");
        assert!(r.into_rule(47, 47).is_err());
    }

    #[test]
    fn rejects_extreme_probability() {
        let r = raw_rule(0.97, 4, "high");
        assert!(r.into_rule(50, 50).is_err());
        let r = raw_rule(0.02, 4, "high");
        assert!(r.into_rule(50, 50).is_err());
    }

    #[test]
    fn rejects_unknown_confidence() {
        let r = raw_rule(0.65, 4, "low");
        assert!(r.into_rule(55, 47).is_err());
    }

    #[test]
    fn rejects_silly_min_edge() {
        let r = raw_rule(0.65, 0, "high");
        assert!(r.into_rule(55, 47).is_err());
        let r = raw_rule(0.65, 50, "high");
        assert!(r.into_rule(55, 47).is_err());
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

    #[test]
    fn extract_finds_array_after_prose() {
        let s = "I need to analyze.\n\nAfter careful review,\n[\n  {\"k\": 1}\n]";
        assert_eq!(extract_json_array(s).unwrap(), "[\n  {\"k\": 1}\n]");
    }

    #[test]
    fn extract_handles_nested_arrays() {
        let s = "prose [{\"a\":[1,2]}] trailing";
        assert_eq!(extract_json_array(s).unwrap(), "[{\"a\":[1,2]}]");
    }

    #[test]
    fn extract_handles_brackets_in_strings() {
        let s = "[{\"q\": \"contains [bracket]\"}]";
        assert_eq!(
            extract_json_array(s).unwrap(),
            "[{\"q\": \"contains [bracket]\"}]"
        );
    }

    #[test]
    fn extract_returns_none_when_no_array() {
        assert!(extract_json_array("just prose").is_none());
        assert!(extract_json_array("only opening [").is_none());
    }
}
