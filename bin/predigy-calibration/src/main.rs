// Probability calibration report generator for Predigy.
#![allow(clippy::doc_markdown)]

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use predigy_kalshi_rest::types::MarketDetail;
use predigy_kalshi_rest::{Client as KalshiClient, Signer};
use serde::Serialize;
use serde_json::json;
use sqlx::{Postgres, Row, postgres::PgPoolOptions};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "predigy-calibration",
    about = "Generate calibration/reliability reports from model_p snapshots and settled outcomes."
)]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,

    /// Optional Kalshi REST endpoint override.
    #[arg(long, env = "KALSHI_REST_ENDPOINT")]
    kalshi_rest_endpoint: Option<String>,

    /// Kalshi key id. Required only for authenticated venue-position reconciliation.
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: Option<String>,

    /// Path to Kalshi private key PEM. Required only for authenticated venue-position reconciliation.
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compute and store a reliability report for one strategy.
    Report {
        #[arg(long)]
        strategy: String,
        #[arg(long, default_value_t = 30)]
        window_days: i64,
        /// Print without inserting into calibration_reports.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Backfill missing settlement outcomes for predicted tickers via public market detail.
    SyncSettlements {
        #[arg(long)]
        strategy: Option<String>,
        #[arg(long, default_value_t = 90)]
        window_days: i64,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Close stale DB-open rows for settled markets where authenticated venue exposure is flat.
    ReconcileVenueFlat {
        #[arg(long)]
        strategy: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
        /// Actually mutate positions/settlements. Default is a read-only dry run.
        #[arg(long, default_value_t = false)]
        write: bool,
    },
    /// Placeholder for future stat shadow-rule writer.
    ShadowStat,
}

#[derive(Debug, Clone)]
struct PredictionOutcome {
    p: f64,
    outcome: f64,
}

#[derive(Debug, Serialize)]
struct BinReport {
    bin: usize,
    p_min: f64,
    p_max: f64,
    n: usize,
    avg_p: Option<f64>,
    hit_rate: Option<f64>,
    brier: Option<f64>,
}

#[derive(Debug)]
struct Report {
    strategy: String,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    n_predictions: i32,
    n_settled: i32,
    brier: Option<f64>,
    log_loss: Option<f64>,
    bins: serde_json::Value,
    diagnosis: serde_json::Value,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct OpenPositionRow {
    id: i64,
    ticker: String,
    side: String,
    current_qty: i32,
    avg_entry_cents: i32,
}

#[derive(Debug)]
struct VenueFlatCandidate {
    ticker: String,
    db_qty: i32,
    rows: Vec<OpenPositionRow>,
}

#[derive(Debug, Serialize)]
struct VenueFlatReport {
    mode: &'static str,
    strategy: Option<String>,
    db_open_tickers: usize,
    venue_open_tickers: usize,
    venue_flat_candidates: usize,
    checked_candidates: usize,
    eligible_settled: usize,
    closed_tickers: usize,
    closed_position_rows: usize,
    realized_pnl_delta_cents: i64,
    tickers: Vec<VenueFlatTickerReport>,
}

#[derive(Debug, Serialize)]
struct VenueFlatTickerReport {
    ticker: String,
    db_qty: i32,
    venue_qty: i32,
    n_position_rows: usize,
    status: String,
    outcome: Option<f64>,
    settled_at: Option<DateTime<Utc>>,
    action: &'static str,
    realized_pnl_delta_cents: i64,
    reason: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let Args {
        database_url,
        kalshi_rest_endpoint,
        kalshi_key_id,
        kalshi_pem,
        cmd,
    } = Args::parse();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await
        .with_context(|| format!("connect postgres {database_url}"))?;

    match cmd {
        Command::Report {
            strategy,
            window_days,
            dry_run,
        } => {
            if window_days <= 0 {
                return Err(anyhow!("--window-days must be positive"));
            }
            let report = build_report(&pool, &strategy, window_days).await?;
            let rendered = json!({
                "strategy": report.strategy,
                "window_start": report.window_start,
                "window_end": report.window_end,
                "n_predictions": report.n_predictions,
                "n_settled": report.n_settled,
                "brier": report.brier,
                "log_loss": report.log_loss,
                "bins": report.bins,
                "diagnosis": report.diagnosis,
            });
            println!("{}", serde_json::to_string_pretty(&rendered)?);
            if !dry_run {
                insert_report(&pool, &report).await?;
                info!(strategy = %strategy, "inserted calibration report");
            }
        }
        Command::SyncSettlements {
            strategy,
            window_days,
            limit,
            dry_run,
        } => {
            if window_days <= 0 {
                return Err(anyhow!("--window-days must be positive"));
            }
            if limit <= 0 {
                return Err(anyhow!("--limit must be positive"));
            }
            let rest = build_public_rest(kalshi_rest_endpoint.as_deref())?;
            let n = sync_settlements(
                &pool,
                &rest,
                strategy.as_deref(),
                window_days,
                limit,
                dry_run,
            )
            .await?;
            println!(
                "{} {} settlement outcomes",
                if dry_run { "would upsert" } else { "upserted" },
                n
            );
        }
        Command::ReconcileVenueFlat {
            strategy,
            limit,
            write,
        } => {
            if limit <= 0 {
                return Err(anyhow!("--limit must be positive"));
            }
            let rest = build_authed_rest(
                kalshi_rest_endpoint.as_deref(),
                kalshi_key_id.as_deref(),
                kalshi_pem.as_deref(),
            )?;
            let report =
                reconcile_venue_flat(&pool, &rest, strategy.as_deref(), limit, write).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::ShadowStat => {
            println!(
                "shadow-stat is not wired yet; keep stat rules disabled and collect model_p snapshots"
            );
        }
    }
    Ok(())
}

fn build_public_rest(endpoint: Option<&str>) -> Result<KalshiClient> {
    if let Some(base) = endpoint {
        KalshiClient::with_base(base, None).context("build public Kalshi REST client")
    } else {
        KalshiClient::public().context("build public Kalshi REST client")
    }
}

fn build_authed_rest(
    endpoint: Option<&str>,
    key_id: Option<&str>,
    pem_path: Option<&Path>,
) -> Result<KalshiClient> {
    let key_id = key_id.context("KALSHI_KEY_ID is required for reconcile-venue-flat")?;
    let pem_path = pem_path.context("KALSHI_PEM is required for reconcile-venue-flat")?;
    let pem = std::fs::read_to_string(pem_path)
        .with_context(|| format!("read PEM at {}", pem_path.display()))?;
    let signer = Signer::from_pem(key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    if let Some(base) = endpoint {
        KalshiClient::with_base(base, Some(signer))
            .context("build authenticated Kalshi REST client")
    } else {
        KalshiClient::authed(signer).context("build authenticated Kalshi REST client")
    }
}

async fn build_report(pool: &sqlx::PgPool, strategy: &str, window_days: i64) -> Result<Report> {
    let window_end = Utc::now();
    let window_start = window_end - Duration::days(window_days);

    let n_predictions_i64: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM model_p_snapshots
        WHERE strategy = $1 AND ts >= $2 AND ts <= $3
        ",
    )
    .bind(strategy)
    .bind(window_start)
    .bind(window_end)
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query(
        r"
        SELECT DISTINCT ON (s.strategy, s.ticker)
               s.model_p, st.resolved_value
        FROM model_p_snapshots s
        JOIN settlements st ON st.ticker = s.ticker
        WHERE s.strategy = $1
          AND s.ts >= $2
          AND s.ts <= $3
          AND st.resolved_value BETWEEN 0 AND 1
        ORDER BY s.strategy, s.ticker, s.ts DESC
        ",
    )
    .bind(strategy)
    .bind(window_start)
    .bind(window_end)
    .fetch_all(pool)
    .await?;

    let mut samples = Vec::with_capacity(rows.len());
    for row in rows {
        let p: f64 = row.try_get("model_p")?;
        let outcome: f64 = row.try_get("resolved_value")?;
        if (0.0..=1.0).contains(&p) && (0.0..=1.0).contains(&outcome) {
            samples.push(PredictionOutcome { p, outcome });
        }
    }

    let (brier, log_loss) = metrics(&samples);
    let bins = serde_json::to_value(build_bins(&samples))?;
    let diagnosis = json!({
        "source": "predigy-calibration report",
        "sample_method": "latest model_p snapshot per settled ticker in window",
        "status": if samples.is_empty() { "no_settled_samples" } else { "ok" },
        "selection_bias_note": "shadow predictions are preferred; traded fills alone are not enough",
    });

    Ok(Report {
        strategy: strategy.to_string(),
        window_start,
        window_end,
        n_predictions: i32::try_from(n_predictions_i64).unwrap_or(i32::MAX),
        n_settled: i32::try_from(samples.len()).unwrap_or(i32::MAX),
        brier,
        log_loss,
        bins,
        diagnosis,
    })
}

async fn sync_settlements(
    pool: &sqlx::PgPool,
    rest: &KalshiClient,
    strategy: Option<&str>,
    window_days: i64,
    limit: i64,
    dry_run: bool,
) -> Result<usize> {
    let window_start = Utc::now() - Duration::days(window_days);
    let tickers: Vec<String> = sqlx::query_scalar(
        r"
        SELECT DISTINCT s.ticker
        FROM model_p_snapshots s
        LEFT JOIN settlements st ON st.ticker = s.ticker
        WHERE st.ticker IS NULL
          AND s.ts >= $1
          AND ($2::TEXT IS NULL OR s.strategy = $2)
        ORDER BY s.ticker
        LIMIT $3
        ",
    )
    .bind(window_start)
    .bind(strategy)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut upsertable = 0_usize;
    for ticker in tickers {
        let detail = rest
            .market_detail(&ticker)
            .await
            .with_context(|| format!("fetch market detail {ticker}"))?
            .market;
        let Some(outcome) = market_outcome_value(&detail) else {
            continue;
        };
        upsertable += 1;
        if dry_run {
            continue;
        }
        upsert_market_and_settlement(pool, &detail, outcome).await?;
    }
    Ok(upsertable)
}

async fn upsert_market_and_settlement(
    pool: &sqlx::PgPool,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    upsert_market_and_settlement_tx(&mut tx, detail, outcome).await?;
    tx.commit().await?;
    Ok(())
}

async fn upsert_market_and_settlement_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<DateTime<Utc>> {
    let close_time = parse_rfc3339_utc(Some(detail.close_time.as_str()));
    let settled_at = parse_rfc3339_utc(detail.settled_time.as_deref()).unwrap_or_else(Utc::now);
    let settlement_ts =
        Some(settled_at).or_else(|| parse_rfc3339_utc(detail.expected_expiration_time.as_deref()));
    let payload = json!({
        "source": "predigy-calibration settlement-sync",
        "event_ticker": detail.event_ticker,
        "status": detail.status,
        "result": detail.result,
        "market_result": detail.market_result,
        "settled_time": detail.settled_time,
        "floor_strike": detail.floor_strike,
        "cap_strike": detail.cap_strike,
        "strike_type": detail.strike_type,
    });
    sqlx::query(
        r"
        INSERT INTO markets (
            ticker, venue, market_type, title, settlement_ts,
            close_time, tags, payload
        ) VALUES ($1, 'kalshi', 'binary', $2, $3, $4, $5, $6)
        ON CONFLICT (ticker) DO UPDATE
        SET title = COALESCE(EXCLUDED.title, markets.title),
            settlement_ts = COALESCE(EXCLUDED.settlement_ts, markets.settlement_ts),
            close_time = COALESCE(EXCLUDED.close_time, markets.close_time),
            payload = EXCLUDED.payload,
            last_updated_at = now()
        ",
    )
    .bind(&detail.ticker)
    .bind(&detail.title)
    .bind(settlement_ts)
    .bind(close_time)
    .bind(vec![
        "kalshi-settled".to_string(),
        "calibration".to_string(),
    ])
    .bind(&payload)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r"
        INSERT INTO settlements (ticker, resolved_value, settled_at, source, payload)
        VALUES ($1, $2, $3, 'kalshi-market-detail', $4)
        ON CONFLICT (ticker) DO UPDATE
        SET resolved_value = EXCLUDED.resolved_value,
            settled_at = EXCLUDED.settled_at,
            source = EXCLUDED.source,
            payload = EXCLUDED.payload
        ",
    )
    .bind(&detail.ticker)
    .bind(outcome)
    .bind(settled_at)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(settled_at)
}

async fn reconcile_venue_flat(
    pool: &sqlx::PgPool,
    rest: &KalshiClient,
    strategy: Option<&str>,
    limit: i64,
    write: bool,
) -> Result<VenueFlatReport> {
    let open_rows = fetch_open_positions(pool, strategy).await?;
    let venue_by_ticker = fetch_venue_positions(rest).await?;
    let (db_open_tickers, candidates) = venue_flat_candidates(open_rows, &venue_by_ticker);
    let venue_flat_candidates = candidates.len();

    let mut report = VenueFlatReport {
        mode: if write { "write" } else { "dry-run" },
        strategy: strategy.map(ToOwned::to_owned),
        db_open_tickers,
        venue_open_tickers: venue_by_ticker.len(),
        venue_flat_candidates,
        checked_candidates: 0,
        eligible_settled: 0,
        closed_tickers: 0,
        closed_position_rows: 0,
        realized_pnl_delta_cents: 0,
        tickers: Vec::new(),
    };

    for candidate in candidates.into_iter().take(limit as usize) {
        report.checked_candidates += 1;
        let detail = rest
            .market_detail(&candidate.ticker)
            .await
            .with_context(|| format!("fetch market detail {}", candidate.ticker))?
            .market;
        let venue_qty = venue_by_ticker.get(&candidate.ticker).copied().unwrap_or(0);
        let status = detail.status.clone();
        let outcome = market_outcome_value(&detail);
        let settled = final_settlement_outcome(&detail);
        let Some((outcome_value, settled_at)) = settled else {
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome,
                settled_at: None,
                action: "skipped",
                realized_pnl_delta_cents: 0,
                reason: "venue flat but market detail has no final settled binary outcome"
                    .to_string(),
            });
            continue;
        };

        report.eligible_settled += 1;
        let expected_pnl = candidate_settlement_pnl(&candidate, outcome_value)?;
        if write {
            let (closed_rows, realized_delta) =
                close_settled_venue_flat_ticker(pool, &candidate, &detail, outcome_value).await?;
            report.closed_tickers += usize::from(closed_rows > 0);
            report.closed_position_rows += closed_rows;
            report.realized_pnl_delta_cents += realized_delta;
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome: Some(outcome_value),
                settled_at: Some(settled_at),
                action: "closed",
                realized_pnl_delta_cents: realized_delta,
                reason: format!(
                    "closed {closed_rows} DB-open rows against settled venue-flat market"
                ),
            });
        } else {
            report.tickers.push(VenueFlatTickerReport {
                ticker: candidate.ticker,
                db_qty: candidate.db_qty,
                venue_qty,
                n_position_rows: candidate.rows.len(),
                status,
                outcome: Some(outcome_value),
                settled_at: Some(settled_at),
                action: "would_close",
                realized_pnl_delta_cents: expected_pnl,
                reason: "dry run: would close DB-open rows against settled venue-flat market"
                    .to_string(),
            });
        }
    }

    Ok(report)
}

