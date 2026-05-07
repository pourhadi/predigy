//! Authed-channel execution-data consumer.
//!
//! Subscribes a dedicated `kalshi-md` connection to the
//! authenticated `fill` and `market_positions` channels and
//! pushes every event into the OMS via [`Oms::apply_execution`].
//!
//! Why a dedicated connection (rather than re-using the public
//! market-data router):
//!
//! - Cleaner state separation. The router state machine handles
//!   sid → ticker mappings for hundreds of public tickers and
//!   resnapshots on sequence gaps; the authed channels have a
//!   different lifecycle (one sid covers all the user's markets,
//!   no public seq gaps to worry about).
//! - Smaller blast radius. A misbehaving authed-channel parse
//!   doesn't have to share a task with the book-update fan-out.
//! - The WS server treats authed channels as
//!   "all the user's markets, regardless of subscription set" —
//!   so no ticker-list mutation work is needed.
//!
//! Latency picture (this is the load-bearing piece of Phase 4a):
//!
//! - REST submit (Phase 4a): ~200ms to ack
//! - REST `/portfolio/fills` poll (legacy): ~500ms median, longer tails
//! - **WS push fill (this module): ~10ms**
//! - FIX submit (Phase 4b): <1ms
//!
//! With WS-push fills wired, REST-submit latency is the binding
//! constraint until FIX lands. Strategies that don't need sub-ms
//! submit (stat, settlement, wx-stat) are not blocked on Phase 4b.

use anyhow::{Context as _, Result};
use predigy_engine_core::oms::{ExecutionStatus, ExecutionUpdate, Oms};
use predigy_kalshi_md::{
    Channel, Client as MdClient, Connection as MdConnection, Event as MdEvent, FillBody,
};
use predigy_kalshi_rest::Signer;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Configuration for the execution-data consumer.
#[derive(Clone)]
pub struct ExecDataConfig {
    pub kalshi_key_id: String,
    pub kalshi_pem: String,
    pub ws_endpoint: Option<url::Url>,
}

impl std::fmt::Debug for ExecDataConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecDataConfig")
            .field("ws_endpoint", &self.ws_endpoint)
            .finish_non_exhaustive()
    }
}

/// Public handle. Drop or call `shutdown` to abort the task.
pub struct ExecDataConsumer {
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for ExecDataConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecDataConsumer").finish_non_exhaustive()
    }
}

