//! `predigy-paper-trader` — shadow-execute the stat-curator's
//! rule output against live Kalshi prices, log to `paper_trades`,
//! reconcile against settlement, score by source/category.
//!
//! ## Why this exists
//!
//! The `stat` strategy (Claude-Sonnet-driven `model_p` per market)
//! has flexible reach across Kalshi's same-day markets — sports,
//! politics, world events, daily macro. But it has **no proven
//! calibration** in production. Enabling it live without evidence
//! is what blew up `wx-stat` overnight (see `docs/STATE_LOG.md`
//! 2026-05-09 17:45 UTC).
//!
//! Paper-trading lets us prove the model_p is positive-EV after
//! fees on a per-category basis BEFORE risking real cash. The
//! lifecycle:
//!
//! 1. `stat-curator` (existing, hourly cron) generates rules into
//!    `stat-rules.json` and writes `model_p_snapshots` to DB.
//! 2. **This binary** runs every 5 min: reads the rules file,
//!    fetches live Kalshi orderbooks, computes the same edge the
//!    `stat-trader` strategy would compute, and inserts a
//!    `paper_trades` row when edge clears `min_edge_cents`.
//! 3. **Reconcile** subcommand polls public Kalshi for settlement
//!    of any unsettled paper trade past its `settlement_date`,
//!    fills in `paper_pnl_cents`.
//! 4. **Report** subcommand aggregates by `source`/`category` and
//!    prints after-fee EV, hit rate, Brier — the evidence base
//!    for "should this rule category go live?"
//!
//! Idempotency: paper_trades has a UNIQUE INDEX on
//! (strategy, ticker, side, settlement_date). Replaying the same
//! curator output is a no-op.

