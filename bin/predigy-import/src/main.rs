// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `predigy-import`: read existing JSON state + rule files, mirror
//! into the Postgres database. Idempotent — safe to re-run.
//!
//! Used during the Phase-1 migration (see docs/ARCHITECTURE.md):
//! the engine refactor is built alongside the existing daemons,
//! and this tool keeps the DB in sync with the legacy JSON state
//! until the engine takes over the write path.
//!
//! ```text
//! predigy-import --database-url postgresql:///predigy \
//!                --config-dir   ~/.config/predigy
//! ```

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "predigy-import",
    about = "Mirror existing JSON state into Postgres."
)]
struct Args {
    /// Connection string. With peer auth on UNIX socket the
    /// canonical value is `postgresql:///predigy` (no host).
    #[arg(long, default_value = "postgresql:///predigy")]
    database_url: String,

    /// Where the legacy JSON state files live.
    #[arg(long, default_value = "/Users/dan/.config/predigy")]
    config_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&args.database_url)
        .await
        .with_context(|| format!("connect to {}", args.database_url))?;
    info!(url = %args.database_url, "connected to predigy DB");

    // ---- Markets — bootstrapped from anywhere a ticker appears ----
    let mut tickers: HashSet<String> = HashSet::new();
    collect_oms_tickers(&args.config_dir, &mut tickers).await?;
    collect_rule_tickers(&args.config_dir, &mut tickers).await?;
    info!(n = tickers.len(), "discovered tickers across JSON state");

    // Insert minimal market rows so FK references in intents /
    // fills / positions work. Subsequent runs upsert (no-op if
    // row already exists; bumps last_updated_at).
    let mut markets_inserted = 0u64;
    for ticker in &tickers {
        let n = sqlx::query(
            "INSERT INTO markets (ticker, venue, market_type, last_updated_at)
             VALUES ($1, 'kalshi', 'binary', now())
             ON CONFLICT (ticker) DO UPDATE SET last_updated_at = now()",
        )
        .bind(ticker)
        .execute(&pool)
        .await?
        .rows_affected();
        markets_inserted += n;
    }
    info!(n = markets_inserted, "markets upserted");

    // ---- OMS state files: intents + fills + positions ----
    let oms_files = [
        ("latency", "oms-state.json"),
        ("stat", "oms-state-stat.json"),
        ("cross-arb", "oms-state-cross-arb.json"),
        ("settlement", "oms-state-settlement.json"),
    ];
    let mut intents_inserted = 0u64;
    for (strategy, file) in oms_files {
        let path = args.config_dir.join(file);
        let oms = match read_oms_state(&path).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                info!(?path, "oms state missing; skipping");
                continue;
            }
            Err(e) => {
                warn!(?path, error = %e, "oms state unreadable; skipping");
                continue;
            }
        };
        let n = oms.orders.len();
        info!(strategy, ?path, n, "importing oms orders");
        for tracked in oms.orders {
            insert_intent_from_tracked(&pool, strategy, &tracked).await?;
            intents_inserted += 1;
        }
    }
    info!(n = intents_inserted, "intents upserted");

    // ---- Rules: stat-rules.json, wx-rules.json ----
    // wx-stat-rules.json is consumed directly by the dedicated wx-stat
    // strategy in the consolidated engine. Importing it as `stat` would
    // double-fire weather exposure under two strategy IDs.
    let rule_files = [
        ("stat", "stat-rules.json", RuleFormat::StatRule),
        ("latency", "wx-rules.json", RuleFormat::LatencyRule),
    ];
    let mut rules_inserted = 0u64;
    for (strategy, file, fmt) in rule_files {
        let path = args.config_dir.join(file);
        let n = import_rules(&pool, strategy, &path, fmt).await?;
        if n > 0 {
            info!(strategy, ?path, n, "rules upserted");
        }
        rules_inserted += n;
    }
    let disabled_wx_stat =
        disable_rules_for_source(&pool, "stat", &args.config_dir.join("wx-stat-rules.json"))
            .await?;
    if disabled_wx_stat > 0 {
        info!(
            n = disabled_wx_stat,
            "disabled legacy imported wx-stat rules"
        );
    }
    info!(n = rules_inserted, "rules upserted total");

    // ---- Summary ----
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT COUNT(*) FROM markets),
           (SELECT COUNT(*) FROM intents),
           (SELECT COUNT(*) FROM rules),
           (SELECT COUNT(*) FROM positions)",
    )
    .fetch_one(&pool)
    .await?;
    println!(
        "DB now has: {} markets, {} intents, {} rules, {} positions",
        row.0, row.1, row.2, row.3
    );

    Ok(())
}

