//! Wires the strategy into WS market data and the OMS.
//!
//! Single tokio task, single `select!`: each iteration races
//! `md.next_event` (book updates, drives the strategy),
//! `oms.next_event` (logs the order lifecycle), and the caller's stop
//! future (graceful shutdown).
//!
//! Single-task design means both `&mut md` and `&mut oms` live
//! together without sharing — submitting on the OMS uses `&self`
//! (it sends through an `mpsc`), so submits don't conflict with the
//! `&mut self` `next_event` borrow.

use crate::strategy::{ArbConfig, ArbStrategy, Evaluation};
use anyhow::Result;
use predigy_book::{ApplyOutcome, OrderBook};
use predigy_core::market::MarketTicker;
use predigy_kalshi_md::{Channel, Connection as MdConnection, Event as MdEvent};
use predigy_oms::{OmsEvent, OmsHandle};
use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub markets: Vec<MarketTicker>,
    pub arb: ArbConfig,
    /// When `true`, evaluate the strategy and log opportunities but
    /// do not submit. Useful for shaking down the wiring against live
    /// data with no capital at risk.
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct Runner {
    config: RunnerConfig,
}

impl Runner {
    #[must_use]
    pub fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    /// Subscribe to the configured markets, drive the strategy, and
    /// run until `stop` resolves or one of the underlying streams
    /// closes.
    pub async fn run<F>(self, mut md: MdConnection, mut oms: OmsHandle, stop: F) -> Result<()>
    where
        F: std::future::Future<Output = ()>,
    {
        let market_strs: Vec<String> = self
            .config
            .markets
            .iter()
            .map(|m| m.as_str().to_string())
            .collect();
        let req_id = md
            .subscribe(
                &[Channel::OrderbookDelta, Channel::Ticker, Channel::Trade],
                &market_strs,
            )
            .await
            .map_err(|e| anyhow::anyhow!("md subscribe: {e}"))?;
        info!(
            req_id,
            markets = ?market_strs,
            dry_run = self.config.dry_run,
            "arb-trader subscribed"
        );

        let mut books: HashMap<MarketTicker, OrderBook> = self
            .config
            .markets
            .iter()
            .map(|m| (m.clone(), OrderBook::new(m.as_str())))
            .collect();
        let mut strategy = ArbStrategy::new(self.config.arb.clone());

        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => {
                    info!("arb-trader received stop signal");
                    break;
                }
                ev = md.next_event() => {
                    let Some(ev) = ev else {
                        info!("md event stream closed; arb-trader exiting");
                        break;
                    };
                    handle_md_event(ev, &mut books, &mut strategy, &oms, self.config.dry_run).await;
                }
                ev = oms.next_event() => {
                    let Some(ev) = ev else {
                        info!("oms event stream closed; arb-trader exiting");
                        break;
                    };
                    log_oms_event(&ev);
                }
            }
        }
        oms.close().await;
        Ok(())
    }
}

