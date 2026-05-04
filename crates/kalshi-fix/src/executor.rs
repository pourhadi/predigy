//! `oms::Executor` over a Kalshi FIX 4.4 TCP+TLS session.
//!
//! ## Architecture
//!
//! Mirrors `predigy-kalshi-exec::RestExecutor`:
//!
//! 1. Caller calls [`FixExecutor::spawn`]; the returned tuple of
//!    `(executor, ExecutionReport receiver)` plugs into
//!    `predigy_oms::Oms::spawn`.
//! 2. The crate spawns a background tokio task that owns the TCP
//!    stream + the [`crate::Session`]:
//!    - Logs on (sends Logon, waits for Logon response, starts
//!      heartbeat timer).
//!    - Drives a select-loop over (a) outbound commands from
//!      submit/cancel and (b) inbound bytes from the venue.
//!    - On inbound `ExecutionReport`: parses, translates to
//!      `predigy_oms::ExecutionReport`, sends to the OMS.
//! 3. On any session error the task drops, the executor's report
//!    channel closes, and the OMS surfaces a downstream
//!    `ExecutorError::SessionClosed` for in-flight submits.
//!
//! ## What this *doesn't* do (yet)
//!
//! See the crate-level docs in `lib.rs`. Most importantly: no
//! ResendRequest gap fill, no auto-reconnect, and the Kalshi-
//! specific Logon auth bytes are passed through opaquely from the
//! caller (we don't compute them — that needs Kalshi FIX sandbox
//! access).

use crate::error::Error;
use crate::frame::{FieldList, decode_message, encode};
use crate::messages::{
    ExecKind, build_heartbeat, build_logon, build_new_order_single, build_order_cancel_request,
    parse_execution_report,
};
use crate::session::Session;
use crate::tags::{MSG_TYPE, MSG_TYPE_EXECUTION_REPORT, MSG_TYPE_LOGON};
use predigy_core::fill::Fill;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId};
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_oms::{ExecutionReport, ExecutionReportKind, Executor, ExecutorError};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const REPORT_CAPACITY: usize = 4096;
const CMD_CAPACITY: usize = 256;

/// Configuration for [`FixExecutor::spawn`].
#[derive(Debug, Clone)]
pub struct FixConfig {
    /// `host:port` of the Kalshi FIX gateway.
    pub addr: String,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub heartbeat_secs: u32,
    /// Tag/value pairs appended to the Logon message after the
    /// standard fields. For Kalshi this is where username/password
    /// /HMAC tags go (553 / 554 / 95 / 96).
    pub auth_tags: Vec<(u32, String)>,
    /// If `true`, the Logon includes `141=Y` (ResetSeqNumFlag),
    /// asking the venue to reset both inbound and outbound counters
    /// to 1. Use on first connect; turn off across reconnects once
    /// state is durable.
    pub reset_seq_num: bool,
}

/// Executor handle. Cheap to clone via `Arc` internally; the OMS
/// holds it by value.
pub struct FixExecutor {
    cmd_tx: mpsc::Sender<TaskCmd>,
    /// Tracks tracked orders by cid so cancels can find them and the
    /// task can map ExecutionReports back to (cid, side, action).
    tracked: Arc<Mutex<HashMap<OrderId, TrackedOrder>>>,
    _task: Arc<JoinHandle<()>>,
}

impl std::fmt::Debug for FixExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixExecutor").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct TrackedOrder {
    cid: OrderId,
    market: MarketTicker,
    side: Side,
    action: Action,
}

#[derive(Debug)]
enum TaskCmd {
    Submit {
        order: Order,
        reply: oneshot::Sender<Result<(), ExecutorError>>,
    },
    Cancel {
        cid: OrderId,
        cancel_cid: String,
        reply: oneshot::Sender<Result<(), ExecutorError>>,
    },
}

impl FixExecutor {
    /// Connect to `config.addr`, perform the Logon handshake, and
    /// return `(executor, ExecutionReport receiver)`.
    pub async fn spawn(
        config: FixConfig,
    ) -> Result<(Self, mpsc::Receiver<ExecutionReport>), Error> {
        let stream = TcpStream::connect(&config.addr).await?;
        info!(addr = %config.addr, "fix connected");

        let session = Session::new(
            config.sender_comp_id.clone(),
            config.target_comp_id.clone(),
            1,
            1,
            config.heartbeat_secs,
        );

        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
        let (report_tx, report_rx) = mpsc::channel(REPORT_CAPACITY);
        let tracked = Arc::new(Mutex::new(HashMap::new()));
        let tracked_clone = tracked.clone();

        let task = tokio::spawn(run_task(
            stream,
            session,
            config,
            cmd_rx,
            report_tx,
            tracked_clone,
        ));
        Ok((
            Self {
                cmd_tx,
                tracked,
                _task: Arc::new(task),
            },
            report_rx,
        ))
    }
}