// ─── OMS state JSON shapes ──────────────────────────────────

#[derive(Debug, Deserialize)]
struct OmsState {
    #[serde(default)]
    orders: Vec<TrackedOrder>,
}

#[derive(Debug, Deserialize)]
struct TrackedOrder {
    cid: String,
    order: OrderEnvelope,
    state: String,
    #[serde(default)]
    cumulative_qty: i32,
    #[serde(default)]
    avg_fill_price_cents: Option<i32>,
    #[serde(default)]
    venue_order_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrderEnvelope {
    market: String,
    side: String,
    action: String,
    #[serde(default)]
    price: Option<i32>,
    qty: i32,
    order_type: String,
    tif: String,
}

async fn read_oms_state(path: &Path) -> Result<Option<OmsState>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn collect_oms_tickers(config_dir: &Path, out: &mut HashSet<String>) -> Result<()> {
    for file in [
        "oms-state.json",
        "oms-state-stat.json",
        "oms-state-cross-arb.json",
        "oms-state-settlement.json",
    ] {
        let path = config_dir.join(file);
        if let Some(state) = read_oms_state(&path).await? {
            for o in state.orders {
                out.insert(o.order.market);
            }
        }
    }
    Ok(())
}

async fn collect_rule_tickers(config_dir: &Path, out: &mut HashSet<String>) -> Result<()> {
    for file in ["stat-rules.json", "wx-stat-rules.json"] {
        let path = config_dir.join(file);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let parsed: Vec<StatRuleJson> =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        for r in parsed {
            out.insert(r.kalshi_market);
        }
    }
    // Latency rules wrap their target market differently.
    let path = config_dir.join("wx-rules.json");
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if let Ok(rules) = serde_json::from_slice::<Vec<LatencyRuleJson>>(&bytes) {
            for r in rules {
                if let Some(t) = r.target_market {
                    out.insert(t);
                }
            }
        }
    }
    Ok(())
}

// ─── Intent insertion ───────────────────────────────────────

