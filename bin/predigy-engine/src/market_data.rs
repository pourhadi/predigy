//! Engine market-data router.
//!
//! Wraps `predigy-kalshi-md` (the existing WS client) into the
//! engine-side abstractions:
//!
//! - One `MarketDataRouter` per engine instance, one Kalshi WS
//!   connection.
//! - Maintains per-market `OrderBook` state (snapshots + deltas;
//!   sid-level gap detection triggers WS snapshot resync).
//! - Tracks (sid → market_ticker) mapping since Kalshi events
//!   are keyed by `sid` not by ticker.
//! - Fans out `Event::BookUpdate` to every strategy supervisor
//!   that subscribed to that ticker.
//!
//! The router runs as its own tokio task. Strategy supervisors
//! receive book events through the mpsc senders the router
//! holds.

use predigy_book::{ApplyOutcome, OrderBook};
use predigy_core::market::MarketTicker;
use predigy_engine_core::events::Event;
use predigy_engine_core::strategy::StrategyId;
use predigy_kalshi_md::{
    Channel, Client as MdClient, Connection as MdConnection, Event as MdEvent,
};
use predigy_kalshi_rest::Signer;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Output channel each strategy supervisor exposes for its
/// `Event` queue. The router pushes `BookUpdate`s into here.
pub type StrategyEventTx = mpsc::Sender<Event>;

/// Map from market ticker → set of (strategy id, sender) pairs
/// interested in book updates for that ticker.
#[derive(Debug, Default)]
struct Subscriptions {
    by_ticker: HashMap<String, Vec<(StrategyId, StrategyEventTx)>>,
}

impl Subscriptions {
    fn add(&mut self, ticker: &str, strategy: StrategyId, tx: StrategyEventTx) {
        self.by_ticker
            .entry(ticker.to_string())
            .or_default()
            .push((strategy, tx));
    }

    fn senders_for(&self, ticker: &str) -> Vec<(StrategyId, StrategyEventTx)> {
        self.by_ticker.get(ticker).cloned().unwrap_or_default()
    }

    fn unique_tickers(&self) -> Vec<String> {
        self.by_ticker.keys().cloned().collect()
    }
}

/// Configuration for the router. We take the raw PEM bytes (not a `Signer`)
/// because `Signer` doesn't implement `Clone` and the engine uses the same PEM
/// for multiple venue connections.
#[derive(Clone)]
pub struct RouterConfig {
    pub kalshi_key_id: String,
    pub kalshi_pem: String,
    pub rest_endpoint: Option<String>,
    pub ws_endpoint: Option<url::Url>,
}

impl std::fmt::Debug for RouterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterConfig")
            .field("rest_endpoint", &self.rest_endpoint)
            .field("ws_endpoint", &self.ws_endpoint)
            .finish_non_exhaustive()
    }
}

/// Router state — held inside the spawned task.
struct RouterState {
    subs: Arc<RwLock<Subscriptions>>,
    /// Per-market book state. Built lazily on first snapshot.
    books: HashMap<String, OrderBook>,
    /// Client request id → tickers requested. `subscribed` only carries the
    /// request id and sid; this lets us attach each orderbook sid to the
    /// markets it covers.
    req_to_tickers: HashMap<u64, Vec<String>>,
    /// Orderbook sid → tickers observed/requested on that subscription.
    sid_to_tickers: HashMap<u64, HashSet<String>>,
    /// Last observed exchange sequence per orderbook sid. Kalshi sequences are
    /// subscription-scoped, not per-market.
    sid_last_seq: HashMap<u64, u64>,
    /// Markets currently waiting for a WS `get_snapshot` resync per sid.
    sid_snapshot_pending: HashMap<u64, HashSet<String>>,
    /// Tickers we've already subscribed to. Lets `AddTickers`
    /// skip duplicates (issuing a second subscribe for an already-
    /// subscribed ticker isn't an error but it wastes a req_id).
    subscribed_tickers: std::collections::HashSet<String>,
}

/// Command sent over the router's command channel. Used by
/// background services (e.g. discovery) to dynamically extend
/// the subscription set after the initial WS subscribe.
#[derive(Debug)]
pub enum RouterCommand {
    /// Subscribe to an additional ticker for a given strategy.
    /// The router issues a fresh `Channel::OrderbookDelta +
    /// Channel::Ticker` subscribe for the new ticker and adds
    /// the (strategy, tx) pair to the subscriber set.
    AddTickers {
        strategy: StrategyId,
        markets: Vec<String>,
        tx: StrategyEventTx,
    },
}