async fn fetch_open_positions(
    pool: &sqlx::PgPool,
    strategy: Option<&str>,
) -> Result<Vec<OpenPositionRow>> {
    let rows = sqlx::query_as::<_, OpenPositionRow>(
        r"
        SELECT id, ticker, side, current_qty, avg_entry_cents
        FROM positions
        WHERE closed_at IS NULL
          AND ($1::TEXT IS NULL OR strategy = $1)
        ORDER BY ticker, strategy, side, id
        ",
    )
    .bind(strategy)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn fetch_venue_positions(rest: &KalshiClient) -> Result<HashMap<String, i32>> {
    let positions = rest.positions().await.context("fetch Kalshi positions")?;
    let mut by_ticker = HashMap::new();
    for position in positions.market_positions {
        let qty = venue_position_qty(position.position_contracts);
        if qty != 0 {
            *by_ticker.entry(position.ticker).or_insert(0) += qty;
        }
    }
    Ok(by_ticker)
}

fn venue_position_qty(position_contracts: Option<f64>) -> i32 {
    let Some(qty) = position_contracts else {
        return 0;
    };
    if qty.is_finite() {
        qty.round() as i32
    } else {
        0
    }
}

fn venue_flat_candidates(
    open_rows: Vec<OpenPositionRow>,
    venue_by_ticker: &HashMap<String, i32>,
) -> (usize, Vec<VenueFlatCandidate>) {
    let mut grouped: BTreeMap<String, Vec<OpenPositionRow>> = BTreeMap::new();
    for row in open_rows {
        grouped.entry(row.ticker.clone()).or_default().push(row);
    }
    let db_open_tickers = grouped.len();
    let mut candidates = Vec::new();
    for (ticker, rows) in grouped {
        if venue_by_ticker.get(&ticker).copied().unwrap_or(0) != 0 {
            continue;
        }
        let db_qty = signed_db_qty(&rows);
        candidates.push(VenueFlatCandidate {
            ticker,
            db_qty,
            rows,
        });
    }
    (db_open_tickers, candidates)
}

fn signed_db_qty(rows: &[OpenPositionRow]) -> i32 {
    rows.iter()
        .map(|row| {
            if row.side == "yes" {
                row.current_qty
            } else {
                -row.current_qty
            }
        })
        .sum()
}

fn final_settlement_outcome(detail: &MarketDetail) -> Option<(f64, DateTime<Utc>)> {
    let outcome = market_outcome_value(detail)?;
    if !(0.0..=1.0).contains(&outcome) || !outcome.is_finite() {
        return None;
    }
    if let Some(settled_at) = parse_rfc3339_utc(detail.settled_time.as_deref()) {
        return Some((outcome, settled_at));
    }
    if market_status_is_final(&detail.status) {
        return Some((outcome, Utc::now()));
    }
    None
}

fn market_status_is_final(status: &str) -> bool {
    let normalized: String = status
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "settled" | "finalized" | "resolved" | "closed"
    )
}

