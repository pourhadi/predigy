//! Venue executor trait and execution-report types.
//!
//! `Executor` is the seam the OMS uses to talk to a venue. Production
//! impls live in `predigy-kalshi-exec` (FIX 4.4) and a REST fallback
//! variant. Tests use a stub executor (see [`stub`]) that records
//! submits/cancels and lets the test push synthetic
//! [`ExecutionReport`]s through a paired channel.
//!
//! Reports flow back from the venue asynchronously: the executor owns
//! its session and pumps reports into an `mpsc::Receiver` that the OMS
//! reads from. The trait itself is only the **outbound** half.

use predigy_core::fill::Fill;
use predigy_core::order::{Order, OrderId};
use serde::{Deserialize, Serialize};
use std::future::Future;
use thiserror::Error;

/// Outbound venue interface. The OMS calls these synchronously from the
/// task (await is fine — they're expected to be I/O-bound on the FIX
/// session or a REST request).
pub trait Executor: Send + Sync {
    /// Submit a new order. The returned future resolves once the
    /// outbound message has been queued on the venue session — not
    /// when the venue acks. The ack arrives later as an
    /// [`ExecutionReportKind::Acked`] on the report channel.
    fn submit(&self, order: &Order) -> impl Future<Output = Result<(), ExecutorError>> + Send;

    /// Cancel an order by client id.
    fn cancel(&self, cid: &OrderId) -> impl Future<Output = Result<(), ExecutorError>> + Send;
}

#[derive(Debug, Clone, Error)]
pub enum ExecutorError {
    #[error("session closed")]
    SessionClosed,
    #[error("transport: {0}")]
    Transport(String),
    #[error("rejected by venue: {0}")]
    Rejected(String),
    #[error("unknown order id: {0}")]
    UnknownOrder(OrderId),
}

/// One state-changing event reported by the venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub cid: OrderId,
    /// Wall-clock millis the report was observed at the executor. The
    /// venue's own timestamp may be inside the `kind` payload (e.g. on
    /// fills); this field is the executor's local clock.
    pub ts_ms: i64,
    pub kind: ExecutionReportKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionReportKind {
    /// Venue acknowledged the order — it's now resting in the book (or
    /// matched immediately, in which case a Fill follows).
    Acked {
        /// Venue-assigned order id, alongside our `cid`. Useful for
        /// venue-side debugging and required for some cancel paths.
        venue_order_id: String,
    },
    /// Partial execution. `cumulative_qty` is the total fill quantity
    /// across all fills on this order so far (so the OMS doesn't have
    /// to track it independently — the venue's view is authoritative).
    PartiallyFilled { fill: Fill, cumulative_qty: u32 },
    /// Final execution. Subsequent reports for this `cid` must not arrive.
    Filled { fill: Fill, cumulative_qty: u32 },
    /// Order cancelled (either by us or by the venue's TIF rules).
    Cancelled { reason: String },
    /// Order rejected without ever being acked.
    Rejected { reason: String },
}

impl ExecutionReportKind {
    /// True for the three terminal states. Once the OMS sees one of
    /// these for a given cid it can drop the working-orders entry.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Filled { .. } | Self::Cancelled { .. } | Self::Rejected { .. }
        )
    }
}

/// In-process stub executor for tests. Records every `submit` / `cancel`
/// call and exposes a paired sender so tests can push
/// [`ExecutionReport`]s back through the OMS's report channel.
pub mod stub {
    use super::{ExecutionReport, Executor, ExecutorError};
    use predigy_core::order::{Order, OrderId};
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// Record of one outbound call observed by [`StubExecutor`].
    #[derive(Debug, Clone)]
    pub enum StubCall {
        Submit(Order),
        Cancel(OrderId),
    }

    /// Stub executor. Build with [`channel`] to also obtain a
    /// `Sender<ExecutionReport>` for pushing simulated reports.
    #[derive(Debug, Clone)]
    pub struct StubExecutor {
        calls: Arc<Mutex<Vec<StubCall>>>,
        next_submit_error: Arc<Mutex<Option<ExecutorError>>>,
    }

    impl Default for StubExecutor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl StubExecutor {
        #[must_use]
        pub fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                next_submit_error: Arc::new(Mutex::new(None)),
            }
        }

        /// Snapshot of the call log so far.
        pub fn calls(&self) -> Vec<StubCall> {
            self.calls.lock().unwrap().clone()
        }

        /// Make the **next** `submit` call return this error before
        /// resetting back to the success path. Cancels are unaffected.
        pub fn fail_next_submit(&self, err: ExecutorError) {
            *self.next_submit_error.lock().unwrap() = Some(err);
        }
    }

    impl Executor for StubExecutor {
        fn submit(&self, order: &Order) -> impl Future<Output = Result<(), ExecutorError>> + Send {
            let calls = self.calls.clone();
            let next_err = self.next_submit_error.clone();
            let order = order.clone();
            async move {
                calls.lock().unwrap().push(StubCall::Submit(order));
                if let Some(err) = next_err.lock().unwrap().take() {
                    return Err(err);
                }
                Ok(())
            }
        }

        fn cancel(&self, cid: &OrderId) -> impl Future<Output = Result<(), ExecutorError>> + Send {
            let calls = self.calls.clone();
            let cid = cid.clone();
            async move {
                calls.lock().unwrap().push(StubCall::Cancel(cid));
                Ok(())
            }
        }
    }

    /// Build a [`StubExecutor`] paired with the `Sender`/`Receiver`
    /// halves of an execution-report channel. The OMS reads from the
    /// receiver; tests push synthetic reports through the sender.
    #[must_use]
    pub fn channel(
        capacity: usize,
    ) -> (
        StubExecutor,
        mpsc::Sender<ExecutionReport>,
        mpsc::Receiver<ExecutionReport>,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        (StubExecutor::new(), tx, rx)
    }
}