/// Public handle for the router. Owns the spawned task; drop
/// to abort.
pub struct MarketDataRouter {
    subs: Arc<RwLock<Subscriptions>>,
    cmd_tx: mpsc::Sender<RouterCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for MarketDataRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketDataRouter").finish_non_exhaustive()
    }
}

impl MarketDataRouter {
    /// Build the router and connect to Kalshi. Doesn't subscribe
    /// to anything yet — call `register_strategy` for each
    /// strategy module then `start_subscriptions` once registration
    /// completes.
    pub async fn connect(config: RouterConfig) -> anyhow::Result<Self> {
        let md_signer = Signer::from_pem(&config.kalshi_key_id, &config.kalshi_pem)
            .map_err(|e| anyhow::anyhow!("md signer: {e}"))?;
        let md_client = MdClient::new(md_signer)?;
        let connection = md_client.connect();
        let subs = Arc::new(RwLock::new(Subscriptions::default()));

        let (cmd_tx, cmd_rx) = mpsc::channel::<RouterCommand>(128);

        let router_state = RouterState {
            subs: subs.clone(),
            books: HashMap::new(),
            req_to_tickers: HashMap::new(),
            sid_to_tickers: HashMap::new(),
            sid_last_seq: HashMap::new(),
            sid_snapshot_pending: HashMap::new(),
            subscribed_tickers: std::collections::HashSet::new(),
        };

        let task = tokio::spawn(router_task(connection, router_state, cmd_rx));
        Ok(Self { subs, cmd_tx, task })
    }

    /// Cloneable handle for issuing dynamic-subscription commands
    /// to the router from background services (discovery loop, etc.)
    /// without exposing the full `MarketDataRouter`.
    pub fn command_tx(&self) -> mpsc::Sender<RouterCommand> {
        self.cmd_tx.clone()
    }

    /// Register a strategy's interest in a set of markets. The
    /// `tx` channel will receive `BookUpdate` events for those
    /// tickers.
    pub async fn register_strategy(
        &self,
        strategy: StrategyId,
        markets: &[MarketTicker],
        tx: StrategyEventTx,
    ) {
        let mut s = self.subs.write().await;
        for m in markets {
            s.add(m.as_str(), strategy, tx.clone());
        }
        info!(
            strategy = strategy.0,
            n_markets = markets.len(),
            "router: strategy registered"
        );
    }

    /// Returns the union of all registered tickers — used by the
    /// engine to issue the initial WS subscribe.
    pub async fn subscribed_markets(&self) -> Vec<String> {
        self.subs.read().await.unique_tickers()
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn router_task(
    mut connection: MdConnection,
    mut state: RouterState,
    mut cmd_rx: mpsc::Receiver<RouterCommand>,
) {
    // Wait until the engine populates subscriptions before issuing
    // the first subscribe. Polling-based: check the subscription
    // registry every 250ms until it's non-empty, then issue the
    // subscribe and start the event loop.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        let tickers = {
            let s = state.subs.read().await;
            s.unique_tickers()
        };
        if tickers.is_empty() {
            continue;
        }
        // Subscribe to orderbook deltas + ticker.
        match connection
            .subscribe(&[Channel::OrderbookDelta, Channel::Ticker], &tickers)
            .await
        {
            Ok(req_id) => {
                info!(
                    req_id = req_id,
                    n_tickers = tickers.len(),
                    "router: subscribe submitted"
                );
                state.req_to_tickers.insert(req_id, tickers.clone());
                for t in &tickers {
                    state.subscribed_tickers.insert(t.clone());
                }
                break;
            }
            Err(e) => {
                warn!(error = %e, "router: initial subscribe failed; will retry");
                continue;
            }
        }
    }

    // Main event loop — multiplex Kalshi events with router
    // commands (dynamic subscribe requests from discovery).
    loop {
        tokio::select! {
            ev = connection.next_event() => {
                let Some(ev) = ev else {
                    warn!("router: kalshi-md connection closed");
                    return;
                };
                if let Err(e) = handle_event(ev, &mut state, &mut connection).await {
                    warn!(error = %e, "router: event-handling error");
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    info!("router: command channel closed; exiting");
                    return;
                };
                if let Err(e) = handle_command(cmd, &mut state, &mut connection).await {
                    warn!(error = %e, "router: command-handling error");
                }
            }
        }
    }
}