impl ExecDataConsumer {
    /// Connect, subscribe to authed channels, and start consuming.
    pub async fn connect(
        config: ExecDataConfig,
        pool: PgPool,
        oms: Arc<dyn Oms>,
    ) -> Result<Self> {
        let signer = Signer::from_pem(&config.kalshi_key_id, &config.kalshi_pem)
            .map_err(|e| anyhow::anyhow!("exec_data signer: {e}"))?;
        let md_client = match config.ws_endpoint {
            Some(ep) => MdClient::with_endpoint(ep, Some(signer)),
            None => MdClient::new(signer)?,
        };
        let connection = md_client.connect();
        let task = tokio::spawn(consumer_task(connection, pool, oms));
        Ok(Self { task })
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn consumer_task(
    mut connection: MdConnection,
    pool: PgPool,
    oms: Arc<dyn Oms>,
) {
    // Initial subscribe — the authed channels cover ALL the
    // account's markets, so an empty market_tickers list is the
    // right shape (the kalshi-md crate explicitly accepts that
    // for all-authed subscriptions; see client.rs:147).
    let channels = [Channel::Fill, Channel::MarketPositions];
    let market_tickers: Vec<String> = Vec::new();

    // Retry the first subscribe a few times — the WS connection
    // task spins up asynchronously.
    let mut backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(5);
    for attempt in 1..=20u32 {
        match connection.subscribe(&channels, &market_tickers).await {
            Ok(req_id) => {
                info!(
                    req_id,
                    n_channels = channels.len(),
                    "exec_data: subscribed to authed channels"
                );
                break;
            }
            Err(e) if attempt < 20 => {
                warn!(
                    attempt,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "exec_data: initial subscribe failed; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
            Err(e) => {
                warn!(error = %e, "exec_data: giving up on initial subscribe");
                return;
            }
        }
    }

    // Main event loop.
    loop {
        let Some(ev) = connection.next_event().await else {
            warn!("exec_data: kalshi-md connection closed");
            return;
        };
        if let Err(e) = handle_event(ev, &pool, &oms).await {
            warn!(error = %e, "exec_data: event handler error");
        }
    }
}

async fn handle_event(ev: MdEvent, pool: &PgPool, oms: &Arc<dyn Oms>) -> Result<()> {
    match ev {
        MdEvent::Fill { sid, body } => {
            if let Err(e) = apply_fill(pool, oms, &body).await {
                warn!(
                    sid,
                    client_order_id = %body.client_order_id,
                    trade_id = %body.trade_id,
                    error = %e,
                    "exec_data: failed to apply fill"
                );
            }
        }
        MdEvent::MarketPosition { sid, body } => {
            // Phase 4a: log only. Position-state divergence vs the
            // OMS ledger is the reconciliation loop's job (Phase 4b
            // will wire that — for now we surface the venue's view
            // for forensic audit).
            debug!(
                sid,
                ticker = %body.market_ticker,
                position_fp = %body.position_fp,
                "exec_data: venue position update"
            );
        }
        MdEvent::Subscribed { sid, channel, .. } => {
            info!(sid, channel, "exec_data: server confirmed authed subscribe");
        }
        MdEvent::ServerError { req_id, code, msg } => {
            warn!(?req_id, code, msg, "exec_data: server error");
        }
        MdEvent::Disconnected { attempt, reason } => {
            warn!(attempt, reason, "exec_data: disconnected");
        }
        MdEvent::Reconnected => {
            // Saved subs are replayed automatically by the kalshi-md
            // background task. We may have missed fills during the
            // gap — Phase 4b's reconcile loop catches up via REST
            // `list_fills(min_ts=last_seen)`.
            info!("exec_data: reconnected; saved subs replayed");
        }
        MdEvent::Malformed { error, raw } => {
            warn!(
                error,
                raw_excerpt = raw.chars().take(160).collect::<String>().as_str(),
                "exec_data: malformed frame"
            );
        }
        MdEvent::UnhandledType { raw } => {
            debug!(
                raw_excerpt = raw.chars().take(160).collect::<String>().as_str(),
                "exec_data: unhandled message type"
            );
        }
        // Public channels — we don't subscribe to them on this
        // connection, but `kalshi-md` still surfaces them on the
        // shared event enum.
        MdEvent::Snapshot { .. }
        | MdEvent::Delta { .. }
        | MdEvent::Ticker { .. }
        | MdEvent::Trade { .. } => {
            debug!("exec_data: ignoring public-channel event");
        }
    }
    Ok(())
}

/// Translate a Kalshi WS `FillBody` into an `ExecutionUpdate` and
/// hand it to the OMS. Computes the new cumulative_qty by
/// reading the current row + the incremental fill quantity.
///
/// Idempotency: the OMS dedupes on `venue_fill_id` via the
/// `fills.venue_fill_id` unique index. Replayed WS fills (across
/// reconnects, or arriving alongside a REST poll) collapse to a
/// single applied fill.
async fn apply_fill(
    pool: &PgPool,
    oms: &Arc<dyn Oms>,
    body: &FillBody,
) -> Result<()> {
    // Read the originating intent's current cumulative_qty + qty
    // so we can decide PartialFill vs Filled and compute the new
    // absolute cumulative.
    let intent_row: Option<(i32, i32)> = sqlx::query_as(
        "SELECT cumulative_qty, qty FROM intents WHERE client_id = $1",
    )
    .bind(&body.client_order_id)
    .fetch_optional(pool)
    .await
    .with_context(|| format!("read intent for cid {}", body.client_order_id))?;

    let Some((cum_qty, target_qty)) = intent_row else {
        // Fill arrived for an intent we don't know about. This
        // can happen during the migration (legacy stat-trader
        // submits the order; engine sees the fill via WS). Skip
        // — the legacy daemon's REST poller owns it. Once Live,
        // this should be rare; if it's noisy in production, we
        // log+drop is the right behavior (engine doesn't try to
        // create-from-thin-air).
        debug!(
            client_order_id = %body.client_order_id,
            "exec_data: fill for unknown intent (likely legacy daemon's order)"
        );
        return Ok(());
    };

    let fill_qty = parse_count_fp(&body.count_fp)?;
    let fill_price_cents = parse_price_dollars(&body.yes_price_dollars)?;
    let fee_cents = match body.fee_cost.as_deref() {
        Some(s) => Some(parse_dollars_to_cents(s)?),
        None => None,
    };

    let new_cumulative = cum_qty.saturating_add(fill_qty);
    let status = if new_cumulative >= target_qty {
        ExecutionStatus::Filled
    } else {
        ExecutionStatus::PartialFill
    };

    // Stash the venue payload for forensic replay.
    let payload = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);

    let update = ExecutionUpdate {
        client_id: body.client_order_id.clone(),
        venue_order_id: Some(body.order_id.clone()),
        venue_fill_id: Some(body.trade_id.clone()),
        status,
        cumulative_qty: new_cumulative,
        avg_fill_price_cents: Some(fill_price_cents),
        last_fill_qty: Some(fill_qty),
        last_fill_price_cents: Some(fill_price_cents),
        last_fill_fee_cents: fee_cents,
        venue_payload: payload,
    };

    oms.apply_execution(update)
        .await
        .map_err(|e| anyhow::anyhow!("apply_execution: {e}"))?;

    info!(
        client_order_id = %body.client_order_id,
        trade_id = %body.trade_id,
        order_id = %body.order_id,
        ticker = %body.market_ticker,
        fill_qty,
        fill_price_cents,
        new_cumulative,
        target_qty,
        ?status,
        "exec_data: fill applied"
    );
    Ok(())
}

/// `"1.00"` → `1`, `"3.00"` → `3`. Kalshi quotes count in
/// decimal-dollar fixed-point; we round down to whole contracts
/// (partial-contract fills don't exist on Kalshi).
fn parse_count_fp(s: &str) -> Result<i32> {
    let f: f64 = s
        .parse()
        .with_context(|| format!("parse count_fp '{s}'"))?;
    let n = f.floor() as i64;
    i32::try_from(n).map_err(|_| anyhow::anyhow!("count_fp out of i32 range: {s}"))
}

/// `"0.42"` → `42`, `"0.4200"` → `42`. Kalshi prices are
/// dollar-decimal strings.
fn parse_price_dollars(s: &str) -> Result<i32> {
    let f: f64 = s
        .parse()
        .with_context(|| format!("parse price_dollars '{s}'"))?;
    let cents = (f * 100.0).round() as i64;
    if !(0..=100).contains(&cents) {
        return Err(anyhow::anyhow!("price out of range [0,1] dollars: {s}"));
    }
    i32::try_from(cents).map_err(|_| anyhow::anyhow!("price out of i32 range: {s}"))
}

/// Fee as decimal-dollar string → integer cents. Kalshi fees are
/// always non-negative; signed math handled by the OMS.
fn parse_dollars_to_cents(s: &str) -> Result<i32> {
    let f: f64 = s
        .parse()
        .with_context(|| format!("parse dollars '{s}'"))?;
    let cents = (f * 100.0).round() as i64;
    i32::try_from(cents).map_err(|_| anyhow::anyhow!("fee out of i32 range: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_count_fp_handles_typical_shapes() {
        assert_eq!(parse_count_fp("1.00").unwrap(), 1);
        assert_eq!(parse_count_fp("3").unwrap(), 3);
        assert_eq!(parse_count_fp("12.99").unwrap(), 12);
    }

    #[test]
    fn parse_price_dollars_handles_typical_shapes() {
        assert_eq!(parse_price_dollars("0.42").unwrap(), 42);
        assert_eq!(parse_price_dollars("0.4200").unwrap(), 42);
        assert_eq!(parse_price_dollars("0.99").unwrap(), 99);
        assert_eq!(parse_price_dollars("0.00").unwrap(), 0);
    }

    #[test]
    fn parse_price_dollars_rejects_out_of_range() {
        assert!(parse_price_dollars("1.50").is_err());
        assert!(parse_price_dollars("-0.10").is_err());
    }

    #[test]
    fn parse_dollars_to_cents_handles_fees() {
        assert_eq!(parse_dollars_to_cents("0.07").unwrap(), 7);
        assert_eq!(parse_dollars_to_cents("0.0014").unwrap(), 0);
        assert_eq!(parse_dollars_to_cents("1.23").unwrap(), 123);
    }
}
