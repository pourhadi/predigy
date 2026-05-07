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
use predigy_engine::{config::EngineConfig, oms_db::DbBackedOms, registry::StrategyRegistry};
use predigy_engine_core::oms::KillSwitchView;
use sqlx::postgres::PgPoolOptions;
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

    // 4. Build the OMS. Marked `_oms` until Phase 3 wires the
    //    first strategy that consumes it; the Arc ensures the
    //    instance lives for the rest of `main`.
    let _oms: Arc<dyn predigy_engine_core::oms::Oms> = Arc::new(DbBackedOms::new(
        pool.clone(),
        config.default_risk_caps.clone(),
        kill_switch.clone(),
    ));
    info!("predigy-engine: oms ready");

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

    // 6. Strategy registry. Empty for Phase 2 — first registration
    //    lands in Phase 3 with the stat-trader port.
    let registry = StrategyRegistry::new();
    info!(
        n_strategies = registry.iter_ids().await.len(),
        "predigy-engine: strategy registry built"
    );

    // 7. (Future) market-data router + Kalshi WS subscription.
    //    Lands when the first strategy registers.

    // 8. Wait on shutdown signal.
    info!("predigy-engine: ready (idle); awaiting shutdown signal");
    wait_for_shutdown().await;

    // 9. Drain.
    info!("predigy-engine: shutdown initiated; draining");
    watcher_handle.abort();
    pool.close().await;
    info!("predigy-engine: shutdown complete");
    Ok(())
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
