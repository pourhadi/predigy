//! Self-subscribe dispatcher (Audit A5 plumbing).
//!
//! Strategies that want to subscribe to additional markets at
//! runtime (e.g. latency, which has no static
//! `subscribed_markets` and only needs books for tickers it
//! actually holds positions in) call
//! [`StrategyState::subscribe_to_markets`]. That sends a
//! [`SelfSubscribeRequest`] over an mpsc owned by main.rs.
//!
//! This dispatcher consumes those requests and translates each
//! into a `RouterCommand::AddTickers` with the *requesting
//! strategy's own supervisor event_tx*, so the resulting book
//! updates land back in the strategy's queue.

use predigy_engine_core::events::Event;
use predigy_engine_core::state::SelfSubscribeRequest;
use predigy_engine_core::strategy::StrategyId;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::market_data::RouterCommand;

const QUEUE_CAPACITY: usize = 256;

pub struct SelfSubscribeDispatcher {
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for SelfSubscribeDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfSubscribeDispatcher")
            .finish_non_exhaustive()
    }
}

impl SelfSubscribeDispatcher {
    /// Producer/consumer channel pair.
    pub fn channel() -> (
        mpsc::Sender<SelfSubscribeRequest>,
        mpsc::Receiver<SelfSubscribeRequest>,
    ) {
        mpsc::channel(QUEUE_CAPACITY)
    }

    /// Start the dispatcher. `strategy_event_txs` maps each
    /// supervised strategy id to its supervisor event_tx so the
    /// dispatcher can attach the right one when forwarding to
    /// the router.
    pub fn start(
        rx: mpsc::Receiver<SelfSubscribeRequest>,
        router_tx: mpsc::Sender<RouterCommand>,
        strategy_event_txs: HashMap<StrategyId, mpsc::Sender<Event>>,
    ) -> Self {
        let task = tokio::spawn(dispatcher_task(rx, router_tx, strategy_event_txs));
        Self { task }
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn dispatcher_task(
    mut rx: mpsc::Receiver<SelfSubscribeRequest>,
    router_tx: mpsc::Sender<RouterCommand>,
    event_txs: HashMap<StrategyId, mpsc::Sender<Event>>,
) {
    info!(
        n_strategies = event_txs.len(),
        "self-subscribe dispatcher started"
    );
    while let Some(req) = rx.recv().await {
        let Some(event_tx) = event_txs.get(&req.strategy).cloned() else {
            warn!(
                strategy = req.strategy.0,
                "self-subscribe: no event_tx for strategy; dropping"
            );
            continue;
        };
        let markets: Vec<String> = req
            .markets
            .into_iter()
            .map(|m| m.as_str().to_string())
            .collect();
        if markets.is_empty() {
            continue;
        }
        let cmd = RouterCommand::AddTickers {
            strategy: req.strategy,
            markets,
            tx: event_tx,
        };
        if router_tx.send(cmd).await.is_err() {
            warn!("self-subscribe: router cmd channel closed; exiting");
            return;
        }
    }
    info!("self-subscribe: producer channel closed; exiting");
}
