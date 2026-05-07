//! Discovery service — periodic Kalshi-REST scan that emits
//! [`Event::DiscoveryDelta`] into a strategy's supervisor and
//! auto-registers newly-found tickers with the market-data router.
//!
//! Lifecycle (per [`DiscoverySubscription`]):
//!
//! 1. Operator config (engine main) declares which strategies
//!    want which subscriptions; this module spawns one
//!    [`DiscoveryWorker`] per (strategy, subscription) pair.
//! 2. Worker polls Kalshi `GET /markets?series_ticker=…` for each
//!    series in the subscription, paginating to completion.
//! 3. Worker filters by `expected_expiration_time` (preferred for
//!    per-event games) falling back to `close_time` (daily/weather
//!    markets). Markets settling outside `[now, now+max_secs]`
//!    are dropped.
//! 4. Diff vs the previous tick → `added` and `removed` ticker
//!    sets.
//! 5. For `added`: tell the router to subscribe (so book deltas
//!    flow), THEN push `DiscoveryDelta` into the supervisor's
//!    event channel. Order matters — strategies should be ready
//!    to receive book updates as soon as they see the delta.
//! 6. For `removed`: push the delta. Router-side unsubscribe is
//!    best-effort (kalshi-md exposes unsubscribe-by-sid which we
//!    don't track here; settled markets stop emitting deltas
//!    naturally).
//!
//! Failure modes: REST scan errors are warn-logged and retried
//! on the next tick. The 429-retry wrapper inside `kalshi-rest`
//! handles burst rate-limits transparently. We don't push a
//! delta on a failed tick — the strategy just sees the
//! previous-tick state.

use anyhow::{Context as _, Result};
use predigy_engine_core::discovery::{DiscoveredMarket, DiscoverySubscription};
use predigy_engine_core::events::Event;
use predigy_engine_core::strategy::StrategyId;
use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::MarketSummary;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::market_data::RouterCommand;
use crate::supervisor::Supervisor;

/// Public handle. Drop or call `shutdown` to abort all worker
/// tasks.
pub struct DiscoveryService {
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for DiscoveryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryService")
            .field("n_workers", &self.workers.len())
            .finish_non_exhaustive()
    }
}

impl DiscoveryService {
    /// Spawn one worker per (strategy, subscription) pair the
    /// supervisors declared. The supervisor's `event_tx` is used
    /// to push `Event::DiscoveryDelta`; the router's `command_tx`
    /// is used to register newly-found tickers.
    pub fn start(
        rest: Arc<RestClient>,
        router_tx: mpsc::Sender<RouterCommand>,
        supervisors: &[&Supervisor],
        subscriptions_by_strategy: &HashMap<StrategyId, Vec<DiscoverySubscription>>,
    ) -> Self {
        let mut workers = Vec::new();
        for sup in supervisors {
            let Some(subs) = subscriptions_by_strategy.get(&sup.id) else {
                continue;
            };
            for sub in subs {
                let worker = DiscoveryWorker {
                    strategy: sup.id,
                    subscription: sub.clone(),
                    rest: rest.clone(),
                    router_tx: router_tx.clone(),
                    event_tx: sup.event_tx.clone(),
                };
                let handle = tokio::spawn(worker.run());
                workers.push(handle);
            }
        }
        info!(n_workers = workers.len(), "discovery: service started");
        Self { workers }
    }

    pub async fn shutdown(self, grace: Duration) {
        for h in self.workers {
            h.abort();
            let _ = tokio::time::timeout(grace, h).await;
        }
    }
}

/// Per-(strategy, subscription) polling loop.
struct DiscoveryWorker {
    strategy: StrategyId,
    subscription: DiscoverySubscription,
    rest: Arc<RestClient>,
    router_tx: mpsc::Sender<RouterCommand>,
    event_tx: mpsc::Sender<Event>,
}

