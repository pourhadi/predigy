//! OMS runtime: a single tokio task that owns all order state.
//!
//! ## Architecture
//!
//! Every input crosses a channel boundary into the task. The task owns
//! the [`AccountState`], the per-order map, the cid allocator, the
//! risk engine, and the executor. There is no shared mutable state
//! and no locks — race conditions that "double-fire orders" (the plan's
//! cited worst case) are structurally impossible.
//!
//! ```text
//! strategy ──submit/cancel──▶ cmd_rx        ┌───────────┐
//! kill switch    ──▶          cmd_rx ─────▶ │  Oms task │ ──submit──▶ Executor
//! venue ──ExecutionReport──▶ reports_rx ──▶ │           │ ──cancel──▶
//! tests        ──reconcile──▶ cmd_rx        └─────┬─────┘
//!                                                 │
//!                                                 ▼ event_tx
//!                                            consumer (logging /
//!                                                       audit / strategy
//!                                                       state mirror)
//! ```
//!
//! Public API: spawn the task with [`Oms::spawn`] and use the returned
//! [`OmsHandle`] from any number of strategies.

use crate::cid::CidAllocator;
use crate::executor::{ExecutionReport, ExecutionReportKind, Executor, ExecutorError};
use crate::position_math::apply_fill;
use crate::record::OrderRecord;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId, OrderState};
use predigy_core::price::Price;
use predigy_core::side::Side;
use predigy_risk::{AccountState, Decision, Reason, RiskEngine};
use std::collections::HashMap;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const CMD_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 4096;

/// Configuration handed to [`Oms::spawn`].
#[derive(Debug, Clone)]
pub struct OmsConfig {
    /// Identifier embedded in client order ids and used for log
    /// filtering.
    pub strategy_id: String,
    /// Cid allocator backing. Use [`CidBacking::InMemory`] for tests
    /// and [`CidBacking::Persistent`] in production so cids survive
    /// restarts (see [`crate::cid::CidStore`]).
    pub cid_backing: CidBacking,
    /// Account-state + orders-map persistence. Use
    /// [`StateBacking::Persistent`] in production so a process crash
    /// doesn't lose the daily-loss breaker, kill-switch state, or
    /// the in-flight orders ledger. Default is `InMemory` for tests
    /// and one-shot scripts.
    pub state_backing: crate::persistence::StateBacking,
}

#[derive(Debug, Clone)]
pub enum CidBacking {
    /// Start at `start_seq`. Cids are not persisted — on restart they
    /// reset.
    InMemory { start_seq: u64 },
    /// Persistent file-backed allocation. The OMS pre-claims chunks
    /// to avoid an fsync per submit; at most `chunk_size − 1` cids
    /// are wasted across a crash but no cid is ever reused.
    Persistent {
        store_path: std::path::PathBuf,
        chunk_size: u64,
    },
}

impl Default for OmsConfig {
    fn default() -> Self {
        Self {
            strategy_id: "default".into(),
            cid_backing: CidBacking::InMemory { start_seq: 0 },
            state_backing: crate::persistence::StateBacking::InMemory,
        }
    }
}

