//! OMS (Order Management System) abstractions. The OMS sits
//! between strategy intents and the venue. It owns:
//!
//! - **Idempotency**: client_id is the primary key in `intents`,
//!   so a strategy that emits the same intent twice gets the
//!   second one swallowed (DB rejects on conflict). Strategies
//!   construct stable client_ids; the OMS doesn't generate them.
//!
//! - **Risk caps enforcement**: per-strategy and global limits on
//!   total notional, daily loss, in-flight order count, and
//!   contract count per side per market. Cap violations reject
//!   the intent before it reaches the venue.
//!
//! - **Kill switch enforcement**: armed switches at strategy or
//!   global scope reject all new intents and (depending on
//!   reason) signal a flush of held positions.
//!
//! - **Status persistence**: every state transition lands in
//!   `intents` (current status, cumulative qty, etc.) AND
//!   `intent_events` (append-only timeline). FIX
//!   ExecutionReports + REST fill polls both feed this.
//!
//! - **Reconciliation**: a periodic loop pulls the venue's view
//!   of our open orders + positions and diffs vs DB; mismatches
//!   surface as `EngineError::Oms` for the supervisor to alert
//!   on.

use crate::error::{EngineError, EngineResult};
use crate::intent::Intent;
use serde::{Deserialize, Serialize};

/// Outcome of submitting an intent to the OMS.
#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    /// Intent persisted, sent to the venue, awaiting ack.
    Submitted {
        client_id: String,
        venue: VenueChoice,
    },
    /// Intent already exists with this client_id; OMS treats as
    /// idempotent no-op. The strategy can ignore.
    Idempotent {
        client_id: String,
        current_status: String,
    },
    /// Intent rejected by risk / kill-switch BEFORE reaching the
    /// venue. The strategy decides whether to log + retry later.
    Rejected { reason: RejectionReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueChoice {
    Fix,
    Rest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RejectionReason {
    KillSwitchArmed {
        scope: String,
    },
    DailyLossExceeded {
        strategy: &'static str,
    },
    NotionalExceeded {
        scope: String,
        current_cents: i64,
        limit_cents: i64,
    },
    ContractCapExceeded {
        ticker: String,
        side: String,
        current: i32,
        limit: i32,
    },
    TooManyInFlight {
        strategy: &'static str,
        in_flight: i32,
        limit: i32,
    },
    RateLimited {
        window_ms: u64,
    },
    UnknownMarket {
        ticker: String,
    },
    InvalidIntent {
        reason: String,
    },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::KillSwitchArmed { scope } => write!(f, "kill switch armed: {scope}"),
            RejectionReason::DailyLossExceeded { strategy } => {
                write!(f, "{strategy} daily loss cap reached")
            }
            RejectionReason::NotionalExceeded {
                scope,
                current_cents,
                limit_cents,
            } => {
                write!(
                    f,
                    "{scope} notional cap: ${} > ${}",
                    current_cents / 100,
                    limit_cents / 100
                )
            }
            RejectionReason::ContractCapExceeded {
                ticker,
                side,
                current,
                limit,
            } => {
                write!(f, "{ticker}.{side} contract cap: {current} > {limit}")
            }
            RejectionReason::TooManyInFlight {
                strategy,
                in_flight,
                limit,
            } => {
                write!(f, "{strategy} too many in-flight: {in_flight}/{limit}")
            }
            RejectionReason::RateLimited { window_ms } => {
                write!(f, "rate-limited ({window_ms}ms window)")
            }
            RejectionReason::UnknownMarket { ticker } => write!(f, "unknown market {ticker}"),
            RejectionReason::InvalidIntent { reason } => write!(f, "invalid: {reason}"),
        }
    }
}

