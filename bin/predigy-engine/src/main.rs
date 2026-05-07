// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `predigy-engine` binary entrypoint.
//!
//! Boot sequence:
//!
//! 1. Init structured tracing.
//! 2. Load `EngineConfig` from env (CLI flags override).
//! 3. Connect to Postgres; run pending migrations.
//! 4. Build OMS + kill-switch view.
//! 5. Spawn the kill-switch watcher (DB + file fallback).
//! 6. Build the strategy registry.
//! 7. Per registered strategy, spawn a `Supervisor`.
//! 8. Connect to Kalshi (REST + WS today; FIX in Phase 4).
//! 9. Spawn the market-data router (WS → strategy event channels).
//! 10. Spawn the reconciliation loop.
//! 11. Wait on shutdown signal (SIGINT/SIGTERM).
//! 12. Drain — close strategies in dependency order, flush
//!     pending OMS writes, close DB pool.
//!
//! At Phase 2 (this commit) the engine boots cleanly with the
//! supervisor scaffolding, but no strategies are registered yet
//! — they land in Phase 3+. The dashboard moves to read the DB
//! in this same phase (separate change).

use anyhow::{Context as _, Result};
use predigy_engine::{
    config::EngineConfig,
    cross_strategy_bus::{self, CrossStrategyBus},
    discovery_service::DiscoveryService,
    exec_data::{ExecDataConfig, ExecDataConsumer},
    external_feeds::{ExternalFeeds, build_subscriber_map, nws_config_from_env},
    market_data::{MarketDataRouter, RouterConfig},
    oms_db::DbBackedOms,
    pair_file_service::{PairFileConfig, PairFileService, pair_file_from_env},
    registry::StrategyRegistry,
    self_subscribe::SelfSubscribeDispatcher,
    supervisor::{RestartPolicy, Supervisor},
    venue_rest::{VenueRest, VenueRestConfig},
};
use predigy_engine_core::Db;
use predigy_engine_core::discovery::DiscoverySubscription;
use predigy_engine_core::oms::KillSwitchView;
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_strategy_cross_arb::{CrossArbConfig, CrossArbStrategy};
use predigy_strategy_latency::LatencyStrategy;
use predigy_strategy_settlement::{SettlementConfig, SettlementStrategy};
use predigy_strategy_stat::{StatConfig, StatStrategy};
use predigy_strategy_wx_stat::{WxStatConfig, WxStatStrategy, rule_file_from_env as wx_stat_rule_file_from_env};
use predigy_strategy_internal_arb::{
    InternalArbConfig, InternalArbStrategy,
    config_file_from_env as internal_arb_config_from_env,
};
use predigy_strategy_implication_arb::{
    ImplicationArbConfig, ImplicationArbStrategy,
    config_file_from_env as implication_arb_config_from_env,
};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let config = EngineConfig::from_env().context("loading engine config from env")?;
    info!(
        database_url = %config.database_url,
        kalshi_endpoint = ?config.kalshi_rest_endpoint,
        http_bind = %config.http_bind,
        "predigy-engine: booting"
    );

    // 1. Postgres pool with retry. The DB MUST be reachable
    //    before we accept any other side effects.
    let pool = connect_with_retry(&config.database_url).await?;
    info!("predigy-engine: connected to postgres");

    // 2. Migrations on startup. The committed migrations live
    //    under `migrations/` at the workspace root; sqlx::migrate!
    //    embeds them at compile time.
    if config.auto_migrate {
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .context("running pending sqlx migrations")?;
        info!("predigy-engine: migrations applied");
    }

    // 3. Kill-switch view shared across the OMS + watcher.
    let kill_switch = Arc::new(KillSwitchView::new());

    // Initial sync from the DB / file fallback.
    sync_kill_switch(&pool, &config.kill_switch_file, &kill_switch).await;

    // 4. Build the OMS at the configured mode (Shadow by default;
    //    Live activates the venue submitter).
    let oms: Arc<dyn predigy_engine_core::oms::Oms> = Arc::new(DbBackedOms::new_with_mode(
        pool.clone(),
        config.default_risk_caps.clone(),
        kill_switch.clone(),
        config.engine_mode,
    ));
    info!(mode = ?config.engine_mode, "predigy-engine: oms ready");

    // 5. Spawn the kill-switch watcher. Polls the DB +
    //    file-based fallback every 5s.
    let watcher_handle = {
        let pool = pool.clone();
        let file = config.kill_switch_file.clone();
        let ks = kill_switch.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                sync_kill_switch(&pool, &file, &ks).await;
            }
        })
    };

    // 6. Strategy registry — Phase 3.2 ships StatStrategy as the
    //    first registered module. More strategies land in Phase 5.
    let registry = StrategyRegistry::new();
    register_strategies(&registry).await;
    info!(
        n_strategies = registry.iter_ids().await.len(),
        "predigy-engine: strategy registry built"
    );

    // 7. Market-data router. Connects to Kalshi WS, fans out
    //    book updates to subscribed strategy supervisors.
    let pem = std::fs::read_to_string(&config.kalshi_pem_path)
        .with_context(|| format!("read PEM at {}", config.kalshi_pem_path.display()))?;
    let router_cfg = RouterConfig {
        kalshi_key_id: config.kalshi_key_id.clone(),
        kalshi_pem: pem.clone(),
        rest_endpoint: config.kalshi_rest_endpoint.clone(),
        ws_endpoint: config.kalshi_ws_endpoint.clone(),
    };
    let router = MarketDataRouter::connect(router_cfg).await?;
    info!("predigy-engine: market-data router connected");

    // 7a. Exec-data consumer — dedicated WS connection to the
    //     authed `fill` + `market_positions` channels. Pushes
    //     fills into the OMS at <50ms median (vs ~500ms for the
    //     legacy REST poller). Same venue connection style as the
    //     market-data router; separate task for clean state.
    let exec_data = ExecDataConsumer::connect(
        ExecDataConfig {
            kalshi_key_id: config.kalshi_key_id.clone(),
            kalshi_pem: pem.clone(),
            ws_endpoint: config.kalshi_ws_endpoint.clone(),
        },
        pool.clone(),
        oms.clone(),
    )
    .await?;
    info!("predigy-engine: exec-data consumer connected");

    // 7b. REST venue submitter — polls `intents WHERE
    //     status='submitted'` and pushes them to Kalshi. Pairs
    //     with the WS exec-data consumer to close the order
    //     lifecycle (submit via REST, fills via WS push). Runs
    //     in both Shadow and Live mode; in Shadow the queue is
    //     always empty (intents land at status='shadow').
    let venue_rest = VenueRest::start(
        VenueRestConfig {
            kalshi_key_id: config.kalshi_key_id.clone(),
            kalshi_pem: pem.clone(),
            rest_endpoint: config.kalshi_rest_endpoint.clone(),
            poll_interval: config.venue_rest_poll_interval,
        },
        pool.clone(),
    )
    .await?;
    info!(
        poll_ms = config.venue_rest_poll_interval.as_millis() as u64,
        "predigy-engine: venue-rest submitter started"
    );

    // 8. Spawn one supervisor per strategy. Each supervisor owns
    //    its strategy instance + its event channel + its
    //    StrategyState (DB handle). The router pushes
    //    Event::BookUpdate into the supervisor's queue. We also
    //    capture each strategy's declared discovery subscriptions
    //    so the discovery service can be started below.
    let db = Db::connect(&config.database_url).await?;

    // Phase 6 — cross-strategy bus producer/consumer pair.
    // Channel created BEFORE supervisor spawn so each
    // StrategyState can be wired with the producer-side tx.
    // Dispatcher starts AFTER all supervisors are up so the
    // subscriber map is final.
    let (xstrat_tx, xstrat_rx) = CrossStrategyBus::channel();

    // Audit A5 — self-subscribe channel. Strategies that want
    // dynamic per-position book subscriptions (latency) call
    // state.subscribe_to_markets which sends here; the
    // dispatcher (started after supervisor spawn) routes to
    // the router.
    let (self_sub_tx, self_sub_rx) = SelfSubscribeDispatcher::channel();

    let mut supervisors: Vec<Supervisor> = Vec::new();
    let mut discovery_subs: HashMap<StrategyId, Vec<DiscoverySubscription>> = HashMap::new();
    let mut external_subscribers: Vec<(
        StrategyId,
        &'static str,
        tokio::sync::mpsc::Sender<predigy_engine_core::events::Event>,
    )> = Vec::new();
    let mut xstrat_subscribers: Vec<(
        StrategyId,
        &'static str,
        tokio::sync::mpsc::Sender<predigy_engine_core::events::Event>,
    )> = Vec::new();
    for (id, _) in registry.instantiate_all().await {
        let factory = strategy_factory(id);
        let strategy = factory();
        let state = StrategyState::new(db.clone(), id.0)
            .with_cross_strategy_tx(xstrat_tx.clone())
            .with_self_subscribe_tx(self_sub_tx.clone());
        let markets = strategy
            .subscribed_markets(&state)
            .await
            .unwrap_or_default();
        let subs = strategy.discovery_subscriptions();
        if !subs.is_empty() {
            discovery_subs.insert(id, subs);
        }
        let ext_subs = strategy.external_subscriptions();
        let xstrat_subs = strategy.cross_strategy_subscriptions();
        let supervisor = Supervisor::spawn(
            id,
            Arc::from(factory),
            oms.clone(),
            state,
            RestartPolicy::default(),
        );
        router
            .register_strategy(id, &markets, supervisor.event_tx.clone())
            .await;
        for feed in ext_subs {
            external_subscribers.push((id, feed, supervisor.event_tx.clone()));
        }
        for topic in xstrat_subs {
            xstrat_subscribers.push((id, topic, supervisor.event_tx.clone()));
        }
        info!(
            strategy = id.0,
            n_markets = markets.len(),
            n_discovery_subs = discovery_subs.get(&id).map_or(0, Vec::len),
            "predigy-engine: strategy supervisor spawned + registered with router"
        );
        supervisors.push(supervisor);
    }

    // 8c. Cross-strategy bus dispatcher. Spawns only when at
    //     least one strategy declared a cross-strategy
    //     subscription. Drop the engine's local tx clone — every
    //     supervisor's StrategyState already holds its own clone,
    //     so the bus's mpsc stays alive as long as any producer
    //     is alive. When all producers exit, the rx returns
    //     `None` and the bus task cleanly terminates.
    drop(xstrat_tx);
    let by_topic = cross_strategy_bus::build_subscriber_map(xstrat_subscribers);
    let xstrat_bus = CrossStrategyBus::start_dispatching(xstrat_rx, by_topic);

    // Audit A5 — self-subscribe dispatcher. Drop the engine's
    // local tx; supervisors hold their own clones via
    // StrategyState. Map each strategy id to its supervisor
    // event_tx so the dispatcher can route AddTickers commands
    // back to the right queue.
    drop(self_sub_tx);
    let strategy_event_txs: HashMap<StrategyId, tokio::sync::mpsc::Sender<predigy_engine_core::events::Event>> =
        supervisors
            .iter()
            .map(|s| (s.id, s.event_tx.clone()))
            .collect();
    let self_sub_dispatcher =
        SelfSubscribeDispatcher::start(self_sub_rx, router.command_tx(), strategy_event_txs);

    // 8a. External-feed dispatcher — single NWS connection
    //     fanned out to every supervisor that opted in via
    //     `external_subscriptions()`. Skipped at boot if no
    //     supervisor opted in OR if `PREDIGY_NWS_USER_AGENT`
    //     isn't set (NWS requires identifying contact info; we
    //     refuse to spawn it without).
    let external_feeds = if external_subscribers.is_empty() {
        info!("external_feeds: no subscribers; skipping");
        None
    } else {
        let nws = nws_config_from_env();
        if nws.is_none() {
            warn!(
                "external_feeds: PREDIGY_NWS_USER_AGENT not set — \
                 NWS-dependent strategies (latency) won't fire this run"
            );
            None
        } else {
            let subs_map = build_subscriber_map(external_subscribers);
            let svc = ExternalFeeds::start(nws, &subs_map)?;
            Some(svc)
        }
    };

    // 8b. Pair-file service — watches the cross-arb-curator's
    //     pair file for changes and emits Event::PairUpdate to
    //     the cross-arb supervisor. Auto-registers added Kalshi
    //     tickers with the router AND added Polymarket assets
    //     with the external-feeds dispatcher. Skipped if either
    //     PREDIGY_CROSS_ARB_PAIR_FILE isn't set OR there's no
    //     cross-arb supervisor running OR the polymarket feed
    //     wasn't started.
    let pair_file = if let Some(path) = pair_file_from_env() {
        let cross_arb_sup = supervisors
            .iter()
            .find(|s| s.id == predigy_strategy_cross_arb::STRATEGY_ID);
        match (
            cross_arb_sup,
            external_feeds.as_ref().and_then(|f| f.poly_tx.clone()),
        ) {
            (Some(sup), Some(poly_tx)) => {
                let cfg = PairFileConfig {
                    path,
                    poll_interval: Duration::from_secs(30),
                    strategy: sup.id,
                };
                let svc =
                    PairFileService::start(cfg, router.command_tx(), poly_tx, sup.event_tx.clone());
                info!("predigy-engine: pair-file service started");
                Some(svc)
            }
            (None, _) => {
                warn!(
                    "PREDIGY_CROSS_ARB_PAIR_FILE set but cross-arb supervisor not running; \
                     pair-file watcher disabled"
                );
                None
            }
            (_, None) => {
                warn!(
                    "PREDIGY_CROSS_ARB_PAIR_FILE set but Polymarket feed not started \
                     (no supervisors declared 'polymarket' subscription); \
                     pair-file watcher disabled"
                );
                None
            }
        }
    } else {
        None
    };

    // 8c. Discovery service — periodic Kalshi REST scan that
    //     auto-registers tickers with the router and pushes
    //     Event::DiscoveryDelta into strategies. Spawned only if
    //     at least one supervised strategy declared a discovery
    //     subscription; settlement is the canonical case (its
    //     market set rotates every few hours as games come into
    //     scope).
    let discovery_service = if discovery_subs.is_empty() {
        None
    } else {
        let signer = Signer::from_pem(&config.kalshi_key_id, &pem)
            .map_err(|e| anyhow::anyhow!("discovery signer: {e}"))?;
        let rest_client = if let Some(base) = config.kalshi_rest_endpoint.as_deref() {
            RestClient::with_base(base, Some(signer))
        } else {
            RestClient::authed(signer)
        }
        .map_err(|e| anyhow::anyhow!("discovery rest client: {e}"))?;
        let rest_arc = Arc::new(rest_client);
        let supervisor_refs: Vec<&Supervisor> = supervisors.iter().collect();
        let svc = DiscoveryService::start(
            rest_arc,
            router.command_tx(),
            &supervisor_refs,
            &discovery_subs,
        );
        Some(svc)
    };

    // 9. Wait on shutdown signal.
    info!(
        n_supervisors = supervisors.len(),
        "predigy-engine: ready (running); awaiting shutdown signal"
    );
    wait_for_shutdown().await;

    // 10. Drain.
    info!("predigy-engine: shutdown initiated; draining");
    watcher_handle.abort();
    if let Some(svc) = discovery_service {
        svc.shutdown(config.shutdown_grace).await;
    }
    if let Some(bus) = xstrat_bus {
        bus.shutdown(config.shutdown_grace).await;
    }
    self_sub_dispatcher.shutdown(config.shutdown_grace).await;
    if let Some(svc) = pair_file {
        svc.shutdown(config.shutdown_grace).await;
    }
    if let Some(svc) = external_feeds {
        svc.shutdown(config.shutdown_grace).await;
    }
    for sup in supervisors {
        sup.shutdown(config.shutdown_grace).await;
    }
    venue_rest.shutdown(config.shutdown_grace).await;
    exec_data.shutdown(config.shutdown_grace).await;
    router.shutdown(config.shutdown_grace).await;
    pool.close().await;
    info!("predigy-engine: shutdown complete");
    Ok(())
}

