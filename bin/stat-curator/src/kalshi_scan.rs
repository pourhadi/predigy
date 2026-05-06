//! Kalshi REST scanner for `stat-curator`.
//!
//! Different from `wx-curator`'s scan in two ways:
//!
//! 1. **Broader category set.**  We scan markets in Sports,
//!    Politics, Elections, World, Economics, and Culture — anywhere
//!    Claude can plausibly produce a calibrated probability from
//!    its training-data + general-purpose reasoning.  Climate-and-
//!    weather is intentionally excluded; that lane is owned by
//!    `wx-curator` + `latency-trader` and works on event-fire rules,
//!    not probability calibration.
//!
//! 2. **Settlement-horizon filter.**  Statistical bets compound
//!    poorly when held for months — the model probability drifts
//!    with new information that we don't re-calibrate against
//!    intra-trade.  We restrict candidates to markets settling
//!    within `max_days_to_settle` (default 14) so the curator's
//!    daily re-run can keep the rule set fresh.
//!
//! No Anthropic call; this module is pure REST + filtering.

use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::MarketSummary;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Categories the curator scans.  Excludes "Climate and Weather"
/// (owned by `wx-curator`).  The list is small enough to enumerate;
/// adding more would mean wider coverage but bigger Anthropic prompts.
pub const DEFAULT_CATEGORIES: &[&str] = &[
    "Sports",
    "Politics",
    "Elections",
    "World",
    "Economics",
    "Culture",
];

#[derive(Debug, Clone)]
pub struct StatMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub close_time: String,
    pub yes_ask_cents: u8,
    pub no_ask_cents: u8,
    pub category: String,
}

impl StatMarket {
    /// Filter markets where YES is at the rails or has no quotes.
    /// Same logic as `wx-curator`'s actionable filter — markets at
    /// 99¢/1¢/0 quotes have no edge available.
    fn is_actionable(&self) -> bool {
        (3..=97).contains(&self.yes_ask_cents)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("kalshi rest: {0}")]
    Rest(#[from] predigy_kalshi_rest::Error),
}

/// Scan all configured categories and return actionable markets
/// settling within the horizon.
///
/// Returns at most `max_markets` (sorted by `close_time` ascending)
/// to bound the Anthropic-call cost downstream.  The cap is enforced
/// after the actionable + horizon filter, so a tightly-bounded run
/// still gets the freshest markets across all categories.
pub async fn scan_stat_markets(
    rest: &RestClient,
    categories: &[&str],
    max_days_to_settle: i64,
    max_markets: usize,
) -> Result<Vec<StatMarket>, ScanError> {
    let now_unix = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    let horizon_unix = now_unix.saturating_add(max_days_to_settle.saturating_mul(86_400));

    let mut out: Vec<StatMarket> = Vec::new();
    for category in categories {
        let series = match rest.list_series_by_category(category).await {
            Ok(s) => s,
            Err(e) => {
                warn!(category = %category, error = %e, "category lookup failed; skipping");
                continue;
            }
        };
        for s in &series.series {
            let markets = match collect_series_markets(rest, &s.ticker).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(series = %s.ticker, error = %e, "series scan failed; skipping");
                    continue;
                }
            };
            for m in markets {
                let close_unix = parse_close_time_unix(&m.close_time).unwrap_or(i64::MAX);
                if close_unix <= now_unix || close_unix > horizon_unix {
                    continue; // out of horizon
                }
                let sm = build(m, category);
                if sm.is_actionable() {
                    out.push(sm);
                }
            }
        }
    }

    // Sort by close_time ascending — fresher markets first.
    out.sort_by(|a, b| a.close_time.cmp(&b.close_time));
    out.truncate(max_markets);
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

fn build(m: MarketSummary, category: &str) -> StatMarket {
    let yes_ask_cents = dollars_to_cents(m.yes_ask_dollars);
    let no_ask_cents = m
        .yes_bid_dollars
        .map_or(0, |yb| 100u8.saturating_sub(dollars_to_cents(Some(yb))));
    StatMarket {
        ticker: m.ticker,
        event_ticker: m.event_ticker,
        title: m.title,
        close_time: m.close_time,
        yes_ask_cents,
        no_ask_cents,
        category: category.to_string(),
    }
}