impl DiscoveryWorker {
    async fn run(self) {
        let mut tracked: HashMap<String, i64> = HashMap::new();
        let mut tick = tokio::time::interval(self.subscription.interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let scanned = match self.scan().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        strategy = self.strategy.0,
                        error = %e,
                        "discovery: scan failed; will retry"
                    );
                    continue;
                }
            };
            let (added, removed) = diff(&tracked, &scanned);
            if added.is_empty() && removed.is_empty() {
                debug!(
                    strategy = self.strategy.0,
                    watched = tracked.len(),
                    "discovery: no change"
                );
                continue;
            }
            info!(
                strategy = self.strategy.0,
                n_added = added.len(),
                n_removed = removed.len(),
                watched_now = scanned.len(),
                "discovery: delta"
            );

            // 1. Tell the router about the new tickers FIRST so
            //    book updates start flowing before the strategy
            //    sees the DiscoveryDelta.
            if !added.is_empty() {
                let new_tickers: Vec<String> = added.iter().map(|m| m.ticker.clone()).collect();
                let cmd = RouterCommand::AddTickers {
                    strategy: self.strategy,
                    markets: new_tickers,
                    tx: self.event_tx.clone(),
                };
                if self.router_tx.send(cmd).await.is_err() {
                    warn!(
                        strategy = self.strategy.0,
                        "discovery: router command channel closed; exiting"
                    );
                    return;
                }
            }

            // 2. Push the delta into the strategy.
            let delta = Event::DiscoveryDelta {
                added,
                removed: removed
                    .into_iter()
                    .map(predigy_core::market::MarketTicker::new)
                    .collect(),
            };
            if self.event_tx.send(delta).await.is_err() {
                warn!(
                    strategy = self.strategy.0,
                    "discovery: supervisor event channel closed; exiting"
                );
                return;
            }
            tracked = scanned;
        }
    }

    /// One full scan across the configured series. Returns the
    /// resulting (ticker → settle_unix) map.
    async fn scan(&self) -> Result<HashMap<String, i64>> {
        let now_unix = current_unix();
        let cutoff = now_unix.saturating_add(self.subscription.max_secs_to_settle);
        let mut out: HashMap<String, i64> = HashMap::new();
        for series in &self.subscription.series {
            let mut next_cursor: Option<String> = None;
            loop {
                let page = self
                    .rest
                    .list_markets_in_series(
                        series,
                        Some("open"),
                        Some(1000),
                        next_cursor.as_deref(),
                    )
                    .await
                    .with_context(|| format!("list_markets_in_series({series})"))?;
                for m in page.markets {
                    if let Some((ticker, settle)) =
                        eligibility_filter(&m, &self.subscription, now_unix, cutoff)
                    {
                        out.insert(ticker, settle);
                    }
                }
                match page.cursor.as_deref() {
                    Some(c) if !c.is_empty() => next_cursor = Some(c.to_string()),
                    _ => break,
                }
            }
        }
        Ok(out)
    }
}

/// Decide whether a `MarketSummary` belongs in the discovery set
/// for this subscription. Returns `Some((ticker, settle_unix))`
/// if it does, `None` otherwise. Settle time is taken from
/// `expected_expiration_time` when present (per-event games),
/// otherwise from `close_time`.
fn eligibility_filter(
    m: &MarketSummary,
    sub: &DiscoverySubscription,
    now_unix: i64,
    cutoff_unix: i64,
) -> Option<(String, i64)> {
    if sub.require_quote && m.yes_ask_dollars.is_none() {
        return None;
    }
    let settle_iso = m
        .expected_expiration_time
        .as_deref()
        .unwrap_or(m.close_time.as_str());
    let settle = parse_iso8601_to_unix(settle_iso)?;
    if settle <= now_unix || settle > cutoff_unix {
        return None;
    }
    Some((m.ticker.clone(), settle))
}

fn diff(
    prev: &HashMap<String, i64>,
    next: &HashMap<String, i64>,
) -> (Vec<DiscoveredMarket>, Vec<String>) {
    let prev_keys: HashSet<&String> = prev.keys().collect();
    let next_keys: HashSet<&String> = next.keys().collect();
    let added: Vec<DiscoveredMarket> = next_keys
        .difference(&prev_keys)
        .map(|k| DiscoveredMarket {
            ticker: (*k).clone(),
            settle_unix: next[*k],
        })
        .collect();
    let removed: Vec<String> = prev_keys
        .difference(&next_keys)
        .map(|k| (*k).clone())
        .collect();
    (added, removed)
}

fn current_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(0)
}

