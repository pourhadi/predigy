//! Kalshi REST scanner — discovers open Kalshi markets that are
//! candidates for cross-venue pairing against Polymarket.
//!
//! Pulls from a curated set of Kalshi categories that have rough
//! topical overlap with Polymarket's universe. Each scan applies
//! a settlement-horizon filter so we don't waste tokens on
//! long-dated markets that the cross-arb strategy doesn't have an
//! edge on (the convergence trade needs short-term mispricings).

use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::MarketSummary;

/// Categories the Kalshi `/series?category=...` endpoint accepts.
/// Verified May 2026 against live Kalshi API. Sports + Climate
/// added because Polymarket's top-volume markets are heavily
/// weighted to those categories — restricting Kalshi to politics
/// alone limited the curator to 1-2 pairs per scan.
pub const DEFAULT_CATEGORIES: &[&str] = &[
    "Politics",
    "Elections",
    "World",
    "Economics",
    "Sports",
    "Climate and Weather",
    "Culture",
];

#[derive(Debug, Clone)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    /// Best estimate of when this market actually settles. Prefers
    /// `expected_expiration_time` (per-event settlement) when
    /// present, falling back to `close_time` (calendar auction
    /// close). For sports markets these often differ by weeks.
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

/// Walk every series in `categories`, fetch its open markets, and
/// return those that:
///
/// - have an actionable price (3..=97 cents)
/// - settle within `[now_unix, now_unix + max_secs_to_settle]`
///
/// `max_secs_to_settle = i64::MAX` disables the horizon filter.
pub async fn scan_open_markets(
    rest: &RestClient,
    categories: &[&str],
    now_unix: i64,
    max_secs_to_settle: i64,
) -> Result<Vec<KalshiMarket>, ScanError> {
    let cutoff = now_unix.saturating_add(max_secs_to_settle);
    let mut out = Vec::new();
    for cat in categories {
        // Some category names may not be configured on a given
        // Kalshi account — skip 404s and similar quietly rather
        // than failing the whole scan.
        let series = match rest.list_series_by_category(cat).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(category = cat, error = %e, "category lookup failed; skipping");
                continue;
            }
        };
        for s in &series.series {
            let markets = match collect_series_markets(rest, &s.ticker).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(series = %s.ticker, error = %e, "series scan failed; skipping");
                    continue;
                }
            };
            for m in markets {
                let km = build(m);
                if !km.is_actionable() {
                    continue;
                }
                let Some(t) = parse_iso8601_to_unix(&km.close_time) else {
                    continue;
                };
                if t > now_unix && t <= cutoff {
                    out.push(km);
                }
            }
        }
    }
    Ok(out)
}

/// Backwards-compatible wrapper used by the existing one-shot
/// callers and tests. Defaults to no horizon filter.
pub async fn scan_political_markets(rest: &RestClient) -> Result<Vec<KalshiMarket>, ScanError> {
    scan_open_markets(rest, DEFAULT_CATEGORIES, 0, i64::MAX).await
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
    // Per-event settlement time wins when present and earlier than
    // the calendar close. Sports markets bake the auction close
    // weeks past the actual game.
    let settle = pick_settle_time(m.expected_expiration_time.as_deref(), &m.close_time);
    KalshiMarket {
        ticker: m.ticker,
        event_ticker: m.event_ticker,
        title: m.title,
        close_time: settle,
        yes_ask_cents,
        no_ask_cents,
    }
}

fn pick_settle_time(expected: Option<&str>, close: &str) -> String {
    let close_unix = parse_iso8601_to_unix(close);
    let expected_unix = expected.and_then(parse_iso8601_to_unix);
    match (expected_unix, close_unix, expected) {
        (Some(e), Some(c), Some(es)) if e < c => es.to_string(),
        (Some(_), None, Some(es)) => es.to_string(),
        _ => close.to_string(),
    }
}

fn dollars_to_cents(dollars: Option<f64>) -> u8 {
    let Some(d) = dollars else { return 0 };
    let cents = (d * 100.0).round() as i32;
    u8::try_from(cents.clamp(0, 100)).unwrap_or(0)
}

/// Tiny RFC3339 parser. Accepts `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[10] != b'T' {
        return None;
    }
    let year: i32 = std::str::from_utf8(bytes.get(0..4)?).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(bytes.get(5..7)?).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(bytes.get(8..10)?).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let min: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let sec: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour) * 3_600 + i64::from(min) * 60 + i64::from(sec))
}

#[allow(clippy::cast_possible_wrap)]
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m_signed = m as i32;
    let mp = if m_signed > 2 {
        m_signed - 3
    } else {
        m_signed + 9
    };
    let doy = (153 * mp + 2) as u32 / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
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

    #[test]
    fn build_prefers_expected_when_earlier() {
        let mut m = ms("KX-A", "KX-A", "Game tonight", 0.5, 0.5);
        m.close_time = "2026-12-31T00:00:00Z".into();
        m.expected_expiration_time = Some("2026-05-08T00:00:00Z".into());
        let k = build(m);
        assert!(k.close_time.starts_with("2026-05-08"));
    }

    #[test]
    fn build_falls_back_to_close_time() {
        let mut m = ms("KX-A", "KX-A", "Macro market", 0.5, 0.5);
        m.expected_expiration_time = None;
        let k = build(m);
        assert!(k.close_time.starts_with("2026-12-31"));
    }
}
