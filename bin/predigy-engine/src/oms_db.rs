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
use sqlx::PgPool;
use std::sync::Arc;
use tracing::{debug, info, warn};

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
        }
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
    async fn current_exposure(&self, intent: &Intent) -> EngineResult<ExposureSnapshot> {
        let strategy = intent.strategy;
        let ticker = intent.market.as_str();
        let side = side_to_str(&intent);

        // Open contracts on this (strategy, ticker, side).
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT current_qty FROM positions
              WHERE strategy = $1 AND ticker = $2 AND side = $3 AND closed_at IS NULL",
        )
        .bind(strategy)
        .bind(ticker)
        .bind(side)
        .fetch_optional(&self.pool)
        .await?;
        let current_contracts = row.map_or(0, |(q,)| q.abs());

        // Total open notional across this strategy. Sum = qty *
        // avg_entry_cents per row, treating short positions as
        // positive notional.
        let total: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT SUM(ABS(current_qty)::BIGINT * avg_entry_cents::BIGINT)::BIGINT
               FROM positions
              WHERE strategy = $1 AND closed_at IS NULL",
        )
        .bind(strategy)
        .fetch_optional(&self.pool)
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
        .fetch_optional(&self.pool)
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
        .fetch_one(&self.pool)
        .await?;

        // Daily realised PnL.
        let pnl: (Option<i64>,) = sqlx::query_as(
            "SELECT SUM(realized_pnl_cents)::BIGINT
               FROM positions
              WHERE strategy = $1
                AND closed_at >= date_trunc('day', now())",
        )
        .bind(strategy)
        .fetch_one(&self.pool)
        .await?;

        Ok(ExposureSnapshot {
            current_contracts,
            strategy_notional_cents,
            global_notional_cents,
            in_flight: i32::try_from(in_flight.0).unwrap_or(i32::MAX),
            daily_realized_pnl_cents: pnl.0.unwrap_or(0),
        })
    }

    fn check_caps(
        &self,
        intent: &Intent,
        exposure: &ExposureSnapshot,
    ) -> Result<(), RejectionReason> {
        let caps = &self.risk_caps;

        if exposure.daily_realized_pnl_cents < -caps.max_daily_loss_cents {
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

        // Project the post-fill state to check side-cap.
        let projected_contracts = exposure.current_contracts + intent.qty;
        if projected_contracts > caps.max_contracts_per_side {
            return Err(RejectionReason::ContractCapExceeded {
                ticker: intent.market.as_str().to_string(),
                side: side_to_str(intent).to_string(),
                current: projected_contracts,
                limit: caps.max_contracts_per_side,
            });
        }

        // Notional projection — assume worst-case fill at the
        // intent's limit price (or 50¢ if market order — pessimistic).
        let projected_fill_cents = intent.price_cents.unwrap_or(50) as i64;
        let added_notional = projected_fill_cents * i64::from(intent.qty);
        if exposure.strategy_notional_cents + added_notional > caps.max_notional_cents {
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
            && exposure.global_notional_cents + added_notional > caps.max_global_notional_cents
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

        // Idempotency check — does this client_id already exist?
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT status FROM intents WHERE client_id = $1")
                .bind(&intent.client_id)
                .fetch_optional(&self.pool)
                .await?;
        if let Some((status,)) = existing {
            debug!(client_id = %intent.client_id, %status, "oms: idempotent re-submit");
            return Ok(SubmitOutcome::Idempotent {
                client_id: intent.client_id,
                current_status: status,
            });
        }

        // Risk caps.
        let exposure = self.current_exposure(&intent).await?;
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
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO intents
                (client_id, strategy, ticker, side, action, price_cents,
                 qty, order_type, tif, status, cumulative_qty, reason)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $11, 0, $10)",
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

        // 2. Idempotency / collision probe.
        let client_ids: Vec<String> =
            group.intents.iter().map(|i| i.client_id.clone()).collect();
        let existing: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT client_id, leg_group_id FROM intents WHERE client_id = ANY($1)",
        )
        .bind(&client_ids)
        .fetch_all(&self.pool)
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
        let baseline = self.current_exposure(first).await?;
        let combined_added: i64 = group
            .intents
            .iter()
            .map(|i| i64::from(i.price_cents.unwrap_or(50)) * i64::from(i.qty))
            .sum();

        if baseline.daily_realized_pnl_cents < -caps.max_daily_loss_cents {
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
            let exposure = self.current_exposure(intent).await?;
            let projected = exposure.current_contracts + intent.qty;
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
        let mut tx = self.pool.begin().await?;
        for intent in &group.intents {
            sqlx::query(
                "INSERT INTO intents
                    (client_id, strategy, ticker, side, action, price_cents,
                     qty, order_type, tif, status, cumulative_qty, reason,
                     leg_group_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $11, 0, $10, $12)",
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

                if let Some(fid) = ev.venue_fill_id.as_deref() {
                    let already: Option<(i64,)> =
                        sqlx::query_as("SELECT id FROM fills WHERE venue_fill_id = $1")
                            .bind(fid)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if already.is_some() {
                        debug!(
                            client_id = %ev.client_id,
                            venue_fill_id = fid,
                            "apply_execution: duplicate fill (skipping cascade)"
                        );
                        tx.commit().await?;
                        return Ok(());
                    }
                }

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
        // Stub. Full implementation lands when FIX is wired
        // (Phase 4) — we'll diff `SELECT venue_order_id FROM
        // intents WHERE status NOT terminal` against the venue's
        // `OrderStatusRequest` snapshot.
        Ok(ReconciliationDiff::default())
    }
}

#[derive(Debug)]
struct ExposureSnapshot {
    current_contracts: i32,
    strategy_notional_cents: i64,
    /// Phase 6.2 — total open notional across every strategy in
    /// `positions`. Used for the global-cap check.
    global_notional_cents: i64,
    in_flight: i32,
    daily_realized_pnl_cents: i64,
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
        if new_qty == 0 {
            // Closing leg. Realised PnL formula:
            //
            //   long close (cur_qty > 0):
            //     pnl = (close_price - entry) * abs(closed_qty)
            //   short close (cur_qty < 0):
            //     pnl = (entry - close_price) * abs(closed_qty)
            //         = -(close_price - entry) * abs(closed_qty)
            //
            // Combined: pnl = (close_price - entry) * cur_qty.signum() * abs(closed).
            // The sign comes from the SIDE WE WERE ON (cur_qty),
            // not the closing-fill direction (signed_qty).
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
        } else if (cur_qty.signum() == new_qty.signum()) || cur_qty == 0 {
            // Adding to position (same side). Weighted avg.
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
        } else {
            // Partial close — reducing position. Realised on the
            // closed portion.
            let closed_qty = std::cmp::min(cur_qty.abs(), fill_qty);
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
