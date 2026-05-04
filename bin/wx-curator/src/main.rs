// Vendor names appear throughout the doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `wx-curator`: scan Kalshi weather markets, ask Claude to propose
//! `LatencyRule`s, validate, write to a JSON file the
//! `latency-trader` binary can consume.
//!
//! ```text
//! wx-curator \
//!   --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!   --output ./wx-rules.json \
//!   --batch-size 30 --max-batches 3 \
//!   --write     # omit for dry-run
//! ```
//!
//! Without `--write`, the binary prints the proposed rules to stdout
//! as pretty JSON and exits — useful for eyeballing the LLM's output
//! before committing it to the rule file the live trader reads.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use latency_trader::LatencyRule;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wx_curator::{propose_rules, scan_weather_markets};

#[derive(Debug, Parser)]
#[command(
    name = "wx-curator",
    about = "Curate latency-trader rules for Kalshi weather markets via Claude."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// Output path for the rule JSON. Format matches what
    /// `latency-trader --rule-file` reads.
    #[arg(long, default_value = "wx-rules.json")]
    output: PathBuf,

    /// Markets per Anthropic call. Larger batches amortise the
    /// system-prompt cost but risk truncating output. ~30-40 is
    /// the sweet spot for Sonnet 4.6 with our 4 KB max_tokens.
    #[arg(long, default_value_t = 30)]
    batch_size: usize,

    /// Hard cap on Anthropic calls per run. Stops a runaway scan
    /// from racking up API spend if Kalshi returns thousands of
    /// markets. Each call costs ~\$0.03.
    #[arg(long, default_value_t = 5)]
    max_batches: usize,

    /// Write the curated rules to `--output`. Without this, the
    /// rules are printed to stdout (dry-run mode).
    #[arg(long, default_value_t = false)]
    write: bool,
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

    info!("scanning Kalshi for open weather markets");
    let markets = scan_weather_markets(&rest)
        .await
        .map_err(|e| anyhow!("scan: {e}"))?;
    info!(found = markets.len(), "weather markets discovered");
    if markets.is_empty() {
        return Err(anyhow!(
            "no actionable weather markets found — bailing rather than writing an empty rule file"
        ));
    }

    let max_markets = args.batch_size * args.max_batches;
    let to_evaluate: Vec<_> = markets.into_iter().take(max_markets).collect();
    info!(
        evaluating = to_evaluate.len(),
        batch_size = args.batch_size,
        "calling Anthropic"
    );

    let mut all_rules: Vec<LatencyRule> = Vec::new();
    let mut dropped_invalid: Vec<(String, String)> = Vec::new();
    let mut batch_failures = 0usize;
    for (i, batch) in to_evaluate.chunks(args.batch_size).enumerate() {
        info!(batch = i, n = batch.len(), "calling claude on batch");
        let raw = match propose_rules(batch).await {
            Ok(r) => r,
            Err(e) => {
                // One batch failing (decode error from a truncated
                // response, transient API hiccup, etc.) shouldn't
                // throw out the rules from the batches that
                // succeeded.
                warn!(batch = i, error = %e, "batch failed; continuing");
                batch_failures += 1;
                continue;
            }
        };
        for r in raw {
            let market = r.market_ticker.clone();
            match r.into_rule() {
                Ok(rule) => all_rules.push(rule),
                Err(why) => dropped_invalid.push((market, why)),
            }
        }
        // Be polite to the API and don't burst.
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
    for (m, why) in &dropped_invalid {
        warn!(market = %m, why = %why, "dropped invalid rule");
    }

    if args.write {
        let json = serde_json::to_string_pretty(&all_rules)?;
        if let Some(parent) = args.output.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let tmp = args.output.with_extension("tmp");
        tokio::fs::write(&tmp, &json)
            .await
            .with_context(|| format!("write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &args.output)
            .await
            .with_context(|| format!("rename to {}", args.output.display()))?;
        println!(
            "wrote {} rules to {}",
            all_rules.len(),
            args.output.display()
        );
    } else {
        let json = serde_json::to_string_pretty(&all_rules)?;
        println!("{json}");
        eprintln!(
            "dry-run: {} rules proposed, {} dropped. Use --write to commit to {}",
            all_rules.len(),
            dropped_invalid.len(),
            args.output.display()
        );
    }
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