/// Failure modes when spawning an OMS task. The cid store and the
/// state snapshot can each fail independently — keeping them as
/// separate variants lets the operator tell which one to fix.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("cid store: {0}")]
    Cid(#[from] crate::cid::CidError),
    #[error("state snapshot: {0}")]
    State(#[from] crate::persistence::StateError),
}

/// One-shot handle returned by [`Oms::spawn`]. Drop or call
/// [`OmsHandle::close`] to terminate the runtime task.
#[derive(Debug)]
pub struct OmsHandle {
    cmd_tx: mpsc::Sender<OmsCmd>,
    event_rx: mpsc::Receiver<OmsEvent>,
    task: Option<JoinHandle<()>>,
}

impl OmsHandle {
    /// Submit an `Intent`. Returns the allocated `OrderId` once risk
    /// has approved and the executor has accepted it. If risk rejects
    /// or the executor errors, the corresponding [`OmsError`] is
    /// returned and the order is not booked.
    pub async fn submit(&self, intent: Intent) -> Result<OrderId, OmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(OmsCmd::Submit {
                intent,
                reply: reply_tx,
            })
            .await
            .map_err(|_| OmsError::Closed)?;
        reply_rx.await.map_err(|_| OmsError::Closed)?
    }

    /// Request cancel of a working order by client id.
    pub async fn cancel(&self, cid: OrderId) -> Result<(), OmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(OmsCmd::Cancel {
                cid,
                reply: reply_tx,
            })
            .await
            .map_err(|_| OmsError::Closed)?;
        reply_rx.await.map_err(|_| OmsError::Closed)?
    }

    /// Arm the kill switch. Subsequent submits reject with
    /// `OmsError::KillSwitch`. Pre-existing working orders are
    /// untouched here — the binary is responsible for issuing a mass
    /// cancel via the executor (Kalshi FIX `35=q`).
    pub async fn arm_kill_switch(&self) -> Result<(), OmsError> {
        self.cmd_tx
            .send(OmsCmd::ArmKillSwitch)
            .await
            .map_err(|_| OmsError::Closed)
    }

    pub async fn disarm_kill_switch(&self) -> Result<(), OmsError> {
        self.cmd_tx
            .send(OmsCmd::DisarmKillSwitch)
            .await
            .map_err(|_| OmsError::Closed)
    }

    /// Cheap clone-able control surface — arm/disarm only. Used by
    /// background tasks (e.g. flag-file watchers, control sockets)
    /// that need to issue control commands without owning the
    /// `OmsHandle` event-receiver half.
    pub fn control(&self) -> OmsControl {
        OmsControl {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Externally-supplied reconciliation: the venue's authoritative
    /// view of `(market, side) -> contracts`. The OMS compares this to
    /// its own ledger and emits `OmsEvent::Reconciled` (with any
    /// mismatches) before returning. The binary is expected to fetch
    /// these via `predigy_kalshi_rest::Client::positions` on a timer.
    pub async fn reconcile(
        &self,
        venue_positions: HashMap<(MarketTicker, Side), u32>,
    ) -> Result<(), OmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(OmsCmd::Reconcile {
                venue_positions,
                reply: reply_tx,
            })
            .await
            .map_err(|_| OmsError::Closed)?;
        reply_rx.await.map_err(|_| OmsError::Closed)?;
        Ok(())
    }

    /// Pop the next event. Returns `None` once the task has fully
    /// exited and its event queue is drained.
    pub async fn next_event(&mut self) -> Option<OmsEvent> {
        self.event_rx.recv().await
    }

    /// Graceful shutdown: drain commands, wait for the task to exit.
    pub async fn close(mut self) {
        let _ = self.cmd_tx.send(OmsCmd::Shutdown).await;
        if let Some(handle) = self.task.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for OmsHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}

/// Cloneable control surface returned by [`OmsHandle::control`].
/// Lets background tasks arm/disarm the kill switch (and, in the
/// future, other control commands) without owning the OMS handle.
#[derive(Debug, Clone)]
pub struct OmsControl {
    cmd_tx: mpsc::Sender<OmsCmd>,
}

impl OmsControl {
    pub async fn arm_kill_switch(&self) -> Result<(), OmsError> {
        self.cmd_tx
            .send(OmsCmd::ArmKillSwitch)
            .await
            .map_err(|_| OmsError::Closed)
    }

    pub async fn disarm_kill_switch(&self) -> Result<(), OmsError> {
        self.cmd_tx
            .send(OmsCmd::DisarmKillSwitch)
            .await
            .map_err(|_| OmsError::Closed)
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum OmsError {
    #[error("kill switch is armed")]
    KillSwitch,
    #[error("risk rejected: {0}")]
    RiskRejected(Reason),
    #[error("executor: {0}")]
    Executor(String),
    #[error("unknown order id: {0}")]
    UnknownOrder(OrderId),
    #[error("oms task closed")]
    Closed,
}

/// Externally-observable events emitted by the OMS as the order
/// lifecycle progresses. Strategy code that cares about its own fills
/// should subscribe; downstream observability code reads the same
/// stream.
#[derive(Debug, Clone)]
pub enum OmsEvent {
    Submitted {
        cid: OrderId,
        order: Order,
    },
    Acked {
        cid: OrderId,
        venue_order_id: String,
    },
    PartiallyFilled {
        cid: OrderId,
        delta_qty: u32,
        cumulative_qty: u32,
        fill_price: Price,
    },
    Filled {
        cid: OrderId,
        delta_qty: u32,
        cumulative_qty: u32,
        fill_price: Price,
    },
    Cancelled {
        cid: OrderId,
        reason: String,
    },
    Rejected {
        cid: OrderId,
        reason: String,
    },
    PositionUpdated {
        market: MarketTicker,
        side: Side,
        new_qty: u32,
        new_avg_entry_cents: u16,
        realized_pnl_delta_cents: i64,
    },
    Reconciled {
        mismatches: Vec<PositionMismatch>,
    },
    KillSwitchArmed,
    KillSwitchDisarmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionMismatch {
    pub market: MarketTicker,
    pub side: Side,
    pub oms_qty: u32,
    pub venue_qty: u32,
}

#[derive(Debug)]
enum OmsCmd {
    Submit {
        intent: Intent,
        reply: oneshot::Sender<Result<OrderId, OmsError>>,
    },
    Cancel {
        cid: OrderId,
        reply: oneshot::Sender<Result<(), OmsError>>,
    },
    ArmKillSwitch,
    DisarmKillSwitch,
    Reconcile {
        venue_positions: HashMap<(MarketTicker, Side), u32>,
        reply: oneshot::Sender<()>,
    },
    Shutdown,
}

/// The OMS state container. Use [`Oms::spawn`] rather than constructing
/// directly.
pub struct Oms<E: Executor> {
    config: OmsConfig,
    risk: RiskEngine,
    executor: E,
    state: AccountState,
    orders: HashMap<OrderId, OrderRecord>,
    cid_alloc: CidAllocator,
}

impl<E: Executor> std::fmt::Debug for Oms<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oms")
            .field("strategy_id", &self.config.strategy_id)
            .field("orders", &self.orders.len())
            .finish_non_exhaustive()
    }
}

impl<E: Executor + 'static> Oms<E> {
    /// Spawn the OMS task and return a handle. Caller owns the
    /// [`mpsc::Receiver`] half of the executor's report channel and
    /// passes it in here; `Oms` owns the executor itself.
    pub fn spawn(
        config: OmsConfig,
        risk: RiskEngine,
        executor: E,
        reports: mpsc::Receiver<ExecutionReport>,
    ) -> OmsHandle {
        Self::try_spawn(config, risk, executor, reports)
            .expect("OmsConfig::cid_backing initialisation; use try_spawn for fallible variant")
    }

    /// Fallible spawn — reports cid-store and state-snapshot I/O
    /// errors instead of panicking. Production binaries should call
    /// this and surface the error in their startup path.
    pub fn try_spawn(
        config: OmsConfig,
        risk: RiskEngine,
        executor: E,
        reports: mpsc::Receiver<ExecutionReport>,
    ) -> Result<OmsHandle, SpawnError> {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let cid_alloc = match &config.cid_backing {
            CidBacking::InMemory { start_seq } => {
                CidAllocator::new(&config.strategy_id, *start_seq)
            }
            CidBacking::Persistent {
                store_path,
                chunk_size,
            } => CidAllocator::with_store_and_chunk(
                &config.strategy_id,
                crate::cid::CidStore::new(store_path.clone()),
                *chunk_size,
            )
            .map_err(SpawnError::Cid)?,
        };

        // Rehydrate prior persisted state, if any. Missing file =
        // first run = empty state. Schema mismatch / parse failure =
        // bail loudly so the operator notices rather than the OMS
        // silently restarting from zero (would re-arm the daily-loss
        // breaker, lose track of in-flight orders).
        let (state, orders) =
            if let crate::persistence::StateBacking::Persistent { path } = &config.state_backing {
                if let Some(snap) = crate::persistence::load(path).map_err(SpawnError::State)? {
                    let count = snap.orders.len();
                    info!(?path, orders = count, "oms state loaded from snapshot");
                    crate::persistence::rehydrate(snap, std::time::Instant::now())
                } else {
                    info!(?path, "oms state path empty; starting fresh");
                    (AccountState::new(), HashMap::new())
                }
            } else {
                (AccountState::new(), HashMap::new())
            };

        let oms = Self {
            config,
            risk,
            executor,
            state,
            orders,
            cid_alloc,
        };
        let task = tokio::spawn(run_task(oms, cmd_rx, reports, event_tx));
        Ok(OmsHandle {
            cmd_tx,
            event_rx,
            task: Some(task),
        })
    }
}

async fn run_task<E: Executor>(
    mut oms: Oms<E>,
    mut cmd_rx: mpsc::Receiver<OmsCmd>,
    mut reports_rx: mpsc::Receiver<ExecutionReport>,
    event_tx: mpsc::Sender<OmsEvent>,
) {
    info!(strategy = %oms.config.strategy_id, "oms task starting");
    loop {
        tokio::select! {
            // Bias toward execution reports so a steady fill stream
            // doesn't starve under heavy submit load — fills mutate
            // shared state risk reads, so propagating them quickly is
            // strictly safer.
            biased;
            maybe_report = reports_rx.recv() => {
                let Some(report) = maybe_report else {
                    info!("execution-report channel closed; oms exiting");
                    break;
                };
                if oms.handle_report(report, &event_tx).await.is_err() {
                    break;
                }
                oms.persist_state();
            }
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    info!("command channel closed; oms exiting");
                    break;
                };
                match cmd {
                    OmsCmd::Shutdown => break,
                    other => {
                        if oms.handle_command(other, &event_tx).await.is_err() {
                            break;
                        }
                        oms.persist_state();
                    }
                }
            }
        }
    }
    info!("oms task exiting");
}