impl Executor for FixExecutor {
    fn submit(&self, order: &Order) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let cmd_tx = self.cmd_tx.clone();
        let tracked = self.tracked.clone();
        let order = order.clone();
        async move {
            tracked.lock().unwrap().insert(
                order.client_id.clone(),
                TrackedOrder {
                    cid: order.client_id.clone(),
                    market: order.market.clone(),
                    side: order.side,
                    action: order.action,
                },
            );
            let (reply_tx, reply_rx) = oneshot::channel();
            cmd_tx
                .send(TaskCmd::Submit {
                    order,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ExecutorError::SessionClosed)?;
            reply_rx.await.map_err(|_| ExecutorError::SessionClosed)?
        }
    }

    fn cancel(&self, cid: &OrderId) -> impl Future<Output = Result<(), ExecutorError>> + Send {
        let cmd_tx = self.cmd_tx.clone();
        let tracked = self.tracked.clone();
        let cid = cid.clone();
        async move {
            let exists = tracked.lock().unwrap().contains_key(&cid);
            if !exists {
                return Err(ExecutorError::UnknownOrder(cid));
            }
            // Cancel needs its own ClOrdID per FIX 4.4 semantics; we
            // derive one from the original cid + a `:c` suffix.
            let cancel_cid = format!("{}:c", cid.as_str());
            let (reply_tx, reply_rx) = oneshot::channel();
            cmd_tx
                .send(TaskCmd::Cancel {
                    cid: cid.clone(),
                    cancel_cid,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ExecutorError::SessionClosed)?;
            reply_rx.await.map_err(|_| ExecutorError::SessionClosed)?
        }
    }
}

async fn run_task(
    stream: TcpStream,
    mut session: Session,
    config: FixConfig,
    mut cmd_rx: mpsc::Receiver<TaskCmd>,
    report_tx: mpsc::Sender<ExecutionReport>,
    tracked: Arc<Mutex<HashMap<OrderId, TrackedOrder>>>,
) {
    let (mut reader, mut writer) = stream.into_split();

    // 1. Send Logon.
    let logon_seq = session.next_out_seq();
    let body = build_logon(
        &session.sender_comp_id,
        &session.target_comp_id,
        logon_seq,
        config.heartbeat_secs,
        config.reset_seq_num,
        &config.auth_tags,
        now_ms(),
    );
    if writer.write_all(&encode(&body)).await.is_err() {
        warn!("fix: failed to send Logon; aborting");
        return;
    }

    // 2. Read until we see the venue's Logon response.
    let mut inbound = Vec::with_capacity(4096);
    if !await_logon_ack(&mut reader, &mut inbound, &mut session).await {
        warn!("fix: did not receive Logon ack; aborting");
        return;
    }
    info!("fix: logon ack'd; entering main loop");

    // 3. Main loop: select between cmd_rx and inbound bytes. A
    // periodic heartbeat is scheduled at half the agreed interval.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(u64::from(
        session.heartbeat_secs.max(1),
    )));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            biased;
            n = reader.read(&mut buf) => {
                match n {
                    Ok(0) => {
                        info!("fix: peer closed");
                        return;
                    }
                    Ok(n) => {
                        inbound.extend_from_slice(&buf[..n]);
                        if dispatch_inbound(&mut inbound, &mut session, &report_tx, &tracked).await {
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "fix: read error");
                        return;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return; };
                match cmd {
                    TaskCmd::Submit { order, reply } => {
                        let seq = session.next_out_seq();
                        match build_new_order_single(
                            &session.sender_comp_id,
                            &session.target_comp_id,
                            seq,
                            &order,
                            now_ms(),
                        ) {
                            Ok(body) => {
                                let bytes = encode(&body);
                                let res = writer
                                    .write_all(&bytes)
                                    .await
                                    .map_err(|e| ExecutorError::Transport(e.to_string()));
                                let _ = reply.send(res);
                            }
                            Err(e) => {
                                let _ = reply.send(Err(ExecutorError::Rejected(e.to_string())));
                            }
                        }
                    }
                    TaskCmd::Cancel { cid, cancel_cid, reply } => {
                        let entry = tracked.lock().unwrap().get(&cid).cloned();
                        let Some(t) = entry else {
                            let _ = reply.send(Err(ExecutorError::UnknownOrder(cid)));
                            continue;
                        };
                        let seq = session.next_out_seq();
                        let body = build_order_cancel_request(
                            &session.sender_comp_id,
                            &session.target_comp_id,
                            seq,
                            &cancel_cid,
                            &t.cid,
                            &t.market,
                            t.side,
                            t.action,
                            now_ms(),
                        );
                        let bytes = encode(&body);
                        let res = writer
                            .write_all(&bytes)
                            .await
                            .map_err(|e| ExecutorError::Transport(e.to_string()));
                        let _ = reply.send(res);
                    }
                }
            }
            _ = heartbeat.tick() => {
                let seq = session.next_out_seq();
                let body = build_heartbeat(
                    &session.sender_comp_id,
                    &session.target_comp_id,
                    seq,
                    None,
                    now_ms(),
                );
                if writer.write_all(&encode(&body)).await.is_err() {
                    warn!("fix: heartbeat write failed; aborting");
                    return;
                }
            }
        }
    }
}

