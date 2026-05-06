//! Live discovery loop for sports markets approaching settlement.
//!
//! Periodically polls Kalshi REST for the configured series, parses
//! `expected_expiration_time` (the per-event settlement, NOT the
//! calendar `close_time` which is often weeks out), and emits a
//! `DiscoveryDelta` describing which tickers to subscribe vs
//! unsubscribe relative to the previous tick.
//!
//! The trader main loop owns the subscribe/unsubscribe side
//! effects and the WS sid bookkeeping; this module is a pure data
//! pipeline plus a periodic timer.

use predigy_kalshi_rest::Client as RestClient;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Default sports series swept on each tick when the operator
/// doesn't supply `--series`. Per-event semantics (one market per
/// game) so `expected_expiration_time` ≈ game end time.
pub const DEFAULT_SERIES: &[&str] = &[
    "KXNBASERIES",
    "KXMLBGAME",
    "KXAHLGAME",
    "KXTACAPORTGAME",
    "KXEKSTRAKLASAGAME",
    "KXDFBPOKALGAME",
    "KXUECLGAME",
    "KXNWSLGAME",
    "KXNHLGAME",
    "KXNFL1HWINNER",
];

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub series: Vec<String>,
    /// How often to poll Kalshi for the universe of open markets.
    pub interval: Duration,
    /// Only watch markets whose expected settlement is within this
    /// window from now. Anything further out is dropped (we'll pick
    /// it up on a later tick when it gets closer).
    pub max_secs_to_settle: i64,
    /// Skip markets without a usable `yes_ask` price. Saves the
    /// strategy from evaluating on empty books.
    pub require_quote: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoveryDelta {
    /// New tickers to subscribe + their per-event settle epoch.
    pub add: Vec<(String, i64)>,
    /// Tickers to unsubscribe (already settled, fell out of window,
    /// or no longer listed open).
    pub remove: Vec<String>,
}

pub fn spawn(
    rest: RestClient,
    config: DiscoveryConfig,
    initial_seed: Vec<String>,
) -> mpsc::Receiver<DiscoveryDelta> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        run(rest, config, initial_seed, tx).await;
    });
    rx
}

async fn run(
    rest: RestClient,
    config: DiscoveryConfig,
    initial_seed: Vec<String>,
    tx: mpsc::Sender<DiscoveryDelta>,
) {
    let mut tracked: HashMap<String, i64> = HashMap::new();
    // The operator-supplied seed gets watched immediately; their
    // settle epoch is filled in by the first discovery tick (or
    // dropped if the market doesn't exist anymore).
    if !initial_seed.is_empty() {
        let seed_add: Vec<(String, i64)> = initial_seed.iter().map(|t| (t.clone(), 0)).collect();
        if tx
            .send(DiscoveryDelta {
                add: seed_add,
                remove: Vec::new(),
            })
            .await
            .is_err()
        {
            return;
        }
        for t in &initial_seed {
            tracked.insert(t.clone(), 0);
        }
    }

    let mut tick = tokio::time::interval(config.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let scanned = match scan(&rest, &config).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "discovery: scan failed; will retry on next tick");
                continue;
            }
        };
        let delta = diff(&tracked, &scanned);
        if delta.add.is_empty() && delta.remove.is_empty() {
            debug!(watched = tracked.len(), "discovery: no change");
            continue;
        }
        info!(
            added = delta.add.len(),
            removed = delta.remove.len(),
            watched_now = scanned.len(),
            "discovery: delta"
        );
        if tx.send(delta).await.is_err() {
            return;
        }
        tracked = scanned;
    }
}

async fn scan(
    rest: &RestClient,
    config: &DiscoveryConfig,
) -> Result<HashMap<String, i64>, predigy_kalshi_rest::Error> {
    let now_unix = current_unix();
    let cutoff = now_unix.saturating_add(config.max_secs_to_settle);
    let mut out: HashMap<String, i64> = HashMap::new();
    for series in &config.series {
        let mut next_cursor: Option<String> = None;
        loop {
            let page = rest
                .list_markets_in_series(series, Some("open"), Some(1000), next_cursor.as_deref())
                .await?;
            for m in page.markets {
                if config.require_quote && m.yes_ask_dollars.is_none() {
                    continue;
                }
                let Some(et) = m.expected_expiration_time.as_deref() else {
                    continue;
                };
                let Some(settle) = parse_iso8601_to_unix(et) else {
                    continue;
                };
                if settle <= now_unix || settle > cutoff {
                    continue;
                }
                out.insert(m.ticker, settle);
            }
            match page.cursor.as_deref() {
                Some(c) if !c.is_empty() => next_cursor = Some(c.to_string()),
                _ => break,
            }
        }
    }
    Ok(out)
}

fn diff(prev: &HashMap<String, i64>, next: &HashMap<String, i64>) -> DiscoveryDelta {
    let prev_keys: HashSet<&String> = prev.keys().collect();
    let next_keys: HashSet<&String> = next.keys().collect();
    let add: Vec<(String, i64)> = next_keys
        .difference(&prev_keys)
        .map(|k| ((*k).clone(), next[*k]))
        .collect();
    let remove: Vec<String> = prev_keys
        .difference(&next_keys)
        .map(|k| (*k).clone())
        .collect();
    DiscoveryDelta { add, remove }
}

fn current_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(0)
}

/// Minimal RFC3339 parser for the `expected_expiration_time` /
/// `close_time` shape Kalshi emits (`"YYYY-MM-DDTHH:MM:SSZ"`).
/// Avoids pulling in a date crate just for this.
pub fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let year: i32 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let min: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let sec: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds_in_day = i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);
    Some(days * 86400 + seconds_in_day)
}

/// Howard Hinnant's `days_from_civil` (epoch 1970-01-01).
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

    #[test]
    fn diff_emits_only_new_and_gone() {
        let mut prev = HashMap::new();
        prev.insert("A".to_string(), 100);
        prev.insert("B".to_string(), 200);
        let mut next = HashMap::new();
        next.insert("B".to_string(), 200);
        next.insert("C".to_string(), 300);
        let d = diff(&prev, &next);
        assert_eq!(d.add, vec![("C".to_string(), 300)]);
        assert_eq!(d.remove, vec!["A".to_string()]);
    }

    #[test]
    fn parse_iso_round_trip_known_unix() {
        // 2026-05-06T00:00:00Z = 1778025600 (verified via `date -u -j
        // -f "%Y-%m-%dT%H:%M:%SZ" 2026-05-06T00:00:00Z +%s`).
        assert_eq!(
            parse_iso8601_to_unix("2026-05-06T00:00:00Z"),
            Some(1_778_025_600)
        );
    }

    #[test]
    fn parse_iso_handles_seconds() {
        assert_eq!(
            parse_iso8601_to_unix("2026-05-06T00:01:30Z"),
            Some(1_778_025_690)
        );
    }
}