/// Build the in-process strategy registry. Each strategy lives
/// in its own crate; this is the single place the engine wires
/// them in. Adding a new strategy = add it here + add the dep.
async fn register_strategies(registry: &StrategyRegistry) {
    use predigy_engine::registry::StrategyHandle;
    use predigy_strategy_latency::STRATEGY_ID as LATENCY_ID;
    use predigy_strategy_settlement::STRATEGY_ID as SETTLEMENT_ID;
    use predigy_strategy_stat::STRATEGY_ID as STAT_ID;

    registry
        .register(StrategyHandle::new(STAT_ID, || {
            Box::new(StatStrategy::new(StatConfig::from_env())) as Box<dyn Strategy>
        }))
        .await;
    registry
        .register(StrategyHandle::new(SETTLEMENT_ID, || {
            Box::new(SettlementStrategy::new(SettlementConfig::from_env())) as Box<dyn Strategy>
        }))
        .await;
    // Latency only registers if a rules file is configured —
    // without rules the strategy is a no-op and we'd rather not
    // claim the supervisor slot. PREDIGY_LATENCY_RULE_FILE points
    // at a JSON file; bad path / bad JSON warns + skips.
    if let Some(path) = latency_rules_path() {
        registry
            .register(StrategyHandle::new(LATENCY_ID, move || {
                build_latency_strategy(&path)
            }))
            .await;
    }

    // Cross-arb only registers if a pair-file path is configured.
    // The strategy is pure pair-file driven; without pairs
    // there's no reason to claim the supervisor slot.
    use predigy_strategy_cross_arb::STRATEGY_ID as CROSS_ARB_ID;
    if pair_file_from_env().is_some() {
        registry
            .register(StrategyHandle::new(CROSS_ARB_ID, || {
                Box::new(CrossArbStrategy::new(CrossArbConfig::from_env())) as Box<dyn Strategy>
            }))
            .await;
    }

    // Wx-stat (S2): consumes the wx-stat-curator JSON output
    // directly via mtime-poll. Skip if the file path env var is
    // unset — the strategy has nothing to act on without it.
    use predigy_strategy_wx_stat::STRATEGY_ID as WX_STAT_ID;
    if let Some(path) = wx_stat_rule_file_from_env() {
        registry
            .register(StrategyHandle::new(WX_STAT_ID, move || {
                Box::new(WxStatStrategy::new(WxStatConfig::from_env(path.clone()))) as Box<dyn Strategy>
            }))
            .await;
    }

    // Internal-arb (S3): mutually-exclusive Kalshi event-family
    // sum-to-1 arbitrage. Skip if the family-config file env var
    // is unset.
    use predigy_strategy_internal_arb::STRATEGY_ID as INTERNAL_ARB_ID;
    if let Some(path) = internal_arb_config_from_env() {
        registry
            .register(StrategyHandle::new(INTERNAL_ARB_ID, move || {
                Box::new(InternalArbStrategy::new(InternalArbConfig::from_env(path.clone()))) as Box<dyn Strategy>
            }))
            .await;
    }

    // Implication-arb (S9): two-leg implication-pair arbitrage
    // (child ⊂ parent). Skip if the pair-config env var is unset.
    use predigy_strategy_implication_arb::STRATEGY_ID as IMPL_ARB_ID;
    if let Some(path) = implication_arb_config_from_env() {
        registry
            .register(StrategyHandle::new(IMPL_ARB_ID, move || {
                Box::new(ImplicationArbStrategy::new(
                    ImplicationArbConfig::from_env(path.clone()),
                )) as Box<dyn Strategy>
            }))
            .await;
    }
}

