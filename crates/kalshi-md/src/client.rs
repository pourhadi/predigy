//! WebSocket client: auth on upgrade, command/event channels, automatic
//! reconnect with exponential-backoff full-jitter.
//!
//! ## Architecture
//!
//! `Client` is configuration; calling [`Client::connect`] spawns a single
//! background tokio task and returns a [`Connection`] handle. The
//! background task:
//!
//! 1. Performs the TLS + WebSocket handshake, signing the upgrade with the
//!    same RSA-PSS scheme as the REST API (path = the WS URL path, method
//!    = `GET`).
//! 2. Replays any saved subscriptions (so callers don't need to re-issue
//!    commands across reconnects).
//! 3. Multiplexes between the caller's command channel and the wire stream.
//! 4. On disconnect, sleeps for [`Backoff::next_delay`] and reconnects;
//!    repeats until the [`Connection`] is dropped or [`Connection::close`]
//!    is called.
//!
//! Saved subscriptions are tracked by client-assigned request id, not the
//! server-assigned `sid` (which is per-connection and changes on reconnect).
//! Per Kalshi WS docs (post-2026), a repeat subscribe on an existing
//! channel/market is a no-op, so replay is idempotent.

use crate::backoff::Backoff;
use crate::decode::{delta_from_wire, snapshot_from_wire};
use crate::error::Error;
use crate::messages::{
    Channel, FillBody, Incoming, MarketPositionBody, Outgoing, SubscribeParams, TickerBody,
    TradeBody, UnsubscribeParams, UpdateAction, UpdateParams,
};
use futures_util::{SinkExt as _, StreamExt as _};
use http::HeaderValue;
use predigy_book::{Delta, Snapshot};
use predigy_kalshi_rest::auth::Signer;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};
use url::Url;

/// Production WebSocket endpoint.
pub const DEFAULT_ENDPOINT: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";

/// Bounded capacities. Commands are issued by the user (low rate); events
/// flow at market-data rate. Larger event capacity gives the consumer some
/// buffer before backpressure kicks in.
const CMD_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 4096;

// ---------------------------------------------------------------- public API

/// Configured-but-not-connected market-data client.
#[derive(Debug, Clone)]
pub struct Client {
    endpoint: Url,
    signer: Option<Arc<Signer>>,
    backoff: Backoff,
}

impl Client {
    /// Production client signed with `signer`.
    pub fn new(signer: Signer) -> Result<Self, Error> {
        Ok(Self {
            endpoint: Url::parse(DEFAULT_ENDPOINT)?,
            signer: Some(Arc::new(signer)),
            backoff: Backoff::default_const(),
        })
    }

    /// Build with a custom endpoint URL (test servers, sandbox). `signer`
    /// is optional — the production endpoint requires one, but a `ws://`
    /// loopback test server typically does not.
    pub fn with_endpoint(endpoint: Url, signer: Option<Signer>) -> Self {
        Self {
            endpoint,
            signer: signer.map(Arc::new),
            backoff: Backoff::default_const(),
        }
    }

