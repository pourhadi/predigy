//! REST venue submitter — Phase 4a's order-entry path.
//!
//! Polls the `intents` table for rows in `submitted` /
//! `cancel_requested` state and pushes them to Kalshi via REST.
//! On ack, flips the row to `acked` and stamps the venue order id.
//! On rejection, flips to `rejected` with the error body
//! preserved in `intent_events.venue_payload`.
//!
//! Why polling rather than NOTIFY: this is the legacy daemons'
//! mental model and keeps the engine's failure domain inside one
//! transaction (poll → submit → write). NOTIFY is cheaper at idle
//! but adds a second failure mode (notification dropped during
//! restart). Phase 4a is a strict ship; we'll revisit if poll
//! latency becomes the binding constraint (it won't until we
//! cross the 1k-orders/sec mark, which the entire account
//! couldn't reach against Kalshi's rate limits regardless).
//!
//! Idempotency: every submitted intent already has a unique
//! `client_order_id` (the `intents.client_id` PK). Kalshi rejects
//! repeated client_order_ids on the venue, so retrying a
//! mid-failure submit is safe — the second attempt collapses to
//! an `Api { status: 409 }` (or similar) which we treat as
//! "already-acked, advance status".
//!
//! Engine mode: when [`crate::oms_db::EngineMode::Shadow`], the
//! OMS writes intents at `status='shadow'` and this worker's
//! query never sees them. So the same worker code is safe to run
//! in both modes — Shadow simply finds no work to do.

use anyhow::{Context as _, Result};
use predigy_core::side::Side;
use predigy_kalshi_rest::types::{
    CreateOrderRequest, OrderAction, OrderSideV2, SelfTradePreventionV2, TimeInForceV2,
};
use predigy_kalshi_rest::{Client as RestClient, Error as RestError, Signer};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Configuration for the venue submitter.
#[derive(Clone)]
pub struct VenueRestConfig {
    pub kalshi_key_id: String,
    pub kalshi_pem: String,
    pub rest_endpoint: Option<String>,
    pub poll_interval: Duration,
}

impl std::fmt::Debug for VenueRestConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VenueRestConfig")
            .field("rest_endpoint", &self.rest_endpoint)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

/// Public handle. Drop or call `shutdown` to abort.
pub struct VenueRest {
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for VenueRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VenueRest").finish_non_exhaustive()
    }
}

impl VenueRest {
    pub async fn start(config: VenueRestConfig, pool: PgPool) -> Result<Self> {
        let signer = Signer::from_pem(&config.kalshi_key_id, &config.kalshi_pem)
            .map_err(|e| anyhow::anyhow!("venue_rest signer: {e}"))?;
        let rest = if let Some(base) = config.rest_endpoint.as_deref() {
            RestClient::with_base(base, Some(signer))
                .map_err(|e| anyhow::anyhow!("venue_rest client: {e}"))?
        } else {
            RestClient::authed(signer).map_err(|e| anyhow::anyhow!("venue_rest client: {e}"))?
        };
        let rest = Arc::new(rest);
        let task = tokio::spawn(submitter_task(pool, rest, config.poll_interval));
        Ok(Self { task })
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn submitter_task(pool: PgPool, rest: Arc<RestClient>, poll_interval: Duration) {
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = drain_submits(&pool, &rest).await {
            warn!(error = %e, "venue_rest: drain_submits error");
        }
        if let Err(e) = drain_cancels(&pool, &rest).await {
            warn!(error = %e, "venue_rest: drain_cancels error");
        }
    }
}

/// Process every `submitted` intent. We pull a batch, then submit
/// them serially against the REST endpoint. Kalshi's per-key rate
/// limit (~10–30 orders/sec depending on tier) is enforced via
/// the natural single-task serialisation here; we don't need a
/// separate rate limiter at this throughput.
async fn drain_submits(pool: &PgPool, rest: &Arc<RestClient>) -> Result<()> {
    let rows: Vec<SubmittedIntent> = sqlx::query_as::<_, SubmittedIntent>(
        "SELECT i.client_id, i.strategy, i.ticker, i.side, i.action,
                i.price_cents, i.qty, i.order_type, i.tif, i.post_only,
                EXISTS (
                    SELECT 1
                      FROM positions p
                     WHERE p.strategy = i.strategy
                       AND p.ticker = i.ticker
                       AND p.side = i.side
                       AND p.closed_at IS NULL
                       AND ((i.action = 'sell' AND p.current_qty > 0 AND i.qty <= p.current_qty)
                         OR (i.action = 'buy' AND p.current_qty < 0 AND i.qty <= ABS(p.current_qty)))
                ) AS reduce_only
           FROM intents i
          WHERE i.status = 'submitted'
          ORDER BY i.submitted_at
          LIMIT 64",
    )
    .fetch_all(pool)
    .await
    .context("query submitted intents")?;

    for row in rows {
        if let Err(e) = submit_one(pool, rest, &row).await {
            warn!(
                client_id = %row.client_id,
                ticker = %row.ticker,
                error = %e,
                "venue_rest: submit_one failed"
            );
        }
    }
    Ok(())
}

async fn submit_one(pool: &PgPool, rest: &Arc<RestClient>, row: &SubmittedIntent) -> Result<()> {
    let req = build_create_request(row)?;
    let attempt_payload = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);