async fn insert_intent_from_tracked(
    pool: &PgPool,
    strategy: &str,
    tracked: &TrackedOrder,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO intents
            (client_id, strategy, ticker, side, action, price_cents,
             qty, order_type, tif, status, cumulative_qty,
             avg_fill_price_cents, venue_order_id, submitted_at,
             last_updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now(), now())
         ON CONFLICT (client_id) DO UPDATE
         SET status = EXCLUDED.status,
             cumulative_qty = EXCLUDED.cumulative_qty,
             avg_fill_price_cents = EXCLUDED.avg_fill_price_cents,
             venue_order_id = EXCLUDED.venue_order_id,
             last_updated_at = now()",
    )
    .bind(&tracked.cid)
    .bind(strategy)
    .bind(&tracked.order.market)
    .bind(&tracked.order.side)
    .bind(&tracked.order.action)
    .bind(tracked.order.price)
    .bind(tracked.order.qty)
    .bind(&tracked.order.order_type)
    .bind(&tracked.order.tif)
    .bind(&tracked.state)
    .bind(tracked.cumulative_qty)
    .bind(tracked.avg_fill_price_cents)
    .bind(tracked.venue_order_id.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Rules ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum RuleFormat {
    /// `[{kalshi_market, model_p, side, min_edge_cents}, ...]`
    StatRule,
    /// LatencyRule shape — has alert-trigger metadata around the
    /// ticker.
    LatencyRule,
}

#[derive(Debug, Deserialize)]
struct StatRuleJson {
    kalshi_market: String,
    model_p: f64,
    side: String,
    #[serde(default = "default_edge")]
    min_edge_cents: i32,
}

fn default_edge() -> i32 {
    5
}

#[derive(Debug, Deserialize)]
struct LatencyRuleJson {
    /// LatencyRule's actual schema is richer (alert event type,
    /// state filter, etc.) — for the import we just want the
    /// target market for FK satisfaction; the strategy module
    /// will own the full rule shape post-port. Prefer
    /// `target_market` if present, fall back to `kalshi_market`.
    #[serde(default, alias = "kalshi_market")]
    target_market: Option<String>,
}

async fn import_rules(pool: &PgPool, strategy: &str, path: &Path, fmt: RuleFormat) -> Result<u64> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let n = match fmt {
        RuleFormat::StatRule => {
            let parsed: Vec<StatRuleJson> = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            let n = parsed.len() as u64;
            let active_tickers: Vec<String> =
                parsed.iter().map(|r| r.kalshi_market.clone()).collect();
            for r in &parsed {
                upsert_stat_rule(pool, strategy, r, path).await?;
            }
            disable_missing_stat_rules(pool, strategy, path, &active_tickers).await?;
            n
        }
        RuleFormat::LatencyRule => {
            // Latency rules are kept in a richer schema we don't
            // need to fully model in `rules` yet — the strategy
            // module port will supersede this. For now, we record
            // a placeholder rule per target ticker so downstream
            // queries see them.
            let parsed: Vec<LatencyRuleJson> = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            let mut n = 0u64;
            for r in parsed {
                if let Some(t) = r.target_market {
                    upsert_latency_placeholder(pool, strategy, &t, path).await?;
                    n += 1;
                }
            }
            n
        }
    };
    Ok(n)
}

async fn disable_missing_stat_rules(
    pool: &PgPool,
    strategy: &str,
    source_path: &Path,
    active_tickers: &[String],
) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE rules
            SET enabled = false,
                fitted_at = now()
          WHERE strategy = $1
            AND source = $2
            AND enabled = true
            AND NOT (ticker = ANY($3))",
    )
    .bind(strategy)
    .bind(source_label(source_path))
    .bind(active_tickers)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

async fn disable_rules_for_source(
    pool: &PgPool,
    strategy: &str,
    source_path: &Path,
) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE rules
            SET enabled = false,
                fitted_at = now()
          WHERE strategy = $1
            AND source = $2
            AND enabled = true",
    )
    .bind(strategy)
    .bind(source_label(source_path))
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

async fn upsert_stat_rule(
    pool: &PgPool,
    strategy: &str,
    rule: &StatRuleJson,
    source_path: &Path,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rules (strategy, ticker, side, model_p, min_edge_cents,
                            source, fitted_at, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, now(), true)
         ON CONFLICT (strategy, ticker) DO UPDATE
         SET side = EXCLUDED.side,
             model_p = EXCLUDED.model_p,
             min_edge_cents = EXCLUDED.min_edge_cents,
             source = EXCLUDED.source,
             fitted_at = now(),
             enabled = true",
    )
    .bind(strategy)
    .bind(&rule.kalshi_market)
    .bind(&rule.side)
    .bind(rule.model_p)
    .bind(rule.min_edge_cents)
    .bind(source_label(source_path))
    .execute(pool)
    .await?;
    Ok(())
}

fn source_label(source_path: &Path) -> String {
    format!("import:{}", source_path.display())
}

async fn upsert_latency_placeholder(
    pool: &PgPool,
    strategy: &str,
    ticker: &str,
    source_path: &Path,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rules (strategy, ticker, side, model_p, min_edge_cents,
                            source, fitted_at, enabled)
         VALUES ($1, $2, 'yes', 0.5, 5, $3, now(), false)
         ON CONFLICT (strategy, ticker) DO UPDATE
         SET source = EXCLUDED.source,
             fitted_at = now()",
    )
    .bind(strategy)
    .bind(ticker)
    .bind(format!("import-placeholder:{}", source_path.display()))
    .execute(pool)
    .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

// Suppress unused-import warning when we end up not needing
// chrono types directly in this file.
#[allow(dead_code)]
fn _chrono_alive(_: DateTime<Utc>) {}
