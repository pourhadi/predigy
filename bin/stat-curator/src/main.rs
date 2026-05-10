// Vendor names appear in doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `stat-curator`: scan Kalshi sports/politics/world/economics
//! markets, ask Claude to propose calibrated probabilities, validate,
//! write to a JSON file the `stat-trader` binary can consume.
//!
//! ```text
//! stat-curator \
//!   --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./kalshi.pem \
//!   --output ./stat-rules.json \
//!   --batch-size 25 --max-batches 4 \
//!   --max-days-to-settle 14 \
//!   --write     # omit for dry-run
//! ```
//!
//! Without `--write`, the binary prints the proposed rules to
//! stdout as pretty JSON and exits — useful for eyeballing the
//! LLM's output before committing it to the rule file the live
//! trader reads.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_core::side::Side;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use sqlx::postgres::PgPoolOptions;
use stat_curator::{DEFAULT_CATEGORIES, propose_rules, scan_stat_markets};
use stat_trader::StatRule;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "stat-curator",
    about = "Curate stat-trader probability rules for Kalshi markets via Claude."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// Output path for the rule JSON.  Format matches what
    /// `stat-trader --rule-file` reads.
    #[arg(long, default_value = "stat-rules.json")]
    output: PathBuf,

    /// Markets per Anthropic call.  ~25 is the sweet spot for
    /// Sonnet 4.6 with our 8K max_tokens.
    #[arg(long, default_value_t = 25)]
    batch_size: usize,

    /// Hard cap on Anthropic calls per run.  Each call costs ~$0.02
    /// at default batch size.  4-8 batches is the typical
    /// sustainable cost shape for a daily scan.
    #[arg(long, default_value_t = 5)]
    max_batches: usize,

    /// Settlement-horizon filter: only consider markets settling
    /// within this many days. Statistical bets compound poorly
    /// when held longer because the curator's daily re-run can't
    /// re-calibrate against intra-trade news.
    ///
    /// **2026-05-09: default tightened from 14 → 3.** The
    /// operational objective is same-day or next-settlement
    /// returns. Multi-week horizons mostly produce econ-calendar
    /// markets that Claude has no edge on (paper-trader evidence).
    /// Sports, daily politics, and breaking-news markets all
    /// settle within 1-3 days.
    #[arg(long, default_value_t = 3)]
    max_days_to_settle: i64,

    /// Restart the named launchd job after a successful write.
    /// Used in production to kick the stat-trader so it picks up
    /// new rules without waiting for its own poll cadence.
    #[arg(long)]
    restart_job: Option<String>,

    /// Write the curated rules to `--output`.  Without this, the
    /// rules are printed to stdout (dry-run mode).
    #[arg(long, default_value_t = false)]
    write: bool,

    /// Shadow-write generated rules to Postgres as disabled stat
    /// rules plus model_p snapshots. This is calibration evidence
    /// only; it never enables live stat trading.
    #[arg(long, default_value_t = false)]
    shadow_db: bool,

    /// Postgres DSN for `--shadow-db`.
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return Err(anyhow!(
            "ANTHROPIC_API_KEY is not set; export it from your shell profile"
        ));
    }

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;
    let signer = Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(signer))
    } else {
        RestClient::authed(signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;

    let max_markets = args.batch_size * args.max_batches;
    info!(
        categories = ?DEFAULT_CATEGORIES,
        max_days_to_settle = args.max_days_to_settle,
        max_markets,
        "scanning Kalshi for actionable stat-arb markets"
    );
    let markets = scan_stat_markets(
        &rest,
        DEFAULT_CATEGORIES,
        args.max_days_to_settle,
        max_markets,
    )
    .await
    .map_err(|e| anyhow!("scan: {e}"))?;
    info!(found = markets.len(), "stat-arb candidates discovered");

    if markets.is_empty() {
        // Always write an empty file so a stale rule set from a
        // previous run doesn't keep firing.  But warn loudly.
        warn!("no actionable markets found — writing empty rule file");
        if args.write {
            write_rules(&[], &args.output).await?;
        } else {
            println!("[]");
        }
        return Ok(());
    }

    let mut all_rules: Vec<StatRule> = Vec::new();
    let mut dropped_invalid: Vec<(String, String)> = Vec::new();
    let mut accepted_audit: Vec<String> = Vec::new();
    let mut batch_failures = 0usize;
    for (i, batch) in markets.chunks(args.batch_size).enumerate() {
        info!(batch = i, n = batch.len(), "calling claude on batch");
        let raw = match propose_rules(batch).await {
            Ok(r) => r,
            Err(e) => {
                warn!(batch = i, error = %e, "batch failed; continuing");
                batch_failures += 1;
                continue;
            }
        };
        // Index the batch's markets by ticker so we can look up the
        // current ask prices when validating each curated rule.
        let mut by_ticker: std::collections::HashMap<&str, &stat_curator::StatMarket> =
            std::collections::HashMap::new();
        for m in batch {
            by_ticker.insert(m.ticker.as_str(), m);
        }
        for r in raw {
            let market_str = r.kalshi_ticker.clone();
            let Some(market) = by_ticker.get(market_str.as_str()) else {
                dropped_invalid.push((
                    market_str.clone(),
                    "ticker not in batch — Claude hallucinated".into(),
                ));
                continue;
            };
            match r.into_rule(market.yes_ask_cents, market.no_ask_cents) {
                Ok((rule, _side, audit)) => {
                    all_rules.push(rule);
                    accepted_audit.push(audit);
                }
                Err(why) => dropped_invalid.push((market_str, why)),
            }
        }
        // Be polite to the API.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if batch_failures > 0 {
        warn!(batch_failures, "some batches failed; partial ruleset");
    }
    info!(
        kept = all_rules.len(),
        dropped = dropped_invalid.len(),
        "rule synthesis done"
    );
    for audit in &accepted_audit {
        info!(audit = %audit, "accepted rule");
    }
    for (m, why) in &dropped_invalid {
        warn!(market = %m, why = %why, "dropped invalid rule");
    }

    if args.shadow_db {
        shadow_write_rules(&args.database_url, &all_rules).await?;
        info!(
            n_rules = all_rules.len(),
            "stat shadow rules written disabled to DB"
        );
    }

    if args.write {
        write_rules(&all_rules, &args.output).await?;
        println!(
            "wrote {} rules to {}",
            all_rules.len(),
            args.output.display()
        );
        if let Some(job) = &args.restart_job {
            kickstart_job(job);
        }
    } else {
        let json = serde_json::to_string_pretty(&all_rules)?;
        println!("{json}");
        eprintln!(
            "dry-run: {} rules proposed, {} dropped.  Use --write to commit to {}",
            all_rules.len(),
            dropped_invalid.len(),
            args.output.display()
        );
    }
    Ok(())
}

async fn shadow_write_rules(database_url: &str, rules: &[StatRule]) -> Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(database_url)
        .await
        .with_context(|| format!("connect postgres {database_url}"))?;
    for rule in rules {
        let ticker = rule.kalshi_market.as_str();
        sqlx::query(
            r"
            INSERT INTO markets (ticker, venue, market_type, title, tags, payload)
            VALUES ($1, 'kalshi', 'binary', $2, $3, $4)
            ON CONFLICT (ticker) DO UPDATE
            SET last_updated_at = now(),
                payload = COALESCE(markets.payload, EXCLUDED.payload)
            ",
        )
        .bind(ticker)
        .bind(format!("stat shadow {ticker}"))
        .bind(vec!["stat-shadow".to_string()])
        .bind(serde_json::json!({
            "source": "stat-curator --shadow-db",
            "non_executing": true
        }))
        .execute(&pool)
        .await?;

        sqlx::query(
            r"
            INSERT INTO model_p_snapshots (strategy, ticker, raw_p, model_p, source, detail)
            VALUES ('stat', $1, $2, $2, 'stat-curator-shadow', $3)
            ",
        )
        .bind(ticker)
        .bind(rule.model_p)
        .bind(serde_json::json!({
            "side": side_str(rule.side),
            "min_edge_cents": rule.min_edge_cents,
            "settlement_date": rule.settlement_date,
            "generated_at_utc": rule.generated_at_utc,
            "enabled": false
        }))
        .execute(&pool)
        .await?;

        sqlx::query(
            r"
            INSERT INTO rules (
                strategy, ticker, side, model_p, min_edge_cents,
                expires_at, source, enabled
            ) VALUES ('stat', $1, $2, $3, $4, NULL, 'stat-curator-shadow', false)
            ON CONFLICT (strategy, ticker) DO UPDATE
            SET side = EXCLUDED.side,
                model_p = EXCLUDED.model_p,
                min_edge_cents = EXCLUDED.min_edge_cents,
                source = EXCLUDED.source,
                fitted_at = now(),
                enabled = false
            ",
        )
        .bind(ticker)
        .bind(side_str(rule.side))
        .bind(rule.model_p)
        .bind(i32::try_from(rule.min_edge_cents).unwrap_or(i32::MAX))
        .execute(&pool)
        .await?;
    }
    Ok(())
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Yes => "yes",
        Side::No => "no",
    }
}

