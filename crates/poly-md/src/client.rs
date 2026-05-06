//! Polymarket WS client: connect (no auth), subscribe by `asset_id`,
//! decode events, reconnect with exponential backoff and re-subscription.
//!
//! Architecture mirrors `predigy-kalshi-md`: a single background tokio
//! task owns the WS stream and multiplexes between a command channel
//! (subscribe / shutdown) and incoming frames decoded into typed
//! [`Event`]s. Differences from the Kalshi client:
//!
//! - **No auth.** Polymarket's market channel is public.
//! - **No request ids.** Polymarket subscribe is a single payload, not a
//!   command/response with correlation ids. There's also no in-band
//!   unsubscribe — the documented mechanism is to close the connection,
//!   so we drop unsubscribe support entirely.
//! - **Subscribe-by-asset.** The unit of subscription is an `asset_id`
//!   (token), not a market. Saved state is `Vec<asset_id>`; on reconnect
//!   we send one consolidated subscribe.

use crate::backoff::Backoff;
use crate::error::Error;
use crate::messages::{
    BookEvent, Incoming, LastTradePriceEvent, PriceChangeEvent, Subscribe, TickSizeChangeEvent,
};
use futures_util::{SinkExt as _, StreamExt as _};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};
use url::Url;

/// Production WebSocket endpoint for the public market channel.
pub const DEFAULT_ENDPOINT: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

const CMD_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 4096;

/// Configured-but-not-connected client.
#[derive(Debug, Clone)]
pub struct Client {
    endpoint: Url,
    backoff: Backoff,
    /// If `Some(d)`, send a text-frame `"PING"` message every `d` to
    /// keep the connection alive.  `None` (the default) preserves
    /// the original behavior of relying on protocol-level keepalive
    /// only.
    ///
    /// Why opt-in: Polymarket's CLOB WS endpoint observed in
    /// production drops connections every ~2 minutes without
    /// application-level traffic, even though tokio-tungstenite
    /// auto-responds to protocol-level Pings.  Setting this to
    /// ~10s appears (per Polymarket's docs and third-party
    /// clients) to keep the connection alive indefinitely; left
    /// opt-in until validated against their live server, since
    /// pushing the wrong wire format could trigger immediate
    /// disconnects instead of the steady 2-min cadence.
    text_ping_interval: Option<Duration>,
}

impl Client {
    /// Production client against the documented market-channel endpoint.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            endpoint: Url::parse(DEFAULT_ENDPOINT)?,
            backoff: Backoff::default_const(),
            text_ping_interval: None,
        })
    }

    /// Build with a custom endpoint (test servers / future regional shards).
    pub fn with_endpoint(endpoint: Url) -> Self {
        Self {
            endpoint,
            backoff: Backoff::default_const(),
            text_ping_interval: None,
        }
    }

    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Enable text-frame `"PING"` keepalive at the given cadence.
    /// 10 seconds is the conservative default we'd pick if/when
    /// validated; 30 seconds matches Polymarket's documented
    /// minimum.  See [`text_ping_interval`](Self::text_ping_interval).
    #[must_use]
    pub fn with_text_ping_interval(mut self, interval: Duration) -> Self {
        self.text_ping_interval = Some(interval);
        self
    }

    /// Spawn the background task and return a handle. As with the Kalshi
    /// client, initial connection failures don't error here — they
    /// surface as `Event::Disconnected` and the task enters backoff.
    pub fn connect(&self) -> Connection {
        let (cmd_tx, cmd_rx) = mpsc::channel::<TaskCmd>(CMD_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<Event>(EVENT_CAPACITY);
        let task = tokio::spawn(run_task(RunCtx {
            endpoint: self.endpoint.clone(),
            backoff: self.backoff,
            text_ping_interval: self.text_ping_interval,
            cmd_rx,
            event_tx,
        }));
        Connection {
            cmd_tx,
            event_rx,
            task: Some(task),
        }
    }
}

#[derive(Debug)]
pub struct Connection {
    cmd_tx: mpsc::Sender<TaskCmd>,
    event_rx: mpsc::Receiver<Event>,
    task: Option<JoinHandle<()>>,
}

