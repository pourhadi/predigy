// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `cross-arb-curator`: scan Kalshi politics + Polymarket markets,
//! ask Claude for high-confidence cross-venue pairs, write the
//! pair file `cross-arb-trader` consumes.
//!
//! Two run modes:
//!
//! - **One-shot** (default): one full sweep, write the file, exit.
//!   Used for initial seeding and for testing.
//! - **Watch** (`--watch`): long-running daemon. Each tick, scan
//!   for NEW Polymarket candidates (those not in the persistent
//!   `seen_poly_ids` set), drop pairs whose Kalshi market settled,
//!   and only call Claude when there's genuinely new material to
//!   evaluate. The whole point: a tick with no fresh Polymarket
//!   markets costs ~2 cheap REST calls and zero LLM tokens. Only
//!   actually new candidates pay for Anthropic.
//!
//! ```text
//! cross-arb-curator \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --output ~/.config/predigy/cross-arb-pairs.txt \
//!     --state  ~/.config/predigy/cross-arb-state.json \
//!     --max-poly 100 --batch-size 25 --max-batches 4 \
//!     --watch --interval-secs 600 \
//!     --restart-job com.predigy.cross-arb \
//!     --write
//! ```

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use cross_arb_curator::{
    CuratorState, DEFAULT_CATEGORIES, KalshiMarket, PolyMarket, StoredPair, filter_for_batch,
    propose_pairs, scan_open_markets, scan_top_markets,
};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

    /// Persistent state file. Tracks the active pair set + every
    /// Polymarket id ever sent to Claude. Required for `--watch`;
    /// optional for one-shot.
    #[arg(long)]
    state: Option<PathBuf>,

    /// Cap on Polymarket markets pulled per tick (sorted by volume
    /// desc). Higher values surface more new candidates per tick.
    #[arg(long, default_value_t = 100)]
    max_poly: usize,

    /// Polymarket markets per Anthropic call. ~25 keeps token
    /// usage predictable given Polymarket's long descriptions.
    #[arg(long, default_value_t = 25)]
    batch_size: usize,

    /// Hard cap on Anthropic calls per tick.
    #[arg(long, default_value_t = 4)]
    max_batches: usize,

    /// Polymarket liquidity floor (USD). Anything thinner gets
    /// dropped before going to Claude — pairs against unfillable
    /// Polymarket sides are useless.
    #[arg(long, default_value_t = 5_000.0)]
    min_poly_liquidity: f64,

    /// Settlement-horizon cap (days). Cross-arb is a convergence
    /// strategy — capturing short-term price divergences that
    /// resolve when both venues see the same data. Multi-month
    /// event contracts (e.g. annual macro markets) lock capital
    /// for months without giving the strategy room to capture an
    /// edge, so we filter them out at the curator. Default 60 d.
    /// Existing pairs whose Polymarket market is over the horizon
    /// (or no longer listed) are dropped from state on each tick.
    #[arg(long, default_value_t = 60)]
    max_days_to_settle: i64,

    /// Long-running mode: tick every `--interval-secs`, only call
    /// Claude on new Polymarket candidates each tick.
    #[arg(long, default_value_t = false)]
    watch: bool,

    /// Watch-mode tick interval. Default 10 min — captures new
    /// Polymarket markets within ~10 min of them surfacing in the
    /// top-volume list.
    #[arg(long, default_value_t = 600)]
    interval_secs: u64,

    /// On a successful pair-set change, kickstart this launchd job
    /// so it picks up the new pair file. Optional.
    #[arg(long)]
    restart_job: Option<String>,

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

    if args.watch {
        let state_path = args
            .state
            .clone()
            .ok_or_else(|| anyhow!("--watch requires --state to persist seen-set across ticks"))?;
        info!(
            interval_secs = args.interval_secs,
            state = %state_path.display(),
            "watch mode enabled"
        );
        let mut tick = tokio::time::interval(Duration::from_secs(args.interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stop = wait_for_ctrl_c();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => {
                    info!("watch: stop");
                    return Ok(());
                }
                _ = tick.tick() => {
                    if let Err(e) = run_tick(&rest, &args, Some(&state_path)).await {
                        warn!(error = %e, "tick failed; will retry");
                    }
                }
            }
        }
    }

    // One-shot mode.
    run_tick(&rest, &args, args.state.as_deref()).await?;
    Ok(())
}

