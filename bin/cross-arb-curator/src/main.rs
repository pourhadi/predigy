// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `cross-arb-curator`: scan Kalshi politics + Polymarket markets,
//! ask Claude for high-confidence cross-venue pairs, write the
//! pair file `cross-arb-trader` consumes.
//!
//! ```text
//! cross-arb-curator \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --output ./cross-arb-pairs.txt \
//!     --max-poly 50 --batch-size 25 --max-batches 4 \
//!     --write
//! ```
//!
//! Without `--write`, the proposed pair list is printed to stdout
//! (dry-run). Use it to eyeball Claude's output before committing
//! to the live rule file.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use cross_arb_curator::{
    filter_for_batch, propose_pairs, scan_political_markets, scan_top_markets,
};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "cross-arb-curator",
    about = "Curate cross-venue Kalshi/Polymarket pairs via Claude."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// Output path for the pair file. Format matches what
    /// `cross-arb-trader` reads (one
    /// `KALSHI_TICKER=POLY_TOKEN_ID` per line, comments allowed).
    #[arg(long, default_value = "cross-arb-pairs.txt")]
    output: PathBuf,

    /// Cap on Polymarket markets pulled (sorted by volume desc).
    /// Each batch of size `--batch-size` is sent to Claude.
    #[arg(long, default_value_t = 60)]
    max_poly: usize,

    /// Polymarket markets per Anthropic call. ~25 keeps token
    /// usage predictable given Polymarket's long descriptions.
    #[arg(long, default_value_t = 25)]
    batch_size: usize,

    /// Hard cap on Anthropic calls per run.
    #[arg(long, default_value_t = 4)]
    max_batches: usize,

    /// Polymarket liquidity floor (USD). Anything thinner gets
    /// dropped before going to Claude — pairs against unfillable
    /// Polymarket sides are useless.
    #[arg(long, default_value_t = 5_000.0)]
    min_poly_liquidity: f64,

    /// Write the curated pairs to `--output`. Without this, the
    /// pairs are printed to stdout (dry-run).
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

    info!("scanning Kalshi political/elections/world/economics markets");
    let kalshi = scan_political_markets(&rest)
        .await
        .map_err(|e| anyhow!("kalshi scan: {e}"))?;
    info!(found = kalshi.len(), "kalshi markets discovered");
    if kalshi.is_empty() {
        return Err(anyhow!(
            "no actionable Kalshi political markets — bailing rather than writing an empty pair file"
        ));
    }

    info!("scanning top Polymarket markets by volume");
    let poly = scan_top_markets(args.max_poly, args.min_poly_liquidity)
        .await
        .map_err(|e| anyhow!("polymarket scan: {e}"))?;
    info!(
        found = poly.len(),
        min_liquidity_usd = args.min_poly_liquidity,
        "polymarket markets discovered"
    );
    if poly.is_empty() {
        return Err(anyhow!(
            "no actionable Polymarket markets at the given liquidity floor"
        ));
    }

    let mut all_pairs: Vec<(String, String, String)> = Vec::new();
    let mut dropped: Vec<(String, String, String)> = Vec::new();
    let mut batch_failures = 0usize;
    for (i, batch) in poly.chunks(args.batch_size).enumerate() {
        if i >= args.max_batches {
            warn!(
                skipped = poly.len() - i * args.batch_size,
                "max_batches cap hit; stopping"
            );
            break;
        }
        let kalshi_filtered = filter_for_batch(&kalshi, batch);
        if kalshi_filtered.is_empty() {
            info!(
                batch = i,
                "no Kalshi markets share keywords with this Polymarket batch; skipping"
            );
            continue;
        }
        info!(
            batch = i,
            kalshi_total = kalshi.len(),
            kalshi_filtered = kalshi_filtered.len(),
            poly = batch.len(),
            "calling claude on batch"
        );
        let raw = match propose_pairs(&kalshi_filtered, batch).await {
            Ok(r) => r,
            Err(e) => {
                warn!(batch = i, error = %e, "batch failed; continuing");
                batch_failures += 1;
                continue;
            }
        };
        for r in raw {
            match r.validate() {
                Ok(()) => all_pairs.push((
                    r.kalshi_ticker.clone(),
                    r.poly_token_id.clone(),
                    r.reasoning.clone(),
                )),
                Err(why) => dropped.push((r.kalshi_ticker, r.poly_token_id, why)),
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if batch_failures > 0 {
        warn!(batch_failures, "some batches failed; partial pair set");
    }

    // Dedup by Kalshi ticker — same Kalshi market may match
    // multiple Polymarket twins; keep the first proposal.
    all_pairs.sort_by(|a, b| a.0.cmp(&b.0));
    all_pairs.dedup_by(|a, b| a.0 == b.0);

    info!(
        kept = all_pairs.len(),
        dropped = dropped.len(),
        "pair synthesis done"
    );
    for (k, p, why) in &dropped {
        warn!(kalshi = %k, poly_token = %p, why = %why, "dropped invalid pair");
    }

    if args.write {
        let mut out = String::new();
        out.push_str("# cross-arb pairs — generated by cross-arb-curator\n");
        out.push_str("# format: KALSHI_TICKER=POLYMARKET_YES_TOKEN_ID\n");
        out.push_str("# review reasoning above each pair before running cross-arb-trader live\n\n");
        for (k, p, reasoning) in &all_pairs {
            let _ = writeln!(out, "# {reasoning}\n{k}={p}\n");
        }
        if let Some(parent) = args.output.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let tmp = args.output.with_extension("tmp");
        tokio::fs::write(&tmp, &out)
            .await
            .with_context(|| format!("write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &args.output)
            .await
            .with_context(|| format!("rename to {}", args.output.display()))?;
        println!(
            "wrote {} pairs to {}",
            all_pairs.len(),
            args.output.display()
        );
    } else {
        for (k, p, reasoning) in &all_pairs {
            println!("# {reasoning}");
            println!("{k}={p}");
            println!();
        }
        eprintln!(
            "dry-run: {} pairs proposed, {} dropped. Use --write to commit to {}",
            all_pairs.len(),
            dropped.len(),
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