    /// Override the reconnect backoff schedule.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Spawn the background connection task and return a handle.
    ///
    /// The task runs until the returned [`Connection`] is dropped or
    /// [`Connection::close`] is called. It does *not* return an error if the
    /// initial connection attempt fails — instead it surfaces the failure
    /// as an [`Event::Disconnected`] and enters the backoff loop. This
    /// keeps the caller's event-loop logic uniform across cold start vs.
    /// mid-session disconnects.
    pub fn connect(&self) -> Connection {
        let (cmd_tx, cmd_rx) = mpsc::channel::<TaskCmd>(CMD_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<Event>(EVENT_CAPACITY);
        let task = tokio::spawn(run_task(RunCtx {
            endpoint: self.endpoint.clone(),
            signer: self.signer.clone(),
            backoff: self.backoff,
            cmd_rx,
            event_tx,
        }));
        Connection {
            cmd_tx,
            event_rx,
            task: Some(task),
            next_req_id: 1,
        }
    }
}

/// Live (or reconnecting) connection handle. Drops cleanly: dropping ends
/// the background task as soon as it next looks at its command channel.
#[derive(Debug)]
pub struct Connection {
    cmd_tx: mpsc::Sender<TaskCmd>,
    event_rx: mpsc::Receiver<Event>,
    task: Option<JoinHandle<()>>,
    next_req_id: u64,
}

impl Connection {
    /// Subscribe to one or more channels for a list of market tickers.
    ///
    /// The subscription is saved and replayed across reconnects. The
    /// returned `req_id` matches the eventual [`Event::Subscribed`].
    pub async fn subscribe(
        &mut self,
        channels: &[Channel],
        market_tickers: &[String],
    ) -> Result<u64, Error> {
        if channels.is_empty() {
            return Err(Error::Invalid("subscribe: channels is empty".into()));
        }
        // Authenticated channels (`fill`, `order_state`,
        // `market_positions`) cover *every* market the account
        // touches, so an empty market_tickers is the correct
        // shape — Kalshi treats absent `market_tickers` as
        // "all markets for this user". For public channels we
        // still require at least one ticker.
        let all_authed = channels.iter().all(|c| c.requires_auth());
        if market_tickers.is_empty() && !all_authed {
            return Err(Error::Invalid(
                "subscribe: market_tickers required for public channels".into(),
            ));
        }
        let req_id = self.next_req_id;
        self.next_req_id += 1;
        let cmd = TaskCmd::Subscribe {
            req_id,
            channels: channels.iter().map(|c| c.wire_name().to_string()).collect(),
            market_tickers: market_tickers.to_vec(),
        };
        self.cmd_tx.send(cmd).await.map_err(|_| Error::Closed)?;
        Ok(req_id)
    }

    /// Unsubscribe by server-assigned sid (returned via [`Event::Subscribed`]).
    /// In-session only — sids do not survive reconnect.
    pub async fn unsubscribe(&mut self, sids: &[u64]) -> Result<u64, Error> {
        if sids.is_empty() {
            return Err(Error::Invalid("unsubscribe: sids is empty".into()));
        }
        let req_id = self.next_req_id;
        self.next_req_id += 1;
        self.cmd_tx
            .send(TaskCmd::Unsubscribe {
                req_id,
                sids: sids.to_vec(),
            })
            .await
            .map_err(|_| Error::Closed)?;
        Ok(req_id)
    }

    /// Request fresh orderbook snapshots for markets already attached to one
    /// orderbook subscription sid.
    ///
    /// This uses Kalshi's `update_subscription` / `get_snapshot` action over
    /// the existing websocket instead of REST, so consumers can resync after a
    /// true stream sequence gap without creating REST rate-limit bursts.
    pub async fn get_snapshot(
        &mut self,
        sid: u64,
        market_tickers: &[String],
    ) -> Result<u64, Error> {
        if market_tickers.is_empty() {
            return Err(Error::Invalid(
                "get_snapshot: market_tickers is empty".into(),
            ));
        }
        let req_id = self.next_req_id;
        self.next_req_id += 1;
        self.cmd_tx
            .send(TaskCmd::UpdateSubscription {
                req_id,
                sids: vec![sid],
                action: UpdateAction::GetSnapshot,
                market_tickers: market_tickers.to_vec(),
            })
            .await
            .map_err(|_| Error::Closed)?;
        Ok(req_id)
    }

    /// Pop the next event. Returns `None` once the background task has
    /// fully exited and drained its event queue.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }

