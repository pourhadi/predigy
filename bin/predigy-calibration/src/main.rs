// Probability calibration report generator for Predigy.
#![allow(clippy::doc_markdown)]

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use predigy_kalshi_rest::Client as KalshiClient;
use predigy_kalshi_rest::types::MarketDetail;
use serde::Serialize;
use serde_json::json;
use sqlx::{Row, postgres::PgPoolOptions};
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

    /// Optional Kalshi REST endpoint override for public settlement sync.
    #[arg(long, env = "KALSHI_REST_ENDPOINT")]
    kalshi_rest_endpoint: Option<String>,

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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let Args {
        database_url,
        kalshi_rest_endpoint,
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
            let rest = if let Some(base) = kalshi_rest_endpoint.as_deref() {
                KalshiClient::with_base(base, None).context("build public Kalshi REST client")?
            } else {
                KalshiClient::public().context("build public Kalshi REST client")?
            };
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
        Command::ShadowStat => {
            println!(
                "shadow-stat is not wired yet; keep stat rules disabled and collect model_p snapshots"
            );
        }
    }
    Ok(())
}

async fn build_report(pool: &sqlx::PgPool, strategy: &str, window_days: i64) -> Result<Report> {
    let window_end = Utc::now();
    let window_start = window_end - Duration::days(window_days);

    let n_predictions_i64: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM model_p_snapshots
        WHERE strategy = $1 AND ts >= $2 AND ts <= $3
        "#,
    )
    .bind(strategy)
    .bind(window_start)
    .bind(window_end)
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (s.strategy, s.ticker)
               s.model_p, st.resolved_value
        FROM model_p_snapshots s
        JOIN settlements st ON st.ticker = s.ticker
        WHERE s.strategy = $1
          AND s.ts >= $2
          AND s.ts <= $3
          AND st.resolved_value BETWEEN 0 AND 1
        ORDER BY s.strategy, s.ticker, s.ts DESC
        "#,
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
        r#"
        SELECT DISTINCT s.ticker
        FROM model_p_snapshots s
        LEFT JOIN settlements st ON st.ticker = s.ticker
        WHERE st.ticker IS NULL
          AND s.ts >= $1
          AND ($2::TEXT IS NULL OR s.strategy = $2)
        ORDER BY s.ticker
        LIMIT $3
        "#,
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
    let close_time = parse_rfc3339_utc(Some(detail.close_time.as_str()));
    let settled_at = parse_rfc3339_utc(detail.settled_time.as_deref()).unwrap_or_else(Utc::now);
    let settlement_ts =
        Some(settled_at).or_else(|| parse_rfc3339_utc(detail.expected_expiration_time.as_deref()));
    let payload = json!({
        "source": "predigy-calibration sync-settlements",
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
        r#"
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
        "#,
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
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO settlements (ticker, resolved_value, settled_at, source, payload)
        VALUES ($1, $2, $3, 'kalshi-market-detail', $4)
        ON CONFLICT (ticker) DO UPDATE
        SET resolved_value = EXCLUDED.resolved_value,
            settled_at = EXCLUDED.settled_at,
            source = EXCLUDED.source,
            payload = EXCLUDED.payload
        "#,
    )
    .bind(&detail.ticker)
    .bind(outcome)
    .bind(settled_at)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
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
        r#"
        INSERT INTO calibration_reports (
            strategy, window_start, window_end, n_predictions,
            n_settled, brier, log_loss, net_pnl_cents,
            baseline, bins, diagnosis
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        "#,
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
}