async fn handle_command(
    cmd: RouterCommand,
    state: &mut RouterState,
    connection: &mut MdConnection,
) -> anyhow::Result<()> {
    match cmd {
        RouterCommand::AddTickers {
            strategy,
            markets,
            tx,
        } => {
            // Update the subscriber registry first so book updates
            // for these tickers are routed to the strategy as
            // soon as the venue starts pushing them.
            {
                let mut s = state.subs.write().await;
                for m in &markets {
                    s.add(m, strategy, tx.clone());
                }
            }
            // Filter to the tickers we haven't subscribed to yet
            // — repeated subscribes are harmless but cluttery in
            // the WS log.
            let new: Vec<String> = markets
                .into_iter()
                .filter(|m| state.subscribed_tickers.insert(m.clone()))
                .collect();
            if new.is_empty() {
                return Ok(());
            }
            match connection
                .subscribe(&[Channel::OrderbookDelta, Channel::Ticker], &new)
                .await
            {
                Ok(req_id) => {
                    state.req_to_tickers.insert(req_id, new.clone());
                    info!(
                        req_id,
                        n_new_tickers = new.len(),
                        strategy = strategy.0,
                        "router: dynamic subscribe submitted"
                    );
                }
                Err(e) => {
                    // Roll back the subscribed_tickers entries
                    // so a retry has a clean shot.
                    for m in &new {
                        state.subscribed_tickers.remove(m);
                    }
                    warn!(
                        strategy = strategy.0,
                        error = %e,
                        "router: dynamic subscribe failed"
                    );
                }
            }
        }
    }
    Ok(())
}

async fn handle_event(
    ev: MdEvent,
    state: &mut RouterState,
    connection: &mut MdConnection,
) -> anyhow::Result<()> {
    match ev {
        MdEvent::Subscribed {
            req_id,
            sid,
            channel,
        } => {
            if channel == Channel::OrderbookDelta.wire_name() {
                if let Some(req_id) = req_id
                    && let Some(tickers) = state.req_to_tickers.get(&req_id)
                {
                    state
                        .sid_to_tickers
                        .entry(sid)
                        .or_default()
                        .extend(tickers.iter().cloned());
                }
                state.sid_last_seq.remove(&sid);
                state.sid_snapshot_pending.remove(&sid);
            }
            debug!(sid, channel, "router: server confirmed subscribe");
        }
        MdEvent::Snapshot {
            sid,
            market,
            snapshot,
        } => {
            state
                .sid_to_tickers
                .entry(sid)
                .or_default()
                .insert(market.clone());
            if let Some((expected, got)) = observe_sid_seq(state, sid, snapshot.seq) {
                warn!(
                    sid,
                    market,
                    expected,
                    got,
                    "router: sid sequence gap on snapshot; requesting WS snapshots"
                );
                let markets = known_sid_markets(state, sid);
                request_sid_snapshot(state, connection, sid, markets).await?;
            }
            // Borrow scopes split: apply mutation in one block,
            // clone the book, then fan out using the clone +
            // the immutable subscription registry.
            let book_clone = {
                let book = state
                    .books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                book.apply_snapshot(snapshot);
                book.clone()
            };
            mark_snapshot_ready(state, sid, &market);
            fan_out(&state.subs, &market, &book_clone).await;
        }
        MdEvent::Delta { sid, delta } => {
            // Route by `delta.market` (from the wire body), NOT
            // by sid. Kalshi multiplexes multiple tickers under
            // one subscription id; `sid_to_ticker` only stores
            // ONE ticker per sid (last-write-wins) so it routes
            // every delta to that one ticker's book and rejects
            // the rest as `WrongMarket`. The wire body's
            // `market_ticker` is authoritative.
            let market = delta.market.clone();
            state
                .sid_to_tickers
                .entry(sid)
                .or_default()
                .insert(market.clone());
            if let Some((expected, got)) = observe_sid_seq(state, sid, delta.seq) {
                warn!(
                    sid,
                    market, expected, got, "router: sid sequence gap; requesting WS snapshots"
                );
                let markets = known_sid_markets(state, sid);
                request_sid_snapshot(state, connection, sid, markets).await?;
                return Ok(());
            }
            if pending_snapshot(state, sid, &market) {
                debug!(
                    sid,
                    market, "router: dropping delta while awaiting WS snapshot"
                );
                return Ok(());
            }
            if !state.books.contains_key(&market) {
                warn!(
                    sid,
                    market, "router: delta before snapshot; requesting WS snapshot"
                );
                request_sid_snapshot(state, connection, sid, vec![market]).await?;
                return Ok(());
            }
            let outcome_clone = {
                let book = state
                    .books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                let outcome = book.apply_delta_unsequenced(&delta);
                let cloned = book.clone();
                (outcome, cloned)
            };
            match outcome_clone.0 {
                ApplyOutcome::Ok => {
                    fan_out(&state.subs, &market, &outcome_clone.1).await;
                }
                ApplyOutcome::Gap { expected, got } => {
                    warn!(
                        market,
                        expected, got, "router: unexpected per-book sequence gap"
                    );
                }
                ApplyOutcome::WrongMarket => {
                    warn!(market, "router: delta wrong-market; ignoring");
                }
            }
        }
        MdEvent::Ticker { .. } | MdEvent::Trade { .. } => {
            // Strategies don't yet consume ticker / trade events;
            // we keep them subscribed for low-latency last-trade
            // signals once strategies opt in.
        }
        MdEvent::Disconnected { attempt, reason } => {
            warn!(attempt, reason, "router: kalshi-md disconnected");
        }
        MdEvent::Reconnected => {
            state.books.clear();
            state.sid_to_tickers.clear();
            state.sid_last_seq.clear();
            state.sid_snapshot_pending.clear();
            info!("router: kalshi-md reconnected; books may be stale until next snapshot");
        }
        MdEvent::ServerError { req_id, code, msg } => {
            warn!(?req_id, code, msg, "router: kalshi-md server error");
        }
        MdEvent::Malformed { raw, error } => {
            warn!(
                error,
                raw_excerpt = &raw.chars().take(100).collect::<String>().as_str(),
                "router: malformed frame"
            );
        }
        MdEvent::UnhandledType { raw } => {
            debug!(
                raw_excerpt = &raw.chars().take(100).collect::<String>().as_str(),
                "router: unhandled message type"
            );
        }
        MdEvent::Fill { .. } | MdEvent::MarketPosition { .. } => {
            // Authed channels — strategies don't consume directly;
            // these flow into the OMS reconciliation path.
        }
    }
    Ok(())
}