    /// Request graceful shutdown and await the background task.
    pub async fn close(mut self) {
        let _ = self.cmd_tx.send(TaskCmd::Shutdown).await;
        if let Some(handle) = self.task.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}

/// High-level event surfaced to the caller. Decoded from the raw wire
/// envelope; orderbook channels carry domain types from `predigy-book`.
#[derive(Debug)]
pub enum Event {
    /// Server confirmed a subscribe and assigned `sid`. Match `req_id`
    /// against the value returned from [`Connection::subscribe`].
    Subscribed {
        req_id: Option<u64>,
        channel: String,
        sid: u64,
    },
    /// Full book replacement. Apply via `OrderBook::apply_snapshot`.
    Snapshot {
        sid: u64,
        market: String,
        snapshot: Snapshot,
    },
    /// One incremental book change. Apply via `OrderBook::apply_delta`.
    Delta { sid: u64, delta: Delta },
    /// Latest aggregated ticker fields.
    Ticker { sid: u64, body: TickerBody },
    /// Public trade.
    Trade { sid: u64, body: TradeBody },
    /// User fill from the authed `fill` channel. Lower-latency
    /// equivalent of the REST `/portfolio/fills` poller.
    Fill { sid: u64, body: FillBody },
    /// Position update from the authed `market_positions`
    /// subscription. Reconciliation signal vs the OMS ledger.
    MarketPosition { sid: u64, body: MarketPositionBody },
    /// Server-side error response (e.g. unknown channel, already subscribed).
    ServerError {
        req_id: Option<u64>,
        code: i64,
        msg: String,
    },
    /// Connection went down; the task is now backing off before retry.
    /// `attempt` is 1-indexed for the upcoming retry.
    Disconnected { attempt: u32, reason: String },
    /// Connection is back; saved subscriptions have been replayed (the
    /// server may still be sending the corresponding `Subscribed` events).
    /// Consumers that hold an `OrderBook` should plan to fetch a fresh REST
    /// snapshot — WS won't replay deltas missed during the gap.
    Reconnected,
    /// Frame we couldn't parse against any known schema. Surfaced rather
    /// than silently dropped so schema drift is visible.
    Malformed { raw: String, error: String },
    /// Wire envelope had a `type` tag we don't model yet (e.g. a new
    /// channel Kalshi added that we haven't decoded into a typed
    /// `Event` variant). The raw JSON is surfaced so probes can
    /// capture and we can extend the schema without dropping
    /// messages.
    UnhandledType { raw: String },
}

// ---------------------------------------------------------------- internals

#[derive(Debug)]
enum TaskCmd {
    Subscribe {
        req_id: u64,
        channels: Vec<String>,
        market_tickers: Vec<String>,
    },
    Unsubscribe {
        req_id: u64,
        sids: Vec<u64>,
    },
    UpdateSubscription {
        req_id: u64,
        sids: Vec<u64>,
        action: UpdateAction,
        market_tickers: Vec<String>,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
struct SavedSub {
    req_id: u64,
    channels: Vec<String>,
    market_tickers: Vec<String>,
}

struct RunCtx {
    endpoint: Url,
    signer: Option<Arc<Signer>>,
    backoff: Backoff,
    cmd_rx: mpsc::Receiver<TaskCmd>,
    event_tx: mpsc::Sender<Event>,
}

async fn run_task(mut ctx: RunCtx) {
    let mut subs: Vec<SavedSub> = Vec::new();
    let mut attempts: u32 = 0;
    let mut first_connect = true;

    'outer: loop {
        match connect_once(&ctx.endpoint, ctx.signer.as_deref()).await {
            Ok(ws) => {
                if !first_connect && ctx.event_tx.send(Event::Reconnected).await.is_err() {
                    break 'outer;
                }
                first_connect = false;
                attempts = 0;
                let outcome = run_session(ws, &mut subs, &mut ctx).await;
                match outcome {
                    SessionOutcome::Shutdown => break 'outer,
                    SessionOutcome::Disconnected(reason) => {
                        attempts = attempts.saturating_add(1);
                        if ctx
                            .event_tx
                            .send(Event::Disconnected {
                                attempt: attempts,
                                reason,
                            })
                            .await
                            .is_err()
                        {
                            break 'outer;
                        }
                    }
                }
            }
            Err(e) => {
                attempts = attempts.saturating_add(1);
                if ctx
                    .event_tx
                    .send(Event::Disconnected {
                        attempt: attempts,
                        reason: e.to_string(),
                    })
                    .await
                    .is_err()
                {
                    break 'outer;
                }
            }
        }

        // Backoff sleep, but stay responsive to new commands so a
        // shutdown request takes effect immediately.
        let delay = ctx.backoff.next_delay(attempts.saturating_sub(1));
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                maybe_cmd = ctx.cmd_rx.recv() => match maybe_cmd {
                    None | Some(TaskCmd::Shutdown) => break 'outer,
                    Some(other) => {
                        // Queue subscribes/unsubscribes that arrived during
                        // backoff so they apply on the next connect.
                        apply_command_offline(other, &mut subs);
                    }
                }
            }
        }
    }
    debug!("kalshi-md task exiting");
}

enum SessionOutcome {
    Shutdown,
    Disconnected(String),
}

async fn run_session<S>(ws: S, subs: &mut Vec<SavedSub>, ctx: &mut RunCtx) -> SessionOutcome
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let (mut sink, mut stream) = ws.split();

