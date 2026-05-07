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
    discovery_service::DiscoveryService,
    exec_data::{ExecDataConfig, ExecDataConsumer},
    market_data::{MarketDataRouter, RouterConfig},
    oms_db::DbBackedOms,
    registry::StrategyRegistry,
    supervisor::{RestartPolicy, Supervisor},
    venue_rest::{VenueRest, VenueRestConfig},
};
use predigy_engine_core::discovery::DiscoverySubscription;
use predigy_engine_core::oms::KillSwitchView;
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use predigy_engine_core::Db;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_strategy_settlement::{SettlementConfig, SettlementStrategy};
use predigy_strategy_stat::{StatConfig, StatStrategy};
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
    let config = EngineConfig::from_env()
        .context("loading engine config from env")?;
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
    let mut supervisors: Vec<Supervisor> = Vec::new();
    let mut discovery_subs: HashMap<StrategyId, Vec<DiscoverySubscription>> = HashMap::new();
    for (id, _) in registry.instantiate_all().await {
        let factory = strategy_factory(id);
        let strategy = factory();
        let state = StrategyState::new(db.clone(), id.0);
        let markets = strategy.subscribed_markets(&state).await.unwrap_or_default();
        let subs = strategy.discovery_subscriptions();
        if !subs.is_empty() {
            discovery_subs.insert(id, subs);
        }
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
        info!(
            strategy = id.0,
            n_markets = markets.len(),
            n_discovery_subs = discovery_subs.get(&id).map_or(0, Vec::len),
            "predigy-engine: strategy supervisor spawned + registered with router"
        );
        supervisors.push(supervisor);
    }

    // 8a. Discovery service — periodic Kalshi REST scan that
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
    use predigy_strategy_settlement::STRATEGY_ID as SETTLEMENT_ID;
    use predigy_strategy_stat::STRATEGY_ID as STAT_ID;

    registry
        .register(StrategyHandle::new(STAT_ID, || {
            Box::new(StatStrategy::new(StatConfig::default())) as Box<dyn Strategy>
        }))
        .await;
    registry
        .register(StrategyHandle::new(SETTLEMENT_ID, || {
            Box::new(SettlementStrategy::new(SettlementConfig::default())) as Box<dyn Strategy>
        }))
        .await;
}

/// Per-strategy factory used by the supervisor for restart-on-
/// panic. Mirrors `register_strategies` above.
fn strategy_factory(
    id: predigy_engine_core::strategy::StrategyId,
) -> Box<dyn Fn() -> Box<dyn Strategy> + Send + Sync> {
    use predigy_strategy_settlement::STRATEGY_ID as SETTLEMENT_ID;
    use predigy_strategy_stat::STRATEGY_ID as STAT_ID;
    if id == STAT_ID {
        Box::new(|| Box::new(StatStrategy::new(StatConfig::default())) as Box<dyn Strategy>)
    } else if id == SETTLEMENT_ID {
        Box::new(|| {
            Box::new(SettlementStrategy::new(SettlementConfig::default())) as Box<dyn Strategy>
        })
    } else {
        Box::new(move || panic!("no factory wired for strategy {id:?}"))
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
                db_armed,
                "kill-switch: ARMED (engine refusing new entries)"
            );
        } else {
            view.clear();
            info!("kill-switch: cleared");
        }
    }
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("install SIGINT handler");
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