/// Per-strategy factory used by the supervisor for restart-on-
/// panic. Mirrors `register_strategies` above.
fn strategy_factory(
    id: predigy_engine_core::strategy::StrategyId,
) -> Box<dyn Fn() -> Box<dyn Strategy> + Send + Sync> {
    use predigy_strategy_cross_arb::STRATEGY_ID as CROSS_ARB_ID;
    use predigy_strategy_latency::STRATEGY_ID as LATENCY_ID;
    use predigy_strategy_settlement::STRATEGY_ID as SETTLEMENT_ID;
    use predigy_strategy_stat::STRATEGY_ID as STAT_ID;
    use predigy_strategy_wx_stat::STRATEGY_ID as WX_STAT_ID;
    if id == STAT_ID {
        Box::new(|| Box::new(StatStrategy::new(StatConfig::from_env())) as Box<dyn Strategy>)
    } else if id == SETTLEMENT_ID {
        Box::new(|| {
            Box::new(SettlementStrategy::new(SettlementConfig::from_env())) as Box<dyn Strategy>
        })
    } else if id == LATENCY_ID {
        let path = latency_rules_path()
            .expect("LATENCY_ID registered without a rule-file path; engine startup invariant");
        Box::new(move || build_latency_strategy(&path))
    } else if id == CROSS_ARB_ID {
        Box::new(|| {
            Box::new(CrossArbStrategy::new(CrossArbConfig::from_env())) as Box<dyn Strategy>
        })
    } else if id == WX_STAT_ID {
        let path = wx_stat_rule_file_from_env()
            .expect("WX_STAT_ID registered without a rule-file path; engine startup invariant");
        Box::new(move || {
            Box::new(WxStatStrategy::new(WxStatConfig::from_env(path.clone()))) as Box<dyn Strategy>
        })
    } else if id == predigy_strategy_internal_arb::STRATEGY_ID {
        let path = internal_arb_config_from_env().expect(
            "internal-arb registered without a config-file path; engine startup invariant",
        );
        Box::new(move || {
            Box::new(InternalArbStrategy::new(InternalArbConfig::from_env(
                path.clone(),
            ))) as Box<dyn Strategy>
        })
    } else if id == predigy_strategy_implication_arb::STRATEGY_ID {
        let path = implication_arb_config_from_env().expect(
            "implication-arb registered without a config-file path; engine startup invariant",
        );
        Box::new(move || {
            Box::new(ImplicationArbStrategy::new(
                ImplicationArbConfig::from_env(path.clone()),
            )) as Box<dyn Strategy>
        })
    } else {
        Box::new(move || panic!("no factory wired for strategy {id:?}"))
    }
}