impl Connection {
    /// Subscribe to one or more `asset_id`s. Saved across reconnects.
    /// Idempotent: subscribing to the same asset twice is a no-op.
    pub async fn subscribe(&mut self, asset_ids: &[String]) -> Result<(), Error> {
        if asset_ids.is_empty() {
            return Err(Error::Invalid("subscribe: asset_ids is empty".into()));
        }
        self.cmd_tx
            .send(TaskCmd::Subscribe(asset_ids.to_vec()))
            .await
            .map_err(|_| Error::Closed)
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }

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

/// High-level event surfaced to the caller.
#[derive(Debug)]
pub enum Event {
    Book(BookEvent),
    PriceChange(PriceChangeEvent),
    LastTradePrice(LastTradePriceEvent),
    TickSizeChange(TickSizeChangeEvent),
    /// Connection dropped; entering backoff. `attempt` is 1-indexed for
    /// the upcoming retry.
    Disconnected {
        attempt: u32,
        reason: String,
    },
    /// Reconnected; saved subscription was replayed. Consumers should
    /// expect a fresh `Book` for each subscribed asset.
    Reconnected,
    /// Frame couldn't be parsed as a known `event_type`.
    Malformed {
        raw: String,
        error: String,
    },
}

#[derive(Debug)]
enum TaskCmd {
    Subscribe(Vec<String>),
    Shutdown,
}

struct RunCtx {
    endpoint: Url,
    backoff: Backoff,
    /// See [`Client::text_ping_interval`].
    text_ping_interval: Option<Duration>,
    cmd_rx: mpsc::Receiver<TaskCmd>,
    event_tx: mpsc::Sender<Event>,
}

async fn run_task(mut ctx: RunCtx) {
    let mut subs: BTreeSet<String> = BTreeSet::new();
    let mut attempts: u32 = 0;
    let mut first_connect = true;

    'outer: loop {
        match connect_once(&ctx.endpoint).await {
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

        let delay = ctx.backoff.next_delay(attempts.saturating_sub(1));
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                maybe_cmd = ctx.cmd_rx.recv() => match maybe_cmd {
                    None | Some(TaskCmd::Shutdown) => break 'outer,
                    Some(TaskCmd::Subscribe(ids)) => {
                        subs.extend(ids);
                    }
                }
            }
        }
    }
    debug!("poly-md task exiting");
}

enum SessionOutcome {
    Shutdown,
    Disconnected(String),
}

