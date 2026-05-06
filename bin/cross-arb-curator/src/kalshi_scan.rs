//! Kalshi REST scanner — discovers open political/election/world
//! markets that are candidates for cross-venue pairing against
//! Polymarket. Mirrors the shape of `wx-curator/src/kalshi_scan.rs`
//! but pulls from a different category.

use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::MarketSummary;

const CATEGORIES: &[&str] = &["Politics", "Elections", "World", "Economics"];

#[derive(Debug, Clone)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub close_time: String,
    pub yes_ask_cents: u8,
    pub no_ask_cents: u8,
}

impl KalshiMarket {
    /// Filter rails / empty-book markets — same predicate as the
    /// weather curator. A 0¢ or 99¢ market has no edge to take.
    fn is_actionable(&self) -> bool {
        (3..=97).contains(&self.yes_ask_cents)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("kalshi rest: {0}")]
    Rest(#[from] predigy_kalshi_rest::Error),
}

/// Walk every series in the cross-arb candidate categories,
/// then for each series fetch its open markets via the
/// `series_ticker` filter. Filters out rails + empty-book markets.
pub async fn scan_political_markets(rest: &RestClient) -> Result<Vec<KalshiMarket>, ScanError> {
    let mut out = Vec::new();
    for cat in CATEGORIES {
        let series = rest.list_series_by_category(cat).await?;
        for s in &series.series {
            let markets = collect_series_markets(rest, &s.ticker).await?;
            for m in markets {
                let km = build(m);
                if km.is_actionable() {
                    out.push(km);
                }
            }
        }
    }
    Ok(out)
}

async fn collect_series_markets(
    rest: &RestClient,
    series_ticker: &str,
) -> Result<Vec<MarketSummary>, ScanError> {
    let mut accum = Vec::new();
    let mut next_cursor: Option<String> = None;
    loop {
        let response = rest
            .list_markets_in_series(
                series_ticker,
                Some("open"),
                Some(1000),
                next_cursor.as_deref(),
            )
            .await?;
        accum.extend(response.markets);
        match response.cursor.as_deref() {
            Some(c) if !c.is_empty() => next_cursor = Some(c.to_string()),
            _ => break,
        }
    }
    Ok(accum)
}

fn build(m: MarketSummary) -> KalshiMarket {
    let yes_ask_cents = dollars_to_cents(m.yes_ask_dollars);
    let no_ask_cents = m
        .yes_bid_dollars
        .map_or(0, |yb| 100u8.saturating_sub(dollars_to_cents(Some(yb))));
    KalshiMarket {
        ticker: m.ticker,
        event_ticker: m.event_ticker,
        title: m.title,
        close_time: m.close_time,
        yes_ask_cents,
        no_ask_cents,
    }
}

fn dollars_to_cents(dollars: Option<f64>) -> u8 {
    let Some(d) = dollars else { return 0 };
    let cents = (d * 100.0).round() as i32;
    u8::try_from(cents.clamp(0, 100)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ticker: &str, event: &str, title: &str, ya: f64, yb: f64) -> MarketSummary {
        MarketSummary {
            ticker: ticker.into(),
            event_ticker: event.into(),
            status: "active".into(),
            title: title.into(),
            yes_bid_dollars: Some(yb),
            yes_ask_dollars: Some(ya),
            last_price_dollars: None,
            close_time: "2026-12-31T00:00:00Z".into(),
            expected_expiration_time: None,
            can_close_early: None,
        }
    }

    #[test]
    fn build_extracts_prices() {
        let m = ms("KX-PRES-26", "KX-PRES", "President 2026", 0.45, 0.43);
        let k = build(m);
        assert_eq!(k.yes_ask_cents, 45);
        assert!(k.is_actionable());
    }

    #[test]
    fn drops_railroaded() {
        let m = ms("KX-X", "KX-X", "edge", 0.99, 0.98);
        let k = build(m);
        assert!(!k.is_actionable());
    }
}
