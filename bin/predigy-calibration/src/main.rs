// Probability calibration report generator for Predigy.
#![allow(clippy::doc_markdown)]

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use predigy_kalshi_rest::{Client as KalshiClient, Signer};
use predigy_venue_reconcile::{
    market_outcome_value, reconcile_venue_flat, upsert_market_and_settlement,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgPoolOptions};
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
    ticker: String,
    p: f64,
    outcome: f64,
    source: Option<String>,
    detail: Option<Value>,
}

#[derive(Debug, Clone)]
struct SnapshotOutcomeRow {
    ticker: String,
    ts: DateTime<Utc>,
    model_p: f64,
    source: Option<String>,
    detail: Option<Value>,
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
        SELECT s.ticker,
               s.ts,
               s.model_p,
               s.source,
               s.detail,
               st.resolved_value
        FROM model_p_snapshots s
        JOIN settlements st ON st.ticker = s.ticker
        WHERE s.strategy = $1
          AND s.ts >= $2
          AND s.ts <= $3
          AND st.resolved_value BETWEEN 0 AND 1
        ORDER BY s.ticker, s.ts DESC
        ",
    )
    .bind(strategy)
    .bind(window_start)
    .bind(window_end)
    .fetch_all(pool)
    .await?;

    let mut excluded: HashMap<&'static str, usize> = HashMap::new();
    let mut latest_by_ticker: BTreeMap<String, SnapshotOutcomeRow> = BTreeMap::new();
    let raw_settled_snapshots = rows.len();
    for row in rows {
        let candidate = SnapshotOutcomeRow {
            ticker: row.try_get("ticker")?,
            ts: row.try_get("ts")?,
            model_p: row.try_get("model_p")?,
            source: row.try_get("source")?,
            detail: row.try_get("detail")?,
            outcome: row.try_get("resolved_value")?,
        };
        if let Some(reason) = snapshot_exclusion_reason(strategy, &candidate) {
            *excluded.entry(reason).or_default() += 1;
            continue;
        }
        if !(0.0..=1.0).contains(&candidate.model_p) || !(0.0..=1.0).contains(&candidate.outcome) {
            *excluded
                .entry("probability_or_outcome_out_of_range")
                .or_default() += 1;
            continue;
        }
        latest_by_ticker
            .entry(candidate.ticker.clone())
            .and_modify(|existing| {
                if candidate.ts > existing.ts {
                    *existing = candidate.clone();
                }
            })
            .or_insert(candidate);
    }

    let samples: Vec<PredictionOutcome> = latest_by_ticker
        .into_values()
        .map(|row| PredictionOutcome {
            ticker: row.ticker,
            p: row.model_p,
            outcome: row.outcome,
            source: row.source,
            detail: row.detail,
        })
        .collect();

    let (brier, log_loss) = metrics(&samples);
    let (baseline_brier, baseline_log_loss, base_rate) = baseline_metrics(&samples);
    let bins = serde_json::to_value(build_bins(&samples))?;
    let diagnosis = build_diagnosis(
        strategy,
        raw_settled_snapshots,
        &samples,
        &excluded,
        brier,
        baseline_brier,
        baseline_log_loss,
        base_rate,
    )?;

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

fn snapshot_exclusion_reason(
    strategy: &str,
    candidate: &SnapshotOutcomeRow,
) -> Option<&'static str> {
    let detail = candidate.detail.as_ref()?;
    if detail
        .get("calibration_sample_eligible")
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Some("calibration_sample_eligible_false");
    }
    if strategy == "wx-stat" {
        let recorded_date = detail.get("settlement_date").and_then(Value::as_str);
        let canonical_date = wx_stat_ticker_settlement_date(&candidate.ticker);
        if let (Some(recorded), Some(canonical)) = (recorded_date, canonical_date.as_deref())
            && recorded != canonical
        {
            return Some("wx_stat_settlement_date_mismatch");
        }
    }
    None
}

fn wx_stat_ticker_settlement_date(ticker: &str) -> Option<String> {
    let token = ticker.split('-').nth(1)?;
    if token.len() != 7 {
        return None;
    }
    let yy: u32 = token.get(..2)?.parse().ok()?;
    let mon = token.get(2..5)?;
    let dd: u32 = token.get(5..7)?.parse().ok()?;
    if !(1..=31).contains(&dd) {
        return None;
    }
    let mm = match mon.to_ascii_uppercase().as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    };
    Some(format!("20{yy:02}-{mm:02}-{dd:02}"))
}

fn baseline_metrics(samples: &[PredictionOutcome]) -> (Option<f64>, Option<f64>, Option<f64>) {
    if samples.is_empty() {
        return (None, None, None);
    }
    let base_rate = samples.iter().map(|s| s.outcome).sum::<f64>() / samples.len() as f64;
    let baseline: Vec<PredictionOutcome> = samples
        .iter()
        .map(|s| PredictionOutcome {
            ticker: s.ticker.clone(),
            p: base_rate,
            outcome: s.outcome,
            source: s.source.clone(),
            detail: s.detail.clone(),
        })
        .collect();
    let (brier, log_loss) = metrics(&baseline);
    (brier, log_loss, Some(base_rate))
}