fn candidate_settlement_pnl(candidate: &VenueFlatCandidate, outcome: f64) -> Result<i64> {
    candidate
        .rows
        .iter()
        .map(|row| position_settlement_pnl(row, outcome))
        .sum()
}

fn position_settlement_pnl(row: &OpenPositionRow, yes_outcome: f64) -> Result<i64> {
    let settlement_price_cents = settlement_price_for_side(&row.side, yes_outcome)
        .with_context(|| format!("unknown position side {:?} for {}", row.side, row.ticker))?;
    Ok(i64::from(settlement_price_cents - row.avg_entry_cents)
        * i64::from(row.current_qty.signum())
        * i64::from(row.current_qty.abs()))
}

fn settlement_price_for_side(side: &str, yes_outcome: f64) -> Option<i32> {
    match side {
        "yes" => Some((yes_outcome * 100.0).round() as i32),
        "no" => Some(((1.0 - yes_outcome) * 100.0).round() as i32),
        _ => None,
    }
}

async fn close_settled_venue_flat_ticker(
    pool: &sqlx::PgPool,
    candidate: &VenueFlatCandidate,
    detail: &MarketDetail,
    outcome: f64,
) -> Result<(usize, i64)> {
    let mut tx = pool.begin().await?;
    let settled_at = upsert_market_and_settlement_tx(&mut tx, detail, outcome).await?;
    let mut closed_rows = 0_usize;
    let mut realized_delta = 0_i64;
    for row in &candidate.rows {
        let pnl_delta = position_settlement_pnl(row, outcome)?;
        let result = sqlx::query(
            r"
            UPDATE positions
            SET current_qty = 0,
                closed_at = $2,
                last_fill_at = $2,
                realized_pnl_cents = realized_pnl_cents + $3
            WHERE id = $1
              AND closed_at IS NULL
            ",
        )
        .bind(row.id)
        .bind(settled_at)
        .bind(pnl_delta)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() > 0 {
            closed_rows += 1;
            realized_delta += pnl_delta;
        }
    }
    tx.commit().await?;
    Ok((closed_rows, realized_delta))
}

