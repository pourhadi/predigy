//! Trade ledger — derives `Trade` rows from the engine's existing
//! `positions` / `intents` / `fills` tables.
//!
//! Each row in `positions` is one trade lifecycle. The ledger
//! enriches it with:
//!
//! - `exit_reason` parsed from the closing intent's `reason`
//!   (matched by `(strategy, ticker, side)` with a closing
//!   `action`).
//! - `intended_edge_cents` from the entry intent's `reason`.
//! - `n_fills` count from the `fills` table over the position's
//!   lifecycle.
//! - `leg_group_id` from the entry intent (Audit I7).
//!
//! The matching window is `[opened_at, closed_at)` for closed
//! positions, `[opened_at, now())` for open ones.

use crate::time_window::TimeWindow;
use crate::types::{ExitReason, Trade, parse_intended_edge};
use chrono::{DateTime, Utc};
use predigy_engine_core::Db;
use sqlx::Row;

/// Load all trades matching the time window. A trade matches if
/// its `opened_at` falls inside the window. Closed trades are
/// guaranteed to have `realized_pnl_cents` populated; open ones
/// have `closed_at = None`.
///
/// `strategy_filter` narrows to one strategy; `None` returns
/// every strategy in the DB.
pub async fn load_trades(
    db: &Db,
    window: TimeWindow,
    strategy_filter: Option<&str>,
) -> Result<Vec<Trade>, sqlx::Error> {
    let pool = db.pool();

    // Load positions (one row = one trade).
    let rows = sqlx::query(
        r"
        SELECT
            p.strategy,
            p.ticker,
            p.side,
            p.current_qty,
            p.avg_entry_cents,
            p.realized_pnl_cents,
            p.fees_paid_cents,
            p.opened_at,
            p.closed_at,
            p.last_fill_at
        FROM positions p
        WHERE p.opened_at >= $1
          AND p.opened_at < $2
          AND ($3::TEXT IS NULL OR p.strategy = $3)
        ORDER BY p.strategy, p.opened_at
        ",
    )
    .bind(window.start)
    .bind(window.end)
    .bind(strategy_filter)
    .fetch_all(pool)
    .await?;

    let mut trades: Vec<Trade> = Vec::with_capacity(rows.len());
    for row in rows {
        let strategy: String = row.get("strategy");
        let ticker: String = row.get("ticker");
        let side: String = row.get("side");
        let current_qty: i32 = row.get("current_qty");
        let avg_entry_cents: i32 = row.get("avg_entry_cents");
        let realized_pnl_cents: i64 = row.get("realized_pnl_cents");
        let fees_paid_cents: i64 = row.get("fees_paid_cents");
        let opened_at: DateTime<Utc> = row.get("opened_at");
        let closed_at: Option<DateTime<Utc>> = row.get("closed_at");

        // Defaults that get filled by the enrich pass below.
        trades.push(Trade {
            strategy,
            ticker,
            side,
            qty_open: 0, // filled below from open intent
            qty_remaining: current_qty,
            avg_entry_cents,
            avg_exit_cents: None,
            realized_pnl_cents,
            fees_paid_cents,
            opened_at,
            closed_at,
            hold_seconds: closed_at.map(|c| (c - opened_at).num_seconds()),
            exit_reason: None,
            leg_group_id: None,
            n_fills: 0,
            intended_edge_cents: None,
        });
    }

    // Enrich each trade with intent + fill data. We do this in a
    // single batch query per join rather than N+1.
    enrich_with_intents(pool, &mut trades).await?;
    enrich_with_fills(pool, &mut trades).await?;
    Ok(trades)
}

/// For each trade, find:
///   - the entry intent (action='buy', earliest in the lifecycle):
///     for `qty_open`, `intended_edge_cents`, `leg_group_id`.
///   - the closing intent (action='sell' for long; latest before
///     `closed_at`): for `exit_reason`, `avg_exit_cents`.
///
/// This is a per-trade lookup; we batch by `(strategy, ticker)`
/// to keep queries linear.
async fn enrich_with_intents(
    pool: &sqlx::PgPool,
    trades: &mut [Trade],
) -> Result<(), sqlx::Error> {
    for t in trades.iter_mut() {
        // Entry intent: earliest matching the position's open.
        let entry: Option<(i32, Option<String>, Option<uuid::Uuid>)> = sqlx::query_as(
            r"
            SELECT qty, reason, leg_group_id
            FROM intents
            WHERE strategy = $1
              AND ticker = $2
              AND side = $3
              AND action = 'buy'
              AND submitted_at >= $4
              AND submitted_at <= COALESCE($5, NOW())
              AND status IN ('filled','partial_fill','acked','submitted')
            ORDER BY submitted_at ASC
            LIMIT 1
            ",
        )
        .bind(&t.strategy)
        .bind(&t.ticker)
        .bind(&t.side)
        .bind(t.opened_at)
        .bind(t.closed_at)
        .fetch_optional(pool)
        .await?;
        if let Some((qty, reason, lgid)) = entry {
            t.qty_open = qty;
            t.leg_group_id = lgid;
            if let Some(r) = reason.as_deref() {
                t.intended_edge_cents = parse_intended_edge(r);
            }
        }

        // Closing intent: only meaningful for closed trades.
        if let Some(closed_at) = t.closed_at {
            let close: Option<(i32, String)> = sqlx::query_as(
                r"
                SELECT price_cents, COALESCE(reason, '') AS reason
                FROM intents
                WHERE strategy = $1
                  AND ticker = $2
                  AND side = $3
                  AND action = 'sell'
                  AND submitted_at >= $4
                  AND submitted_at <= $5
                  AND status IN ('filled','partial_fill','acked','submitted')
                ORDER BY submitted_at DESC
                LIMIT 1
                ",
            )
            .bind(&t.strategy)
            .bind(&t.ticker)
            .bind(&t.side)
            .bind(t.opened_at)
            .bind(closed_at + chrono::Duration::seconds(60))
            .fetch_optional(pool)
            .await?;
            if let Some((price, reason)) = close {
                t.avg_exit_cents = Some(price);
                t.exit_reason = Some(ExitReason::parse_reason(&reason));
            } else {
                // No matching closing intent — likely venue auto-
                // settled at expiry.
                t.exit_reason = Some(ExitReason::Settled);
            }
        }
    }
    Ok(())
}

