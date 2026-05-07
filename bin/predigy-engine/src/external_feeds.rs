//! External-feed dispatcher.
//!
//! Spawns the external data feeds the engine consumes (today: NWS
//! active alerts; future: Polymarket book, NBM cycle publish) and
//! fans each event out to the strategy supervisors that opted in
//! via [`Strategy::external_subscriptions`].
//!
//! ## Why a dispatcher
//!
//! The legacy daemons each spawned their own NWS subscription —
//! cheap individually, but the consolidated engine only wants ONE
//! NWS connection (so we don't blow past NWS's rate-limit
//! recommendations and so the dedup file stays consistent). The
//! dispatcher is that single point.
//!
//! ## Translation
//!
//! `predigy_ext_feeds::NwsAlert` is the wire-shaped struct from the
//! ext-feeds crate; `engine_core::ExternalEvent::NwsAlert` carries
//! the engine-side `NwsAlertPayload` shim. The dispatcher copies
//! field-for-field at the boundary so engine-core doesn't depend on
//! ext-feeds (preserves the layered crate graph).

use anyhow::{Context as _, Result};
use predigy_engine_core::events::{Event, ExternalEvent};
use predigy_engine_core::events::predigy_core_compat::NwsAlertPayload;
use predigy_engine_core::strategy::StrategyId;
use predigy_ext_feeds::{
    spawn_nws, NwsAlert, NwsAlertsConfig, MIN_POLL_INTERVAL,
};
use predigy_poly_md::{Client as PolyClient, Event as PolyEvent};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Configuration for the NWS feed slice of the dispatcher.
#[derive(Debug, Clone)]
pub struct NwsConfig {
    /// 2-letter state codes (`["TX", "OK"]`). Empty = all states.
    pub states: Vec<String>,
    /// User-Agent header NWS requires; format `"(app, contact)"`.
    pub user_agent: String,
    /// Poll cadence; floored to [`MIN_POLL_INTERVAL`] inside the
    /// ext-feeds crate.
    pub poll_interval: Duration,
    /// Optional persisted seen-id set so cross-restart we don't
    /// re-fire on already-known alerts.
    pub seen_path: Option<PathBuf>,
}

impl NwsConfig {
    fn enforced_interval(&self) -> Duration {
        if self.poll_interval < MIN_POLL_INTERVAL {
            MIN_POLL_INTERVAL
        } else {
            self.poll_interval
        }
    }
}

/// Command sent to the Polymarket dispatcher's command channel.
/// Used by the pair-file service to dynamically extend the
/// asset-id subscription set after the engine boots.
#[derive(Debug)]
pub enum PolyFeedCommand {
    /// Subscribe to additional asset_ids on the Poly WS. Polymarket
    /// has no in-band unsubscribe — removed pairs simply stop being
    /// referenced by the strategy; their stream remains open until
    /// the WS reconnects (saved subs are filtered on reconnect, see
    /// the `prune_subs` helper).
    AddAssets(Vec<String>),
    /// Drop assets from the saved-subscription list so the next
    /// reconnect doesn't re-subscribe to them.
    PruneAssets(Vec<String>),
}

/// Cloneable handle for the Polymarket dispatcher's command
/// channel. Returned from `ExternalFeeds::start()` so the
/// pair-file service can issue subscribe requests.
pub type PolyCommandTx = mpsc::Sender<PolyFeedCommand>;

/// Public handle. Drop or call `shutdown` to abort the spawned
/// dispatcher tasks.
pub struct ExternalFeeds {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Cloneable handle for the Polymarket dispatcher's command
    /// channel. `None` if poly wasn't spawned this boot.
    pub poly_tx: Option<PolyCommandTx>,
}