/// Minimal RFC3339 parser for Kalshi's `expected_expiration_time`
/// / `close_time` shape (`"YYYY-MM-DDTHH:MM:SSZ"`). Same shape as
/// the legacy settlement-trader's parser; duplicated here to keep
/// the engine binary's deps minimal.
fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
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

    fn ms(
        ticker: &str,
        close_time: &str,
        expected: Option<&str>,
        yes_ask: Option<f64>,
    ) -> MarketSummary {
        MarketSummary {
            ticker: ticker.into(),
            event_ticker: "EVT".into(),
            status: "open".into(),
            title: "T".into(),
            yes_bid_dollars: None,
            yes_ask_dollars: yes_ask,
            last_price_dollars: None,
            close_time: close_time.into(),
            expected_expiration_time: expected.map(str::to_string),
            can_close_early: None,
            floor_strike: None,
            cap_strike: None,
            strike_type: None,
            occurrence_datetime: None,
        }
    }

    fn sub(max_secs: i64, require_quote: bool) -> DiscoverySubscription {
        DiscoverySubscription {
            series: vec!["KXMLBGAME".into()],
            interval_secs: 60,
            max_secs_to_settle: max_secs,
            require_quote,
        }
    }

    #[test]
    fn parse_iso_round_trip_known_unix() {
        assert_eq!(
            parse_iso8601_to_unix("2026-05-06T00:00:00Z"),
            Some(1_778_025_600)
        );
    }

    // 1_700_000_000 = 2023-11-14 22:13:20 UTC.
    // now + 100  = 2023-11-14 22:15:00 UTC.
    // now + 1800 = 2023-11-14 22:43:20 UTC.
    // now + 3600 = 2023-11-14 23:13:20 UTC.

    #[test]
    fn eligibility_prefers_expected_over_close() {
        let now = 1_700_000_000_i64;
        let cutoff = now + 1800; // 30 min
        let market = ms(
            "KX-A",
            "2023-11-14T22:43:20Z", // close_time = now + 1800 (still inside cutoff)
            Some("2023-11-14T22:15:00Z"), // expected = now + 100
            Some(0.95),
        );
        let result = eligibility_filter(&market, &sub(1800, false), now, cutoff);
        assert!(result.is_some(), "expected market to pass filter");
        let (_t, settle) = result.unwrap();
        // expected_expiration_time wins.
        assert_eq!(settle, now + 100);
    }

    #[test]
    fn eligibility_falls_back_to_close_when_expected_absent() {
        let now = 1_700_000_000_i64;
        let cutoff = now + 1800;
        let market = ms("KX-B", "2023-11-14T22:15:00Z", None, Some(0.95));
        let result = eligibility_filter(&market, &sub(1800, false), now, cutoff);
        assert!(result.is_some());
        assert_eq!(result.unwrap().1, now + 100);
    }

    #[test]
    fn eligibility_drops_outside_window() {
        let now = 1_700_000_000_i64;
        let cutoff = now + 1800;
        // Settle is 1 hour out — past 30-min cutoff.
        let market = ms("KX-C", "2023-11-14T23:13:20Z", None, Some(0.95));
        assert!(eligibility_filter(&market, &sub(1800, false), now, cutoff).is_none());
    }

    #[test]
    fn eligibility_drops_already_settled() {
        let now = 1_700_000_000_i64;
        let cutoff = now + 1800;
        // Settle is 1 day BEFORE now.
        let market = ms("KX-D", "2023-11-13T22:13:20Z", None, Some(0.95));
        assert!(eligibility_filter(&market, &sub(1800, false), now, cutoff).is_none());
    }

    #[test]
    fn eligibility_drops_quote_missing_when_required() {
        let now = 1_700_000_000_i64;
        let cutoff = now + 1800;
        let market = ms("KX-E", "2023-11-14T22:15:00Z", None, None);
        assert!(eligibility_filter(&market, &sub(1800, true), now, cutoff).is_none());
        // Same market, require_quote=false: passes.
        assert!(eligibility_filter(&market, &sub(1800, false), now, cutoff).is_some());
    }

    #[test]
    fn diff_detects_add_and_remove() {
        let mut prev = HashMap::new();
        prev.insert("A".to_string(), 100);
        prev.insert("B".to_string(), 200);
        let mut next = HashMap::new();
        next.insert("B".to_string(), 200);
        next.insert("C".to_string(), 300);
        let (added, removed) = diff(&prev, &next);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].ticker, "C");
        assert_eq!(added[0].settle_unix, 300);
        assert_eq!(removed, vec!["A".to_string()]);
    }
}
