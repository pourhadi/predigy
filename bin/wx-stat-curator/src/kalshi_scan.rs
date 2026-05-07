//! Scan Kalshi temperature-market series for actionable markets.
//!
//! Different from `wx-curator`'s scan in two ways:
//!
//! 1. **Filtered series.** We only ingest series this curator can
//!    price — `KXHIGH*` and `KXLOW*` daily-temperature markets.
//!    Hurricane / snowfall / hourly-directional markets are
//!    deliberately out of scope (Phase 1 — see WX_STAT_PLAN).
//! 2. **Carries the structured fields**: `floor_strike`,
//!    `cap_strike`, `strike_type`, `occurrence_datetime`. These
//!    come from the extended `MarketSummary` and are forwarded
//!    verbatim so `forecast_to_p` can compute model_p without
//!    re-parsing.

use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::{MarketSummary, SeriesSummary};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct TempMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
    pub close_time: String,
    pub yes_ask_cents: u8,
    pub no_ask_cents: u8,
    pub strike_type: Option<String>,
    pub floor_strike: Option<f64>,
    pub cap_strike: Option<f64>,
    pub occurrence_datetime: Option<String>,
}

impl TempMarket {
    /// Reject 99¢/1¢ rails and empty books — same logic as
    /// wx-curator. Markets with no quotes have no edge available.
    pub fn is_actionable(&self) -> bool {
        (3..=97).contains(&self.yes_ask_cents)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("kalshi rest: {0}")]
    Rest(#[from] predigy_kalshi_rest::Error),
}

/// Scan all Climate-and-Weather series and return only the
/// daily-high / daily-low markets that have actionable quotes.
pub async fn scan_temp_markets(rest: &RestClient) -> Result<Vec<TempMarket>, ScanError> {
    let series = rest.list_series_by_category("Climate and Weather").await?;
    let mut out = Vec::new();
    for s in &series.series {
        if !is_temp_series(s) {
            continue;
        }
        let markets = match collect_series_markets(rest, &s.ticker).await {
            Ok(m) => m,
            Err(e) => {
                warn!(series = %s.ticker, error = %e, "series scan failed; skipping");
                continue;
            }
        };
        for m in markets {
            let tm = build(m, &s.ticker);
            if tm.is_actionable() {
                out.push(tm);
            }
        }
    }
    out.sort_by(|a, b| a.close_time.cmp(&b.close_time));
    Ok(out)
}

/// Whether a series ticker looks like a daily-temperature market
/// (KXHIGH* / KXLOW*) — and not snowfall / hurricane / hourly.
/// Excludes hourly directional (`KXTEMPMI...`) and monthly avg
/// (`KXAVGTEMP...`).
fn is_temp_series(s: &SeriesSummary) -> bool {
    let t = &s.ticker;
    if t.starts_with("KXHIGHT") || t.starts_with("KXLOWT") {
        return true;
    }
    // KXHIGH<airport> / KXLOW<airport>; exclude KXHIGHT* (already
    // handled above), KXHIGHTDC etc. that have a 'T' in them.
    if t.starts_with("KXHIGH") || t.starts_with("KXLOW") {
        return true;
    }
    false
}

async fn collect_series_markets(
    rest: &RestClient,
    series_ticker: &str,
) -> Result<Vec<MarketSummary>, ScanError> {
    let mut accum = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let response = rest
            .list_markets_in_series(series_ticker, Some("open"), Some(1000), cursor.as_deref())
            .await?;
        accum.extend(response.markets);
        match response.cursor.as_deref() {
            Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    Ok(accum)
}

fn build(m: MarketSummary, series_ticker: &str) -> TempMarket {
    let yes_ask_cents = dollars_to_cents(m.yes_ask_dollars);
    let no_ask_cents = m
        .yes_bid_dollars
        .map_or(0, |yb| 100u8.saturating_sub(dollars_to_cents(Some(yb))));
    TempMarket {
        ticker: m.ticker,
        event_ticker: m.event_ticker,
        series_ticker: series_ticker.to_string(),
        title: m.title,
        close_time: m.close_time,
        yes_ask_cents,
        no_ask_cents,
        strike_type: m.strike_type,
        floor_strike: m.floor_strike,
        cap_strike: m.cap_strike,
        occurrence_datetime: m.occurrence_datetime,
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

    fn series(ticker: &str) -> SeriesSummary {
        SeriesSummary {
            ticker: ticker.into(),
            title: None,
            category: None,
        }
    }

    #[test]
    fn is_temp_series_accepts_high_low_prefixes() {
        assert!(is_temp_series(&series("KXHIGHDEN")));
        assert!(is_temp_series(&series("KXLOWNY")));
        assert!(is_temp_series(&series("KXHIGHTDC")));
        assert!(is_temp_series(&series("KXLOWTPHX")));
    }

    #[test]
    fn is_temp_series_rejects_other_weather_series() {
        assert!(!is_temp_series(&series("KXSFOSNOWM")));
        assert!(!is_temp_series(&series("HURCLAND")));
        assert!(!is_temp_series(&series("KXAVGTEMP")));
        assert!(!is_temp_series(&series("KXKILAUEA")));
    }

    fn ms(
        ticker: &str,
        event: &str,
        title: &str,
        ya: f64,
        yb: f64,
        floor: Option<f64>,
        st: Option<&str>,
    ) -> MarketSummary {
        MarketSummary {
            ticker: ticker.into(),
            event_ticker: event.into(),
            status: "active".into(),
            title: title.into(),
            yes_bid_dollars: Some(yb),
            yes_ask_dollars: Some(ya),
            last_price_dollars: None,
            close_time: "2026-05-08T06:59:00Z".into(),
            expected_expiration_time: None,
            can_close_early: None,
            floor_strike: floor,
            cap_strike: None,
            strike_type: st.map(str::to_string),
            occurrence_datetime: Some("2026-05-07T14:00:00Z".into()),
        }
    }

    #[test]
    fn build_carries_strike_metadata() {
        let m = ms(
            "KXHIGHDEN-26MAY07-T68",
            "KXHIGHDEN-26MAY07",
            ">68F",
            0.48,
            0.47,
            Some(68.0),
            Some("greater"),
        );
        let tm = build(m, "KXHIGHDEN");
        assert_eq!(tm.floor_strike, Some(68.0));
        assert_eq!(tm.strike_type.as_deref(), Some("greater"));
        assert_eq!(tm.series_ticker, "KXHIGHDEN");
        assert!(tm.is_actionable());
    }

    #[test]
    fn build_drops_railroaded() {
        let m = ms("X", "X", "edge", 0.99, 0.98, Some(50.0), Some("greater"));
        let tm = build(m, "X");
        assert!(!tm.is_actionable());
    }
}
