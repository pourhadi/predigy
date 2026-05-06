//! Anthropic API client + pair synthesis. Mirrors the wx-curator
//! `agent` module — same Sonnet 4.6 endpoint, same Messages API,
//! same untagged `text` content extraction.

use crate::kalshi_scan::KalshiMarket;
use crate::poly_scan::PolyMarket;
use crate::prompt::{SYSTEM_PROMPT, user_message};
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

/// One curated pair. `serde` shape matches what we instruct Claude
/// to emit (see `prompt.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedPair {
    pub kalshi_ticker: String,
    pub poly_token_id: String,
    #[serde(default)]
    pub reasoning: String,
    /// `"high" | "medium" | "low"`. Curator drops `"low"`.
    #[serde(default)]
    pub settlement_alignment: String,
    /// `"high" | "medium"`.
    #[serde(default)]
    pub confidence: String,
}

impl CuratedPair {
    /// Validate + normalise. Returns `Err(reason)` if the pair
    /// should be dropped (low alignment / missing fields).
    pub fn validate(&self) -> Result<(), String> {
        if self.kalshi_ticker.is_empty() {
            return Err("empty kalshi_ticker".into());
        }
        // Polymarket token ids are very long decimal strings (see
        // the gamma-API response shape). Reject anything too short
        // to be plausible — Claude has been known to invent or
        // truncate.
        if self.poly_token_id.len() < 30 || !self.poly_token_id.chars().all(|c| c.is_ascii_digit())
        {
            return Err(format!(
                "poly_token_id {:?} doesn't look like a Polymarket token id",
                self.poly_token_id
            ));
        }
        match self.settlement_alignment.as_str() {
            "high" | "medium" => {}
            "low" => return Err("settlement_alignment=low".into()),
            other => return Err(format!("unknown settlement_alignment {other:?}")),
        }
        match self.confidence.as_str() {
            "high" | "medium" => Ok(()),
            other => Err(format!("unknown/low confidence {other:?}")),
        }
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

/// Send a Kalshi-list + Polymarket-list batch to Claude. Returns
/// the raw `CuratedPair` proposals; caller validates each.
pub async fn propose_pairs(
    kalshi: &[KalshiMarket],
    poly: &[PolyMarket],
) -> Result<Vec<CuratedPair>, CuratorError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| CuratorError::MissingApiKey)?;
    let user = user_message(kalshi, poly);
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
    let pairs: Vec<CuratedPair> = serde_json::from_str(cleaned)
        .map_err(|e| CuratorError::Decode(format!("pairs array: {e} — raw: {raw}")))?;
    if pairs.is_empty() {
        warn!(
            kalshi = kalshi.len(),
            poly = poly.len(),
            "model returned 0 pairs"
        );
    } else {
        info!(proposed = pairs.len(), "pairs proposed");
    }
    Ok(pairs)
}

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

    fn p(token: &str, conf: &str, sa: &str) -> CuratedPair {
        CuratedPair {
            kalshi_ticker: "KX-A".into(),
            poly_token_id: token.into(),
            reasoning: "test".into(),
            settlement_alignment: sa.into(),
            confidence: conf.into(),
        }
    }

    #[test]
    fn validate_accepts_high_confidence_with_long_numeric_token() {
        let pair = p(&"1".repeat(40), "high", "high");
        assert!(pair.validate().is_ok());
    }

    #[test]
    fn validate_rejects_short_token() {
        let pair = p("short", "high", "high");
        assert!(pair.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_numeric_token() {
        // 40-char hex is not all digits.
        let pair = p(&"a".repeat(40), "high", "high");
        assert!(pair.validate().is_err());
    }

    #[test]
    fn validate_rejects_low_alignment() {
        let pair = p(&"1".repeat(40), "high", "low");
        assert!(pair.validate().is_err());
    }

    #[test]
    fn validate_rejects_unknown_confidence() {
        let pair = p(&"1".repeat(40), "yolo", "high");
        assert!(pair.validate().is_err());
    }

    #[test]
    fn strip_fences_unwraps_json_blocks() {
        assert_eq!(strip_markdown_fences("```json\n[]\n```"), "[]");
        assert_eq!(strip_markdown_fences("```\n[]\n```"), "[]");
        assert_eq!(strip_markdown_fences("[]"), "[]");
    }
}
