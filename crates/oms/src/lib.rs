//! Order management system: the central state machine that owns the
//! account's position ledger and is the only thing that calls the
//! venue executor.
//!
//! Per the plan: "Strategies own no I/O — they emit `Intent` values
//! into a channel; OMS owns all venue I/O." The OMS runs in a single
//! tokio task; everything that wants to mutate state crosses an
//! `mpsc` boundary, so race conditions that "double-fire orders"
//! (the cited worst case) are structurally impossible.
//!
//! Together with `predigy-risk`, this crate is one of the two where
//! every change requires two-person review.
//!
//! ## What's covered (and what isn't)
//!
//! Covered:
//! - Synchronous pre-trade risk check on the OMS task.
//! - Deterministic per-strategy client order ids.
//! - Full lifecycle tracking: pending → acked → partially filled →
//!   filled / cancelled / rejected.
//! - Position bookkeeping with VWAP, including realised P&L on sells.
//! - Reconciliation against an externally-supplied venue position
//!   snapshot (mismatches surfaced as events).
//! - Kill switch.
//!
//! Deferred (Phase 2 follow-ups):
//! - Durable cid sequence storage so cids never repeat across
//!   restarts. Today the binary supplies a starting seq via
//!   [`OmsConfig::cid_backing`] (use `CidBacking::Persistent` for
//!   production).
//! - Mass-cancel wiring on kill-switch arm. Requires the FIX exec.
//! - Persistent OMS state (`sqlx`/Postgres) per the plan. Today the
//!   ledger is in-memory.
//! - Order-amend support (Kalshi's `OrderCancelReplaceRequest`).

pub mod cid;
pub mod executor;
pub mod kill_watcher;
pub mod persistence;
pub mod position_math;
pub mod record;
pub mod runtime;

pub use cid::CidAllocator;
pub use cid::{CidError, CidStore};
pub use executor::{
    ExecutionReport, ExecutionReportKind, Executor, ExecutorError,
    stub::{StubCall, StubExecutor, channel as stub_channel},
};
pub use kill_watcher::spawn_kill_watcher;
pub use persistence::{PersistedOmsState, StateBacking, StateError};
pub use position_math::{PositionUpdate, apply_fill};
pub use record::OrderRecord;
pub use runtime::{
    CidBacking, Oms, OmsConfig, OmsControl, OmsError, OmsEvent, OmsHandle, PositionMismatch,
};