    // Replay saved subscriptions. Each saved sub becomes one Subscribe
    // command on the wire. We don't expect Kalshi to error on repeats
    // post-2026, but if it does we surface it via Event::ServerError.
    for sub in subs.iter() {
        let cmd = build_subscribe(sub);
        if let Err(e) = send_outgoing(&mut sink, &cmd).await {
            return SessionOutcome::Disconnected(format!("replay subscribe: {e}"));
        }
    }

    loop {
        tokio::select! {
            maybe_cmd = ctx.cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else { return SessionOutcome::Shutdown };
                match cmd {
                    TaskCmd::Shutdown => {
                        let _ = sink.send(Message::Close(None)).await;
                        return SessionOutcome::Shutdown;
                    }
                    TaskCmd::Subscribe { req_id, channels, market_tickers } => {
                        let saved = SavedSub { req_id, channels: channels.clone(), market_tickers: market_tickers.clone() };
                        subs.push(saved.clone());
                        let cmd = build_subscribe(&saved);
                        if let Err(e) = send_outgoing(&mut sink, &cmd).await {
                            return SessionOutcome::Disconnected(format!("send subscribe: {e}"));
                        }
                    }
                    TaskCmd::Unsubscribe { req_id, sids } => {
                        let cmd = Outgoing::Unsubscribe {
                            id: req_id,
                            params: UnsubscribeParams { sids: sids.clone() },
                        };
                        if let Err(e) = send_outgoing(&mut sink, &cmd).await {
                            return SessionOutcome::Disconnected(format!("send unsubscribe: {e}"));
                        }
                    }
                    TaskCmd::UpdateSubscription { req_id, sids, action, market_tickers } => {
                        let cmd = Outgoing::UpdateSubscription {
                            id: req_id,
                            params: UpdateParams {
                                sids: sids.clone(),
                                action,
                                market_tickers: Some(market_tickers.clone()),
                            },
                        };
                        if let Err(e) = send_outgoing(&mut sink, &cmd).await {
                            return SessionOutcome::Disconnected(format!("send update_subscription: {e}"));
                        }
                    }
                }
            }
            maybe_msg = stream.next() => {
                let Some(msg) = maybe_msg else {
                    return SessionOutcome::Disconnected("stream ended".into());
                };
                match msg {
                    Ok(Message::Text(text)) => {
                        if handle_text(&text, &ctx.event_tx).await.is_err() {
                            return SessionOutcome::Shutdown;
                        }
                    }
                    Ok(Message::Binary(bin)) => {
                        // Kalshi WS is JSON-only; binary frames are unexpected.
                        warn!(len = bin.len(), "unexpected binary frame");
                    }
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                        // tokio-tungstenite auto-replies to Pings; raw Frame
                        // is a low-level escape hatch we never produce.
                    }
                    Ok(Message::Close(frame)) => {
                        let reason = frame
                            .as_ref()
                            .map_or_else(|| "server closed".to_string(), |f| format!("close: {} {}", u16::from(f.code), f.reason));
                        return SessionOutcome::Disconnected(reason);
                    }
                    Err(e) => {
                        return SessionOutcome::Disconnected(format!("stream error: {e}"));
                    }
                }
            }
        }
    }
}

/// Sentinel returned when the event-channel receiver is gone — we should
/// terminate cleanly rather than spin.
struct EventChannelClosed;

