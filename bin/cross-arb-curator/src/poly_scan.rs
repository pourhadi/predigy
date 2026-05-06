//! Polymarket gamma-API scanner. Pulls active, non-closed markets
//! sorted by volume, returns just the fields we need to pair them
//! against Kalshi.
//!
//! API reference: `https://gamma-api.polymarket.com/markets`. No
//! auth required for public market data.

use serde::Deserialize;
use std::time::Duration;

const POLY_GAMMA: &str = "https://gamma-api.polymarket.com/markets";

#[derive(Debug, Clone)]
pub struct PolyMarket {
    pub id: String,
    pub question: String,
    pub description: String,
    /// First entry = YES `asset_id`, second = NO. The
    /// `cross-arb-trader` `--pair` flag wires Kalshi to the YES
    /// side, so we surface that one.
    pub yes_token_id: String,
    pub end_date_iso: Option<String>,
    pub yes_price: f64,
    pub no_price: f64,
    pub volume_num: f64,
    pub liquidity_num: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum PolyError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(String),
}

/// Pull the top-N most-liquid active Polymarket markets.
/// `min_liquidity_usd` is a hard floor — anything thinner gets
/// dropped before we send to Claude (keeps token spend down +
/// avoids pairing against unfillable Polymarket sides).
pub async fn scan_top_markets(
    limit: usize,
    min_liquidity_usd: f64,
) -> Result<Vec<PolyMarket>, PolyError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("predigy/cross-arb-curator")
        .build()?;

    // Gamma supports `?order=volume&ascending=false&limit=...`.
    // Fetch a generous number to leave room for the liquidity
    // filter to drop the dust.
    let fetch_n = (limit * 3).max(50);
    let url = format!(
        "{POLY_GAMMA}?active=true&closed=false&order=volume&ascending=false&limit={fetch_n}"
    );
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(PolyError::Decode(format!(
            "gamma {status}: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    let raw: Vec<RawMarket> = serde_json::from_slice(&bytes)
        .map_err(|e| PolyError::Decode(format!("parse markets array: {e}")))?;

    let mut out = Vec::with_capacity(limit);
    for r in raw {
        if let Some(m) = r.into_market()
            && m.liquidity_num >= min_liquidity_usd
        {
            out.push(m);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RawMarket {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// JSON-encoded `["yes_token_id", "no_token_id"]` — Polymarket
    /// returns this as a string, not as an array.
    #[serde(default, rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
    /// Same: stringified JSON `["0.555", "0.445"]`.
    #[serde(default, rename = "outcomePrices")]
    outcome_prices: Option<String>,
    #[serde(default, rename = "endDateIso")]
    end_date_iso: Option<String>,
    #[serde(default, rename = "volumeNum")]
    volume_num: Option<f64>,
    #[serde(default, rename = "liquidityNum")]
    liquidity_num: Option<f64>,
}

impl RawMarket {
    fn into_market(self) -> Option<PolyMarket> {
        let id = self.id?;
        let question = self.question?;
        let description = self.description.unwrap_or_default();
        let token_ids: Vec<String> =
            serde_json::from_str(self.clob_token_ids.as_deref().unwrap_or("[]")).ok()?;
        let yes_token_id = token_ids.into_iter().next()?;
        let prices: Vec<String> =
            serde_json::from_str(self.outcome_prices.as_deref().unwrap_or("[]")).ok()?;
        let yes_price = prices.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let no_price = prices.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        // Polymarket gives prices in [0,1]; outcome_prices sums to ~1.
        // Drop markets in the rails (resolved already in spirit).
        if !(0.02..=0.98).contains(&yes_price) {
            return None;
        }
        Some(PolyMarket {
            id,
            question,
            description,
            yes_token_id,
            end_date_iso: self.end_date_iso,
            yes_price,
            no_price,
            volume_num: self.volume_num.unwrap_or(0.0),
            liquidity_num: self.liquidity_num.unwrap_or(0.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_market_parses_canonical_shape() {
        let raw = r#"{
            "id": "540816",
            "question": "Russia-Ukraine Ceasefire before GTA VI?",
            "description": "Resolves YES if X.",
            "clobTokenIds": "[\"850\", \"252\"]",
            "outcomePrices": "[\"0.555\", \"0.445\"]",
            "endDateIso": "2026-07-31",
            "volumeNum": 1655948.4,
            "liquidityNum": 52260.83
        }"#;
        let r: RawMarket = serde_json::from_str(raw).unwrap();
        let m = r.into_market().unwrap();
        assert_eq!(m.id, "540816");
        assert_eq!(m.yes_token_id, "850");
        assert!((m.yes_price - 0.555).abs() < 1e-6);
    }

    #[test]
    fn raw_market_drops_rails() {
        let raw = r#"{
            "id": "1",
            "question": "Q",
            "description": "",
            "clobTokenIds": "[\"a\", \"b\"]",
            "outcomePrices": "[\"0.99\", \"0.01\"]",
            "endDateIso": "",
            "volumeNum": 10,
            "liquidityNum": 10
        }"#;
        let r: RawMarket = serde_json::from_str(raw).unwrap();
        assert!(r.into_market().is_none());
    }

    #[test]
    fn raw_market_drops_missing_token_ids() {
        let raw = r#"{
            "id": "2",
            "question": "Q",
            "description": "",
            "clobTokenIds": "[]",
            "outcomePrices": "[\"0.5\", \"0.5\"]",
            "endDateIso": "",
            "volumeNum": 10,
            "liquidityNum": 10
        }"#;
        let r: RawMarket = serde_json::from_str(raw).unwrap();
        assert!(r.into_market().is_none());
    }
}