async fn run_session<S>(ws: S, subs: &mut BTreeSet<String>, ctx: &mut RunCtx) -> SessionOutcome
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let (mut sink, mut stream) = ws.split();

    // If we have any saved subs at all, send an initial subscribe.
    if !subs.is_empty()
        && let Err(e) = send_subscribe(&mut sink, subs).await
    {
        return SessionOutcome::Disconnected(format!("send subscribe: {e}"));
    }

    // Optional text-PING keepalive.  When `text_ping_interval` is
    // None, we still construct an Interval but with a long-enough
    // period that it won't fire during any realistic session — and
    // in that case the select branch is gated off by the
    // `if ctx.text_ping_interval.is_some()` guard.  Using a finite
    // duration here (7 days) avoids the `Instant::now() + Duration`
    // overflow that `Duration::MAX` triggers.
    //
    // When Some(d), we tick every `d` and push a "PING" text frame.
    // Polymarket's documented response is "PONG", which
    // `handle_text` recognises and drops without parsing.
    let ping_period = ctx
        .text_ping_interval
        .unwrap_or(Duration::from_hours(168));
    let mut ping_tick = tokio::time::interval(ping_period);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't double-send a PING
    // alongside the initial subscribe.
    ping_tick.reset();

    loop {
        tokio::select! {
            _ = ping_tick.tick(), if ctx.text_ping_interval.is_some() => {
                // Polymarket-compatible app-level keepalive.  Wire format:
                // a single text frame containing the ASCII string "PING".
                // The server replies with "PONG", which `handle_text`
                // recognises and drops.  Any send error here means the
                // socket is gone; bubble up the disconnect so the outer
                // loop can reconnect.
                if let Err(e) = sink.send(Message::Text("PING".into())).await {
                    return SessionOutcome::Disconnected(format!("ping: {e}"));
                }
            }
            maybe_cmd = ctx.cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else { return SessionOutcome::Shutdown };
                match cmd {
                    TaskCmd::Shutdown => {
                        let _ = sink.send(Message::Close(None)).await;
                        return SessionOutcome::Shutdown;
                    }
                    TaskCmd::Subscribe(ids) => {
                        let mut new_ones = Vec::new();
                        for id in ids {
                            if subs.insert(id.clone()) {
                                new_ones.push(id);
                            }
                        }
                        if !new_ones.is_empty() {
                            // Polymarket has no documented incremental subscribe;
                            // sending another subscribe with just the new ids is
                            // the de-facto pattern in third-party clients.
                            if let Err(e) = send_subscribe_for(&mut sink, &new_ones).await {
                                return SessionOutcome::Disconnected(format!("send subscribe: {e}"));
                            }
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
                        warn!(len = bin.len(), "unexpected binary frame");
                    }
                    Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                    Ok(Message::Close(frame)) => {
                        let reason = frame.as_ref().map_or_else(
                            || "server closed".to_string(),
                            |f| format!("close: {} {}", u16::from(f.code), f.reason),
                        );
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

struct EventChannelClosed;

async fn handle_text(raw: &str, event_tx: &mpsc::Sender<Event>) -> Result<(), EventChannelClosed> {
    // Polymarket sometimes batches events into a JSON array. Probe the
    // first non-whitespace character and dispatch accordingly so a single
    // incoming frame can produce multiple events.
    let trimmed = raw.trim_start();
    // App-level PING/PONG keepalive: when we send "PING" the server
    // responds with "PONG"; drop it silently so it doesn't surface
    // as a Malformed event.  Match exactly — anything else routes
    // through the normal JSON-decoding path.
    if trimmed == "PONG" || trimmed.eq_ignore_ascii_case("pong") {
        return Ok(());
    }
    let parse_result = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<Incoming>>(raw).map(EventBatch::Many)
    } else {
        serde_json::from_str::<Incoming>(raw).map(EventBatch::One)
    };
    match parse_result {
        Ok(EventBatch::One(m)) => {
            if let Some(ev) = incoming_to_event(m, raw)
                && event_tx.send(ev).await.is_err()
            {
                return Err(EventChannelClosed);
            }
        }
        Ok(EventBatch::Many(ms)) => {
            for m in ms {
                if let Some(ev) = incoming_to_event(m, raw)
                    && event_tx.send(ev).await.is_err()
                {
                    return Err(EventChannelClosed);
                }
            }
        }
        Err(e) => {
            if event_tx
                .send(Event::Malformed {
                    raw: raw.to_string(),
                    error: e.to_string(),
                })
                .await
                .is_err()
            {
                return Err(EventChannelClosed);
            }
        }
    }
    Ok(())
}

enum EventBatch {
    One(Incoming),
    Many(Vec<Incoming>),
}

fn incoming_to_event(msg: Incoming, raw: &str) -> Option<Event> {
    match msg {
        Incoming::Book(b) => Some(Event::Book(b)),
        Incoming::PriceChange(p) => Some(Event::PriceChange(p)),
        Incoming::LastTradePrice(t) => Some(Event::LastTradePrice(t)),
        Incoming::TickSizeChange(t) => Some(Event::TickSizeChange(t)),
        Incoming::Other => {
            debug!(raw = %raw, "ignored unknown event_type");
            None
        }
    }
}

async fn send_subscribe<S>(
    sink: &mut S,
    subs: &BTreeSet<String>,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let ids: Vec<String> = subs.iter().cloned().collect();
    send_subscribe_for(sink, &ids).await
}

async fn send_subscribe_for<S>(
    sink: &mut S,
    ids: &[String],
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let payload = Subscribe::for_assets(ids.to_vec());
    let json = serde_json::to_string(&payload).expect(
        "Subscribe payload should always serialise; fixed schema with no float / map fields",
    );
    sink.send(Message::Text(json.into())).await
}

async fn connect_once(
    endpoint: &Url,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Error,
> {
    let request = endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| Error::Upgrade(format!("build request: {e}")))?;
    info!(endpoint = %endpoint, "poly-md ws connecting");
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    Ok(ws)
}
