//! In-memory `oms::Executor` that matches orders against a
//! [`BookStore`].
//!
//! Each `submit` call:
//! 1. Converts the order to a `Match` via [`crate::matching::match_ioc`].
//!    The book is mutated in place to consume any matched liquidity.
//! 2. Synthesises an `Acked` `ExecutionReport` (immediate, since the
//!    sim has no separate venue ack stage).
//! 3. Synthesises a `Filled` / `PartiallyFilled` report from the
//!    match, and a `Cancelled { reason: "ioc remainder" }` if the
//!    order didn't fully fill (IOC semantics).
//!
//! `cancel` immediately emits a `Cancelled` report for the cid. There
//! is no resting-orders ledger today — the sim only models takers —
//! so cancel against an unknown cid returns `UnknownOrder` without
//! emitting anything.
//!
//! GTC and Fok orders are explicitly rejected with `Unsupported`. The
//! queue-position model that would back them lands in a follow-up.

use crate::book_store::BookStore;
use crate::matching::{Match, match_ioc};
use predigy_core::order::{Order, OrderId, TimeInForce};
use predigy_oms::{ExecutionReport, ExecutionReportKind, Executor, ExecutorError};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::debug;

const REPORT_CAPACITY: usize = 4096;

/// In-memory executor for a deterministic backtest. Spawn with
/// [`SimExecutor::spawn`] and hand the returned tuple straight into
/// [`predigy_oms::Oms::spawn`].
pub struct SimExecutor {
    books: BookStore,
    report_tx: mpsc::Sender<ExecutionReport>,
    next_venue_id: Arc<Mutex<u64>>,
    /// Tracks live cids so cancel can validate before emitting a
    /// `Cancelled`. The sim only retains entries for orders that
    /// haven't already terminated (IOC orders almost always reach a
    /// terminal state on submit, so this stays empty in arb-style
    /// tests).
    live: Arc<Mutex<std::collections::HashSet<OrderId>>>,
}

impl std::fmt::Debug for SimExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimExecutor").finish_non_exhaustive()
    }
}

impl SimExecutor {
    /// Build a sim executor over `books`. Returns the executor (move
    /// into `Oms::spawn`) and the report receiver (also into
    /// `Oms::spawn`).
    pub fn spawn(books: BookStore) -> (Self, mpsc::Receiver<ExecutionReport>) {
        let (report_tx, report_rx) = mpsc::channel(REPORT_CAPACITY);
        let executor = Self {
            books,
            report_tx,
            next_venue_id: Arc::new(Mutex::new(0)),
            live: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        (executor, report_rx)
    }

    fn next_venue_id(&self) -> String {
        let mut g = self.next_venue_id.lock().unwrap();
        let id = *g;
        *g += 1;
        format!("sim-{id:08}")
    }
}

impl Executor for SimExecutor {
    fn submit(&self, order: &Order) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let books = self.books.clone();
        let report_tx = self.report_tx.clone();
        let live = self.live.clone();
        let venue_id = self.next_venue_id();
        let order = order.clone();
        async move {
            if !matches!(order.tif, TimeInForce::Ioc) {
                return Err(ExecutorError::Rejected(format!(
                    "sim only supports IOC; got {tif:?}",
                    tif = order.tif
                )));
            }

            // Match in a tight scope so the BookStore mutex isn't
            // held across the report-channel awaits below.
            let outcome =
                books.with_book_mut(&order.market, |book| match_ioc(book, &order, now_ms()));
            // Market not in the book store — treat as no liquidity
            // and surface as Rejected so the OMS doesn't book a phantom.
            let Some(outcome) = outcome else {
                return Err(ExecutorError::Rejected(format!(
                    "sim has no book for {}",
                    order.market
                )));
            };

            // Always emit Acked first — the sim accepts the order
            // (the venue would have, too) before we report fills.
            send_report(
                &report_tx,
                ExecutionReport {
                    cid: order.client_id.clone(),
                    ts_ms: now_ms(),
                    kind: ExecutionReportKind::Acked {
                        venue_order_id: venue_id,
                    },
                },
            )
            .await?;

            match outcome {
                Match::Unsupported(reason) => Err(ExecutorError::Rejected(reason.to_string())),
                Match::NoLiquidity => {
                    // IOC with no fill → cancelled.
                    send_report(
                        &report_tx,
                        ExecutionReport {
                            cid: order.client_id.clone(),
                            ts_ms: now_ms(),
                            kind: ExecutionReportKind::Cancelled {
                                reason: "ioc no liquidity at limit".into(),
                            },
                        },
                    )
                    .await?;
                    Ok(())
                }
                Match::Filled {
                    fill,
                    cumulative_qty,
                    terminal,
                } => {
                    let kind = if terminal {
                        ExecutionReportKind::Filled {
                            fill,
                            cumulative_qty,
                        }
                    } else {
                        // IOC with a partial then unfilled remainder
                        // → emit PartiallyFilled, then a Cancelled
                        // for the remainder.
                        live.lock().unwrap().insert(order.client_id.clone());
                        ExecutionReportKind::PartiallyFilled {
                            fill,
                            cumulative_qty,
                        }
                    };
                    send_report(
                        &report_tx,
                        ExecutionReport {
                            cid: order.client_id.clone(),
                            ts_ms: now_ms(),
                            kind,
                        },
                    )
                    .await?;
                    if !terminal {
                        send_report(
                            &report_tx,
                            ExecutionReport {
                                cid: order.client_id.clone(),
                                ts_ms: now_ms(),
                                kind: ExecutionReportKind::Cancelled {
                                    reason: "ioc remainder after partial".into(),
                                },
                            },
                        )
                        .await?;
                        live.lock().unwrap().remove(&order.client_id);
                    }
                    Ok(())
                }
            }
        }
    }