impl std::fmt::Debug for ExternalFeeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalFeeds")
            .field("n_tasks", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

impl ExternalFeeds {
    /// Construct + start the dispatcher. Only spawns feeds that at
    /// least one supervisor opted into via
    /// `Strategy::external_subscriptions()`. The `subscribers` map
    /// is keyed by feed id — the well-known string each strategy's
    /// `external_subscriptions()` returns ("nws_alerts" today).
    pub fn start(
        nws: Option<NwsConfig>,
        subscribers: &HashMap<&'static str, Vec<(StrategyId, mpsc::Sender<Event>)>>,
    ) -> Result<Self> {
        let mut tasks = Vec::new();
        let mut poly_tx: Option<PolyCommandTx> = None;

        if let Some(cfg) = nws {
            if let Some(consumers) = subscribers.get("nws_alerts") {
                if !consumers.is_empty() {
                    let consumers = consumers.clone();
                    let cfg = NwsAlertsConfig {
                        states: cfg.states.clone(),
                        poll_interval: cfg.enforced_interval(),
                        user_agent: cfg.user_agent.clone(),
                        base_url: None,
                        seen_path: cfg.seen_path.clone(),
                    };
                    let (rx, _feed_handle) = spawn_nws(cfg)
                        .map_err(|e| anyhow::anyhow!("spawn_nws: {e}"))?;
                    let consumers_arc = Arc::new(consumers);
                    let handle =
                        tokio::spawn(nws_dispatcher_task(rx, consumers_arc));
                    tasks.push(handle);
                    info!("external_feeds: NWS dispatcher started");
                } else {
                    info!("external_feeds: NWS configured but no subscribers; skipping");
                }
            }
        }

        if let Some(consumers) = subscribers.get("polymarket") {
            if !consumers.is_empty() {
                let client = PolyClient::new()
                    .map_err(|e| anyhow::anyhow!("poly client: {e}"))?;
                let connection = client.connect();
                let (cmd_tx, cmd_rx) = mpsc::channel::<PolyFeedCommand>(64);
                let consumers_arc = Arc::new(consumers.clone());
                let handle = tokio::spawn(poly_dispatcher_task(
                    connection,
                    cmd_rx,
                    consumers_arc,
                ));
                tasks.push(handle);
                poly_tx = Some(cmd_tx);
                info!("external_feeds: Polymarket dispatcher started");
            }
        }

        Ok(Self { tasks, poly_tx })
    }

    pub async fn shutdown(self, grace: Duration) {
        for h in self.tasks {
            h.abort();
            let _ = tokio::time::timeout(grace, h).await;
        }
    }
}

/// Pump NwsAlerts from the ext-feeds receiver, translate to the
/// engine-side payload shape, and fan out to subscribers.
async fn nws_dispatcher_task(
    mut rx: mpsc::Receiver<NwsAlert>,
    consumers: Arc<Vec<(StrategyId, mpsc::Sender<Event>)>>,
) {
    while let Some(alert) = rx.recv().await {
        let payload = nws_alert_to_payload(alert);
        let ev = Event::External(ExternalEvent::NwsAlert(payload));
        for (strategy, tx) in consumers.iter() {
            // Use try_send so a slow strategy doesn't backpressure
            // the dispatcher (alerts are fan-out; one strategy's
            // queue full shouldn't stall the others).
            if let Err(e) = tx.try_send(ev.clone()) {
                warn!(
                    strategy = strategy.0,
                    error = %e,
                    "external_feeds: nws fan-out failed (queue full or closed)"
                );
            }
        }
    }
    info!("external_feeds: NWS receiver closed; dispatcher exiting");
}

/// Polymarket dispatcher loop. Owns the WS connection, multiplexes
/// `PolyFeedCommand`s and incoming Polymarket events. On each
/// `Book` or `PriceChange`, derives best-bid/best-ask and fans out
/// an `ExternalEvent::PolymarketBook` to every subscribed
/// supervisor.
async fn poly_dispatcher_task(
    mut connection: predigy_poly_md::Connection,
    mut cmd_rx: mpsc::Receiver<PolyFeedCommand>,
    consumers: Arc<Vec<(StrategyId, mpsc::Sender<Event>)>>,
) {
    use std::collections::BTreeSet;
    let mut saved_subs: BTreeSet<String> = BTreeSet::new();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    info!("external_feeds: poly cmd channel closed");
                    return;
                };
                match cmd {
                    PolyFeedCommand::AddAssets(assets) => {
                        let new: Vec<String> = assets
                            .into_iter()
                            .filter(|a| saved_subs.insert(a.clone()))
                            .collect();
                        if new.is_empty() {
                            continue;
                        }
                        if let Err(e) = connection.subscribe(&new).await {
                            // Roll back so a retry has a clean shot.
                            for a in &new {
                                saved_subs.remove(a);
                            }
                            warn!(
                                n = new.len(),
                                error = %e,
                                "external_feeds: poly subscribe failed"
                            );
                        } else {
                            info!(
                                n_new = new.len(),
                                n_total = saved_subs.len(),
                                "external_feeds: poly subscribed to additional assets"
                            );
                        }
                    }
                    PolyFeedCommand::PruneAssets(assets) => {
                        for a in assets {
                            saved_subs.remove(&a);
                        }
                    }
                }
            }
            ev = connection.next_event() => {
                let Some(ev) = ev else {
                    warn!("external_feeds: poly connection closed");
                    return;
                };
                handle_poly_event(ev, &consumers).await;
            }
        }
    }
}