/// Sentinel: event channel was closed by the consumer. Shut down rather
/// than spin against `event_tx.send()`.
struct EventChannelClosed;

impl<E: Executor> Oms<E> {
    /// Snapshot account + orders to disk if `state_backing` is
    /// configured for persistence. Called after every mutation
    /// (submit, ack, fill, cancel, kill-switch arm, reconcile).
    ///
    /// IO failures are logged but don't propagate — the in-memory
    /// state is the live source of truth, and a write that fails
    /// once is likely to fail again on the next mutation, so
    /// surfacing it noisily without crashing the OMS task is the
    /// right trade-off. Production deploys should monitor for the
    /// `state snapshot save failed` warning.
    fn persist_state(&self) {
        let crate::persistence::StateBacking::Persistent { path } = &self.config.state_backing
        else {
            return;
        };
        let snap = crate::persistence::snapshot(&self.state, &self.orders);
        if let Err(e) = crate::persistence::save(path, &snap) {
            warn!(error = %e, ?path, "state snapshot save failed; in-memory state still authoritative");
        }
    }
}

impl<E: Executor> Oms<E> {
    async fn handle_command(
        &mut self,
        cmd: OmsCmd,
        event_tx: &mpsc::Sender<OmsEvent>,
    ) -> Result<(), EventChannelClosed> {
        match cmd {
            OmsCmd::Submit { intent, reply } => {
                let result = self.handle_submit(intent, event_tx).await;
                let _ = reply.send(result);
            }
            OmsCmd::Cancel { cid, reply } => {
                let result = self.handle_cancel(cid).await;
                let _ = reply.send(result);
            }
            OmsCmd::ArmKillSwitch => {
                self.state.arm_kill_switch();
                // Best-effort mass cancel of every live order. The
                // executor's individual cancel might fail (already
                // filled, race with a partial fill) — log and keep
                // going; the goal is "stop new exposure" which the
                // kill-switch flag already guarantees by rejecting
                // future submits.
                let live: Vec<OrderId> = self
                    .orders
                    .iter()
                    .filter_map(|(cid, r)| {
                        if r.is_terminal() || r.cancel_in_flight {
                            None
                        } else {
                            Some(cid.clone())
                        }
                    })
                    .collect();
                let count = live.len();
                for cid in live {
                    if let Some(r) = self.orders.get_mut(&cid) {
                        r.cancel_in_flight = true;
                    }
                    if let Err(e) = self.executor.cancel(&cid).await {
                        warn!(cid = %cid, error = %e, "kill-switch cancel failed; continuing");
                        if let Some(r) = self.orders.get_mut(&cid) {
                            r.cancel_in_flight = false;
                        }
                    }
                }
                info!(
                    orders_cancelled = count,
                    "kill switch armed; cancels dispatched"
                );
                if event_tx.send(OmsEvent::KillSwitchArmed).await.is_err() {
                    return Err(EventChannelClosed);
                }
            }
            OmsCmd::DisarmKillSwitch => {
                self.state.disarm_kill_switch();
                if event_tx.send(OmsEvent::KillSwitchDisarmed).await.is_err() {
                    return Err(EventChannelClosed);
                }
            }
            OmsCmd::Reconcile {
                venue_positions,
                reply,
            } => {
                let mismatches = self.diff_positions(&venue_positions);
                if event_tx
                    .send(OmsEvent::Reconciled { mismatches })
                    .await
                    .is_err()
                {
                    return Err(EventChannelClosed);
                }
                let _ = reply.send(());
            }
            OmsCmd::Shutdown => {} // handled in run_task
        }
        Ok(())
    }