/// Risk caps applied to every intent. Per-strategy AND global
/// caps both checked; the smaller binding limit wins.
#[derive(Debug, Clone)]
pub struct RiskCaps {
    /// Hard ceiling on open-position notional **per strategy**,
    /// in cents.
    pub max_notional_cents: i64,
    /// **Phase 6.2** — hard ceiling on open-position notional
    /// **across all strategies**, in cents. Caps the total
    /// dollar amount the engine has at risk in the venue at any
    /// moment. The OMS checks both per-strategy and global; the
    /// smaller binding limit wins.
    ///
    /// This is the cross-strategy correlate of `max_notional_cents`.
    /// Without it a stat trade + a settlement trade could each be
    /// within their own per-strategy cap but jointly blow past
    /// what the operator wants the engine to risk in total.
    ///
    /// `0` disables the global cap (per-strategy caps still apply).
    pub max_global_notional_cents: i64,
    /// Realised + unrealised loss for the day before refusing
    /// new entries.
    pub max_daily_loss_cents: i64,
    /// Hard ceiling on contracts per (ticker, side) — caps
    /// directional exposure.
    pub max_contracts_per_side: i32,
    /// In-flight (submitted-but-not-filled-or-cancelled) order
    /// count cap.
    pub max_in_flight: i32,
    /// Rate limiter: at most N orders per window.
    pub max_orders_per_window: u32,
    pub rate_window_ms: u64,
}

impl RiskCaps {
    /// Tight defaults suitable for the $50-cap shake-down phase.
    /// Override per-strategy as confidence grows.
    pub fn shake_down() -> Self {
        Self {
            max_notional_cents: 500, // $5/strategy
            // 4 strategies × $5/strategy = $20/global. The
            // global cap is intentionally LESS than the sum so
            // it actually binds; otherwise it'd be a dead knob.
            max_global_notional_cents: 1500, // $15
            max_daily_loss_cents: 200,       // $2
            max_contracts_per_side: 3,
            max_in_flight: 10,
            max_orders_per_window: 5,
            rate_window_ms: 1000,
        }
    }
}

/// Trait the engine uses to drive the OMS. Implementations live
/// in the engine binary (database-backed) and in tests
/// (in-memory).
#[async_trait::async_trait]
pub trait Oms: Send + Sync {
    async fn submit(&self, intent: Intent) -> EngineResult<SubmitOutcome>;

    async fn cancel(&self, client_id: &str) -> EngineResult<()>;

    /// Apply a venue-side update (FIX ExecutionReport, REST fill
    /// poll, WS execution event). Updates `intents` row + appends
    /// to `intent_events` + cascades to `positions`.
    async fn apply_execution(&self, ev: ExecutionUpdate) -> EngineResult<()>;

    /// One pass of reconciliation against the venue snapshot.
    /// Logs + emits structured diff events but doesn't auto-
    /// repair; that's an operator decision.
    async fn reconcile(&self) -> EngineResult<ReconciliationDiff>;
}

#[derive(Debug, Clone)]
pub struct ExecutionUpdate {
    pub client_id: String,
    pub venue_order_id: Option<String>,
    /// Per-fill venue id. Kalshi assigns a distinct id to each
    /// fill (FIX `ExecID`, REST `trade_id`); this is the dedup
    /// key for the `fills` table. Required when `status` is
    /// `Filled` or `PartialFill`; optional otherwise.
    pub venue_fill_id: Option<String>,
    pub status: ExecutionStatus,
    pub cumulative_qty: i32,
    pub avg_fill_price_cents: Option<i32>,
    pub last_fill_qty: Option<i32>,
    pub last_fill_price_cents: Option<i32>,
    pub last_fill_fee_cents: Option<i32>,
    pub venue_payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Submitted,
    Acked,
    PartialFill,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Default)]
pub struct ReconciliationDiff {
    pub orders_only_in_db: Vec<String>,
    pub orders_only_at_venue: Vec<String>,
    pub status_mismatches: Vec<(String, String, String)>, // (client_id, db_status, venue_status)
}

impl ReconciliationDiff {
    pub fn is_clean(&self) -> bool {
        self.orders_only_in_db.is_empty()
            && self.orders_only_at_venue.is_empty()
            && self.status_mismatches.is_empty()
    }
}

/// Constructed by the engine and shared (via `Arc`) with the
/// OMS implementation so kill-switch flips take effect on the
/// next intent submission without a DB query in the hot path.
#[derive(Debug)]
pub struct KillSwitchView {
    armed: std::sync::atomic::AtomicBool,
}

impl KillSwitchView {
    pub fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn clear(&self) {
        self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for KillSwitchView {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
fn _engine_error_alive(_: EngineError) {}