async fn handle_poly_event(
    ev: PolyEvent,
    consumers: &Arc<Vec<(StrategyId, mpsc::Sender<Event>)>>,
) {
    let payload = match ev {
        PolyEvent::Book(b) => {
            // Derive best bid/best ask from the L2 levels. Polymarket
            // emits levels in price order (highest bid / lowest ask
            // first per the WS docs), but we don't depend on that —
            // we max/min explicitly.
            let best_bid = b
                .bids
                .iter()
                .filter_map(|l| l.price.parse::<f64>().ok())
                .fold(None, |acc, p| {
                    Some(acc.map_or(p, |a: f64| a.max(p)))
                });
            let best_ask = b
                .asks
                .iter()
                .filter_map(|l| l.price.parse::<f64>().ok())
                .fold(None, |acc, p| {
                    Some(acc.map_or(p, |a: f64| a.min(p)))
                });
            ExternalEvent::PolymarketBook {
                asset_id: b.asset_id,
                best_bid,
                best_ask,
            }
        }
        PolyEvent::PriceChange(p) => {
            // PriceChange may carry multiple per-asset entries; emit
            // one ExternalEvent per asset so the strategy's update_poly
            // map gets the latest best_bid/best_ask for each.
            for ch in p.price_changes {
                let payload = ExternalEvent::PolymarketBook {
                    asset_id: ch.asset_id.clone(),
                    best_bid: ch.best_bid.as_deref().and_then(|s| s.parse::<f64>().ok()),
                    best_ask: ch.best_ask.as_deref().and_then(|s| s.parse::<f64>().ok()),
                };
                fan_out_external(consumers, payload);
            }
            return;
        }
        PolyEvent::LastTradePrice(_) | PolyEvent::TickSizeChange(_) => return,
        PolyEvent::Disconnected { attempt, reason } => {
            warn!(attempt, reason, "external_feeds: poly disconnected");
            return;
        }
        PolyEvent::Reconnected => {
            info!("external_feeds: poly reconnected");
            return;
        }
        PolyEvent::Malformed { error, raw } => {
            warn!(
                error,
                raw_excerpt = raw.chars().take(120).collect::<String>().as_str(),
                "external_feeds: poly malformed frame"
            );
            return;
        }
    };
    fan_out_external(consumers, payload);
}

fn fan_out_external(
    consumers: &Arc<Vec<(StrategyId, mpsc::Sender<Event>)>>,
    payload: ExternalEvent,
) {
    let ev = Event::External(payload);
    for (strategy, tx) in consumers.iter() {
        if let Err(e) = tx.try_send(ev.clone()) {
            warn!(
                strategy = strategy.0,
                error = %e,
                "external_feeds: poly fan-out failed"
            );
        }
    }
}