use anyhow::{Context as _, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use predigy_core::fees::taker_fee;
use predigy_core::price::{Price, Qty};
use predigy_core::side::Side;
use predigy_kalshi_rest::types::MarketDetail;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::time::Duration;
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "predigy-paper-trader",
    about = "Shadow-execute stat-curator rules against live prices, score on settlement."
)]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,

    /// Kalshi REST endpoint override (default: production).
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// Kalshi key id for signed requests (only needed for `record`
    /// when fetching authenticated endpoints; orderbook + market
    /// detail are public).
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: Option<String>,

    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read a stat-curator rules JSON file, fetch live Kalshi
    /// touches, and insert a `paper_trades` row for each rule
    /// whose computed edge clears its `min_edge_cents`.
    Record {
        /// Path to stat-rules.json.
        #[arg(long)]
        rules_file: PathBuf,
        /// Strategy label (defaults to "stat").
        #[arg(long, default_value = "stat")]
        strategy: String,
        /// Source label written into the row (e.g. the curator
        /// version + run timestamp). Defaults to "stat-curator".
        #[arg(long, default_value = "stat-curator")]
        source: String,
        /// Per-fire qty for paper accounting. Real strategy uses
        /// Kelly sizing; we use a constant for clean per-trade EV.
        #[arg(long, default_value_t = 1)]
        qty: i32,
    },

    /// For each unsettled `paper_trades` row whose
    /// `settlement_date` is in the past, fetch the underlying
    /// Kalshi market detail and (if the market has resolved)
    /// fill in settlement_outcome + paper_pnl_cents.
    Reconcile {
        /// Process at most this many rows per run.
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },

    /// Print after-fee EV / hit rate / Brier per
    /// (strategy, source, category, settlement_date) bucket over
    /// the recent window.
    Report {
        /// Lookback window in days (default 14).
        #[arg(long, default_value_t = 14)]
        days: i64,
        /// Strategy filter (default "stat").
        #[arg(long, default_value = "stat")]
        strategy: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&args.database_url)
        .await
        .with_context(|| format!("connect postgres {}", args.database_url))?;

    match args.command {
        Command::Record {
            rules_file,
            strategy,
            source,
            qty,
        } => {
            let rest = build_public_rest(args.kalshi_rest_endpoint.as_deref())?;
            let report = run_record(&pool, &rest, &rules_file, &strategy, &source, qty).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Reconcile { limit } => {
            let rest = build_public_rest(args.kalshi_rest_endpoint.as_deref())?;
            let report = run_reconcile(&pool, &rest, limit).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Report { days, strategy } => {
            let report = run_report(&pool, &strategy, days).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();
}

fn build_public_rest(endpoint: Option<&str>) -> Result<RestClient> {
    if let Some(base) = endpoint {
        Ok(RestClient::with_base(base, None)?)
    } else {
        Ok(RestClient::public()?)
    }
}

#[allow(dead_code)]
fn build_authed_rest(
    endpoint: Option<&str>,
    key_id: Option<&str>,
    pem_path: Option<&PathBuf>,
) -> Result<RestClient> {
    let key_id = key_id.context("KALSHI_KEY_ID required")?;
    let pem_path = pem_path.context("KALSHI_PEM required")?;
    let pem = std::fs::read_to_string(pem_path)?;
    let signer = Signer::from_pem(key_id, &pem)?;
    if let Some(base) = endpoint {
        Ok(RestClient::with_base(base, Some(signer))?)
    } else {
        Ok(RestClient::authed(signer)?)
    }
}

// ---------------------------------------------------------------------------
// `record` — read curator output, fetch live touches, insert paper_trades
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RuleFile {
    /// Top-level array (the format `stat-curator` writes).
    Array(Vec<RuleEntry>),
    /// Wrapped object `{ "rules": [...] }` (older variant; tolerate).
    Wrapped { rules: Vec<RuleEntry> },
}

impl RuleFile {
    fn into_rules(self) -> Vec<RuleEntry> {
        match self {
            Self::Array(v) => v,
            Self::Wrapped { rules } => rules,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RuleEntry {
    kalshi_market: String,
    model_p: f64,
    side: SideRaw,
    min_edge_cents: u32,
    settlement_date: Option<String>,
    #[serde(default)]
    generated_at_utc: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SideRaw {
    Yes,
    No,
}

impl SideRaw {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
    fn to_side(&self) -> Side {
        match self {
            Self::Yes => Side::Yes,
            Self::No => Side::No,
        }
    }
}

#[derive(Debug, serde::Serialize, Default)]
struct RecordReport {
    rules_in_file: usize,
    inserted: usize,
    skipped_expired: usize,
    skipped_no_settlement_date: usize,
    skipped_no_book: usize,
    skipped_below_edge: usize,
    skipped_already_recorded: usize,
    skipped_invalid_price: usize,
    fetch_errors: usize,
    rows: Vec<RecordRow>,
}

#[derive(Debug, serde::Serialize)]
struct RecordRow {
    ticker: String,
    side: &'static str,
    action: &'static str,
    entry_price_cents: Option<i32>,
    edge_at_entry_cents: Option<i32>,
    fee_cents: Option<i32>,
    model_p: f64,
    settlement_date: Option<String>,
    reason: String,
}

async fn run_record(
    pool: &PgPool,
    rest: &RestClient,
    path: &PathBuf,
    strategy: &str,
    source: &str,
    qty: i32,
) -> Result<RecordReport> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let rules = serde_json::from_slice::<RuleFile>(&raw)
        .with_context(|| format!("parse {}", path.display()))?
        .into_rules();

    let mut report = RecordReport {
        rules_in_file: rules.len(),
        ..Default::default()
    };

    let today = Utc::now().date_naive();

    for rule in rules {
        // Curator output sometimes has null settlement_date. Fall
        // back to fetching market_detail and deriving from
        // `expected_expiration_time` (preferred for can_close_early
        // markets) or `close_time` (calendar fallback). One extra
        // REST call per rule with missing date — acceptable on
        // a 5-min cadence.
        let date_str = match rule.settlement_date.clone() {
            Some(s) => s,
            None => match derive_settlement_date(rest, &rule.kalshi_market).await {
                Some(s) => s,
                None => {
                    report.skipped_no_settlement_date += 1;
                    report.rows.push(RecordRow {
                        ticker: rule.kalshi_market,
                        side: rule.side.as_str(),
                        action: "skipped",
                        entry_price_cents: None,
                        edge_at_entry_cents: None,
                        fee_cents: None,
                        model_p: rule.model_p,
                        settlement_date: None,
                        reason: "no settlement_date and market_detail derivation failed"
                            .to_string(),
                    });
                    continue;
                }
            },
        };
        let Ok(settlement) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") else {
            report.skipped_no_settlement_date += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "skipped",
                entry_price_cents: None,
                edge_at_entry_cents: None,
                fee_cents: None,
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: "settlement_date does not parse as YYYY-MM-DD".to_string(),
            });
            continue;
        };
        if settlement < today {
            report.skipped_expired += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "skipped",
                entry_price_cents: None,
                edge_at_entry_cents: None,
                fee_cents: None,
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: "settlement_date already passed".to_string(),
            });
            continue;
        }

        // Idempotency probe — if we already wrote a paper_trade
        // for this (strategy, ticker, side, settlement_date),
        // skip without fetching.
        let already: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM paper_trades
              WHERE strategy = $1 AND ticker = $2 AND side = $3 AND settlement_date = $4",
        )
        .bind(strategy)
        .bind(&rule.kalshi_market)
        .bind(rule.side.as_str())
        .bind(settlement)
        .fetch_optional(pool)
        .await?;
        if already.is_some() {
            report.skipped_already_recorded += 1;
            continue;
        }

        // Fetch the live orderbook snapshot. Public REST.
        let snap = match rest.orderbook_snapshot(&rule.kalshi_market).await {
            Ok(s) => s,
            Err(e) => {
                warn!(ticker = %rule.kalshi_market, error = %e, "paper-trader: orderbook fetch failed");
                report.fetch_errors += 1;
                report.rows.push(RecordRow {
                    ticker: rule.kalshi_market,
                    side: rule.side.as_str(),
                    action: "error",
                    entry_price_cents: None,
                    edge_at_entry_cents: None,
                    fee_cents: None,
                    model_p: rule.model_p,
                    settlement_date: Some(date_str),
                    reason: format!("orderbook fetch failed: {e}"),
                });
                continue;
            }
        };

        // Same derive-ask trick as stat-trader::derive_ask.
        let touch = match rule.side.to_side() {
            Side::Yes => snap
                .no_bids
                .first()
                .map(|(p, q)| (100u8.saturating_sub(p.cents()), *q)),
            Side::No => snap
                .yes_bids
                .first()
                .map(|(p, q)| (100u8.saturating_sub(p.cents()), *q)),
        };
        let Some((ask_cents, _qty_at_touch)) = touch else {
            report.skipped_no_book += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "skipped",
                entry_price_cents: None,
                edge_at_entry_cents: None,
                fee_cents: None,
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: "one-sided book or no liquidity".to_string(),
            });
            continue;
        };
        if !(1..=99).contains(&ask_cents) {
            report.skipped_invalid_price += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "skipped",
                entry_price_cents: Some(i32::from(ask_cents)),
                edge_at_entry_cents: None,
                fee_cents: None,
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: "ask outside 1..=99c".to_string(),
            });
            continue;
        }

        // Edge logic mirrors stat-trader::build_intent.
        let bet_p = match rule.side.to_side() {
            Side::Yes => rule.model_p,
            Side::No => 1.0 - rule.model_p,
        };
        let raw_edge_cents = (bet_p * 100.0 - f64::from(ask_cents)).round() as i32;
        let kalshi_price = match Price::from_cents(ask_cents) {
            Ok(p) => p,
            Err(_) => {
                report.skipped_invalid_price += 1;
                continue;
            }
        };
        let probe_qty = Qty::new(1).expect("Qty 1 always valid");
        let fee_per_contract =
            i32::try_from(taker_fee(kalshi_price, probe_qty)).unwrap_or(i32::MAX);
        let after_fee_edge = raw_edge_cents - fee_per_contract;
        if after_fee_edge < i32::try_from(rule.min_edge_cents).unwrap_or(i32::MAX) {
            report.skipped_below_edge += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "skipped",
                entry_price_cents: Some(i32::from(ask_cents)),
                edge_at_entry_cents: Some(after_fee_edge),
                fee_cents: Some(fee_per_contract),
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: format!(
                    "edge {after_fee_edge}c below threshold {}c",
                    rule.min_edge_cents
                ),
            });
            continue;
        }

        let category = derive_category(&rule.kalshi_market);
        let detail_json = serde_json::json!({
            "min_edge_cents": rule.min_edge_cents,
            "raw_edge_cents": raw_edge_cents,
            "after_fee_edge_cents": after_fee_edge,
            "fee_per_contract_cents": fee_per_contract,
            "ask_qty_at_touch": touch.map(|(_, q)| q).unwrap_or(0),
            "generated_at_utc": rule.generated_at_utc,
        });

        // INSERT ... ON CONFLICT DO NOTHING handles concurrent
        // recorders (we shouldn't have any, but cheap insurance).
        let result = sqlx::query(
            r"
            INSERT INTO paper_trades (
                strategy, ticker, side, qty,
                entry_price_cents,
                model_p, raw_p, min_edge_cents, edge_at_entry_cents, fee_cents,
                settlement_date,
                source, category, detail
            ) VALUES (
                $1, $2, $3, $4,
                $5,
                $6, $6, $7, $8, $9,
                $10,
                $11, $12, $13
            )
            ON CONFLICT (strategy, ticker, side, settlement_date) DO NOTHING
            ",
        )
        .bind(strategy)
        .bind(&rule.kalshi_market)
        .bind(rule.side.as_str())
        .bind(qty)
        .bind(i32::from(ask_cents))
        .bind(rule.model_p)
        .bind(i32::try_from(rule.min_edge_cents).unwrap_or(i32::MAX))
        .bind(after_fee_edge)
        .bind(fee_per_contract)
        .bind(settlement)
        .bind(source)
        .bind(category)
        .bind(detail_json)
        .execute(pool)
        .await?;

        if result.rows_affected() == 1 {
            report.inserted += 1;
            report.rows.push(RecordRow {
                ticker: rule.kalshi_market,
                side: rule.side.as_str(),
                action: "inserted",
                entry_price_cents: Some(i32::from(ask_cents)),
                edge_at_entry_cents: Some(after_fee_edge),
                fee_cents: Some(fee_per_contract),
                model_p: rule.model_p,
                settlement_date: Some(date_str),
                reason: "paper trade recorded".to_string(),
            });
        } else {
            report.skipped_already_recorded += 1;
        }
    }

    Ok(report)
}