/// Single curator pass. Pure with respect to side effects beyond
/// the file system + Anthropic API: load state, scan, diff, call
/// Claude on fresh candidates only, write outputs.
async fn run_tick(rest: &RestClient, args: &Args, state_path: Option<&Path>) -> Result<()> {
    let mut state = state_path.map(CuratorState::load).unwrap_or_default();
    let prev_pair_count = state.pairs.len();

    let now_unix = current_unix();
    let kalshi_max_secs = args.max_days_to_settle.saturating_mul(86_400);
    info!(categories = ?DEFAULT_CATEGORIES, max_days_to_settle = args.max_days_to_settle, "scanning Kalshi markets");
    let kalshi = scan_open_markets(rest, DEFAULT_CATEGORIES, now_unix, kalshi_max_secs)
        .await
        .map_err(|e| anyhow!("kalshi scan: {e}"))?;
    let kalshi_open: HashSet<&str> = kalshi.iter().map(|k| k.ticker.as_str()).collect();
    let dropped_settled = state.retain_open(&kalshi_open);
    if !dropped_settled.is_empty() {
        info!(
            dropped = dropped_settled.len(),
            tickers = ?dropped_settled,
            "dropped pairs whose Kalshi side is no longer open or fell out of horizon"
        );
    }
    info!(found = kalshi.len(), "kalshi markets in horizon");
    if kalshi.is_empty() {
        warn!("no actionable Kalshi markets in horizon — skipping LLM call");
        write_outputs(&state, args, state_path, prev_pair_count).await?;
        return Ok(());
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

    // Apply settlement-horizon filter: cross-arb is a convergence
    // strategy and shouldn't be holding multi-month event contracts.
    let cutoff_unix = now_unix.saturating_add(args.max_days_to_settle * 86_400);
    let in_horizon: std::collections::HashMap<String, bool> = poly
        .iter()
        .map(|p| {
            let ok = poly_in_horizon(p, now_unix, cutoff_unix);
            (p.id.clone(), ok)
        })
        .collect();
    let dropped_horizon = state.retain_pairs(|p| {
        in_horizon
            .get(p.poly_market_id.as_str())
            .copied()
            .unwrap_or(false)
    });
    if !dropped_horizon.is_empty() {
        info!(
            dropped = dropped_horizon.len(),
            tickers = ?dropped_horizon,
            max_days_to_settle = args.max_days_to_settle,
            "dropped pairs out of settlement horizon (or no longer listed)"
        );
    }

    let poly: Vec<PolyMarket> = poly
        .into_iter()
        .filter(|p| in_horizon.get(p.id.as_str()).copied().unwrap_or(false))
        .collect();
    info!(
        in_horizon = poly.len(),
        max_days_to_settle = args.max_days_to_settle,
        "polymarket markets in settlement horizon"
    );

    // The whole point of incremental: only feed NEW Polymarket
    // markets to Claude. Anything we've sent before — paired or
    // rejected — is in `seen_poly_ids` and gets skipped.
    let seen = state.seen_set();
    let already_paired_poly = state.paired_poly();
    let fresh_poly: Vec<PolyMarket> = poly
        .into_iter()
        .filter(|p| !seen.contains(p.id.as_str()) && !already_paired_poly.contains(p.id.as_str()))
        .collect();
    info!(fresh = fresh_poly.len(), "fresh Polymarket candidates");
    if fresh_poly.is_empty() {
        info!("no fresh Polymarket candidates this tick — skipping LLM call");
        write_outputs(&state, args, state_path, prev_pair_count).await?;
        return Ok(());
    }

    // Don't propose against Kalshi markets we've already paired —
    // each Kalshi market should only ever appear in one pair, and
    // re-proposing wastes tokens.
    let already_paired_kalshi = state.paired_kalshi();
    let kalshi_available: Vec<KalshiMarket> = kalshi
        .iter()
        .filter(|k| !already_paired_kalshi.contains(k.ticker.as_str()))
        .cloned()
        .collect();
    if kalshi_available.is_empty() {
        info!("no unpaired Kalshi markets — every actionable ticker is already paired");
        // Still record fresh poly ids as seen so we don't re-pay for them on the next tick.
        state.record_seen(fresh_poly.iter().map(|p| p.id.clone()));
        write_outputs(&state, args, state_path, prev_pair_count).await?;
        return Ok(());
    }

    let mut batch_failures = 0usize;
    for (i, batch) in fresh_poly.chunks(args.batch_size).enumerate() {
        if i >= args.max_batches {
            warn!(
                skipped = fresh_poly.len() - i * args.batch_size,
                "max_batches cap hit; remaining candidates deferred to next tick"
            );
            break;
        }
        let kalshi_filtered = filter_for_batch(&kalshi_available, batch);
        if kalshi_filtered.is_empty() {
            info!(
                batch = i,
                "no Kalshi markets share keywords with this Polymarket batch; skipping"
            );
            // Still mark these as seen — keyword overlap is unlikely
            // to suddenly start matching on the next tick.
            state.record_seen(batch.iter().map(|p| p.id.clone()));
            continue;
        }
        info!(
            batch = i,
            kalshi_total = kalshi.len(),
            kalshi_unpaired = kalshi_available.len(),
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
        // Whatever Claude saw, mark the batch's poly ids as seen
        // — including ones it rejected, since revisiting them later
        // is unlikely to flip Claude's verdict.
        state.record_seen(batch.iter().map(|p| p.id.clone()));
        let poly_id_by_token: std::collections::HashMap<String, String> = batch
            .iter()
            .map(|p| (p.yes_token_id.clone(), p.id.clone()))
            .collect();
        for r in raw {
            match r.validate() {
                Ok(()) => {
                    if state.paired_kalshi().contains(r.kalshi_ticker.as_str()) {
                        warn!(
                            kalshi = %r.kalshi_ticker,
                            "Claude proposed pair on already-paired Kalshi ticker; dropping"
                        );
                        continue;
                    }
                    let Some(poly_market_id) = poly_id_by_token.get(&r.poly_token_id).cloned()
                    else {
                        warn!(
                            kalshi = %r.kalshi_ticker,
                            poly_token = %r.poly_token_id,
                            "Claude returned token id not in this batch; dropping"
                        );
                        continue;
                    };
                    info!(
                        kalshi = %r.kalshi_ticker,
                        poly_market_id = %poly_market_id,
                        "new pair accepted"
                    );
                    state.add_pair(StoredPair {
                        kalshi_ticker: r.kalshi_ticker,
                        poly_token_id: r.poly_token_id,
                        poly_market_id,
                        reasoning: r.reasoning,
                        added_unix: now_unix,
                    });
                }
                Err(why) => warn!(
                    kalshi = %r.kalshi_ticker,
                    poly_token = %r.poly_token_id,
                    why,
                    "dropped invalid pair"
                ),
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if batch_failures > 0 {
        warn!(batch_failures, "some batches failed; partial pair set");
    }

    write_outputs(&state, args, state_path, prev_pair_count).await
}

async fn write_outputs(
    state: &CuratorState,
    args: &Args,
    state_path: Option<&Path>,
    prev_pair_count: usize,
) -> Result<()> {
    let pair_count = state.pairs.len();
    let pair_set_changed = pair_count != prev_pair_count;

    if args.write {
        let mut out = String::new();
        out.push_str("# cross-arb pairs — generated by cross-arb-curator\n");
        out.push_str("# format: KALSHI_TICKER=POLYMARKET_YES_TOKEN_ID\n");
        out.push_str("# review reasoning above each pair before running cross-arb-trader live\n\n");
        for p in &state.pairs {
            let _ = writeln!(
                out,
                "# {}\n{}={}\n",
                p.reasoning, p.kalshi_ticker, p.poly_token_id
            );
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
        info!(
            kept = pair_count,
            output = %args.output.display(),
            "pair file written"
        );
    } else {
        for p in &state.pairs {
            println!("# {}", p.reasoning);
            println!("{}={}", p.kalshi_ticker, p.poly_token_id);
            println!();
        }
        eprintln!(
            "dry-run: {} pairs in state. Use --write to commit to {}",
            pair_count,
            args.output.display()
        );
    }

    if let Some(path) = state_path {
        state
            .save(path)
            .with_context(|| format!("save state to {}", path.display()))?;
    }

    if pair_set_changed
        && args.write
        && let Some(label) = &args.restart_job
    {
        kickstart_job(label);
    }
    Ok(())
}

fn kickstart_job(label: &str) {
    // launchctl kickstart -k restarts the job (or no-ops if it's
    // not loaded). We don't want a missing job to crash the
    // curator — log + carry on.
    let Some(uid) = current_uid() else {
        warn!(job = label, "couldn't resolve uid; skipping kickstart");
        return;
    };
    let target = format!("gui/{uid}/{label}");
    let status = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status();
    match status {
        Ok(s) if s.success() => info!(job = label, "kickstarted launchd job"),
        Ok(s) => warn!(job = label, code = ?s.code(), "kickstart failed"),
        Err(e) => warn!(job = label, error = %e, "kickstart spawn failed"),
    }
}

/// Resolve the current uid via `/usr/bin/id -u`. Avoids pulling in
/// a libc crate or relying on `$UID` (which isn't always exported).
fn current_uid() -> Option<u32> {
    let out = std::process::Command::new("id").arg("-u").output().ok()?;
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    s.parse().ok()
}

fn current_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Settlement-horizon predicate: in-horizon iff Polymarket reports
/// an `endDateIso` within `[now, now + max_days]`. Markets without
/// an end date are conservatively dropped — we'd rather skip a
/// good pair than load up on something we can't reason about.
fn poly_in_horizon(p: &PolyMarket, now_unix: i64, cutoff_unix: i64) -> bool {
    let Some(iso) = p.end_date_iso.as_deref() else {
        return false;
    };
    let Some(t) = parse_iso8601_to_unix(iso) else {
        return false;
    };
    t > now_unix && t <= cutoff_unix
}

/// Minimal RFC3339 parser. Accepts both `YYYY-MM-DD` and
/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` (Polymarket's `endDateIso` is
/// usually the date-only form; some markets include a time).
fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year: i32 = std::str::from_utf8(bytes.get(0..4)?).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(bytes.get(5..7)?).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(bytes.get(8..10)?).ok()?.parse().ok()?;
    let (hour, min, sec) = if bytes.len() >= 19 && bytes[10] == b'T' {
        (
            std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?,
            std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?,
            std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?,
        )
    } else {
        (0u32, 0u32, 0u32)
    };
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour) * 3_600 + i64::from(min) * 60 + i64::from(sec))
}

#[allow(clippy::cast_possible_wrap)]
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m_signed = m as i32;
    let mp = if m_signed > 2 {
        m_signed - 3
    } else {
        m_signed + 9
    };
    let doy = (153 * mp + 2) as u32 / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

async fn wait_for_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %e, "ctrl_c handler failed; running until killed");
        loop {
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
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