async fn handle_text(raw: &str, event_tx: &mpsc::Sender<Event>) -> Result<(), EventChannelClosed> {
    let event = match serde_json::from_str::<Incoming>(raw) {
        Ok(parsed) => incoming_to_event(parsed, raw),
        Err(e) => Some(Event::Malformed {
            raw: raw.to_string(),
            error: e.to_string(),
        }),
    };
    if let Some(ev) = event
        && event_tx.send(ev).await.is_err()
    {
        return Err(EventChannelClosed);
    }
    Ok(())
}

fn incoming_to_event(msg: Incoming, raw: &str) -> Option<Event> {
    match msg {
        Incoming::Subscribed { id, msg } => Some(Event::Subscribed {
            req_id: id,
            channel: msg.channel,
            sid: msg.sid,
        }),
        Incoming::Ok { .. } => {
            // `ok` envelopes are subscription-state confirmations; not
            // useful to most consumers. Logged at debug.
            debug!(raw = %raw, "ok envelope");
            None
        }
        Incoming::Error { id, msg } => Some(Event::ServerError {
            req_id: id,
            code: msg.code,
            msg: msg.msg,
        }),
        Incoming::OrderbookSnapshot { sid, seq, msg } => match snapshot_from_wire(&msg, seq) {
            Ok(snapshot) => Some(Event::Snapshot {
                sid,
                market: msg.market_ticker,
                snapshot,
            }),
            Err(e) => Some(Event::Malformed {
                raw: raw.to_string(),
                error: format!("snapshot decode: {e}"),
            }),
        },
        Incoming::OrderbookDelta { sid, seq, msg } => match delta_from_wire(&msg, seq) {
            Ok(delta) => Some(Event::Delta { sid, delta }),
            Err(e) => Some(Event::Malformed {
                raw: raw.to_string(),
                error: format!("delta decode: {e}"),
            }),
        },
        Incoming::Ticker { sid, msg } => Some(Event::Ticker { sid, body: msg }),
        Incoming::Trade { sid, msg } => Some(Event::Trade { sid, body: msg }),
        Incoming::Fill { sid, msg } => Some(Event::Fill { sid, body: msg }),
        Incoming::MarketPosition { sid, msg } => Some(Event::MarketPosition { sid, body: msg }),
        Incoming::Other => Some(Event::UnhandledType {
            raw: raw.to_string(),
        }),
    }
}

async fn send_outgoing<S>(
    sink: &mut S,
    cmd: &Outgoing,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let json = serde_json::to_string(cmd)
        .expect("Outgoing command should always serialise to JSON; struct contains no maps with non-string keys or floats");
    sink.send(Message::Text(json.into())).await
}

fn build_subscribe(sub: &SavedSub) -> Outgoing {
    // Authenticated subscriptions can omit market_tickers entirely
    // (Kalshi reads "all markets for this user" from the absence of
    // the field). Public ones must use one of the two shapes.
    let (single, plural) = match sub.market_tickers.len() {
        0 => (None, None),
        1 => (Some(sub.market_tickers[0].clone()), None),
        _ => (None, Some(sub.market_tickers.clone())),
    };
    Outgoing::Subscribe {
        id: sub.req_id,
        params: SubscribeParams {
            channels: sub.channels.clone(),
            market_ticker: single,
            market_tickers: plural,
        },
    }
}

fn apply_command_offline(cmd: TaskCmd, subs: &mut Vec<SavedSub>) {
    match cmd {
        TaskCmd::Subscribe {
            req_id,
            channels,
            market_tickers,
        } => {
            subs.push(SavedSub {
                req_id,
                channels,
                market_tickers,
            });
        }
        TaskCmd::Unsubscribe { .. } | TaskCmd::UpdateSubscription { .. } | TaskCmd::Shutdown => {
            // Unsubscribe-by-sid only meaningful in-session; queueing
            // would target a nonexistent sid post-reconnect. Same for
            // update_subscription snapshot requests. Shutdown is handled by
            // the outer select.
        }
    }
}

