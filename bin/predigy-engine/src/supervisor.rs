//! Per-strategy supervisor — owns the tokio task for one
//! strategy, restarts on panic with backoff, routes events into
//! the strategy and intents out to the OMS.
//!
//! The supervisor maintains the boundary between strategy code
//! (which can panic, return errors, get stuck) and the engine's
//! liveness. A panicking strategy never takes down the rest of
//! the engine.

use predigy_engine_core::error::EngineResult;
use predigy_engine_core::events::Event;
use predigy_engine_core::oms::{Oms, SubmitOutcome};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Restart configuration for a supervised strategy.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// If a strategy crashes more than this many times in
    /// `flap_window`, the supervisor stops restarting it and
    /// emits a `EngineError::Strategy` for the engine to surface.
    pub flap_threshold: u32,
    pub flap_window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            flap_threshold: 5,
            flap_window: Duration::from_secs(120),
        }
    }
}

pub struct Supervisor {
    pub id: StrategyId,
    pub event_tx: mpsc::Sender<Event>,
    handle: JoinHandle<()>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").field("id", &self.id).finish()
    }
}

impl Supervisor {
    /// Spawn a strategy task. The supervisor owns the channel
    /// the engine pushes events into; on panic the channel
    /// remains open (engine can keep enqueuing) while a fresh
    /// task replaces the failed one.
    pub fn spawn(
        id: StrategyId,
        strategy_factory: Arc<dyn Fn() -> Box<dyn Strategy> + Send + Sync>,
        oms: Arc<dyn Oms>,
        state: StrategyState,
        policy: RestartPolicy,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel::<Event>(1024);
        let handle = tokio::spawn(supervisor_loop(
            id,
            strategy_factory,
            oms,
            state,
            policy,
            event_rx,
        ));
        Self {
            id,
            event_tx,
            handle,
        }
    }

    /// Stop the supervisor and await the task. Drops the event
    /// sender first so the loop exits naturally.
    pub async fn shutdown(self, grace: Duration) {
        // Closing the sender (by dropping `self`) lets the loop
        // see `None` from `event_rx.recv()` and exit.
        drop(self.event_tx);
        match tokio::time::timeout(grace, self.handle).await {
            Ok(Ok(())) => info!(strategy = ?self.id, "supervisor: clean shutdown"),
            Ok(Err(e)) => {
                warn!(strategy = ?self.id, error = %e, "supervisor: task panicked during shutdown");
            }
            Err(_) => warn!(strategy = ?self.id, "supervisor: shutdown deadline exceeded"),
        }
    }
}

async fn supervisor_loop(
    id: StrategyId,
    factory: Arc<dyn Fn() -> Box<dyn Strategy> + Send + Sync>,
    oms: Arc<dyn Oms>,
    mut state: StrategyState,
    policy: RestartPolicy,
    mut event_rx: mpsc::Receiver<Event>,
) {
    let mut backoff = policy.backoff_initial;
    let mut crash_window: Vec<std::time::Instant> = Vec::new();

    loop {
        // Construct a fresh strategy instance. If the factory
        // itself panics, fail loud and exit — that's a config
        // bug, not a runtime hiccup.
        let mut strategy = factory();

        let outcome = run_one_lifecycle(&mut *strategy, &oms, &mut state, &mut event_rx).await;
        match outcome {
            LoopOutcome::Shutdown => {
                info!(strategy = ?id, "supervisor: event channel closed; exiting");
                return;
            }
            LoopOutcome::Crashed(reason) => {
                error!(
                    strategy = ?id,
                    reason = %reason,
                    "supervisor: strategy crashed; restarting after backoff"
                );
                let now = std::time::Instant::now();
                crash_window.retain(|t| now.duration_since(*t) < policy.flap_window);
                crash_window.push(now);
                if crash_window.len() as u32 > policy.flap_threshold {
                    error!(
                        strategy = ?id,
                        crashes = crash_window.len(),
                        window_secs = policy.flap_window.as_secs(),
                        "supervisor: strategy flapping; STOPPING (engine-level alert needed)"
                    );
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(policy.backoff_max);
            }
        }
    }
}

enum LoopOutcome {
    Shutdown,
    Crashed(String),
}

async fn run_one_lifecycle(
    strategy: &mut dyn Strategy,
    oms: &Arc<dyn Oms>,
    state: &mut StrategyState,
    event_rx: &mut mpsc::Receiver<Event>,
) -> LoopOutcome {
    while let Some(ev) = event_rx.recv().await {
        match strategy.on_event(&ev, state).await {
            Ok(intents) => {
                for intent in intents {
                    if let Err(e) = submit_one(oms, intent).await {
                        warn!(error = %e, "supervisor: oms submit error");
                    }
                }
            }
            Err(e) => {
                return LoopOutcome::Crashed(e.to_string());
            }
        }
    }
    LoopOutcome::Shutdown
}

async fn submit_one(oms: &Arc<dyn Oms>, intent: predigy_engine_core::Intent) -> EngineResult<()> {
    match oms.submit(intent).await? {
        SubmitOutcome::Submitted { client_id, venue } => {
            tracing::info!(%client_id, ?venue, "oms: submitted");
            Ok(())
        }
        SubmitOutcome::Idempotent {
            client_id,
            current_status,
        } => {
            tracing::debug!(%client_id, %current_status, "oms: idempotent re-submit (no-op)");
            Ok(())
        }
        SubmitOutcome::Rejected { reason } => {
            tracing::warn!(reason = %reason, "oms: rejected");
            Ok(())
        }
    }
}