fn nws_alert_to_payload(a: NwsAlert) -> NwsAlertPayload {
    NwsAlertPayload {
        id: a.id,
        event_type: a.event_type,
        severity: a.severity,
        urgency: a.urgency,
        area_desc: a.area_desc,
        states: a.states,
        effective: a.effective,
        onset: a.onset,
        expires: a.expires,
        headline: a.headline,
    }
}

/// Pull `NwsConfig` from env vars. Returns `None` if either the
/// user-agent or the seen-path-or-states isn't set, signalling
/// that NWS shouldn't be spawned this engine boot.
///
/// Required:
/// - `PREDIGY_NWS_USER_AGENT` — NWS's required identifying header.
///
/// Optional:
/// - `PREDIGY_NWS_STATES` — comma-separated 2-letter state codes
///   (default: empty, meaning all states — heavy traffic).
/// - `PREDIGY_NWS_POLL_MS` — poll interval, floored to
///   [`MIN_POLL_INTERVAL`].
/// - `PREDIGY_NWS_SEEN_PATH` — file path for persisted seen-ids.
pub fn nws_config_from_env() -> Option<NwsConfig> {
    let user_agent = std::env::var("PREDIGY_NWS_USER_AGENT").ok()?;
    if user_agent.trim().is_empty() {
        return None;
    }
    let states: Vec<String> = std::env::var("PREDIGY_NWS_STATES")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_uppercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let poll_interval = std::env::var("PREDIGY_NWS_POLL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(Duration::from_secs(30), Duration::from_millis);
    let seen_path = std::env::var("PREDIGY_NWS_SEEN_PATH")
        .ok()
        .map(PathBuf::from);
    Some(NwsConfig {
        states,
        user_agent,
        poll_interval,
        seen_path,
    })
}

/// Build the subscribers map — for each known feed id, gather the
/// (strategy, event_tx) pairs for supervisors that opted in via
/// `Strategy::external_subscriptions`.
pub fn build_subscriber_map(
    pairs: Vec<(StrategyId, &'static str, mpsc::Sender<Event>)>,
) -> HashMap<&'static str, Vec<(StrategyId, mpsc::Sender<Event>)>> {
    let mut by_feed: HashMap<&'static str, Vec<(StrategyId, mpsc::Sender<Event>)>> = HashMap::new();
    for (sid, feed, tx) in pairs {
        by_feed.entry(feed).or_default().push((sid, tx));
    }
    by_feed
}

/// Tiny helper used by main.rs to translate a load error into the
/// engine's `Result` type.
pub fn require_nws_or_log(cfg: Option<NwsConfig>) -> Result<NwsConfig> {
    cfg.with_context(|| {
        "PREDIGY_NWS_USER_AGENT not set; NWS-dependent strategies (latency) won't fire"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nws_alert_to_payload_round_trips_fields() {
        let a = NwsAlert {
            id: "urn:oid:1.2.3".into(),
            event_type: "Tornado Warning".into(),
            severity: "Severe".into(),
            urgency: "Immediate".into(),
            area_desc: "Travis, TX".into(),
            states: vec!["TX".into()],
            effective: Some("2026-05-07T12:00:00Z".into()),
            onset: Some("2026-05-07T12:01:00Z".into()),
            expires: Some("2026-05-07T13:00:00Z".into()),
            headline: Some("test".into()),
        };
        let p = nws_alert_to_payload(a);
        assert_eq!(p.id, "urn:oid:1.2.3");
        assert_eq!(p.event_type, "Tornado Warning");
        assert_eq!(p.severity, "Severe");
        assert_eq!(p.states, vec!["TX".to_string()]);
        assert_eq!(p.headline.as_deref(), Some("test"));
    }

    #[test]
    fn build_subscriber_map_groups_by_feed() {
        let (tx_a, _rx_a) = mpsc::channel::<Event>(1);
        let (tx_b, _rx_b) = mpsc::channel::<Event>(1);
        let pairs = vec![
            (StrategyId("a"), "nws_alerts", tx_a),
            (StrategyId("b"), "nws_alerts", tx_b),
        ];
        let m = build_subscriber_map(pairs);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("nws_alerts").map(Vec::len), Some(2));
    }
}
