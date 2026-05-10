//! Database-backed OMS implementation.
//!
//! Every state transition lives in two places:
//!
//! - `intents` (current status, cumulative qty, etc.) — updated
//!   in place via UPSERT semantics on the `client_id` PK.
//! - `intent_events` (append-only timeline) — one row per
//!   transition for forensic queries + audit.
//!
//! Risk caps enforced before the venue ever sees the intent.
//! Kill-switch checked before risk caps.
//!
//! Transactions: every state-mutating operation runs inside a
//! single Postgres transaction so we can't get half-applied
//! state on crash. The DB is the source of truth for all
//! position + intent state.

use async_trait::async_trait;
use predigy_engine_core::error::{EngineError, EngineResult};
use predigy_engine_core::intent::{Intent, IntentAction, LegGroup, OrderType, Tif};
use predigy_engine_core::oms::{
    ExecutionStatus, ExecutionUpdate, KillSwitchView, Oms, ReconciliationDiff, RejectionReason,
    RiskCaps, SubmitGroupOutcome, SubmitOutcome, VenueChoice,
};
use predigy_kalshi_rest::types::{FillRecord, MarketPosition, OrderRecord};
use predigy_kalshi_rest::{Client as RestClient, Error as RestError};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

type DailyPnlMarkRow = (
    i32,
    String,
    i32,
    i64,
    i64,
    Option<i32>,
    Option<i32>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// Engine execution mode. Production-grade systems should boot
/// in `Shadow` until parity is verified against the legacy
/// daemon, then operator flips to `Live`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine writes intents to the DB at status='shadow' and
    /// does NOT submit to the venue. Use during the migration
    /// to compare engine decisions against the legacy daemon's
    /// fills without dual-trading.
    Shadow,
    /// Engine writes intents at status='submitted' and the venue
    /// path (FIX/REST) actually transmits them to Kalshi.
    Live,
}

impl EngineMode {
    fn initial_status(&self) -> &'static str {
        match self {
            EngineMode::Shadow => "shadow",
            EngineMode::Live => "submitted",
        }
    }
}

/// DB-backed OMS. Cheap to clone (`Arc` internals).
#[derive(Debug, Clone)]
pub struct DbBackedOms {
    pool: PgPool,
    risk_caps: RiskCaps,
    kill_switch: Arc<KillSwitchView>,
    mode: EngineMode,
    rest: Option<Arc<RestClient>>,
}

impl DbBackedOms {
    /// Build a Shadow-mode OMS. Use [`new_with_mode`] for live.
    pub fn new(pool: PgPool, risk_caps: RiskCaps, kill_switch: Arc<KillSwitchView>) -> Self {
        Self::new_with_mode(pool, risk_caps, kill_switch, EngineMode::Shadow)
    }

    /// Build with explicit mode.
    pub fn new_with_mode(
        pool: PgPool,
        risk_caps: RiskCaps,
        kill_switch: Arc<KillSwitchView>,
        mode: EngineMode,
    ) -> Self {
        Self {
            pool,
            risk_caps,
            kill_switch,
            mode,
            rest: None,
        }
    }

    pub fn with_reconciliation_rest(mut self, rest: Arc<RestClient>) -> Self {
        self.rest = Some(rest);
        self
    }

    pub fn mode(&self) -> EngineMode {
        self.mode
    }

    /// Fast path before touching the DB: in-memory kill switch +
    /// basic shape checks. The OMS still hits the DB after this
    /// for risk-cap state but rejecting bad intents early saves
    /// round trips.
    fn pre_check(&self, intent: &Intent) -> Result<(), RejectionReason> {
        // Audit I2 — gate on global OR per-strategy. The
        // RejectionReason carries the binding scope so the
        // operator can see which switch fired in logs / DB.
        if self.kill_switch.is_armed_for(intent.strategy) {
            let scope = if self.kill_switch.is_armed() {
                "global".to_string()
            } else {
                format!("strategy:{}", intent.strategy)
            };
            return Err(RejectionReason::KillSwitchArmed { scope });
        }
        if intent.qty <= 0 {
            return Err(RejectionReason::InvalidIntent {
                reason: format!("non-positive qty {}", intent.qty),
            });
        }
        if intent.client_id.is_empty() {
            return Err(RejectionReason::InvalidIntent {
                reason: "empty client_id".into(),
            });
        }
        Ok(())
    }

    /// Look up current open-position state to check against
    /// per-side and notional caps. Returns the relevant numbers
    /// without locking — the actual write transaction will
    /// re-verify.
    async fn current_exposure(
        &self,
        intent: &Intent,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> EngineResult<ExposureSnapshot> {
        let strategy = intent.strategy;
        let ticker = intent.market.as_str();
        let side = side_to_str(&intent);

        // Open contracts on this (strategy, ticker, side).
        let row: Option<(i32, i32)> = sqlx::query_as(
            "SELECT current_qty, avg_entry_cents FROM positions
              WHERE strategy = $1 AND ticker = $2 AND side = $3 AND closed_at IS NULL",
        )
        .bind(strategy)
        .bind(ticker)
        .bind(side)
        .fetch_optional(&mut **tx)
        .await?;
        let (current_qty, current_avg_entry_cents) = row.unwrap_or((0, 0));

        // Total open notional across this strategy. Sum = qty *
        // avg_entry_cents per row, treating short positions as
        // positive notional.
        let total: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT SUM(ABS(current_qty)::BIGINT * avg_entry_cents::BIGINT)::BIGINT
               FROM positions
              WHERE strategy = $1 AND closed_at IS NULL",
        )
        .bind(strategy)
        .fetch_optional(&mut **tx)
        .await?;
        let strategy_notional_cents = total.and_then(|t| t.0).unwrap_or(0);

        // Phase 6.2 — total open notional across ALL strategies.
        // Same shape as the strategy-scoped query, no WHERE
        // clause on strategy.
        let global_total: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT SUM(ABS(current_qty)::BIGINT * avg_entry_cents::BIGINT)::BIGINT
               FROM positions
              WHERE closed_at IS NULL",
        )
        .fetch_optional(&mut **tx)
        .await?;
        let global_notional_cents = global_total.and_then(|t| t.0).unwrap_or(0);