fn parse_rfc3339_utc(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn market_outcome_value(detail: &MarketDetail) -> Option<f64> {
    detail
        .market_result
        .as_deref()
        .or(detail.result.as_deref())
        .and_then(binary_outcome_value)
}

fn binary_outcome_value(raw: &str) -> Option<f64> {
    let normalized: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "yes" | "y" | "true" | "1" => Some(1.0),
        "no" | "n" | "false" | "0" => Some(0.0),
        _ => None,
    }
}

fn metrics(samples: &[PredictionOutcome]) -> (Option<f64>, Option<f64>) {
    if samples.is_empty() {
        return (None, None);
    }
    let n = samples.len() as f64;
    let brier = samples
        .iter()
        .map(|s| (s.p - s.outcome).powi(2))
        .sum::<f64>()
        / n;
    let log_loss = samples
        .iter()
        .map(|s| {
            let p = s.p.clamp(1.0e-9, 1.0 - 1.0e-9);
            -(s.outcome * p.ln() + (1.0 - s.outcome) * (1.0 - p).ln())
        })
        .sum::<f64>()
        / n;
    (Some(brier), Some(log_loss))
}

fn build_bins(samples: &[PredictionOutcome]) -> Vec<BinReport> {
    (0..10)
        .map(|bin| {
            let p_min = bin as f64 / 10.0;
            let p_max = (bin + 1) as f64 / 10.0;
            let in_bin: Vec<&PredictionOutcome> = samples
                .iter()
                .filter(|s| {
                    if bin == 9 {
                        s.p >= p_min && s.p <= p_max
                    } else {
                        s.p >= p_min && s.p < p_max
                    }
                })
                .collect();
            if in_bin.is_empty() {
                return BinReport {
                    bin,
                    p_min,
                    p_max,
                    n: 0,
                    avg_p: None,
                    hit_rate: None,
                    brier: None,
                };
            }
            let n = in_bin.len() as f64;
            let avg_p = in_bin.iter().map(|s| s.p).sum::<f64>() / n;
            let hit_rate = in_bin.iter().map(|s| s.outcome).sum::<f64>() / n;
            let brier = in_bin
                .iter()
                .map(|s| (s.p - s.outcome).powi(2))
                .sum::<f64>()
                / n;
            BinReport {
                bin,
                p_min,
                p_max,
                n: in_bin.len(),
                avg_p: Some(avg_p),
                hit_rate: Some(hit_rate),
                brier: Some(brier),
            }
        })
        .collect()
}