    fn cancel(&self, cid: &OrderId) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let report_tx = self.report_tx.clone();
        let live = self.live.clone();
        let cid = cid.clone();
        async move {
            if !live.lock().unwrap().remove(&cid) {
                debug!(cid = %cid, "sim cancel: cid not live; nothing to cancel");
                return Err(ExecutorError::UnknownOrder(cid));
            }
            send_report(
                &report_tx,
                ExecutionReport {
                    cid: cid.clone(),
                    ts_ms: now_ms(),
                    kind: ExecutionReportKind::Cancelled {
                        reason: "user cancel".into(),
                    },
                },
            )
            .await?;
            Ok(())
        }
    }
}

async fn send_report(
    tx: &mpsc::Sender<ExecutionReport>,
    report: ExecutionReport,
) -> Result<(), ExecutorError> {
    tx.send(report)
        .await
        .map_err(|_| ExecutorError::SessionClosed)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book_store::BookStore;
    use predigy_book::Snapshot;
    use predigy_core::market::MarketTicker;
    use predigy_core::order::{OrderType, TimeInForce};
    use predigy_core::price::{Price, Qty};
    use predigy_core::side::{Action, Side};

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn buy(side: Side, market: &str, price: u8, qty: u32, tif: TimeInForce) -> Order {
        Order {
            client_id: OrderId::new("c-1"),
            market: MarketTicker::new(market),
            side,
            action: Action::Buy,
            price: p(price),
            qty: q(qty),
            order_type: OrderType::Limit,
            tif,
        }
    }

    fn synthetic_book() -> BookStore {
        let store = BookStore::new();
        let m = MarketTicker::new("X");
        store.apply_snapshot(
            &m,
            Snapshot {
                seq: 1,
                yes_bids: vec![(p(60), 50)],
                no_bids: vec![(p(60), 50)],
            },
        );
        store
    }

    #[tokio::test]
    async fn ioc_fill_emits_acked_then_filled() {
        let store = synthetic_book();
        let (executor, mut reports) = SimExecutor::spawn(store);
        executor
            .submit(&buy(Side::Yes, "X", 40, 30, TimeInForce::Ioc))
            .await
            .unwrap();
        let r1 = reports.recv().await.unwrap();
        assert!(matches!(r1.kind, ExecutionReportKind::Acked { .. }));
        let r2 = reports.recv().await.unwrap();
        match r2.kind {
            ExecutionReportKind::Filled {
                cumulative_qty,
                fill,
            } => {
                assert_eq!(cumulative_qty, 30);
                assert_eq!(fill.price.cents(), 40);
                assert_eq!(fill.qty.get(), 30);
            }
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ioc_no_liquidity_emits_acked_then_cancelled() {
        let store = synthetic_book();
        let (executor, mut reports) = SimExecutor::spawn(store);
        // Limit too low → no match.
        executor
            .submit(&buy(Side::Yes, "X", 30, 10, TimeInForce::Ioc))
            .await
            .unwrap();
        let _ = reports.recv().await.unwrap(); // Acked
        let r = reports.recv().await.unwrap();
        match r.kind {
            ExecutionReportKind::Cancelled { reason } => assert!(reason.contains("ioc")),
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ioc_partial_emits_partial_then_cancel_remainder() {
        let store = synthetic_book();
        let (executor, mut reports) = SimExecutor::spawn(store);
        // Touch has 50 contracts on each side; ask for 80 → fill 50, cancel 30.
        executor
            .submit(&buy(Side::Yes, "X", 40, 80, TimeInForce::Ioc))
            .await
            .unwrap();
        let _ = reports.recv().await.unwrap(); // Acked
        let r2 = reports.recv().await.unwrap();
        match r2.kind {
            ExecutionReportKind::PartiallyFilled { cumulative_qty, .. } => {
                assert_eq!(cumulative_qty, 50);
            }
            other => panic!("expected PartiallyFilled, got {other:?}"),
        }
        let r3 = reports.recv().await.unwrap();
        match r3.kind {
            ExecutionReportKind::Cancelled { reason } => assert!(reason.contains("remainder")),
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_for_unknown_market_rejects() {
        let store = BookStore::new();
        let (executor, _reports) = SimExecutor::spawn(store);
        let err = executor
            .submit(&buy(Side::Yes, "ABSENT", 40, 1, TimeInForce::Ioc))
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Rejected(_)));
    }

    #[tokio::test]
    async fn non_ioc_rejected() {
        let store = synthetic_book();
        let (executor, _reports) = SimExecutor::spawn(store);
        let err = executor
            .submit(&buy(Side::Yes, "X", 40, 1, TimeInForce::Gtc))
            .await
            .unwrap_err();
        match err {
            ExecutorError::Rejected(msg) => assert!(msg.contains("IOC"), "got: {msg}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_unknown_cid_returns_unknown_order() {
        let store = BookStore::new();
        let (executor, _reports) = SimExecutor::spawn(store);
        let err = executor.cancel(&OrderId::new("nope")).await.unwrap_err();
        assert!(matches!(err, ExecutorError::UnknownOrder(_)));
    }
}