        // In-flight orders (any non-terminal status). 'shadow'
        // is a non-venue terminal (engine never sent it); count
        // it out of in-flight so shadow accumulation doesn't
        // exhaust the cap.
        let in_flight: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM intents
              WHERE strategy = $1
                AND status NOT IN ('filled','cancelled','rejected','expired','shadow')",
        )
        .bind(strategy)
        .fetch_one(&mut **tx)
        .await?;

        let rate_window_ms = i64::try_from(self.risk_caps.rate_window_ms).unwrap_or(i64::MAX);
        let rate_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT FROM intents
              WHERE strategy = $1
                AND submitted_at >= now() - ($2::BIGINT * interval '1 millisecond')",
        )
        .bind(strategy)
        .bind(rate_window_ms)
        .fetch_one(&mut **tx)
        .await?;

        let daily = daily_pnl_with_marks(tx, strategy).await?;

        Ok(ExposureSnapshot {
            current_qty,
            current_avg_entry_cents,
            strategy_notional_cents,
            global_notional_cents,
            in_flight: i32::try_from(in_flight.0).unwrap_or(i32::MAX),
            orders_in_rate_window: u32::try_from(rate_count.0).unwrap_or(u32::MAX),
            daily_pnl_cents: daily.pnl_cents,
        })
    }

    fn check_caps(
        &self,
        intent: &Intent,
        exposure: &ExposureSnapshot,
    ) -> Result<(), RejectionReason> {
        let caps = &self.risk_caps;

        let projection = project_position(intent, exposure);
        let risk_reducing = projection.added_notional_cents <= 0
            && projection.projected_abs_contracts <= exposure.current_qty.abs();

        if exposure.daily_pnl_cents < -caps.max_daily_loss_cents && !risk_reducing {
            return Err(RejectionReason::DailyLossExceeded {
                strategy: intent.strategy,
            });
        }
        if exposure.in_flight >= caps.max_in_flight {
            return Err(RejectionReason::TooManyInFlight {
                strategy: intent.strategy,
                in_flight: exposure.in_flight,
                limit: caps.max_in_flight,
            });
        }
        if exposure.orders_in_rate_window >= caps.max_orders_per_window {
            return Err(RejectionReason::RateLimited {
                window_ms: caps.rate_window_ms,
            });
        }

        // Project the post-fill state to check side-cap.
        if projection.projected_abs_contracts > caps.max_contracts_per_side {
            return Err(RejectionReason::ContractCapExceeded {
                ticker: intent.market.as_str().to_string(),
                side: side_to_str(intent).to_string(),
                current: projection.projected_abs_contracts,
                limit: caps.max_contracts_per_side,
            });
        }

        if exposure.strategy_notional_cents + projection.added_notional_cents
            > caps.max_notional_cents
        {
            return Err(RejectionReason::NotionalExceeded {
                scope: format!("strategy:{}", intent.strategy),
                current_cents: exposure.strategy_notional_cents,
                limit_cents: caps.max_notional_cents,
            });
        }

        // Phase 6.2 — global notional cap across all strategies.
        // 0 disables (per-strategy caps still apply); >0 enforces
        // a hard ceiling on engine-wide exposure.
        if caps.max_global_notional_cents > 0
            && exposure.global_notional_cents + projection.added_notional_cents
                > caps.max_global_notional_cents
        {
            return Err(RejectionReason::NotionalExceeded {
                scope: "global".into(),
                current_cents: exposure.global_notional_cents,
                limit_cents: caps.max_global_notional_cents,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Oms for DbBackedOms {
    async fn submit(&self, intent: Intent) -> EngineResult<SubmitOutcome> {
        if let Err(reason) = self.pre_check(&intent) {
            return Ok(SubmitOutcome::Rejected { reason });
        }

        let mut tx = self.pool.begin().await?;
        lock_submit_section(&mut tx).await?;

        // Idempotency check — does this client_id already exist?
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
                .bind(&intent.client_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((status,)) = existing {
            debug!(client_id = %intent.client_id, %status, "oms: idempotent re-submit");
            return Ok(SubmitOutcome::Idempotent {
                client_id: intent.client_id,
                current_status: status,
            });
        }

        // Risk caps.
        let exposure = self.current_exposure(&intent, &mut tx).await?;
        if let Err(reason) = self.check_caps(&intent, &exposure) {
            return Ok(SubmitOutcome::Rejected { reason });
        }

        // Persist with the mode-appropriate initial status. In
        // Live mode the engine venue-router picks it up off this
        // row and pushes to FIX/REST. In Shadow mode the row
        // stays at 'shadow' forever — used during migration to
        // compare engine decisions against the legacy daemon's
        // fills without dual-trading.
        let initial_status = self.mode.initial_status();
        sqlx::query(
            "INSERT INTO intents
                (client_id, strategy, ticker, side, action, price_cents,
                 qty, order_type, tif, status, cumulative_qty, reason, post_only)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $11, 0, $10, $12)",
        )
        .bind(&intent.client_id)
        .bind(intent.strategy)
        .bind(intent.market.as_str())
        .bind(side_to_str(&intent))
        .bind(action_to_str(intent.action))
        .bind(intent.price_cents)
        .bind(intent.qty)
        .bind(order_type_to_str(intent.order_type))
        .bind(tif_to_str(intent.tif))
        .bind(intent.reason.as_deref())
        .bind(initial_status)
        .bind(intent.post_only)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO intent_events (client_id, status, venue_payload)
             VALUES ($1, $2, NULL)",
        )
        .bind(&intent.client_id)
        .bind(initial_status)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(SubmitOutcome::Submitted {
            client_id: intent.client_id,
            // FIX preferred; the engine's venue router decides
            // based on session health. We default to Rest here
            // until Phase 4 plugs in FIX selection logic.
            venue: VenueChoice::Rest,
        })
    }

    async fn submit_group(&self, group: LegGroup) -> EngineResult<SubmitGroupOutcome> {
        // **Audit I7** — atomic multi-leg submit.
        //
        // Flow:
        //   1. Pre-check every leg in isolation (kill switch +
        //      shape). First failure rejects the whole group.
        //   2. Idempotency probe: load every existing
        //      (client_id, leg_group_id) for the legs. If ALL legs
        //      already exist under THIS group_id, return
        //      Idempotent. If any legs exist under a different
        //      group (or with NULL leg_group_id), return
        //      PartialCollision.
        //   3. Compute combined exposure (sum of all leg
        //      notionals) and check per-strategy + global caps.
        //      Each leg also checks its own contract-side and
        //      in-flight caps.
        //   4. Open a single Postgres transaction; insert every
        //      leg's `intents` row with the shared
        //      `leg_group_id`, plus its `intent_events` initial
        //      transition. Commit atomically.
        //
        // Kalshi has no native multi-leg orders; venue-side
        // atomicity is best-effort (the WS rejection cascade is
        // implemented in `apply_execution`). What this function
        // guarantees is *DB-side* atomicity.

        // 1. Per-leg pre-check.
        for intent in &group.intents {
            if let Err(reason) = self.pre_check(intent) {
                return Ok(SubmitGroupOutcome::Rejected {
                    reason,
                    failing_client_id: intent.client_id.clone(),
                });
            }
        }

        let mut tx = self.pool.begin().await?;
        lock_submit_section(&mut tx).await?;

        // 2. Idempotency / collision probe.
        let client_ids: Vec<String> = group.intents.iter().map(|i| i.client_id.clone()).collect();
        let existing: Vec<(String, Option<uuid::Uuid>)> =
            sqlx::query_as("SELECT client_id, leg_group_id FROM intents WHERE client_id = ANY($1)")
                .bind(&client_ids)
                .fetch_all(&mut *tx)
                .await?;
        if !existing.is_empty() {
            // Are ALL members of `group` accounted for, AND under
            // the same group_id?
            let all_present = existing.len() == group.intents.len();
            let same_group = existing
                .iter()
                .all(|(_, gid)| gid.as_ref() == Some(&group.group_id));
            if all_present && same_group {
                debug!(
                    group_id = %group.group_id,
                    n_legs = group.intents.len(),
                    "oms: idempotent leg-group re-submit"
                );
                return Ok(SubmitGroupOutcome::Idempotent {
                    group_id: group.group_id,
                    client_ids,
                });
            }
            // Partial overlap — dangerous. Refuse and let the
            // operator resolve.
            warn!(
                group_id = %group.group_id,
                n_existing = existing.len(),
                n_legs = group.intents.len(),
                "oms: leg-group submit collides with existing rows under different (or no) group"
            );
            return Ok(SubmitGroupOutcome::PartialCollision { existing });
        }

        // 3. Combined exposure check. We compute the strategy's
        //    AND global notional ONCE and add the combined
        //    leg-projected notional. Per-leg side / in-flight
        //    caps still apply individually.
        let caps = &self.risk_caps;
        if group.intents.is_empty() {
            // Defensive — `LegGroup::new` already rejects empty.
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::InvalidIntent {
                    reason: "empty leg group".into(),
                },
                failing_client_id: String::new(),
            });
        }

        // Combined notional first, ~one snapshot of strategy +
        // global state. `current_exposure` is keyed on the leg's
        // ticker for the contract-side check; for combined
        // notional we re-use the snapshot from the first leg.
        let first = &group.intents[0];
        let baseline = self.current_exposure(first, &mut tx).await?;
        let mut combined_added = 0_i64;
        for intent in &group.intents {
            let exposure = self.current_exposure(intent, &mut tx).await?;
            combined_added += project_position(intent, &exposure).added_notional_cents;
        }

        if baseline.daily_pnl_cents < -caps.max_daily_loss_cents {
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::DailyLossExceeded {
                    strategy: first.strategy,
                },
                failing_client_id: first.client_id.clone(),
            });
        }
        if baseline.in_flight + i32::try_from(group.intents.len()).unwrap_or(i32::MAX)
            > caps.max_in_flight
        {
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::TooManyInFlight {
                    strategy: first.strategy,
                    in_flight: baseline.in_flight,
                    limit: caps.max_in_flight,
                },
                failing_client_id: first.client_id.clone(),
            });
        }
        if baseline.orders_in_rate_window + u32::try_from(group.intents.len()).unwrap_or(u32::MAX)
            > caps.max_orders_per_window
        {
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::RateLimited {
                    window_ms: caps.rate_window_ms,
                },
                failing_client_id: first.client_id.clone(),
            });
        }
        if baseline.strategy_notional_cents + combined_added > caps.max_notional_cents {
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::NotionalExceeded {
                    scope: format!("strategy:{}", first.strategy),
                    current_cents: baseline.strategy_notional_cents,
                    limit_cents: caps.max_notional_cents,
                },
                failing_client_id: first.client_id.clone(),
            });
        }
        if caps.max_global_notional_cents > 0
            && baseline.global_notional_cents + combined_added > caps.max_global_notional_cents
        {
            return Ok(SubmitGroupOutcome::Rejected {
                reason: RejectionReason::NotionalExceeded {
                    scope: "global".into(),
                    current_cents: baseline.global_notional_cents,
                    limit_cents: caps.max_global_notional_cents,
                },
                failing_client_id: first.client_id.clone(),
            });
        }

        // Per-leg contract-side cap. Each leg's check is
        // independent of others (different ticker → different
        // `(strategy, ticker, side)` row in `positions`).
        for intent in &group.intents {
            let exposure = self.current_exposure(intent, &mut tx).await?;
            let projected = project_position(intent, &exposure).projected_abs_contracts;
            if projected > caps.max_contracts_per_side {
                return Ok(SubmitGroupOutcome::Rejected {
                    reason: RejectionReason::ContractCapExceeded {
                        ticker: intent.market.as_str().to_string(),
                        side: side_to_str(intent).to_string(),
                        current: projected,
                        limit: caps.max_contracts_per_side,
                    },
                    failing_client_id: intent.client_id.clone(),
                });
            }
        }

        // 4. Atomic insert. All-or-none — if any insert fails the
        //    whole transaction rolls back and we return the DB
        //    error.
        let initial_status = self.mode.initial_status();
        for intent in &group.intents {
            sqlx::query(
                "INSERT INTO intents
                    (client_id, strategy, ticker, side, action, price_cents,
                     qty, order_type, tif, status, cumulative_qty, reason,
                     leg_group_id, post_only)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $11, 0, $10, $12, $13)",
            )
            .bind(&intent.client_id)
            .bind(intent.strategy)
            .bind(intent.market.as_str())
            .bind(side_to_str(intent))
            .bind(action_to_str(intent.action))
            .bind(intent.price_cents)
            .bind(intent.qty)
            .bind(order_type_to_str(intent.order_type))
            .bind(tif_to_str(intent.tif))
            .bind(intent.reason.as_deref())
            .bind(initial_status)
            .bind(group.group_id)
            .bind(intent.post_only)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO intent_events (client_id, status, venue_payload)
                 VALUES ($1, $2, NULL)",
            )
            .bind(&intent.client_id)
            .bind(initial_status)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        info!(
            group_id = %group.group_id,
            n_legs = group.intents.len(),
            combined_notional_cents = combined_added,
            "oms: leg group persisted atomically"
        );

        Ok(SubmitGroupOutcome::Submitted {
            group_id: group.group_id,
            client_ids,
            venue: VenueChoice::Rest,
        })
    }

    async fn cancel(&self, client_id: &str) -> EngineResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE intents SET status = 'cancel_requested', last_updated_at = now()
              WHERE client_id = $1
                AND status NOT IN ('filled','cancelled','rejected','expired')",
        )
        .bind(client_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO intent_events (client_id, status, venue_payload)
             VALUES ($1, 'cancel_requested', NULL)",
        )
        .bind(client_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn apply_execution(&self, ev: ExecutionUpdate) -> EngineResult<()> {
        let mut tx = self.pool.begin().await?;

        if matches!(
            ev.status,
            ExecutionStatus::Filled | ExecutionStatus::PartialFill
        ) && let Some(fid) = ev.venue_fill_id.as_deref()
        {
            let already: Option<(i64,)> =
                sqlx::query_as("SELECT id FROM fills WHERE venue_fill_id = $1")
                    .bind(fid)
                    .fetch_optional(&mut *tx)
                    .await?;
            if already.is_some() {
                debug!(
                    client_id = %ev.client_id,
                    venue_fill_id = fid,
                    "apply_execution: duplicate fill (full no-op)"
                );
                tx.commit().await?;
                return Ok(());
            }
        }

        // 1. Intents row update (status, cumulative qty, avg fill,
        //    venue order id).
        sqlx::query(
            "UPDATE intents
                SET status = $2,
                    cumulative_qty = $3,
                    avg_fill_price_cents = COALESCE($4, avg_fill_price_cents),
                    venue_order_id = COALESCE($5, venue_order_id),
                    last_updated_at = now()
              WHERE client_id = $1",
        )
        .bind(&ev.client_id)
        .bind(execution_status_str(ev.status))
        .bind(ev.cumulative_qty)
        .bind(ev.avg_fill_price_cents)
        .bind(ev.venue_order_id.as_deref())
        .execute(&mut *tx)
        .await?;

        // 2. Append the timeline event.
        sqlx::query(
            "INSERT INTO intent_events (client_id, status, venue_payload)
             VALUES ($1, $2, $3)",
        )
        .bind(&ev.client_id)
        .bind(execution_status_str(ev.status))
        .bind(&ev.venue_payload)
        .execute(&mut *tx)
        .await?;

        // 2b. **Audit I7** — leg-group cancellation cascade.
        //
        // When any leg of a group is venue-rejected, the
        // remaining still-active siblings (status NOT IN terminal
        // set) are marked `cancel_requested` so the venue router
        // sends cancels for them. Skip cascading on Filled —
        // partial fills are handled by the strategy, not this
        // path. Done inside the same transaction as the leg's
        // own status update so we can't observe a partial
        // cascade on crash.
        if matches!(
            ev.status,
            ExecutionStatus::Rejected | ExecutionStatus::Expired
        ) {
            let group_id: Option<(Option<uuid::Uuid>,)> =
                sqlx::query_as("SELECT leg_group_id FROM intents WHERE client_id = $1")
                    .bind(&ev.client_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some((Some(gid),)) = group_id {
                let cascaded: Vec<(String,)> = sqlx::query_as(
                    "UPDATE intents
                        SET status = 'cancel_requested',
                            last_updated_at = now()
                      WHERE leg_group_id = $1
                        AND client_id != $2
                        AND status NOT IN ('filled','cancelled','rejected','expired','cancel_requested')
                      RETURNING client_id",
                )
                .bind(gid)
                .bind(&ev.client_id)
                .fetch_all(&mut *tx)
                .await?;
                for (sibling_id,) in &cascaded {
                    sqlx::query(
                        "INSERT INTO intent_events (client_id, status, venue_payload)
                         VALUES ($1, 'cancel_requested', $2)",
                    )
                    .bind(sibling_id)
                    .bind(serde_json::json!({
                        "cascade_source": ev.client_id,
                        "cascade_reason": execution_status_str(ev.status),
                        "leg_group_id": gid,
                    }))
                    .execute(&mut *tx)
                    .await?;
                }
                if !cascaded.is_empty() {
                    info!(
                        leg_group_id = %gid,
                        triggered_by = %ev.client_id,
                        n_cascaded = cascaded.len(),
                        "oms: leg-group cancellation cascade fired"
                    );
                }
            }
        }

        // 3. Fill row + position update on Filled / PartialFill.
        //
        // Idempotency: every WS-push fill carries a venue-assigned
        // `venue_fill_id` (Kalshi's `trade_id`). The same fill can
        // legitimately arrive twice — WS replays after a reconnect,
        // or REST `list_fills` polling running alongside WS push as
        // belt-and-suspenders. We dedupe on `venue_fill_id` BEFORE
        // running the position cascade, so a replayed fill is a
        // no-op rather than a double-credit. The
        // `fills.venue_fill_id` UNIQUE index would catch it at
        // insert time, but doing the check first means we don't
        // run the (more expensive) position upsert for nothing.
        if matches!(
            ev.status,
            ExecutionStatus::Filled | ExecutionStatus::PartialFill
        ) {
            if let (Some(qty), Some(price)) = (ev.last_fill_qty, ev.last_fill_price_cents) {
                let fee = ev.last_fill_fee_cents.unwrap_or(0);

                // Look up intent metadata for fill row.
                let row: Option<(String, String, String, String)> = sqlx::query_as(
                    "SELECT strategy, ticker, side, action
                       FROM intents WHERE client_id = $1",
                )
                .bind(&ev.client_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some((strategy, ticker, side, action)) = row else {
                    warn!(
                        client_id = %ev.client_id,
                        "apply_execution: fill arrived for unknown intent"
                    );
                    return Err(EngineError::Oms("fill for unknown intent".into()));
                };

                sqlx::query(
                    "INSERT INTO fills
                        (client_id, venue_fill_id, ticker, strategy,
                         side, action, price_cents, qty, fee_cents)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                )
                .bind(&ev.client_id)
                .bind(ev.venue_fill_id.as_deref())
                .bind(&ticker)
                .bind(&strategy)
                .bind(&side)
                .bind(&action)
                .bind(price)
                .bind(qty)
                .bind(fee)
                .execute(&mut *tx)
                .await?;

                // Position lifecycle: insert if new, update qty
                // and avg if existing. Closing leg sets closed_at.
                upsert_position(&mut tx, &strategy, &ticker, &side, &action, qty, price, fee)
                    .await?;
            }
        }

        tx.commit().await?;

        info!(
            client_id = %ev.client_id,
            status = ?ev.status,
            cumulative_qty = ev.cumulative_qty,
            "oms: execution applied"
        );
        Ok(())
    }

    async fn reconcile(&self) -> EngineResult<ReconciliationDiff> {
        let Some(rest) = &self.rest else {
            return Err(EngineError::Oms(
                "reconcile requires an authenticated REST client".into(),
            ));
        };

        let active: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT client_id, venue_order_id, status
               FROM intents
              WHERE status NOT IN ('filled','cancelled','rejected','expired','shadow')
              ORDER BY last_updated_at DESC
              LIMIT 500",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut diff = ReconciliationDiff::default();
        let mut venue_seen = HashSet::new();
        for (client_id, venue_order_id, db_status) in &active {
            let Some(order_id) = venue_order_id.as_deref() else {
                diff.orders_only_in_db.push(client_id.clone());
                continue;
            };
            venue_seen.insert(order_id.to_string());
            match rest.get_order(order_id).await {
                Ok(resp) => {
                    reconcile_order_status(self, &resp.order, client_id, db_status, &mut diff)
                        .await?;
                    catch_up_order_fills(self, rest, client_id, &resp.order).await?;
                }
                Err(RestError::Api { status: 404, body }) => {
                    diff.orders_only_in_db.push(client_id.clone());
                    mark_intent_terminal(
                        &self.pool,
                        client_id,
                        "expired",
                        serde_json::json!({
                            "kind": "reconciliation_order_absent",
                            "venue_order_id": order_id,
                            "body": body,
                        }),
                    )
                    .await?;
                }
                Err(e) => return Err(EngineError::Oms(format!("get_order {order_id}: {e}"))),
            }
        }

        let mut cursor: Option<String> = None;
        loop {
            let resp = rest
                .list_orders(None, Some("resting"), None, Some(100), cursor.as_deref())
                .await
                .map_err(|e| EngineError::Oms(format!("list resting orders: {e}")))?;
            for order in resp.orders {
                if !venue_seen.contains(&order.order_id) {
                    let known: Option<(String,)> =
                        sqlx::query_as("SELECT client_id FROM intents WHERE venue_order_id = $1")
                            .bind(&order.order_id)
                            .fetch_optional(&self.pool)
                            .await?;
                    if known.is_none() {
                        diff.orders_only_at_venue.push(order.order_id);
                    }
                }
            }
            cursor = resp.cursor.filter(|c| !c.is_empty());
            if cursor.is_none() {
                break;
            }
        }

        reconcile_positions(self, rest, &mut diff).await?;
        if diff.is_clean() {
            debug!("oms: reconciliation clean");
        } else {
            warn!(?diff, "oms: reconciliation found drift");
        }
        Ok(diff)
    }
}

#[derive(Debug)]
struct ExposureSnapshot {
    current_qty: i32,
    current_avg_entry_cents: i32,
    strategy_notional_cents: i64,
    /// Phase 6.2 — total open notional across every strategy in
    /// `positions`. Used for the global-cap check.
    global_notional_cents: i64,
    in_flight: i32,
    orders_in_rate_window: u32,
    daily_pnl_cents: i64,
}

#[derive(Debug)]
struct PositionProjection {
    projected_abs_contracts: i32,
    added_notional_cents: i64,
}

#[derive(Debug)]
struct DailyPnlSnapshot {
    pnl_cents: i64,
}

async fn lock_submit_section(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> EngineResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('predigy_oms_submit')::BIGINT)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn signed_intent_qty(intent: &Intent) -> i32 {
    match intent.action {
        IntentAction::Buy => intent.qty,
        IntentAction::Sell => -intent.qty,
    }
}

fn project_position(intent: &Intent, exposure: &ExposureSnapshot) -> PositionProjection {
    let signed_qty = signed_intent_qty(intent);
    let projected_qty = exposure.current_qty + signed_qty;
    let fill_cents = i64::from(intent.price_cents.unwrap_or(50));

    let current_abs = i64::from(exposure.current_qty.abs());
    let projected_abs = i64::from(projected_qty.abs());
    let current_notional = current_abs * i64::from(exposure.current_avg_entry_cents);

    let projected_notional =
        if exposure.current_qty == 0 || signed_qty.signum() == exposure.current_qty.signum() {
            current_notional + fill_cents * i64::from(intent.qty)
        } else if projected_qty.signum() == exposure.current_qty.signum() || projected_qty == 0 {
            projected_abs * i64::from(exposure.current_avg_entry_cents)
        } else {
            // Reversal: the reducing portion closes at the old basis; only the
            // excess beyond flat opens new exposure at the submitted price.
            let reversed_abs = i64::from(intent.qty) - current_abs;
            reversed_abs.max(0) * fill_cents
        };

    PositionProjection {
        projected_abs_contracts: projected_qty.abs(),
        added_notional_cents: projected_notional - current_notional,
    }
}

async fn daily_pnl_with_marks(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    strategy: &str,
) -> EngineResult<DailyPnlSnapshot> {
    let rows: Vec<DailyPnlMarkRow> = sqlx::query_as(
        "SELECT p.current_qty,
                p.side,
                p.avg_entry_cents,
                p.realized_pnl_cents,
                p.fees_paid_cents,
                b.best_yes_bid_cents,
                b.best_yes_ask_cents,
                b.last_update
           FROM positions p
      LEFT JOIN book_snapshots b ON b.ticker = p.ticker
          WHERE p.strategy = $1
            AND (p.closed_at >= date_trunc('day', now()) OR p.closed_at IS NULL)",
    )
    .bind(strategy)
    .fetch_all(&mut **tx)
    .await?;

    let mut pnl = 0_i64;
    let now = chrono::Utc::now();
    for (qty, side, avg_entry, realized, fees, bid, ask, mark_ts) in rows {
        pnl += realized - fees;
        if qty == 0 {
            continue;
        }
        let recent = mark_ts.is_some_and(|ts| now.signed_duration_since(ts).num_seconds() <= 120);
        // Stale or missing marks are valued pessimistically instead of bypassing
        // the daily-loss gate.
        let (mark_cents, _conservative) = risk_mark_cents(&side, qty, bid, ask, recent);
        pnl += i64::from(mark_cents - avg_entry) * i64::from(qty.signum()) * i64::from(qty.abs());
    }
    Ok(DailyPnlSnapshot { pnl_cents: pnl })
}

fn domain_mark_cents(
    side: &str,
    qty: i32,
    best_yes_bid_cents: Option<i32>,
    best_yes_ask_cents: Option<i32>,
) -> Option<i32> {
    match (side, qty.signum()) {
        ("yes", 1) => best_yes_bid_cents,
        ("yes", -1) => best_yes_ask_cents,
        ("no", 1) => best_yes_ask_cents.map(|ask| 100 - ask),
        ("no", -1) => best_yes_bid_cents.map(|bid| 100 - bid),
        _ => None,
    }
}

fn risk_mark_cents(
    side: &str,
    qty: i32,
    best_yes_bid_cents: Option<i32>,
    best_yes_ask_cents: Option<i32>,
    recent: bool,
) -> (i32, bool) {
    if recent
        && let Some(mark) = domain_mark_cents(side, qty, best_yes_bid_cents, best_yes_ask_cents)
    {
        return (mark, false);
    }
    if qty > 0 { (0, true) } else { (100, true) }
}

fn rest_fill_price_cents_for_intent_side(
    fill: &FillRecord,
    intent_side: &str,
) -> EngineResult<i32> {
    let price = match intent_side {
        "yes" => dollars_str_to_cents(&fill.yes_price_dollars),
        "no" => dollars_str_to_cents(&fill.no_price_dollars),
        other => {
            return Err(EngineError::Oms(format!(
                "unknown intent side for REST fill {}: {other}",
                fill.fill_id
            )));
        }
    };
    price.ok_or_else(|| EngineError::Oms(format!("invalid fill price for {}", fill.fill_id)))
}

async fn reconcile_order_status(
    oms: &DbBackedOms,
    order: &OrderRecord,
    client_id: &str,
    db_status: &str,
    diff: &mut ReconciliationDiff,
) -> EngineResult<()> {
    let target = match order.status.as_str() {
        "resting" => "acked",
        "canceled" => "cancelled",
        "executed" => "filled",
        other => other,
    };
    if db_status == target || (target == "acked" && db_status == "partial_fill") {
        return Ok(());
    }
    diff.status_mismatches.push((
        client_id.to_string(),
        db_status.to_string(),
        order.status.clone(),
    ));

    match target {
        "acked" if db_status == "submitted" => {
            mark_intent_status(
                &oms.pool,
                client_id,
                "acked",
                Some(&order.order_id),
                serde_json::json!({"kind": "reconciliation_order_resting", "order": order}),
            )
            .await?;
        }
        "cancelled" => {
            mark_intent_terminal(
                &oms.pool,
                client_id,
                "cancelled",
                serde_json::json!({"kind": "reconciliation_order_cancelled", "order": order}),
            )
            .await?;
        }
        "filled" => {
            let cumulative = order
                .fill_count_fp
                .as_deref()
                .and_then(parse_contracts_fp)
                .unwrap_or(0);
            <DbBackedOms as Oms>::apply_execution(
                oms,
                ExecutionUpdate {
                    client_id: client_id.to_string(),
                    venue_order_id: Some(order.order_id.clone()),
                    venue_fill_id: None,
                    status: ExecutionStatus::Filled,
                    cumulative_qty: cumulative,
                    avg_fill_price_cents: None,
                    last_fill_qty: None,
                    last_fill_price_cents: None,
                    last_fill_fee_cents: None,
                    venue_payload: serde_json::json!({
                        "kind": "reconciliation_order_executed",
                        "order": order,
                    }),
                },
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn catch_up_order_fills(
    oms: &DbBackedOms,
    rest: &Arc<RestClient>,
    client_id: &str,
    order: &OrderRecord,
) -> EngineResult<()> {
    let intent_side: Option<(String,)> =
        sqlx::query_as("SELECT side FROM intents WHERE client_id = $1")
            .bind(client_id)
            .fetch_optional(&oms.pool)
            .await?;
    let Some((side,)) = intent_side else {
        return Err(EngineError::Oms(format!(
            "reconciliation fill catch-up for unknown intent {client_id}"
        )));
    };

    let mut cursor: Option<String> = None;
    loop {
        let resp = rest
            .list_fills(Some(&order.order_id), None, Some(100), cursor.as_deref())
            .await
            .map_err(|e| EngineError::Oms(format!("list_fills {}: {e}", order.order_id)))?;
        for fill in resp.fills {
            apply_rest_fill(oms, client_id, order, &side, &fill).await?;
        }
        cursor = resp.cursor.filter(|c| !c.is_empty());
        if cursor.is_none() {
            break;
        }
    }
    Ok(())
}

async fn apply_rest_fill(
    oms: &DbBackedOms,
    client_id: &str,
    order: &OrderRecord,
    intent_side: &str,
    fill: &FillRecord,
) -> EngineResult<()> {
    let qty = parse_contracts_fp(&fill.count_fp).ok_or_else(|| {
        EngineError::Oms(format!(
            "invalid fill count_fp '{}' for {}",
            fill.count_fp, fill.fill_id
        ))
    })?;
    let price = rest_fill_price_cents_for_intent_side(fill, intent_side)?;
    let fee = fill
        .fee_cost
        .as_deref()
        .and_then(dollars_str_to_cents)
        .unwrap_or(0);
    let cumulative = order
        .fill_count_fp
        .as_deref()
        .and_then(parse_contracts_fp)
        .unwrap_or(qty);
    let remaining = order
        .remaining_count_fp
        .as_deref()
        .and_then(parse_contracts_fp)
        .unwrap_or(0);
    let status = if order.status == "executed" || remaining == 0 {
        ExecutionStatus::Filled
    } else {
        ExecutionStatus::PartialFill
    };
    let fill_id = fill
        .trade_id
        .as_deref()
        .unwrap_or(&fill.fill_id)
        .to_string();
    <DbBackedOms as Oms>::apply_execution(
        oms,
        ExecutionUpdate {
            client_id: client_id.to_string(),
            venue_order_id: Some(order.order_id.clone()),
            venue_fill_id: Some(fill_id),
            status,
            cumulative_qty: cumulative,
            avg_fill_price_cents: Some(price),
            last_fill_qty: Some(qty),
            last_fill_price_cents: Some(price),
            last_fill_fee_cents: Some(fee),
            venue_payload: serde_json::json!({
                "kind": "reconciliation_fill_catchup",
                "fill": fill,
                "order": order,
            }),
        },
    )
    .await
}

async fn reconcile_positions(
    oms: &DbBackedOms,
    rest: &Arc<RestClient>,
    diff: &mut ReconciliationDiff,
) -> EngineResult<()> {
    let venue = rest
        .positions()
        .await
        .map_err(|e| EngineError::Oms(format!("positions: {e}")))?;
    let mut venue_by_ticker = HashMap::new();
    for p in venue.market_positions {
        if let Some(qty) = venue_position_qty(&p) {
            if qty != 0 {
                venue_by_ticker.insert(p.ticker.clone(), qty);
            }
        }
    }

    let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT ticker,
                SUM(CASE WHEN side = 'yes' THEN current_qty ELSE -current_qty END)::BIGINT
           FROM positions
          WHERE closed_at IS NULL
          GROUP BY ticker",
    )
    .fetch_all(&oms.pool)
    .await?;
    let mut db_tickers = HashSet::new();
    for (ticker, qty) in rows {
        let db_qty = i32::try_from(qty.unwrap_or(0)).unwrap_or(0);
        db_tickers.insert(ticker.clone());
        let venue_qty = venue_by_ticker.get(&ticker).copied().unwrap_or(0);
        if db_qty != venue_qty {
            diff.position_mismatches.push((ticker, db_qty, venue_qty));
        }
    }
    for (ticker, venue_qty) in venue_by_ticker {
        if !db_tickers.contains(&ticker) {
            diff.position_mismatches.push((ticker, 0, venue_qty));
        }
    }
    Ok(())
}

async fn mark_intent_status(
    pool: &PgPool,
    client_id: &str,
    status: &str,
    venue_order_id: Option<&str>,
    payload: serde_json::Value,
) -> EngineResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE intents
            SET status = $2,
                venue_order_id = COALESCE($3, venue_order_id),
                last_updated_at = now()
          WHERE client_id = $1",
    )
    .bind(client_id)
    .bind(status)
    .bind(venue_order_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO intent_events (client_id, status, venue_payload) VALUES ($1, $2, $3)")
        .bind(client_id)
        .bind(status)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_intent_terminal(
    pool: &PgPool,
    client_id: &str,
    status: &str,
    payload: serde_json::Value,
) -> EngineResult<()> {
    mark_intent_status(pool, client_id, status, None, payload).await
}

fn parse_contracts_fp(s: &str) -> Option<i32> {
    let v: f64 = s.parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some(v.round() as i32)
}

fn dollars_str_to_cents(s: &str) -> Option<i32> {
    let v: f64 = s.parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some((v * 100.0).round() as i32)
}

fn venue_position_qty(p: &MarketPosition) -> Option<i32> {
    p.position_contracts.map(|q| q.round() as i32)
}

async fn upsert_position(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    strategy: &str,
    ticker: &str,
    side: &str,
    action: &str,
    fill_qty: i32,
    fill_price_cents: i32,
    fee_cents: i32,
) -> EngineResult<()> {
    let signed_qty = if action == "buy" { fill_qty } else { -fill_qty };

    let existing: Option<(i64, i32, i32)> = sqlx::query_as(
        "SELECT id, current_qty, avg_entry_cents
           FROM positions
          WHERE strategy = $1 AND ticker = $2 AND side = $3 AND closed_at IS NULL
          FOR UPDATE",
    )
    .bind(strategy)
    .bind(ticker)
    .bind(side)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some((id, cur_qty, cur_avg)) = existing {
        let new_qty = cur_qty + signed_qty;
        // Branch on whether the fill is adding to, partially
        // closing, fully closing, or reversing the position. The
        // **fill direction** (`signed_qty.signum()`) vs the
        // **current position direction** (`cur_qty.signum()`) is
        // the right discriminator — NOT `new_qty.signum()`. A
        // partial close (sell 2 against +3 long) leaves new_qty
        // positive, which previously matched the "same sign"
        // condition and ran the weighted-avg formula on a
        // reducing fill, corrupting avg_entry_cents.
        let same_direction = cur_qty == 0 || signed_qty.signum() == cur_qty.signum();
        if new_qty == 0 {
            // Full close. Realised PnL formula:
            //
            //   long close (cur_qty > 0):
            //     pnl = (close_price - entry) * abs(closed_qty)
            //   short close (cur_qty < 0):
            //     pnl = (entry - close_price) * abs(closed_qty)
            //         = -(close_price - entry) * abs(closed_qty)
            //
            // Combined: pnl = (close_price - entry) * cur_qty.signum() * abs(closed).
            // The sign comes from the SIDE WE WERE ON (cur_qty),
            // not the closing-fill direction.
            let realised =
                (fill_price_cents - cur_avg) as i64 * cur_qty.signum() as i64 * i64::from(fill_qty);
            sqlx::query(
                "UPDATE positions
                    SET current_qty = 0,
                        closed_at = now(),
                        last_fill_at = now(),
                        realized_pnl_cents = realized_pnl_cents + $2,
                        fees_paid_cents = fees_paid_cents + $3
                  WHERE id = $1",
            )
            .bind(id)
            .bind(realised)
            .bind(i64::from(fee_cents))
            .execute(&mut **tx)
            .await?;
        } else if same_direction {
            // Adding to position (same side as existing or
            // opening from flat). Weighted avg.
            let new_avg = (cur_qty.unsigned_abs() as i64 * i64::from(cur_avg)
                + i64::from(fill_qty) * i64::from(fill_price_cents))
                / new_qty.unsigned_abs() as i64;
            sqlx::query(
                "UPDATE positions
                    SET current_qty = $2,
                        avg_entry_cents = $3,
                        last_fill_at = now(),
                        fees_paid_cents = fees_paid_cents + $4
                  WHERE id = $1",
            )
            .bind(id)
            .bind(new_qty)
            .bind(new_avg as i32)
            .bind(i64::from(fee_cents))
            .execute(&mut **tx)
            .await?;
        } else if new_qty.signum() == cur_qty.signum() {
            // Partial close — fill reduces the position but
            // doesn't cross zero. Realise PnL on the closed
            // portion; avg_entry stays unchanged (the remaining
            // contracts retain their original entry basis).
            let closed_qty = fill_qty;
            let realised = (fill_price_cents - cur_avg) as i64
                * cur_qty.signum() as i64
                * i64::from(closed_qty);
            sqlx::query(
                "UPDATE positions
                    SET current_qty = $2,
                        last_fill_at = now(),
                        realized_pnl_cents = realized_pnl_cents + $3,
                        fees_paid_cents = fees_paid_cents + $4
                  WHERE id = $1",
            )
            .bind(id)
            .bind(new_qty)
            .bind(realised)
            .bind(i64::from(fee_cents))
            .execute(&mut **tx)
            .await?;
        } else {
            // Reversal — fill flips the position to the
            // opposite side. Realise PnL on the previously-held
            // portion (cur_qty.abs()), then reset avg_entry to
            // the fill price for the new opposing portion.
            //
            // Example: cur=+3 long @ 60, sell 5 @ 65 →
            //   close 3 @ 65 (realise 3*(65-60)=15c), open new
            //   short of 2 @ 65.
            let closed_qty = cur_qty.abs();
            let realised = (fill_price_cents - cur_avg) as i64
                * cur_qty.signum() as i64
                * i64::from(closed_qty);
            sqlx::query(
                "UPDATE positions
                    SET current_qty = $2,
                        avg_entry_cents = $3,
                        last_fill_at = now(),
                        realized_pnl_cents = realized_pnl_cents + $4,
                        fees_paid_cents = fees_paid_cents + $5
                  WHERE id = $1",
            )
            .bind(id)
            .bind(new_qty)
            .bind(fill_price_cents)
            .bind(realised)
            .bind(i64::from(fee_cents))
            .execute(&mut **tx)
            .await?;
        }
    } else {
        // New position.
        sqlx::query(
            "INSERT INTO positions
                (strategy, ticker, side, current_qty, avg_entry_cents,
                 fees_paid_cents, opened_at, last_fill_at)
             VALUES ($1, $2, $3, $4, $5, $6, now(), now())",
        )
        .bind(strategy)
        .bind(ticker)
        .bind(side)
        .bind(signed_qty)
        .bind(fill_price_cents)
        .bind(i64::from(fee_cents))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

// ─── Enum-to-string helpers ─────────────────────────────────

fn side_to_str(intent: &Intent) -> &'static str {
    use predigy_core::side::Side;
    match intent.side {
        Side::Yes => "yes",
        Side::No => "no",
    }
}

fn action_to_str(action: IntentAction) -> &'static str {
    match action {
        IntentAction::Buy => "buy",
        IntentAction::Sell => "sell",
    }
}

fn order_type_to_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Limit => "limit",
        OrderType::Market => "market",
    }
}

fn tif_to_str(t: Tif) -> &'static str {
    match t {
        Tif::Ioc => "ioc",
        Tif::Gtc => "gtc",
        Tif::Fok => "fok",
    }
}

fn execution_status_str(s: ExecutionStatus) -> &'static str {
    match s {
        ExecutionStatus::Submitted => "submitted",
        ExecutionStatus::Acked => "acked",
        ExecutionStatus::PartialFill => "partial_fill",
        ExecutionStatus::Filled => "filled",
        ExecutionStatus::Cancelled => "cancelled",
        ExecutionStatus::Rejected => "rejected",
        ExecutionStatus::Expired => "expired",
    }
}

#[cfg(test)]
mod tests {
    use super::{domain_mark_cents, rest_fill_price_cents_for_intent_side, risk_mark_cents};
    use predigy_kalshi_rest::types::FillRecord;

    #[test]
    fn domain_mark_uses_yes_book_for_yes_positions() {
        assert_eq!(domain_mark_cents("yes", 3, Some(42), Some(44)), Some(42));
        assert_eq!(domain_mark_cents("yes", -3, Some(42), Some(44)), Some(44));
    }

    #[test]
    fn domain_mark_complements_yes_book_for_no_positions() {
        assert_eq!(domain_mark_cents("no", 3, Some(84), Some(87)), Some(13));
        assert_eq!(domain_mark_cents("no", -3, Some(84), Some(87)), Some(16));
    }

    #[test]
    fn domain_mark_requires_relevant_book_side() {
        assert_eq!(domain_mark_cents("no", 3, Some(84), None), None);
        assert_eq!(domain_mark_cents("no", -3, None, Some(87)), None);
        assert_eq!(domain_mark_cents("yes", 0, Some(42), Some(44)), None);
    }

    #[test]
    fn risk_mark_uses_domain_mark_when_recent() {
        assert_eq!(
            risk_mark_cents("no", 3, Some(84), Some(87), true),
            (13, false)
        );
        assert_eq!(
            risk_mark_cents("yes", -2, Some(42), Some(44), true),
            (44, false)
        );
    }

    #[test]
    fn risk_mark_conservatively_values_stale_or_missing_marks() {
        assert_eq!(
            risk_mark_cents("yes", 3, Some(42), Some(44), false),
            (0, true)
        );
        assert_eq!(risk_mark_cents("no", 3, Some(84), None, true), (0, true));
        assert_eq!(
            risk_mark_cents("yes", -2, Some(42), Some(44), false),
            (100, true)
        );
        assert_eq!(risk_mark_cents("no", -2, None, Some(87), true), (100, true));
    }

    fn fill_record() -> FillRecord {
        FillRecord {
            fill_id: "fill-test".into(),
            trade_id: Some("trade-test".into()),
            order_id: "order-test".into(),
            market_ticker: Some("KX-TEST".into()),
            ticker: None,
            side: "yes".into(),
            action: String::new(),
            count_fp: "1.00".into(),
            yes_price_dollars: "0.84".into(),
            no_price_dollars: "0.16".into(),
            is_taker: Some(true),
            fee_cost: None,
            ts: None,
            ts_ms: None,
        }
    }

    #[test]
    fn rest_fill_price_uses_intent_side_domain() {
        let fill = fill_record();
        assert_eq!(
            rest_fill_price_cents_for_intent_side(&fill, "yes").unwrap(),
            84
        );
        assert_eq!(
            rest_fill_price_cents_for_intent_side(&fill, "no").unwrap(),
            16
        );
    }

    #[test]
    fn rest_fill_price_rejects_unknown_side() {
        let fill = fill_record();
        assert!(rest_fill_price_cents_for_intent_side(&fill, "maybe").is_err());
    }
}