fn dollars_to_cents(dollars: Option<f64>) -> u8 {
    let Some(d) = dollars else {
        return 0;
    };
    let cents = (d * 100.0).round() as i32;
    u8::try_from(cents.clamp(0, 100)).unwrap_or(0)
}

fn parse_close_time_unix(s: &str) -> Option<i64> {
    // Kalshi close_time is RFC3339 (e.g. "2026-05-08T20:00:00Z").
    // We don't need full RFC3339 parsing; a few well-known formats
    // cover the production output.  Fall back to None when the
    // shape diverges so the caller treats the market as out-of-
    // horizon (safer than treating it as evergreen).
    chrono_lite_unix(s)
}

/// Tiny stub of an RFC3339 parser — only handles
/// `YYYY-MM-DDTHH:MM:SSZ` and `YYYY-MM-DDTHH:MM:SS+00:00`.  Returns
/// None on anything else.
fn chrono_lite_unix(s: &str) -> Option<i64> {
    let s = s.trim_end_matches('Z');
    let s = s.split('+').next().unwrap_or(s);
    let (date_part, time_part) = s.split_once('T')?;
    let mut date_iter = date_part.split('-');
    let year: i32 = date_iter.next()?.parse().ok()?;
    let month: u32 = date_iter.next()?.parse().ok()?;
    let day: u32 = date_iter.next()?.parse().ok()?;
    let mut time_iter = time_part.split(':');
    let hour: u32 = time_iter.next()?.parse().ok()?;
    let minute: u32 = time_iter.next()?.parse().ok()?;
    let second: u32 = time_iter.next()?.parse().ok()?;
    // Convert to unix without external deps.  Naive implementation
    // assumes UTC.  Handles dates from 1970..2100 cleanly.
    let days_in_month = |y: i32, m: u32| -> Option<u32> {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
            4 | 6 | 9 | 11 => Some(30),
            2 => {
                if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    Some(29)
                } else {
                    Some(28)
                }
            }
            _ => None,
        }
    };
    if !(1970..=2100).contains(&year) {
        return None;
    }
    // Validate the day-of-month against the month's actual length.
    let dim = days_in_month(year, month)?;
    if day == 0 || day > dim {
        return None;
    }
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
    }
    for m in 1..month {
        days += i64::from(days_in_month(year, m)?);
    }
    days += i64::from(day - 1);
    let secs = days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    Some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ticker: &str, event: &str, title: &str, ya: f64, yb: f64, close: &str) -> MarketSummary {
        MarketSummary {
            ticker: ticker.into(),
            event_ticker: event.into(),
            status: "active".into(),
            title: title.into(),
            yes_bid_dollars: Some(yb),
            yes_ask_dollars: Some(ya),
            last_price_dollars: None,
            close_time: close.into(),
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
            "KXLAKERS-26MAY07",
            "KXLAKERS",
            "Lakers win May 7",
            0.55,
            0.53,
            "2026-05-07T23:00:00Z",
        );
        let sm = build(m, "Sports");
        assert_eq!(sm.yes_ask_cents, 55);
        assert_eq!(sm.category, "Sports");
        assert!(sm.is_actionable());
    }

    #[test]
    fn drops_railroaded_markets() {
        let m = ms("X", "X", "edge", 0.99, 0.98, "2026-12-31T00:00:00Z");
        assert!(!build(m, "Sports").is_actionable());
    }

    #[test]
    fn drops_empty_book() {
        let m = ms("X", "X", "no quotes", 0.0, 0.0, "2026-12-31T00:00:00Z");
        assert!(!build(m, "Sports").is_actionable());
    }

    #[test]
    fn parses_rfc3339_z() {
        // 2026-05-07 20:00:00 UTC = 1_778_184_000 unix seconds.
        let u = chrono_lite_unix("2026-05-07T20:00:00Z").unwrap();
        assert_eq!(u, 1_778_184_000);
    }

    #[test]
    fn parses_rfc3339_offset() {
        // We strip the offset rather than apply it (production
        // close_time is always Z; this is just for parser
        // robustness against the alternate format).
        let u = chrono_lite_unix("2026-05-07T20:00:00+00:00").unwrap();
        assert_eq!(u, 1_778_184_000);
    }

    #[test]
    fn rejects_bad_format() {
        assert!(chrono_lite_unix("nope").is_none());
        assert!(chrono_lite_unix("not-even-iso").is_none());
        assert!(chrono_lite_unix("").is_none());
    }
}