async fn fan_out(subs: &Arc<RwLock<Subscriptions>>, market: &str, book: &OrderBook) {
    let senders = {
        let s = subs.read().await;
        s.senders_for(market)
    };
    if senders.is_empty() {
        return;
    }
    // Clone the book once per strategy. Books are small (BTreeMap
    // of price levels), <1KB typical.
    for (strategy, tx) in senders {
        let ev = Event::BookUpdate {
            market: MarketTicker::new(market),
            book: book.clone(),
        };
        if let Err(e) = tx.try_send(ev) {
            // Slow strategy: don't block the router. Log + drop.
            warn!(
                strategy = strategy.0,
                error = %e,
                market,
                "router: strategy event queue full or closed; dropping book update"
            );
        }
    }
}

fn observe_sid_seq(state: &mut RouterState, sid: u64, seq: u64) -> Option<(u64, u64)> {
    let Some(last) = state.sid_last_seq.get(&sid).copied() else {
        state.sid_last_seq.insert(sid, seq);
        return None;
    };
    if seq <= last {
        return None;
    }
    state.sid_last_seq.insert(sid, seq);
    let expected = last.saturating_add(1);
    if seq > expected {
        Some((expected, seq))
    } else {
        None
    }
}

fn known_sid_markets(state: &RouterState, sid: u64) -> Vec<String> {
    state
        .sid_to_tickers
        .get(&sid)
        .map(|markets| markets.iter().cloned().collect())
        .unwrap_or_default()
}

fn pending_snapshot(state: &RouterState, sid: u64, market: &str) -> bool {
    state
        .sid_snapshot_pending
        .get(&sid)
        .is_some_and(|pending| pending.contains(market))
}

fn mark_snapshot_ready(state: &mut RouterState, sid: u64, market: &str) {
    if let Some(pending) = state.sid_snapshot_pending.get_mut(&sid) {
        pending.remove(market);
        if pending.is_empty() {
            state.sid_snapshot_pending.remove(&sid);
        }
    }
}

async fn request_sid_snapshot(
    state: &mut RouterState,
    connection: &mut MdConnection,
    sid: u64,
    markets: Vec<String>,
) -> anyhow::Result<()> {
    if markets.is_empty() {
        anyhow::bail!("sid {sid} has no known markets to resnapshot");
    }
    if state
        .sid_snapshot_pending
        .get(&sid)
        .is_some_and(|pending| !pending.is_empty())
    {
        debug!(sid, "router: WS snapshot request already pending");
        return Ok(());
    }

    let pending: HashSet<String> = markets.iter().cloned().collect();
    let req_id = connection.get_snapshot(sid, &markets).await?;
    state.sid_snapshot_pending.insert(sid, pending);
    info!(
        sid,
        req_id,
        n_markets = markets.len(),
        "router: WS snapshot request submitted"
    );
    Ok(())
}