async fn enrich_with_fills(
    pool: &sqlx::PgPool,
    trades: &mut [Trade],
) -> Result<(), sqlx::Error> {
    for t in trades.iter_mut() {
        let row: (i64,) = sqlx::query_as(
            r"
            SELECT COUNT(*)::BIGINT
            FROM fills
            WHERE strategy = $1
              AND ticker = $2
              AND side = $3
              AND ts >= $4
              AND ts <= COALESCE($5, NOW())
            ",
        )
        .bind(&t.strategy)
        .bind(&t.ticker)
        .bind(&t.side)
        .bind(t.opened_at)
        .bind(t.closed_at)
        .fetch_one(pool)
        .await?;
        t.n_fills = i32::try_from(row.0).unwrap_or(i32::MAX);
    }
    Ok(())
}

/// Intent activity over the same window, for fill / reject / cap
/// rate metrics. Returned as a per-strategy aggregate so metrics.rs
/// can fold it in without a second pass.
pub async fn load_intent_activity(
    db: &Db,
    window: TimeWindow,
    strategy_filter: Option<&str>,
) -> Result<std::collections::HashMap<String, IntentActivity>, sqlx::Error> {
    let pool = db.pool();
    let rows = sqlx::query(
        r"
        SELECT strategy, status, COUNT(*)::BIGINT AS n
        FROM intents
        WHERE submitted_at >= $1 AND submitted_at < $2
          AND ($3::TEXT IS NULL OR strategy = $3)
        GROUP BY strategy, status
        ",
    )
    .bind(window.start)
    .bind(window.end)
    .bind(strategy_filter)
    .fetch_all(pool)
    .await?;

    // Cap-rejection telemetry — count from intent_events where the
    // venue_payload includes a notional / cap rejection reason.
    // This is a heuristic approximation; the production OMS logs
    // rejections without persisting structured reason codes.
    let cap_rejected_rows = sqlx::query(
        r"
        SELECT i.strategy, COUNT(DISTINCT ie.client_id)::BIGINT AS n
        FROM intent_events ie
        JOIN intents i ON i.client_id = ie.client_id
        WHERE ie.status = 'rejected'
          AND ie.venue_payload::text ILIKE '%notional%'
          AND i.submitted_at >= $1 AND i.submitted_at < $2
          AND ($3::TEXT IS NULL OR i.strategy = $3)
        GROUP BY i.strategy
        ",
    )
    .bind(window.start)
    .bind(window.end)
    .bind(strategy_filter)
    .fetch_all(pool)
    .await?;

    let mut out: std::collections::HashMap<String, IntentActivity> = std::collections::HashMap::new();
    for row in rows {
        let strategy: String = row.get("strategy");
        let status: String = row.get("status");
        let n: i64 = row.get("n");
        let entry = out.entry(strategy).or_default();
        entry.total += n as u64;
        match status.as_str() {
            "filled" => entry.filled += n as u64,
            "partial_fill" => {
                entry.filled += n as u64;
            }
            "rejected" => entry.rejected += n as u64,
            "cancelled" => entry.cancelled += n as u64,
            _ => {}
        }
    }
    for row in cap_rejected_rows {
        let strategy: String = row.get("strategy");
        let n: i64 = row.get("n");
        out.entry(strategy).or_default().cap_rejected = n as u64;
    }
    Ok(out)
}

#[derive(Debug, Default, Clone)]
pub struct IntentActivity {
    pub total: u64,
    pub filled: u64,
    pub rejected: u64,
    pub cancelled: u64,
    /// Subset of `rejected` whose payload mentions a notional or
    /// cap-related reason. Heuristic — a more reliable signal
    /// would be a structured rejection-reason column on
    /// intent_events, which is a v2 schema change.
    pub cap_rejected: u64,
}
