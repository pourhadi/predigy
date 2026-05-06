//! Kalshi REST scanner — finds open markets in the
//! "Climate and Weather" category that are actionable for
//! `latency-trader` (i.e. resolve in the future, have a non-trivial
//! YES side, and aren't already saturated at 99¢/1¢).
//!
//! Doesn't call Claude — pure REST + filtering.

use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::MarketSummary;

#[derive(Debug, Clone)]
pub struct WeatherMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub close_time: String,
    pub yes_ask_cents: u8,
    pub no_ask_cents: u8,
}

impl WeatherMarket {
    /// Filter markets where YES is too cheap (already-decided NO),
    /// too expensive (already-decided YES), or completely empty
    /// (no quotes). Leaves markets actually in the trading band.
    fn is_actionable(&self) -> bool {
        // Reject anything stuck at the rails. 0 means "no quotes
        // either" — the empty-book market case.
        let mid = self.yes_ask_cents;
        (3..=97).contains(&mid)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("kalshi rest: {0}")]
    Rest(#[from] predigy_kalshi_rest::Error),
}

/// Discover every series in the "Climate and Weather" category,
/// then for each series fetch its open markets via
/// `series_ticker` filter. This is dramatically cheaper than
/// paginating the global `/markets` endpoint, which is dominated
/// by sports/election markets that we'd discard anyway.
pub async fn scan_weather_markets(rest: &RestClient) -> Result<Vec<WeatherMarket>, ScanError> {
    let series = rest.list_series_by_category("Climate and Weather").await?;
    let mut out = Vec::new();
    for s in &series.series {
        let markets = collect_series_markets(rest, &s.ticker).await?;
        for m in markets {
            let wm = build(m);
            if wm.is_actionable() {
                out.push(wm);
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

fn build(m: MarketSummary) -> WeatherMarket {
    let yes_ask_cents = dollars_to_cents(m.yes_ask_dollars);
    let no_ask_cents = m
        .yes_bid_dollars
        .map_or(0, |yb| 100u8.saturating_sub(dollars_to_cents(Some(yb))));
    WeatherMarket {
        ticker: m.ticker,
        event_ticker: m.event_ticker,
        title: m.title,
        close_time: m.close_time,
        yes_ask_cents,
        no_ask_cents,
    }
}

fn dollars_to_cents(dollars: Option<f64>) -> u8 {
    let Some(d) = dollars else {
        return 0;
    };
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
            close_time: "2026-06-01T00:00:00Z".into(),
            expected_expiration_time: None,
            can_close_early: None,
            floor_strike: None,
            cap_strike: None,
            strike_type: None,
            occurrence_datetime: None,
        }
    }

    #[test]
    fn build_extracts_prices() {
        let m = ms(
            "KXHIGHCHI-25-T75",
            "KXHIGHCHI-25",
            "Chicago > 75F",
            0.45,
            0.43,
        );
        let wm = build(m);
        assert_eq!(wm.yes_ask_cents, 45);
        assert!(wm.is_actionable());
    }

    #[test]
    fn drops_railroaded_markets() {
        let m = ms("KXHIGHCHI-X", "KXHIGHCHI-X", "edge", 0.99, 0.98);
        let wm = build(m);
        assert!(!wm.is_actionable(), "99¢ should be rejected");
    }

    #[test]
    fn drops_empty_book() {
        let m = ms("KXHIGHCHI-Y", "KXHIGHCHI-Y", "no quotes", 0.0, 0.0);
        let wm = build(m);
        assert!(!wm.is_actionable());
    }
}