fn build_diagnosis(
    strategy: &str,
    raw_settled_snapshots: usize,
    samples: &[PredictionOutcome],
    excluded: &HashMap<&'static str, usize>,
    brier: Option<f64>,
    baseline_brier: Option<f64>,
    baseline_log_loss: Option<f64>,
    base_rate: Option<f64>,
) -> Result<Value> {
    let avg_p = if samples.is_empty() {
        None
    } else {
        Some(samples.iter().map(|s| s.p).sum::<f64>() / samples.len() as f64)
    };
    let brier_skill_vs_base = match (brier, baseline_brier) {
        (Some(model), Some(base)) if base > 0.0 => Some(1.0 - model / base),
        _ => None,
    };
    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_airport: BTreeMap<String, usize> = BTreeMap::new();
    for sample in samples {
        *by_source
            .entry(
                sample
                    .source
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            )
            .or_default() += 1;
        if let Some(airport) = sample
            .detail
            .as_ref()
            .and_then(|d| d.get("airport"))
            .and_then(Value::as_str)
        {
            *by_airport.entry(airport.to_string()).or_default() += 1;
        }
    }
    let mut worst: Vec<_> = samples.iter().collect();
    worst.sort_by(|a, b| {
        let ea = (a.p - a.outcome).abs();
        let eb = (b.p - b.outcome).abs();
        eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
    });
    let worst_errors: Vec<Value> = worst
        .into_iter()
        .take(10)
        .map(|s| {
            json!({
                "ticker": s.ticker,
                "p": s.p,
                "outcome": s.outcome,
                "abs_error": (s.p - s.outcome).abs(),
                "source": s.source,
                "airport": s.detail.as_ref().and_then(|d| d.get("airport")).and_then(Value::as_str),
                "settlement_date": s.detail.as_ref().and_then(|d| d.get("settlement_date")).and_then(Value::as_str),
            })
        })
        .collect();
    let status = if samples.is_empty() {
        "no_settled_samples"
    } else if samples.len() < 30 {
        "insufficient_clean_settled_samples"
    } else if brier_skill_vs_base.is_some_and(|skill| skill < 0.0) {
        "model_worse_than_base_rate"
    } else {
        "ok"
    };
    Ok(json!({
        "source": "predigy-calibration report",
        "strategy": strategy,
        "sample_method": "latest eligible model_p snapshot per settled ticker in window",
        "status": status,
        "selection_bias_note": "shadow predictions are preferred; traded fills alone are not enough",
        "raw_settled_snapshots": raw_settled_snapshots,
        "eligible_settled_tickers": samples.len(),
        "excluded_settled_snapshots_by_reason": excluded,
        "base_rate": base_rate,
        "avg_p": avg_p,
        "baseline_brier": baseline_brier,
        "baseline_log_loss": baseline_log_loss,
        "brier_skill_vs_base": brier_skill_vs_base,
        "samples_by_source": by_source,
        "samples_by_airport": by_airport,
        "worst_errors": worst_errors,
    }))
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
    fn metrics_are_reasonable_for_perfect_predictions() {
        let samples = vec![sample("KX-A", 0.99, 1.0), sample("KX-B", 0.01, 0.0)];
        let (brier, log_loss) = metrics(&samples);
        assert!(brier.unwrap() < 0.001);
        assert!(log_loss.unwrap() < 0.02);
    }

    #[test]
    fn bins_include_upper_endpoint() {
        let bins = build_bins(&[sample("KX-A", 1.0, 1.0)]);
        assert_eq!(bins[9].n, 1);
        assert_eq!(bins[9].hit_rate, Some(1.0));
    }

    #[test]
    fn wx_stat_ticker_date_parser_handles_market_ticker() {
        assert_eq!(
            wx_stat_ticker_settlement_date("KXHIGHTBOS-26MAY07-T62"),
            Some("2026-05-07".to_string())
        );
    }

    #[test]
    fn wx_stat_date_mismatch_is_excluded() {
        let row = SnapshotOutcomeRow {
            ticker: "KXHIGHTBOS-26MAY07-T62".to_string(),
            ts: Utc::now(),
            model_p: 0.98,
            source: Some("test".to_string()),
            detail: Some(json!({
                "settlement_date": "2026-05-08",
                "calibration_sample_eligible": true,
            })),
            outcome: 0.0,
        };
        assert_eq!(
            snapshot_exclusion_reason("wx-stat", &row),
            Some("wx_stat_settlement_date_mismatch")
        );
    }

    #[test]
    fn wx_stat_matching_ticker_date_is_eligible() {
        let row = SnapshotOutcomeRow {
            ticker: "KXHIGHTBOS-26MAY07-T62".to_string(),
            ts: Utc::now(),
            model_p: 0.98,
            source: Some("test".to_string()),
            detail: Some(json!({
                "settlement_date": "2026-05-07",
                "calibration_sample_eligible": true,
            })),
            outcome: 1.0,
        };
        assert_eq!(snapshot_exclusion_reason("wx-stat", &row), None);
    }

    fn sample(ticker: &str, p: f64, outcome: f64) -> PredictionOutcome {
        PredictionOutcome {
            ticker: ticker.to_string(),
            p,
            outcome,
            source: None,
            detail: None,
        }
    }
}