    debug!(
        client_id = %row.client_id,
        strategy = %row.strategy,
        ticker = %row.ticker,
        side = %row.side,
        action = %row.action,
        qty = row.qty,
        price_cents = ?row.price_cents,
        reduce_only = row.reduce_only,
        "venue_rest: submitting"
    );

    match rest.create_order(&req).await {
        Ok(resp) => {
            let response_payload = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
            mark_acked(pool, &row.client_id, &resp.order_id, response_payload)
                .await
                .context("mark_acked")?;
            info!(
                client_id = %row.client_id,
                venue_order_id = %resp.order_id,
                "venue_rest: order acked"
            );
            Ok(())
        }
        Err(RestError::Api { status, body }) => {
            // Treat 4xx as a venue-side rejection — flip the
            // intent to `rejected` and stash the body. 5xx is
            // ambiguous (could have been received by Kalshi mid-
            // failure); leave the intent at `submitted` to retry
            // on the next poll. Kalshi's 429 (rate limit) is
            // explicitly retryable.
            if status >= 500 || status == 429 {
                debug!(
                    client_id = %row.client_id,
                    status,
                    body_excerpt = body.chars().take(200).collect::<String>().as_str(),
                    "venue_rest: transient venue error; will retry"
                );
                Ok(())
            } else {
                let payload = serde_json::json!({
                    "kind": "venue_rejected",
                    "status": status,
                    "body": body,
                    "request": attempt_payload,
                });
                mark_rejected(pool, &row.client_id, payload)
                    .await
                    .context("mark_rejected")?;
                warn!(
                    client_id = %row.client_id,
                    status,
                    body_excerpt = body.chars().take(200).collect::<String>().as_str(),
                    "venue_rest: order rejected by venue"
                );
                Ok(())
            }
        }
        Err(e) => {
            // Network / decode / auth errors. Leave at
            // `submitted` so the next poll retries. Auth errors
            // in particular need an operator alert — log loudly.
            warn!(
                client_id = %row.client_id,
                error = %e,
                "venue_rest: transport error; will retry"
            );
            Ok(())
        }
    }
}