async fn await_logon_ack<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    inbound: &mut Vec<u8>,
    session: &mut Session,
) -> bool {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => inbound.extend_from_slice(&buf[..n]),
        }
        loop {
            match decode_message(inbound) {
                Ok(Some((fields, consumed))) => {
                    inbound.drain(..consumed);
                    let msg_type = fields.get(MSG_TYPE).unwrap_or("?");
                    if msg_type == MSG_TYPE_LOGON {
                        if session.validate_inbound(&fields).is_err() {
                            return false;
                        }
                        return true;
                    }
                    debug!(msg_type, "fix: ignoring pre-Logon message");
                }
                Ok(None) => break,
                Err(_) => return false,
            }
        }
    }
}

/// Drain whatever full frames are in `inbound`. Returns `true` if
/// the session should shut down (checksum mismatch, seq gap, etc.).
async fn dispatch_inbound(
    inbound: &mut Vec<u8>,
    session: &mut Session,
    report_tx: &mpsc::Sender<ExecutionReport>,
    tracked: &Arc<Mutex<HashMap<OrderId, TrackedOrder>>>,
) -> bool {
    loop {
        match decode_message(inbound) {
            Ok(Some((fields, consumed))) => {
                inbound.drain(..consumed);
                if session.validate_inbound(&fields).is_err() {
                    warn!("fix: session validation failed; closing");
                    return true;
                }
                let msg_type = fields.get(MSG_TYPE).unwrap_or("?").to_string();
                if msg_type == MSG_TYPE_EXECUTION_REPORT
                    && let Err(e) = handle_execution_report(&fields, report_tx, tracked).await
                {
                    warn!(?e, "fix: failed to dispatch execution report");
                }
            }
            Ok(None) => return false,
            Err(e) => {
                warn!(?e, "fix: framing error; closing");
                return true;
            }
        }
    }
}

async fn handle_execution_report(
    fields: &FieldList,
    report_tx: &mpsc::Sender<ExecutionReport>,
    tracked: &Arc<Mutex<HashMap<OrderId, TrackedOrder>>>,
) -> Result<(), Error> {
    let parsed = parse_execution_report(fields)?;
    let cid = OrderId::new(parsed.cl_ord_id.clone());
    // Look up the tracked order to recover side/action for the Fill
    // payload. If the cid isn't tracked here (replay against an old
    // session, or the cancel-ack of a `:c` cid), we fall back to
    // YES/Buy and rely on the OMS to do the right thing.
    let (side, action, market) = tracked.lock().unwrap().get(&cid).map_or_else(
        || (Side::Yes, Action::Buy, MarketTicker::new("")),
        |t| (t.side, t.action, t.market.clone()),
    );
    let kind = match parsed.kind {
        ExecKind::New => ExecutionReportKind::Acked {
            venue_order_id: parsed.venue_order_id.clone(),
        },
        ExecKind::Filled => {
            let fill = build_fill(&cid, &market, side, action, &parsed)?;
            ExecutionReportKind::Filled {
                fill,
                cumulative_qty: parsed.cum_qty,
            }
        }
        ExecKind::PartiallyFilled => {
            let fill = build_fill(&cid, &market, side, action, &parsed)?;
            ExecutionReportKind::PartiallyFilled {
                fill,
                cumulative_qty: parsed.cum_qty,
            }
        }
        ExecKind::Cancelled => ExecutionReportKind::Cancelled {
            reason: parsed.text.clone().unwrap_or_else(|| "venue cancel".into()),
        },
        ExecKind::Rejected => ExecutionReportKind::Rejected {
            reason: parsed.text.clone().unwrap_or_else(|| "venue reject".into()),
        },
        ExecKind::Other(s) => ExecutionReportKind::Cancelled {
            reason: format!("unmapped OrdStatus={s}"),
        },
    };
    let _ = report_tx
        .send(ExecutionReport {
            cid,
            ts_ms: now_ms(),
            kind,
        })
        .await;
    Ok(())
}

fn build_fill(
    cid: &OrderId,
    market: &MarketTicker,
    side: Side,
    action: Action,
    parsed: &crate::messages::ParsedExecutionReport,
) -> Result<Fill, Error> {
    let qty =
        Qty::new(parsed.last_qty.unwrap_or(parsed.cum_qty)).map_err(|_| Error::MalformedTag {
            tag: 32,
            got: parsed.last_qty.unwrap_or(0).to_string(),
        })?;
    let price = parsed.last_px_cents.and_then(|c| Price::from_cents(c).ok());
    let price = price.ok_or_else(|| Error::MissingTag {
        tag: 31,
        msg_type: "8".into(),
    })?;
    Ok(Fill {
        order_id: cid.clone(),
        market: market.clone(),
        side,
        action,
        price,
        qty,
        is_maker: false, // taker by default; venue can flip via custom tag if needed
        fee_cents: 0,
        ts_ms: u64::try_from(now_ms()).unwrap_or(0),
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
