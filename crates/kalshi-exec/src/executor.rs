//! REST-based [`oms::Executor`] for Kalshi.
//!
//! Submits and cancels orders synchronously via
//! `predigy_kalshi_rest::Client`; emits [`ExecutionReport`]s for
//! `Acked` / `Rejected` / `Cancelled` directly off those calls.
//! [`PartiallyFilled`] and [`Filled`] reports come from a background
//! task that polls `/portfolio/fills` on a configurable interval.
//!
//! ## Why polling, not WS?
//!
//! Kalshi exposes a WS `fill` channel for real-time fill notifications
//! (auth-required). Polling is intentionally chosen here for the
//! first-strategy use case (intra-venue arb) — latency on the **fill**
//! side matters far less than on the **quote** side, and polling has
//! one fewer connection to keep alive than a parallel WS session.
//! When market-making lands (Phase 4), we'll add a WS-driven variant
//! behind the same trait.

use crate::error::Error as MapErr;
use crate::mapping::{fill_to_domain, order_to_create_request};
use predigy_core::order::{Order, OrderId};
use predigy_kalshi_rest::Client as RestClient;
use predigy_kalshi_rest::types::FillRecord;
use predigy_oms::{ExecutionReport, ExecutionReportKind, Executor, ExecutorError};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How many `ExecutionReport`s to buffer between the executor and the
/// OMS. Aligns with the OMS's report-channel sizing.
const REPORT_CAPACITY: usize = 4096;

/// Tunables for the polling task. Conservative defaults match
/// non-MM strategies that don't care about sub-second fill latency.
#[derive(Debug, Clone, Copy)]
pub struct PollerConfig {
    /// How often to call `GET /portfolio/fills`. The executor adds
    /// ±10% jitter so multiple deployments don't sync-poll a single
    /// account. Default 500 ms.
    pub interval: Duration,
    /// How far before the executor's start time to begin scanning for
    /// fills. Useful on a process restart to recover fills that
    /// landed during the gap. Default 60 s.
    pub initial_lookback: Duration,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            initial_lookback: Duration::from_mins(1),
        }
    }
}

/// Implements [`oms::Executor`] over Kalshi REST. Construct via
/// [`RestExecutor::spawn`]; the returned tuple is ready to plug into
/// `Oms::spawn`.
pub struct RestExecutor {
    rest: Arc<RestClient>,
    report_tx: mpsc::Sender<ExecutionReport>,
    tracked: Arc<Mutex<TrackedOrders>>,
    /// Aborted on drop so the polling task exits with the OMS shutdown.
    _poll_guard: Arc<PollGuard>,
}

impl std::fmt::Debug for RestExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestExecutor").finish_non_exhaustive()
    }
}

struct PollGuard(JoinHandle<()>);
impl Drop for PollGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RestExecutor {
    /// Spawn the executor + its polling task. Returns the executor
    /// (move into `Oms::spawn`) and the report receiver (also into
    /// `Oms::spawn`).
    pub fn spawn(
        rest: RestClient,
        config: PollerConfig,
    ) -> (Self, mpsc::Receiver<ExecutionReport>) {
        let rest = Arc::new(rest);
        let (report_tx, report_rx) = mpsc::channel(REPORT_CAPACITY);
        let tracked = Arc::new(Mutex::new(TrackedOrders::default()));

        let initial_min_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| {
                let secs = d.as_secs();
                let lookback = config.initial_lookback.as_secs();
                secs.saturating_sub(lookback)
            })
            .map_or(0, |v| i64::try_from(v).unwrap_or(0));

        let poll_task = tokio::spawn(run_poller(
            rest.clone(),
            tracked.clone(),
            report_tx.clone(),
            config.interval,
            initial_min_ts,
        ));

        let executor = Self {
            rest,
            report_tx,
            tracked,
            _poll_guard: Arc::new(PollGuard(poll_task)),
        };
        (executor, report_rx)
    }
}