    async fn handle_submit(
        &mut self,
        intent: Intent,
        event_tx: &mpsc::Sender<OmsEvent>,
    ) -> Result<OrderId, OmsError> {
        if self.state.kill_switch_active() {
            return Err(OmsError::KillSwitch);
        }
        // Synchronous risk check on this task — same thread as the
        // single-writer of state, so the projection is consistent.
        let now = Instant::now();
        let decision = self.risk.check(&intent, &mut self.state, now);
        if let Decision::Reject(reason) = decision {
            return Err(OmsError::RiskRejected(reason));
        }
        // Allocate cid + build the Order.
        let cid = self.cid_alloc.next(&intent.market);
        let order = Order {
            client_id: cid.clone(),
            market: intent.market.clone(),
            side: intent.side,
            action: intent.action,
            price: intent.price,
            qty: intent.qty,
            order_type: intent.order_type,
            tif: intent.tif,
        };
        // Submit before recording — we don't want a phantom order in
        // `orders` if the executor rejects synchronously.
        if let Err(e) = self.executor.submit(&order).await {
            return Err(OmsError::Executor(e.to_string()));
        }
        // Submit succeeded: record state + bump the rate-limit window.
        self.state.record_order_sent(now);
        self.orders
            .insert(cid.clone(), OrderRecord::new(order.clone(), now));
        if event_tx
            .send(OmsEvent::Submitted {
                cid: cid.clone(),
                order,
            })
            .await
            .is_err()
        {
            // Caller dropped the event stream — shut down the task on
            // the next select! tick. Still return the cid so the
            // caller's oneshot resolves.
            warn!("event channel closed during submit; oms will exit");
        }
        Ok(cid)
    }