/// Process every `cancel_requested` intent. We require a
/// `venue_order_id` to cancel — if the cancel was issued before
/// the venue acked the original submit, the cancel races and we
/// retry on the next tick.
async fn drain_cancels(pool: &PgPool, rest: &Arc<RestClient>) -> Result<()> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT client_id, venue_order_id
           FROM intents
          WHERE status = 'cancel_requested'
          ORDER BY last_updated_at
          LIMIT 64",
    )
    .fetch_all(pool)
    .await
    .context("query cancel_requested intents")?;

    for (client_id, venue_order_id) in rows {
        let Some(venue_id) = venue_order_id else {
            // Cancel raced ahead of the venue ack. Skip; will
            // retry next tick. If the venue rejected the original
            // submit (status='rejected'), that intent is already
            // out of cancel_requested so this branch only sees
            // genuinely racing cancels.
            debug!(
                client_id = %client_id,
                "venue_rest: cancel deferred (no venue_order_id yet)"
            );
            continue;
        };
        if let Err(e) = cancel_one(pool, rest, &client_id, &venue_id).await {
            warn!(
                client_id = %client_id,
                venue_order_id = %venue_id,
                error = %e,
                "venue_rest: cancel_one failed"
            );
        }
    }
    Ok(())
}

async fn cancel_one(
    pool: &PgPool,
    rest: &Arc<RestClient>,
    client_id: &str,
    venue_order_id: &str,
) -> Result<()> {
    match rest.cancel_order(venue_order_id).await {
        Ok(resp) => {
            let payload = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
            mark_cancelled(pool, client_id, payload).await?;
            info!(client_id, venue_order_id, "venue_rest: order cancelled");
            Ok(())
        }
        Err(RestError::Api { status, body }) => {
            // 404 = order already gone (filled, expired, or
            // already cancelled). Treat as success — flip to
            // `cancelled` so the row clears the queue. The
            // ExecutionUpdate path may overwrite this on a later
            // fill if the 404 was racing a fill confirmation.
            if status == 404 {
                let payload = serde_json::json!({
                    "kind": "cancel_404_treated_as_cancelled",
                    "body": body,
                });
                mark_cancelled(pool, client_id, payload).await?;
                debug!(
                    client_id,
                    venue_order_id, "venue_rest: cancel got 404; marking cancelled"
                );
                Ok(())
            } else if status >= 500 || status == 429 {
                debug!(
                    client_id,
                    venue_order_id, status, "venue_rest: transient cancel error; will retry"
                );
                Ok(())
            } else {
                // Hard cancel rejection. Log + leave at
                // cancel_requested. Operator can intervene.
                warn!(
                    client_id,
                    venue_order_id,
                    status,
                    body_excerpt = body.chars().take(200).collect::<String>().as_str(),
                    "venue_rest: cancel rejected by venue"
                );
                Ok(())
            }
        }
        Err(e) => {
            warn!(
                client_id,
                venue_order_id,
                error = %e,
                "venue_rest: cancel transport error; will retry"
            );
            Ok(())
        }
    }
}