impl Executor for RestExecutor {
    fn submit(&self, order: &Order) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let rest = self.rest.clone();
        let tracked = self.tracked.clone();
        let report_tx = self.report_tx.clone();
        let order = order.clone();
        async move {
            let req = match order_to_create_request(&order) {
                Ok(r) => r,
                Err(MapErr::Unsupported(reason)) => {
                    let _ = report_tx
                        .send(ExecutionReport {
                            cid: order.client_id.clone(),
                            ts_ms: now_ms(),
                            kind: ExecutionReportKind::Rejected {
                                reason: format!("unsupported intent: {reason}"),
                            },
                        })
                        .await;
                    return Err(ExecutorError::Rejected(reason.to_string()));
                }
                Err(other) => return Err(ExecutorError::Transport(other.to_string())),
            };
            match rest.create_order(&req).await {
                Ok(response) => {
                    tracked.lock().unwrap().on_submit(
                        order.client_id.clone(),
                        response.order_id.clone(),
                        order.qty.get(),
                    );
                    let _ = report_tx
                        .send(ExecutionReport {
                            cid: order.client_id.clone(),
                            ts_ms: now_ms(),
                            kind: ExecutionReportKind::Acked {
                                venue_order_id: response.order_id,
                            },
                        })
                        .await;
                    Ok(())
                }
                Err(e) => {
                    let body = e.to_string();
                    let _ = report_tx
                        .send(ExecutionReport {
                            cid: order.client_id.clone(),
                            ts_ms: now_ms(),
                            kind: ExecutionReportKind::Rejected {
                                reason: body.clone(),
                            },
                        })
                        .await;
                    Err(ExecutorError::Rejected(body))
                }
            }
        }
    }

    fn cancel(&self, cid: &OrderId) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let rest = self.rest.clone();
        let tracked = self.tracked.clone();
        let report_tx = self.report_tx.clone();
        let cid = cid.clone();
        async move {
            let venue_id = tracked.lock().unwrap().venue_id_for(&cid);
            let Some(venue_id) = venue_id else {
                return Err(ExecutorError::UnknownOrder(cid));
            };
            match rest.cancel_order(&venue_id).await {
                Ok(_) => {
                    tracked.lock().unwrap().on_cancel(&cid);
                    let _ = report_tx
                        .send(ExecutionReport {
                            cid: cid.clone(),
                            ts_ms: now_ms(),
                            kind: ExecutionReportKind::Cancelled {
                                reason: "user cancel".into(),
                            },
                        })
                        .await;
                    Ok(())
                }
                Err(predigy_kalshi_rest::Error::Api { status: 404, .. }) => {
                    tracked.lock().unwrap().on_cancel(&cid);
                    Err(ExecutorError::UnknownOrder(cid))
                }
                Err(e) => Err(ExecutorError::Transport(e.to_string())),
            }
        }
    }
}

// ---------------------------------------------------------------- poller

async fn run_poller(
    rest: Arc<RestClient>,
    tracked: Arc<Mutex<TrackedOrders>>,
    report_tx: mpsc::Sender<ExecutionReport>,
    interval: Duration,
    initial_min_ts: i64,
) {
    info!(?interval, initial_min_ts, "kalshi-exec poller starting");
    let mut min_ts = initial_min_ts;
    let mut seen_fills: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        tokio::time::sleep(jittered(interval)).await;
        if report_tx.is_closed() {
            debug!("report channel closed; poller exiting");
            return;
        }
        match rest.list_fills(None, Some(min_ts), Some(1000), None).await {
            Ok(response) => {
                if response.fills.is_empty() {
                    continue;
                }
                // Process newest-first so we can advance min_ts cleanly.
                let mut max_ts_seen = min_ts;
                let mut newest_fills: Vec<&FillRecord> = response
                    .fills
                    .iter()
                    .filter(|f| !seen_fills.contains(&f.fill_id))
                    .collect();
                // Process in chronological order so cumulative_qty
                // accumulates monotonically.
                newest_fills.sort_by_key(|f| f.ts_ms.unwrap_or_else(|| f.ts.unwrap_or(0) * 1000));

                for fill in newest_fills {
                    seen_fills.insert(fill.fill_id.clone());
                    let event_ts = fill.ts_ms.unwrap_or_else(|| fill.ts.unwrap_or(0) * 1000);
                    if event_ts > max_ts_seen {
                        max_ts_seen = event_ts;
                    }
                    let domain_fill = match fill_to_domain(fill) {
                        Ok(f) => f,
                        Err(e) => {
                            warn!(?e, fill_id = %fill.fill_id, "skipping malformed fill");
                            continue;
                        }
                    };
                    // Lock-and-extract in a small scope so the
                    // (non-Send) MutexGuard never crosses the await
                    // below. The OMS would warn-log fills for venue
                    // ids it doesn't know about; we drop them silently
                    // here so untracked orders from other processes on
                    // the same account don't pollute the stream.
                    let outcome: Option<(OrderId, u32, u32, bool)> = {
                        let mut t = tracked.lock().unwrap();
                        match t.note_fill(&fill.order_id, domain_fill.qty.get()) {
                            None => {
                                debug!(venue_id = %fill.order_id, "fill for untracked venue order; skipping");
                                None
                            }
                            Some(entry) => {
                                let cid = entry.cid.clone();
                                let cumulative = entry.cumulative;
                                let target = entry.target;
                                let terminal = cumulative >= target;
                                if terminal {
                                    t.terminal(&cid);
                                }
                                Some((cid, cumulative, target, terminal))
                            }
                        }
                    };
                    let Some((cid, cumulative, target, terminal)) = outcome else {
                        continue;
                    };

                    let kind = if terminal {
                        ExecutionReportKind::Filled {
                            fill: domain_fill,
                            cumulative_qty: cumulative.min(target),
                        }
                    } else {
                        ExecutionReportKind::PartiallyFilled {
                            fill: domain_fill,
                            cumulative_qty: cumulative,
                        }
                    };
                    if report_tx
                        .send(ExecutionReport {
                            cid,
                            ts_ms: now_ms(),
                            kind,
                        })
                        .await
                        .is_err()
                    {
                        debug!("report channel closed mid-batch; poller exiting");
                        return;
                    }
                }

                // Kalshi's `min_ts` is in seconds; advance to one second
                // before the latest seen so we don't miss same-second
                // fills.
                let advance_secs = (max_ts_seen / 1000).saturating_sub(1);
                if advance_secs > min_ts {
                    min_ts = advance_secs;
                }
            }
            Err(e) => {
                warn!(error = %e, "list_fills failed; will retry on next interval");
            }
        }
    }
}