async fn handle_md_event(
    ev: MdEvent,
    books: &mut HashMap<MarketTicker, OrderBook>,
    strategy: &mut ArbStrategy,
    oms: &OmsHandle,
    dry_run: bool,
) {
    let market_for_eval = match ev {
        MdEvent::Snapshot {
            market, snapshot, ..
        } => {
            let key = MarketTicker::new(&market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| OrderBook::new(market));
            book.apply_snapshot(snapshot);
            Some(key)
        }
        MdEvent::Delta { delta, .. } => {
            let key = MarketTicker::new(&delta.market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| OrderBook::new(delta.market.clone()));
            match book.apply_delta(&delta) {
                ApplyOutcome::Ok => Some(key),
                ApplyOutcome::Gap { expected, got } => {
                    warn!(
                        market = %delta.market,
                        expected,
                        got,
                        "WS sequence gap; awaiting fresh snapshot before evaluating arb"
                    );
                    // Drop the local book so we ignore deltas until a
                    // fresh snapshot arrives. md-recorder handles the
                    // REST resync path; we trust the venue to push a
                    // fresh orderbook_snapshot here, and we'll resume
                    // evaluating once it does.
                    books.remove(&key);
                    None
                }
                ApplyOutcome::WrongMarket => None,
            }
        }
        MdEvent::Ticker { .. }
        | MdEvent::Trade { .. }
        | MdEvent::Subscribed { .. }
        | MdEvent::ServerError { .. } => None,
        MdEvent::Disconnected { attempt, reason } => {
            warn!(attempt, reason, "md disconnected");
            None
        }
        MdEvent::Reconnected => {
            // A fresh snapshot follows for each subscribed market;
            // no immediate action needed here.
            info!("md reconnected; awaiting fresh snapshots");
            None
        }
        MdEvent::Malformed { error, .. } => {
            warn!(%error, "malformed md frame; ignored");
            None
        }
    };
    let Some(market) = market_for_eval else {
        return;
    };
    let Some(book) = books.get(&market) else {
        return;
    };
    let evaluation = strategy.evaluate(&market, book, Instant::now());
    log_evaluation(&market, &evaluation);
    if dry_run {
        return;
    }
    submit_intents(oms, &evaluation).await;
}

fn log_evaluation(market: &MarketTicker, ev: &Evaluation) {
    let Some(opp) = &ev.opportunity else { return };
    if ev.intents.is_empty() {
        debug!(
            market = %market,
            edge_per_pair = opp.edge_cents_per_pair,
            throttled = ev.throttled,
            "arb opportunity below threshold or throttled"
        );
    } else {
        info!(
            market = %market,
            size = opp.size,
            yes_buy_price = opp.yes_buy_price.cents(),
            no_buy_price = opp.no_buy_price.cents(),
            edge_per_pair = opp.edge_cents_per_pair,
            edge_total = opp.edge_cents_total,
            "submitting arb pair"
        );
    }
}

async fn submit_intents(oms: &OmsHandle, evaluation: &Evaluation) {
    for intent in &evaluation.intents {
        match oms.submit(intent.clone()).await {
            Ok(cid) => info!(
                cid = %cid,
                market = %intent.market,
                side = ?intent.side,
                "submitted"
            ),
            Err(e) => warn!(
                %e,
                market = %intent.market,
                side = ?intent.side,
                "submit rejected"
            ),
        }
    }
}

fn log_oms_event(ev: &OmsEvent) {
    match ev {
        OmsEvent::Submitted { cid, .. } => debug!(cid = %cid, "oms: submitted"),
        OmsEvent::Acked {
            cid,
            venue_order_id,
        } => {
            info!(cid = %cid, venue_order_id, "oms: acked");
        }
        OmsEvent::Filled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => info!(
            cid = %cid,
            cumulative_qty,
            fill_price = fill_price.cents(),
            "oms: filled"
        ),
        OmsEvent::PartiallyFilled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => info!(
            cid = %cid,
            cumulative_qty,
            fill_price = fill_price.cents(),
            "oms: partial fill"
        ),
        OmsEvent::Cancelled { cid, reason } => info!(cid = %cid, reason, "oms: cancelled"),
        OmsEvent::Rejected { cid, reason } => warn!(cid = %cid, reason, "oms: rejected"),
        OmsEvent::PositionUpdated {
            market,
            side,
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
        } => info!(
            market = %market,
            side = ?side,
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
            "oms: position updated"
        ),
        OmsEvent::Reconciled { mismatches } => {
            if mismatches.is_empty() {
                debug!("oms: reconciled, no mismatches");
            } else {
                warn!(count = mismatches.len(), "oms: reconciled with mismatches");
            }
        }
        OmsEvent::KillSwitchArmed => warn!("oms: kill switch ARMED"),
        OmsEvent::KillSwitchDisarmed => info!("oms: kill switch disarmed"),
    }
}