/// Build the V2 `CreateOrderRequest` from the queued intent.
///
/// Mapping from (Side, Action) to Kalshi V2's (side, action):
///
/// V2 takes the contract leg (yes/no) on `side` and a separate
/// `action` (buy/sell). The intent's `Side::Yes/No` flows
/// directly to `OrderSideV2::Bid`/`Ask` on the YES book — buy-NO
/// and sell-YES both rest on the YES-book ask side, etc. We
/// always submit a single `price` (Kalshi V2 dropped the dual
/// `yes_price`/`no_price` fields; the wire `side` already
/// disambiguates). For NO-side intents we send the YES-equivalent
/// complement.
fn build_create_request(row: &SubmittedIntent) -> Result<CreateOrderRequest> {
    if row.order_type != "limit" {
        // Phase 4a only ships limit orders; market intents are
        // mapped to IOC limit at the worst price by the strategy
        // BEFORE they reach the OMS.
        return Err(anyhow::anyhow!(
            "venue_rest: only limit orders supported in Phase 4a; intent had {}",
            row.order_type
        ));
    }
    let side: Side = match row.side.as_str() {
        "yes" => Side::Yes,
        "no" => Side::No,
        other => return Err(anyhow::anyhow!("unknown side '{other}'")),
    };
    let (wire_side, wire_action) = map_side_action(side, &row.action)?;

    let price_cents: i32 = row
        .price_cents
        .ok_or_else(|| anyhow::anyhow!("limit intent missing price_cents"))?;
    if !(1..=99).contains(&price_cents) {
        return Err(anyhow::anyhow!(
            "limit price out of range [1,99]: {price_cents}"
        ));
    }
    // For NO-side intents, Kalshi expects the YES-equivalent
    // limit. (The wire `side` field places the order on the
    // YES book at the complement.)
    let yes_equiv_cents = match side {
        Side::Yes => u32::try_from(price_cents).unwrap_or(0),
        Side::No => 100u32.saturating_sub(u32::try_from(price_cents).unwrap_or(0)),
    };

    let qty: u32 = u32::try_from(row.qty)
        .map_err(|_| anyhow::anyhow!("intent qty out of range: {}", row.qty))?;

    let (tif, _legacy_post_only) = map_tif(&row.tif);
    Ok(CreateOrderRequest {
        ticker: row.ticker.clone(),
        client_order_id: row.client_id.clone(),
        side: wire_side,
        action: wire_action,
        count: format!("{qty}.00"),
        price: format_cents_to_dollars(yes_equiv_cents),
        time_in_force: tif,
        self_trade_prevention_type: SelfTradePreventionV2::TakerAtCross,
        // Driven from the Intent now that it has its own
        // `post_only` field. The intents.post_only column is the
        // source of truth — see migration 0005 + the maker
        // strategy spec in `plans/2026-05-10-strategic-roadmap.md`.
        post_only: row.post_only.then_some(true),
        reduce_only: row.reduce_only.then_some(true),
    })
}

fn map_side_action(side: Side, action: &str) -> Result<(OrderSideV2, OrderAction)> {
    let act = match action {
        "buy" => OrderAction::Buy,
        "sell" => OrderAction::Sell,
        other => return Err(anyhow::anyhow!("unknown action '{other}'")),
    };
    let wire_side = match (side, act) {
        (Side::Yes, OrderAction::Buy) | (Side::No, OrderAction::Sell) => OrderSideV2::Bid,
        (Side::Yes, OrderAction::Sell) | (Side::No, OrderAction::Buy) => OrderSideV2::Ask,
    };
    Ok((wire_side, act))
}

fn map_tif(tif: &str) -> (TimeInForceV2, Option<bool>) {
    match tif {
        "ioc" => (TimeInForceV2::ImmediateOrCancel, None),
        "fok" => (TimeInForceV2::FillOrKill, None),
        // Anything else (typically "gtc") maps to GTC. The
        // `post_only` flag is not currently driven from the
        // intent; strategies can opt-in once we add it to the
        // intent schema.
        _ => (TimeInForceV2::GoodTillCanceled, None),
    }
}

fn format_cents_to_dollars(cents: u32) -> String {
    let dollars = cents / 100;
    let frac = cents % 100;
    format!("{dollars}.{frac:02}00")
}

async fn mark_acked(
    pool: &PgPool,
    client_id: &str,
    venue_order_id: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE intents
            SET status = 'acked',
                venue_order_id = $2,
                last_updated_at = now()
          WHERE client_id = $1
            AND status = 'submitted'",
    )
    .bind(client_id)
    .bind(venue_order_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO intent_events (client_id, status, venue_payload)
         VALUES ($1, 'acked', $2)",
    )
    .bind(client_id)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_rejected(pool: &PgPool, client_id: &str, payload: serde_json::Value) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE intents
            SET status = 'rejected',
                last_updated_at = now()
          WHERE client_id = $1
            AND status = 'submitted'",
    )
    .bind(client_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO intent_events (client_id, status, venue_payload)
         VALUES ($1, 'rejected', $2)",
    )
    .bind(client_id)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_cancelled(pool: &PgPool, client_id: &str, payload: serde_json::Value) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE intents
            SET status = 'cancelled',
                last_updated_at = now()
          WHERE client_id = $1
            AND status NOT IN ('filled','rejected','expired')",
    )
    .bind(client_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO intent_events (client_id, status, venue_payload)
         VALUES ($1, 'cancelled', $2)",
    )
    .bind(client_id)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Row shape for the queue-poll query.