async fn insert_report(pool: &sqlx::PgPool, report: &Report) -> Result<()> {
    sqlx::query(
        r"
        INSERT INTO calibration_reports (
            strategy, window_start, window_end, n_predictions,
            n_settled, brier, log_loss, net_pnl_cents,
            baseline, bins, diagnosis
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        ",
    )
    .bind(&report.strategy)
    .bind(report.window_start)
    .bind(report.window_end)
    .bind(report.n_predictions)
    .bind(report.n_settled)
    .bind(report.brier)
    .bind(report.log_loss)
    .bind(Option::<i64>::None)
    .bind(Option::<serde_json::Value>::None)
    .bind(&report.bins)
    .bind(&report.diagnosis)
    .execute(pool)
    .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_outcomes_from_common_kalshi_shapes() {
        assert_eq!(binary_outcome_value("yes"), Some(1.0));
        assert_eq!(binary_outcome_value("YES"), Some(1.0));
        assert_eq!(binary_outcome_value("no"), Some(0.0));
        assert_eq!(binary_outcome_value("No"), Some(0.0));
        assert_eq!(binary_outcome_value("not settled"), None);
    }

    #[test]
    fn final_outcome_requires_settlement_marker() {
        let mut detail = market_detail("settled", Some("yes"), None);
        assert!(final_settlement_outcome(&detail).is_some());

        detail.status = "active".to_string();
        assert!(final_settlement_outcome(&detail).is_none());

        detail.settled_time = Some("2026-05-08T12:00:00Z".to_string());
        assert!(final_settlement_outcome(&detail).is_some());
    }

    #[test]
    fn settlement_pnl_uses_side_domain_price() {
        let yes_long = OpenPositionRow {
            id: 1,
            ticker: "KX".to_string(),
            side: "yes".to_string(),
            current_qty: 3,
            avg_entry_cents: 60,
        };
        let no_long = OpenPositionRow {
            id: 2,
            ticker: "KX".to_string(),
            side: "no".to_string(),
            current_qty: 3,
            avg_entry_cents: 20,
        };
        let no_short = OpenPositionRow {
            id: 3,
            ticker: "KX".to_string(),
            side: "no".to_string(),
            current_qty: -3,
            avg_entry_cents: 20,
        };

        assert_eq!(position_settlement_pnl(&yes_long, 1.0).unwrap(), 120);
        assert_eq!(position_settlement_pnl(&yes_long, 0.0).unwrap(), -180);
        assert_eq!(position_settlement_pnl(&no_long, 0.0).unwrap(), 240);
        assert_eq!(position_settlement_pnl(&no_short, 0.0).unwrap(), -240);
    }

    #[test]
    fn venue_flat_candidates_include_net_zero_db_gross() {
        let rows = vec![
            OpenPositionRow {
                id: 1,
                ticker: "KX".to_string(),
                side: "yes".to_string(),
                current_qty: 2,
                avg_entry_cents: 50,
            },
            OpenPositionRow {
                id: 2,
                ticker: "KX".to_string(),
                side: "no".to_string(),
                current_qty: 2,
                avg_entry_cents: 50,
            },
        ];
        let venue = HashMap::new();
        let (db_open, candidates) = venue_flat_candidates(rows, &venue);
        assert_eq!(db_open, 1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].db_qty, 0);
    }

    #[test]
    fn metrics_are_reasonable_for_perfect_predictions() {
        let samples = vec![
            PredictionOutcome {
                p: 0.99,
                outcome: 1.0,
            },
            PredictionOutcome {
                p: 0.01,
                outcome: 0.0,
            },
        ];
        let (brier, log_loss) = metrics(&samples);
        assert!(brier.unwrap() < 0.001);
        assert!(log_loss.unwrap() < 0.02);
    }

    #[test]
    fn bins_include_upper_endpoint() {
        let bins = build_bins(&[PredictionOutcome {
            p: 1.0,
            outcome: 1.0,
        }]);
        assert_eq!(bins[9].n, 1);
        assert_eq!(bins[9].hit_rate, Some(1.0));
    }

    fn market_detail(
        status: &str,
        result: Option<&str>,
        settled_time: Option<&str>,
    ) -> MarketDetail {
        MarketDetail {
            ticker: "KX".to_string(),
            event_ticker: "EV".to_string(),
            title: "test".to_string(),
            status: status.to_string(),
            close_time: "2026-05-08T12:00:00Z".to_string(),
            expected_expiration_time: None,
            can_close_early: None,
            yes_bid_dollars: None,
            yes_ask_dollars: None,
            liquidity_dollars: None,
            volume: None,
            result: result.map(ToOwned::to_owned),
            market_result: None,
            settled_time: settled_time.map(ToOwned::to_owned),
            floor_strike: None,
            cap_strike: None,
            strike_type: None,
        }
    }
}