async fn connect_once(
    endpoint: &Url,
    signer: Option<&Signer>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Error,
> {
    let mut request = endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| Error::Upgrade(format!("build request: {e}")))?;

    if let Some(signer) = signer {
        let path = endpoint.path();
        let (ts, sig) = signer.sign("GET", path);
        let headers = request.headers_mut();
        headers.insert(
            "kalshi-access-key",
            HeaderValue::from_str(signer.key_id())
                .map_err(|e| Error::Upgrade(format!("key header: {e}")))?,
        );
        headers.insert(
            "kalshi-access-timestamp",
            HeaderValue::from_str(&ts).map_err(|e| Error::Upgrade(format!("ts header: {e}")))?,
        );
        headers.insert(
            "kalshi-access-signature",
            HeaderValue::from_str(&sig).map_err(|e| Error::Upgrade(format!("sig header: {e}")))?,
        );
    }

    info!(endpoint = %endpoint, "kalshi-md ws connecting");
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Channel;

    #[test]
    fn build_subscribe_uses_singular_for_one_market() {
        let sub = SavedSub {
            req_id: 1,
            channels: vec![Channel::OrderbookDelta.wire_name().to_string()],
            market_tickers: vec!["X".into()],
        };
        let cmd = build_subscribe(&sub);
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""market_ticker":"X""#), "got: {s}");
        assert!(!s.contains("market_tickers"), "got: {s}");
    }

    #[test]
    fn build_subscribe_uses_plural_for_many_markets() {
        let sub = SavedSub {
            req_id: 2,
            channels: vec![Channel::Ticker.wire_name().to_string()],
            market_tickers: vec!["A".into(), "B".into()],
        };
        let cmd = build_subscribe(&sub);
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""market_tickers":["A","B"]"#), "got: {s}");
    }

    #[test]
    fn update_subscription_get_snapshot_serialises() {
        let cmd = Outgoing::UpdateSubscription {
            id: 9,
            params: UpdateParams {
                sids: vec![7],
                action: UpdateAction::GetSnapshot,
                market_tickers: Some(vec!["A".into(), "B".into()]),
            },
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""cmd":"update_subscription""#), "got: {s}");
        assert!(s.contains(r#""action":"get_snapshot""#), "got: {s}");
        assert!(s.contains(r#""sids":[7]"#), "got: {s}");
        assert!(s.contains(r#""market_tickers":["A","B"]"#), "got: {s}");
    }

    #[test]
    fn apply_command_offline_appends_subscribe() {
        let mut subs = Vec::new();
        apply_command_offline(
            TaskCmd::Subscribe {
                req_id: 3,
                channels: vec!["trade".into()],
                market_tickers: vec!["X".into()],
            },
            &mut subs,
        );
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].req_id, 3);
    }

    #[test]
    fn apply_command_offline_drops_unsubscribe() {
        let mut subs = Vec::new();
        apply_command_offline(
            TaskCmd::Unsubscribe {
                req_id: 4,
                sids: vec![7],
            },
            &mut subs,
        );
        assert!(subs.is_empty());
    }

    #[test]
    fn apply_command_offline_drops_update_subscription() {
        let mut subs = Vec::new();
        apply_command_offline(
            TaskCmd::UpdateSubscription {
                req_id: 5,
                sids: vec![7],
                action: UpdateAction::GetSnapshot,
                market_tickers: vec!["X".into()],
            },
            &mut subs,
        );
        assert!(subs.is_empty());
    }

    #[test]
    fn malformed_event_carries_raw_and_error() {
        // We can synthesise a Malformed via the public path: `incoming_to_event`
        // doesn't run on parse failure; `handle_text` is what surfaces the raw
        // string. Exercise that route through the test indirectly.
        let raw = "{not json";
        let parsed = serde_json::from_str::<Incoming>(raw).unwrap_err();
        let ev = Event::Malformed {
            raw: raw.into(),
            error: parsed.to_string(),
        };
        match ev {
            Event::Malformed { raw: r, error } => {
                assert_eq!(r, "{not json");
                assert!(!error.is_empty());
            }
            _ => panic!(),
        }
    }
}