#[derive(Debug, Clone, sqlx::FromRow)]
struct SubmittedIntent {
    client_id: String,
    strategy: String,
    ticker: String,
    side: String,
    action: String,
    price_cents: Option<i32>,
    qty: i32,
    order_type: String,
    tif: String,
    post_only: bool,
    reduce_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(side: &str, action: &str, price_cents: Option<i32>, tif: &str) -> SubmittedIntent {
        SubmittedIntent {
            client_id: "stat:KXFOO-X:00012345".into(),
            strategy: "stat".into(),
            ticker: "KXFOO-X".into(),
            side: side.into(),
            action: action.into(),
            price_cents,
            qty: 2,
            order_type: "limit".into(),
            tif: tif.into(),
            post_only: false,
            reduce_only: false,
        }
    }

    #[test]
    fn buy_yes_maps_to_bid_yes_at_native_price() {
        let req = build_create_request(&intent("yes", "buy", Some(42), "ioc")).unwrap();
        assert_eq!(req.side, OrderSideV2::Bid);
        assert_eq!(req.action, OrderAction::Buy);
        assert_eq!(req.price, "0.4200");
        assert_eq!(req.count, "2.00");
        assert!(matches!(
            req.time_in_force,
            TimeInForceV2::ImmediateOrCancel
        ));
    }

    #[test]
    fn sell_yes_maps_to_ask_sell_at_native_price() {
        let req = build_create_request(&intent("yes", "sell", Some(42), "gtc")).unwrap();
        assert_eq!(req.side, OrderSideV2::Ask);
        assert_eq!(req.action, OrderAction::Sell);
        assert_eq!(req.price, "0.4200");
    }

    #[test]
    fn reduce_only_is_sent_for_closing_intents() {
        let mut row = intent("yes", "sell", Some(42), "ioc");
        row.reduce_only = true;
        let req = build_create_request(&row).unwrap();
        assert_eq!(req.reduce_only, Some(true));
    }

    #[test]
    fn buy_no_maps_to_ask_at_complement_price() {
        let req = build_create_request(&intent("no", "buy", Some(60), "ioc")).unwrap();
        // Buy NO at 60¢ ≡ ask the YES book at the complement (40¢).
        assert_eq!(req.side, OrderSideV2::Ask);
        assert_eq!(req.action, OrderAction::Buy);
        assert_eq!(req.price, "0.4000");
    }

    #[test]
    fn sell_no_maps_to_bid_at_complement_price() {
        let req = build_create_request(&intent("no", "sell", Some(60), "ioc")).unwrap();
        // Sell NO at 60¢ ≡ bid the YES book at the complement (40¢).
        assert_eq!(req.side, OrderSideV2::Bid);
        assert_eq!(req.action, OrderAction::Sell);
        assert_eq!(req.price, "0.4000");
    }

    #[test]
    fn out_of_range_price_rejects() {
        assert!(build_create_request(&intent("yes", "buy", Some(0), "ioc")).is_err());
        assert!(build_create_request(&intent("yes", "buy", Some(100), "ioc")).is_err());
    }

    #[test]
    fn missing_price_rejects() {
        assert!(build_create_request(&intent("yes", "buy", None, "ioc")).is_err());
    }

    #[test]
    fn non_limit_order_rejects() {
        let mut row = intent("yes", "buy", Some(50), "ioc");
        row.order_type = "market".into();
        assert!(build_create_request(&row).is_err());
    }

    #[test]
    fn format_cents_to_dollars_pads_correctly() {
        assert_eq!(format_cents_to_dollars(42), "0.4200");
        assert_eq!(format_cents_to_dollars(99), "0.9900");
        assert_eq!(format_cents_to_dollars(1), "0.0100");
    }
}