fn latency_rules_path() -> Option<std::path::PathBuf> {
    std::env::var("PREDIGY_LATENCY_RULE_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
}

fn build_latency_strategy(path: &std::path::Path) -> Box<dyn Strategy> {
    match LatencyStrategy::from_json_file(path) {
        Ok(s) => {
            info!(
                rule_file = %path.display(),
                n_rules = s.rule_count(),
                "latency: rules loaded"
            );
            Box::new(s) as Box<dyn Strategy>
        }
        Err(e) => {
            warn!(
                rule_file = %path.display(),
                error = %e,
                "latency: rule file unreadable; running with empty rule set"
            );
            Box::new(LatencyStrategy::with_config(
                predigy_strategy_latency::LatencyConfig::from_env(),
                Vec::new(),
            )) as Box<dyn Strategy>
        }
    }
}

async fn connect_with_retry(url: &str) -> Result<sqlx::PgPool> {
    let mut backoff = Duration::from_secs(1);
    let max = Duration::from_secs(30);
    for attempt in 1..=10 {
        match PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
        {
            Ok(p) => return Ok(p),
            Err(e) if attempt < 10 => {
                warn!(
                    attempt,
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "predigy-engine: postgres connect failed; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max);
            }
            Err(e) => {
                return Err(e).context("connecting to postgres after exhausting retries");
            }
        }
    }
    unreachable!()
}

async fn sync_kill_switch(
    pool: &sqlx::PgPool,
    fallback_file: &std::path::Path,
    view: &Arc<KillSwitchView>,
) {
    // File-based fallback. Predigy's convention (set by the
    // dashboard): the flag file always exists once anyone has
    // toggled it; arming is signalled by **non-empty contents**.
    // Disarming clears the file (truncates to zero bytes). The
    // legacy latency-trader and stat-trader both use this.
    let file_armed = match std::fs::metadata(fallback_file) {
        Ok(meta) => meta.len() > 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(?fallback_file, error = %e, "kill-switch: stat failed");
            false
        }
    };
    // DB scope global.
    let db_armed: bool = match sqlx::query_scalar::<_, bool>(
        "SELECT armed FROM kill_switches WHERE scope = 'global' LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(v)) => v,
        Ok(None) => false,
        Err(e) => {
            warn!(error = %e, "kill-switch: db query failed");
            false
        }
    };
    let armed = file_armed || db_armed;
    if armed != view.is_armed() {
        if armed {
            view.arm();
            warn!(
                file_armed,
                db_armed, "kill-switch: ARMED (engine refusing new entries)"
            );
        } else {
            view.clear();
            info!("kill-switch: cleared");
        }
    }

    // Audit I2 — per-strategy switches. Pull all non-global
    // rows; populate the view for each strategy id we know
    // about. Only the four trader strategies are relevant; the
    // OMS only consults `is_armed_for(strategy)` for those.
    type StatePair = (&'static str, bool);
    let known: &[&'static str] = &[
        predigy_strategy_stat::STRATEGY_ID.0,
        predigy_strategy_settlement::STRATEGY_ID.0,
        predigy_strategy_latency::STRATEGY_ID.0,
        predigy_strategy_cross_arb::STRATEGY_ID.0,
    ];
    match sqlx::query_as::<_, (String, bool)>(
        "SELECT scope, armed FROM kill_switches WHERE scope <> 'global'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            // Map scope -> armed for the strategies we know.
            let mut updates: Vec<StatePair> = Vec::new();
            for known_id in known {
                let armed = rows
                    .iter()
                    .find(|(scope, _)| scope == *known_id)
                    .map_or(false, |(_, a)| *a);
                updates.push((*known_id, armed));
            }
            view.set_strategy_states(&updates);
        }
        Err(e) => warn!(error = %e, "kill-switch: per-strategy db query failed"),
    }
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install SIGINT handler");
    };
    #[cfg(unix)]
    let term = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("shutdown: ctrl-c"),
        () = term => info!("shutdown: SIGTERM"),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