    async fn handle_cancel(&mut self, cid: OrderId) -> Result<(), OmsError> {
        let Some(record) = self.orders.get_mut(&cid) else {
            return Err(OmsError::UnknownOrder(cid));
        };
        if record.is_terminal() {
            return Err(OmsError::UnknownOrder(cid));
        }
        record.cancel_in_flight = true;
        match self.executor.cancel(&cid).await {
            Ok(()) => Ok(()),
            Err(ExecutorError::UnknownOrder(_)) => {
                // Treat as already-gone; the venue's truth wins. Don't
                // synthesise a Cancelled event — the real one will
                // arrive (or has already) on the report channel.
                record.cancel_in_flight = false;
                Err(OmsError::UnknownOrder(cid))
            }
            Err(e) => {
                record.cancel_in_flight = false;
                Err(OmsError::Executor(e.to_string()))
            }
        }
    }

    async fn handle_report(
        &mut self,
        report: ExecutionReport,
        event_tx: &mpsc::Sender<OmsEvent>,
    ) -> Result<(), EventChannelClosed> {
        let cid = report.cid.clone();
        let Some(record) = self.orders.get_mut(&cid) else {
            warn!(cid = %cid, "execution report for unknown cid; ignoring");
            return Ok(());
        };
        let now = Instant::now();
        match report.kind {
            ExecutionReportKind::Acked { venue_order_id } => {
                record.mark_acked(venue_order_id.clone(), now);
                if event_tx
                    .send(OmsEvent::Acked {
                        cid,
                        venue_order_id,
                    })
                    .await
                    .is_err()
                {
                    return Err(EventChannelClosed);
                }
            }
            ExecutionReportKind::PartiallyFilled {
                fill,
                cumulative_qty,
            } => {
                let delta = record.apply_fill(fill.price, cumulative_qty, now, false);
                if delta == 0 {
                    debug!(cid = %cid, "stale partial-fill report; ignoring");
                    return Ok(());
                }
                let market = record.order.market.clone();
                let side = record.order.side;
                let action = record.order.action;
                self.update_position_and_emit(
                    market,
                    side,
                    action,
                    delta,
                    fill.price,
                    cid.clone(),
                    cumulative_qty,
                    false,
                    event_tx,
                )
                .await?;
            }
            ExecutionReportKind::Filled {
                fill,
                cumulative_qty,
            } => {
                let delta = record.apply_fill(fill.price, cumulative_qty, now, true);
                if delta == 0 {
                    debug!(cid = %cid, "stale terminal-fill report; ignoring");
                    return Ok(());
                }
                let market = record.order.market.clone();
                let side = record.order.side;
                let action = record.order.action;
                self.update_position_and_emit(
                    market,
                    side,
                    action,
                    delta,
                    fill.price,
                    cid.clone(),
                    cumulative_qty,
                    true,
                    event_tx,
                )
                .await?;
            }
            ExecutionReportKind::Cancelled { reason } => {
                record.mark_cancelled(now);
                if event_tx
                    .send(OmsEvent::Cancelled { cid, reason })
                    .await
                    .is_err()
                {
                    return Err(EventChannelClosed);
                }
            }
            ExecutionReportKind::Rejected { reason } => {
                let prior_state = record.state;
                record.mark_rejected(now);
                if prior_state == OrderState::Pending {
                    // Pre-ack reject means the order never existed at the
                    // venue; we can drop the record here so the table
                    // doesn't grow with rejected entries.
                    self.orders.remove(&cid);
                }
                if event_tx
                    .send(OmsEvent::Rejected { cid, reason })
                    .await
                    .is_err()
                {
                    return Err(EventChannelClosed);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_position_and_emit(
        &mut self,
        market: MarketTicker,
        side: Side,
        action: predigy_core::side::Action,
        delta_qty: u32,
        fill_price: Price,
        cid: OrderId,
        cumulative_qty: u32,
        terminal: bool,
        event_tx: &mpsc::Sender<OmsEvent>,
    ) -> Result<(), EventChannelClosed> {
        let current_qty = self.state.position(&market, side);
        let current_avg = self.state.avg_entry_cents(&market, side);
        let update = apply_fill(action, current_qty, current_avg, delta_qty, fill_price);
        self.state.set_position(
            market.clone(),
            side,
            update.new_qty,
            update.new_avg_entry_cents,
        );
        if update.realized_pnl_delta_cents != 0 {
            self.state.add_realized_pnl(update.realized_pnl_delta_cents);
        }
        let fill_event = if terminal {
            OmsEvent::Filled {
                cid: cid.clone(),
                delta_qty,
                cumulative_qty,
                fill_price,
            }
        } else {
            OmsEvent::PartiallyFilled {
                cid: cid.clone(),
                delta_qty,
                cumulative_qty,
                fill_price,
            }
        };
        if event_tx.send(fill_event).await.is_err() {
            return Err(EventChannelClosed);
        }
        if event_tx
            .send(OmsEvent::PositionUpdated {
                market,
                side,
                new_qty: update.new_qty,
                new_avg_entry_cents: update.new_avg_entry_cents,
                realized_pnl_delta_cents: update.realized_pnl_delta_cents,
            })
            .await
            .is_err()
        {
            return Err(EventChannelClosed);
        }
        Ok(())
    }

    fn diff_positions(&self, venue: &HashMap<(MarketTicker, Side), u32>) -> Vec<PositionMismatch> {
        let mut mismatches = Vec::new();
        // Walk the venue's view first — a position present at the venue
        // but not in the OMS is a mismatch we definitely want to flag.
        for ((market, side), &venue_qty) in venue {
            let oms_qty = self.state.position(market, *side);
            if oms_qty != venue_qty {
                mismatches.push(PositionMismatch {
                    market: market.clone(),
                    side: *side,
                    oms_qty,
                    venue_qty,
                });
            }
        }
        // And anything the OMS has booked that the venue doesn't know
        // about — typically a leftover from a stale local state, but
        // worth surfacing.
        // Snapshotting the OMS's keys requires a small allocation since
        // AccountState doesn't expose a direct iterator; we walk our
        // own working-orders map instead, which captures every market
        // we've sent something to.
        for record in self.orders.values() {
            let key = (record.order.market.clone(), record.order.side);
            let oms_qty = self.state.position(&key.0, key.1);
            if oms_qty == 0 {
                continue;
            }
            if !venue.contains_key(&key) {
                mismatches.push(PositionMismatch {
                    market: key.0,
                    side: key.1,
                    oms_qty,
                    venue_qty: 0,
                });
            }
        }
        mismatches.sort_by(|a, b| a.market.cmp(&b.market).then(a.side.cmp(&b.side)));
        mismatches.dedup();
        mismatches
    }
}