async fn derive_settlement_date(rest: &RestClient, ticker: &str) -> Option<String> {
    let detail = rest.market_detail(ticker).await.ok()?.market;
    let raw = detail
        .expected_expiration_time
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(detail.close_time.as_str());
    let dt = DateTime::parse_from_rfc3339(raw).ok()?;
    Some(
        dt.with_timezone(&Utc)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// `reconcile` — settle paper trades against public Kalshi outcomes
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct UnsettledRow {
    id: i64,
    ticker: String,
    side: String,
    qty: i32,
    entry_price_cents: i32,
    fee_cents: i32,
}

#[derive(Debug, serde::Serialize, Default)]
struct ReconcileReport {
    candidates: usize,
    settled: usize,
    not_yet_resolved: usize,
    fetch_errors: usize,
    realized_pnl_cents: i64,
    rows: Vec<ReconcileRow>,
}

#[derive(Debug, serde::Serialize)]
struct ReconcileRow {
    ticker: String,
    side: String,
    entry_price_cents: i32,
    settlement_outcome: Option<f64>,
    paper_pnl_cents: Option<i32>,
    action: &'static str,
    reason: String,
}

async fn run_reconcile(pool: &PgPool, rest: &RestClient, limit: i64) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();

    // Pull paper trades whose settlement_date is in the past
    // (or today) and that haven't been settled yet.
    let candidates: Vec<UnsettledRow> = sqlx::query_as(
        r"
        SELECT id, ticker, side, qty, entry_price_cents, fee_cents
          FROM paper_trades
         WHERE settled_at IS NULL
           AND settlement_date <= CURRENT_DATE
         ORDER BY settlement_date ASC, id ASC
         LIMIT $1
        ",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    report.candidates = candidates.len();

    for row in candidates {
        let detail = match rest.market_detail(&row.ticker).await {
            Ok(d) => d.market,
            Err(e) => {
                warn!(ticker = %row.ticker, error = %e, "paper-trader reconcile: market_detail fetch failed");
                report.fetch_errors += 1;
                report.rows.push(ReconcileRow {
                    ticker: row.ticker,
                    side: row.side,
                    entry_price_cents: row.entry_price_cents,
                    settlement_outcome: None,
                    paper_pnl_cents: None,
                    action: "error",
                    reason: format!("market_detail fetch failed: {e}"),
                });
                continue;
            }
        };
        let Some((outcome, settled_at)) = final_settlement_outcome(&detail) else {
            report.not_yet_resolved += 1;
            report.rows.push(ReconcileRow {
                ticker: row.ticker,
                side: row.side,
                entry_price_cents: row.entry_price_cents,
                settlement_outcome: None,
                paper_pnl_cents: None,
                action: "pending",
                reason: format!("market not yet resolved (status={})", detail.status),
            });
            continue;
        };

        let settlement_price = settlement_price_for_side(&row.side, outcome);
        let pnl_cents = (settlement_price - row.entry_price_cents)
            .saturating_mul(row.qty)
            .saturating_sub(row.fee_cents);

        sqlx::query(
            r"
            UPDATE paper_trades
               SET settled_at = $2,
                   settlement_outcome = $3,
                   paper_pnl_cents = $4
             WHERE id = $1
            ",
        )
        .bind(row.id)
        .bind(settled_at)
        .bind(outcome)
        .bind(pnl_cents)
        .execute(pool)
        .await?;

        report.settled += 1;
        report.realized_pnl_cents += i64::from(pnl_cents);
        report.rows.push(ReconcileRow {
            ticker: row.ticker,
            side: row.side,
            entry_price_cents: row.entry_price_cents,
            settlement_outcome: Some(outcome),
            paper_pnl_cents: Some(pnl_cents),
            action: "settled",
            reason: format!("outcome={outcome} settled_at={}", settled_at.to_rfc3339()),
        });
    }

    Ok(report)
}

fn settlement_price_for_side(side: &str, yes_outcome: f64) -> i32 {
    match side {
        "yes" => (yes_outcome * 100.0).round() as i32,
        "no" => ((1.0 - yes_outcome) * 100.0).round() as i32,
        _ => 0,
    }
}

fn final_settlement_outcome(detail: &MarketDetail) -> Option<(f64, DateTime<Utc>)> {
    let outcome = market_outcome_value(detail)?;
    if !(0.0..=1.0).contains(&outcome) || !outcome.is_finite() {
        return None;
    }
    if let Some(settled_at) = detail
        .settled_time
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
    {
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

// ---------------------------------------------------------------------------
// `report` — aggregate paper-trade evidence by source/category/date
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct ReportOutput {
    strategy: String,
    window_start: NaiveDate,
    window_end: NaiveDate,
    overall: BucketSummary,
    by_category: Vec<BucketSummary>,
    by_source: Vec<BucketSummary>,
    by_settlement_date: Vec<BucketSummary>,
}

#[derive(Debug, serde::Serialize, Default)]
struct BucketSummary {
    bucket: String,
    n_recorded: i64,
    n_settled: i64,
    wins: i64,
    losses: i64,
    pushes: i64,
    realized_pnl_cents: i64,
    after_fee_ev_cents_per_trade: f64,
    hit_rate: f64,
    brier_score: f64,
}

#[derive(Debug, sqlx::FromRow)]
struct ReportRow {
    side: String,
    paper_pnl_cents: Option<i32>,
    settlement_outcome: Option<f64>,
    model_p: f64,
    bucket: Option<String>,
}

async fn run_report(pool: &PgPool, strategy: &str, days: i64) -> Result<ReportOutput> {
    let window_end = Utc::now().date_naive();
    let window_start = window_end - chrono::Duration::days(days);

    let overall = compute_bucket(pool, strategy, "ALL", "1=1", &[], window_start).await?;

    let by_category = compute_grouped(pool, strategy, "category", window_start).await?;
    let by_source = compute_grouped(pool, strategy, "source", window_start).await?;
    let by_settlement_date =
        compute_grouped(pool, strategy, "settlement_date::TEXT", window_start).await?;

    Ok(ReportOutput {
        strategy: strategy.to_string(),
        window_start,
        window_end,
        overall,
        by_category,
        by_source,
        by_settlement_date,
    })
}

async fn compute_bucket(
    pool: &PgPool,
    strategy: &str,
    label: &str,
    where_extra: &str,
    bind_extra: &[String],
    window_start: NaiveDate,
) -> Result<BucketSummary> {
    // Single bucket: count recorded, settled, compute metrics.
    let sql = format!(
        r"
        SELECT side, paper_pnl_cents, settlement_outcome, model_p, NULL AS bucket
          FROM paper_trades
         WHERE strategy = $1
           AND entered_at >= $2
           AND ({where_extra})
        "
    );
    let mut q = sqlx::query_as::<_, ReportRow>(&sql)
        .bind(strategy)
        .bind(window_start);
    for b in bind_extra {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await?;
    Ok(summarize(label.to_string(), rows))
}

async fn compute_grouped(
    pool: &PgPool,
    strategy: &str,
    bucket_expr: &str,
    window_start: NaiveDate,
) -> Result<Vec<BucketSummary>> {
    let sql = format!(
        r"
        SELECT side, paper_pnl_cents, settlement_outcome, model_p,
               COALESCE({bucket_expr}, 'unknown') AS bucket
          FROM paper_trades
         WHERE strategy = $1
           AND entered_at >= $2
        "
    );
    let rows = sqlx::query_as::<_, ReportRow>(&sql)
        .bind(strategy)
        .bind(window_start)
        .fetch_all(pool)
        .await?;

    let mut by_bucket: std::collections::BTreeMap<String, Vec<ReportRow>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let key = row.bucket.clone().unwrap_or_else(|| "unknown".to_string());
        by_bucket.entry(key).or_default().push(row);
    }
    let mut out = Vec::with_capacity(by_bucket.len());
    for (k, v) in by_bucket {
        out.push(summarize(k, v));
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.n_settled));
    Ok(out)
}

fn summarize(bucket: String, rows: Vec<ReportRow>) -> BucketSummary {
    let mut s = BucketSummary {
        bucket,
        ..Default::default()
    };
    let mut brier_acc = 0.0_f64;
    for row in &rows {
        s.n_recorded += 1;
        let Some(pnl) = row.paper_pnl_cents else {
            continue;
        };
        let Some(outcome) = row.settlement_outcome else {
            continue;
        };
        s.n_settled += 1;
        s.realized_pnl_cents += i64::from(pnl);
        match pnl.cmp(&0) {
            std::cmp::Ordering::Greater => s.wins += 1,
            std::cmp::Ordering::Less => s.losses += 1,
            std::cmp::Ordering::Equal => s.pushes += 1,
        }
        // Brier on the side-adjusted prediction. For a NO bet,
        // the bet probability is 1 - model_p. The "outcome" we
        // compare against is whether the bet won (1.0 if won,
        // 0.0 if lost), not the YES outcome directly.
        let bet_p = match row.side.as_str() {
            "yes" => row.model_p,
            "no" => 1.0 - row.model_p,
            _ => 0.5,
        };
        let bet_outcome = match row.side.as_str() {
            "yes" => outcome,
            "no" => 1.0 - outcome,
            _ => 0.5,
        };
        let err = bet_p - bet_outcome;
        brier_acc += err * err;
    }
    if s.n_settled > 0 {
        s.hit_rate = s.wins as f64 / s.n_settled as f64;
        s.after_fee_ev_cents_per_trade = s.realized_pnl_cents as f64 / s.n_settled as f64;
        s.brier_score = brier_acc / s.n_settled as f64;
    }
    s
}

// ---------------------------------------------------------------------------
// Category derivation from ticker prefix
// ---------------------------------------------------------------------------

fn derive_category(ticker: &str) -> String {
    let upper = ticker.to_ascii_uppercase();
    let prefix = upper.split('-').next().unwrap_or("");
    if prefix.starts_with("KXMLBGAME")
        || prefix.starts_with("KXNHLGAME")
        || prefix.starts_with("KXNBA")
        || prefix.starts_with("KXNFL")
        || prefix.starts_with("KXMLB")
    {
        return "sport".to_string();
    }
    if prefix.starts_with("KXHIGH")
        || prefix.starts_with("KXLOW")
        || prefix.starts_with("KXTORNADO")
        || prefix.starts_with("KXTEMP")
    {
        return "weather".to_string();
    }
    if prefix.starts_with("KXPAYROLLS")
        || prefix.starts_with("KXECONSTATU")
        || prefix.starts_with("KXEMPLOYRATE")
        || prefix.starts_with("KXCPI")
        || prefix.starts_with("KXFED")
        || prefix.starts_with("KXGDP")
        || prefix.starts_with("KXJOBLESS")
        || prefix.starts_with("KXBRAZILINF")
    {
        return "econ".to_string();
    }
    if prefix.starts_with("KXVOTE")
        || prefix.starts_with("KXPRESV")
        || prefix.starts_with("KXSEN")
        || prefix.starts_with("KXHOUSE")
        || prefix.starts_with("KXGOV")
        || prefix.contains("PRES")
    {
        return "politics".to_string();
    }
    "other".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_classifies_tickers() {
        assert_eq!(derive_category("KXMLBGAME-26MAY09NYMAZ-NYM"), "sport");
        assert_eq!(derive_category("KXNHLGAME-26MAY10MTLBUF-BUF"), "sport");
        assert_eq!(derive_category("KXHIGHTLAX-26MAY09-T68"), "weather");
        assert_eq!(derive_category("KXLOWTOKC-26MAY09-T54"), "weather");
        assert_eq!(derive_category("KXPAYROLLS-26MAY-T20000"), "econ");
        assert_eq!(derive_category("KXECONSTATU3-26MAY-T4.2"), "econ");
        assert_eq!(derive_category("KXVOTEHUBTRUMPUPDOWN"), "politics");
        assert_eq!(derive_category("KXCOLOMBIAPRESR1-26MAY31-AESP"), "politics");
        assert_eq!(derive_category("KXOTHERCAT-XYZ"), "other");
    }

    #[test]
    fn settlement_price_yes_no() {
        assert_eq!(settlement_price_for_side("yes", 1.0), 100);
        assert_eq!(settlement_price_for_side("yes", 0.0), 0);
        assert_eq!(settlement_price_for_side("no", 1.0), 0);
        assert_eq!(settlement_price_for_side("no", 0.0), 100);
    }
}