async fn write_rules(rules: &[StatRule], output: &std::path::Path) -> Result<()> {
    let json = serde_json::to_string_pretty(rules)?;
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let tmp = output.with_extension("tmp");
    tokio::fs::write(&tmp, &json)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, output)
        .await
        .with_context(|| format!("rename to {}", output.display()))?;
    Ok(())
}

/// Best-effort launchctl kickstart.  We deliberately swallow errors
/// — failure to kickstart shouldn't fail the curator run; the
/// trader will pick up the new rules on its own poll cadence.
fn kickstart_job(job: &str) {
    let Some(uid) = current_uid() else {
        warn!(job = %job, "couldn't resolve uid; skipping kickstart");
        return;
    };
    let target = format!("gui/{uid}/{job}");
    let status = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status();
    match status {
        Ok(s) if s.success() => info!(job = %job, "kickstarted launchd job"),
        Ok(s) => warn!(job = %job, exit = ?s.code(), "launchctl kickstart non-zero exit"),
        Err(e) => warn!(job = %job, error = %e, "launchctl kickstart failed"),
    }
}

/// Resolve the running uid by shelling out to `id -u`.  Avoids a
/// libc dependency just for getuid().  Same pattern as cross-arb-
/// curator.
fn current_uid() -> Option<u32> {
    let out = std::process::Command::new("id").arg("-u").output().ok()?;
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    s.parse().ok()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