fn jittered(base: Duration) -> Duration {
    use rand::Rng as _;
    let base_ms = base.as_millis() as i64;
    if base_ms == 0 {
        return Duration::ZERO;
    }
    let jitter_range = base_ms / 10;
    let delta = rand::thread_rng().gen_range(-jitter_range..=jitter_range);
    let total = (base_ms + delta).max(1) as u64;
    Duration::from_millis(total)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- tracking

#[derive(Debug, Default)]
struct TrackedOrders {
    by_cid: HashMap<OrderId, Tracked>,
    venue_to_cid: HashMap<String, OrderId>,
}

#[derive(Debug, Clone)]
struct Tracked {
    cid: OrderId,
    venue_id: String,
    target: u32,
    cumulative: u32,
}

impl TrackedOrders {
    fn on_submit(&mut self, cid: OrderId, venue_id: String, target: u32) {
        self.by_cid.insert(
            cid.clone(),
            Tracked {
                cid: cid.clone(),
                venue_id: venue_id.clone(),
                target,
                cumulative: 0,
            },
        );
        self.venue_to_cid.insert(venue_id, cid);
    }

    fn venue_id_for(&self, cid: &OrderId) -> Option<String> {
        self.by_cid.get(cid).map(|t| t.venue_id.clone())
    }

    fn on_cancel(&mut self, cid: &OrderId) {
        if let Some(t) = self.by_cid.remove(cid) {
            self.venue_to_cid.remove(&t.venue_id);
        }
    }

    /// Increment cumulative for the venue's order id and return the
    /// (mutated) tracked entry — or `None` if the venue id isn't
    /// tracked here (likely a fill from a different process or a stale
    /// order).
    fn note_fill(&mut self, venue_id: &str, qty: u32) -> Option<Tracked> {
        let cid = self.venue_to_cid.get(venue_id)?.clone();
        let t = self.by_cid.get_mut(&cid)?;
        t.cumulative = t.cumulative.saturating_add(qty);
        Some(t.clone())
    }

    fn terminal(&mut self, cid: &OrderId) {
        if let Some(t) = self.by_cid.remove(cid) {
            self.venue_to_cid.remove(&t.venue_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(s: &str) -> OrderId {
        OrderId::new(s)
    }

    #[test]
    fn tracking_round_trip() {
        let mut t = TrackedOrders::default();
        t.on_submit(cid("c1"), "v1".into(), 100);
        assert_eq!(t.venue_id_for(&cid("c1")), Some("v1".into()));

        let r = t.note_fill("v1", 30).unwrap();
        assert_eq!(r.cumulative, 30);
        let r = t.note_fill("v1", 20).unwrap();
        assert_eq!(r.cumulative, 50);
        assert!(t.note_fill("unknown", 1).is_none());

        t.on_cancel(&cid("c1"));
        assert!(t.venue_id_for(&cid("c1")).is_none());
    }

    #[test]
    fn jitter_stays_within_band() {
        // ±10% is the documented behaviour. Sample many.
        let base = Duration::from_millis(100);
        for _ in 0..200 {
            let d = jittered(base);
            assert!(
                d >= Duration::from_millis(90) && d <= Duration::from_millis(110),
                "got {d:?}"
            );
        }
    }

    #[test]
    fn jitter_zero_is_zero() {
        assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
    }
}
