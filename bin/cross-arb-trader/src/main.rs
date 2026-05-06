// Same rationale as the lib half: Polymarket / Kalshi product names
// in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `cross-arb-trader`: trade Kalshi when its prices diverge from
//! Polymarket's reference. Stat-arb, not pure arb — Polymarket is
//! never executed against.
//!
//! ```text
//! cross-arb-trader \
//!     --pair "FED-23DEC-T3.00=0xabc..." \
//!     --pair "FED-23DEC-T3.25=0xdef..." \
//!     --kalshi-key-id $KALSHI_KEY_ID \
//!     --kalshi-pem    /path/to/kalshi.pem \
//!     --max-size 10 --min-edge-cents 2
//! ```

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use cross_arb_trader::{CrossArbConfig, CrossArbStrategy};
use predigy_core::market::MarketTicker;
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::{Channel as KalshiChannel, Client as MdClient};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent, spawn_kill_watcher};
use predigy_poly_md::{Client as PolyClient, Event as PolyEvent};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "cross-arb-trader",
    about = "Cross-venue stat-arb: trade Kalshi when it diverges from Polymarket reference."
)]
struct Args {
    /// Pair Kalshi market ↔ Polymarket asset_id with `=`. Pass once
    /// per pair, e.g. `--pair FED-23DEC=0xabc`. Required iff
    /// `--pair-file` is not given.
    #[arg(long = "pair", value_parser = parse_pair)]
    pairs: Vec<(MarketTicker, String)>,

    /// Path to a pair file (`KALSHI_TICKER=POLY_ASSET_ID` per line,
    /// `#` comments allowed). When set, the daemon polls the file
    /// every `--pair-file-poll-secs`, diffs against the current
    /// pair set, and dynamically subscribes new pairs +
    /// unsubscribes removed ones. Used in production where the
    /// curator daemon writes the file.
    #[arg(long)]
    pair_file: Option<PathBuf>,

    /// How often to re-read `--pair-file`. Lower values pick up
    /// curator-written changes faster at the cost of a syscall per
    /// tick.
    #[arg(long, default_value_t = 30)]
    pair_file_poll_secs: u64,

    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,

    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,

    #[arg(long)]
    kalshi_ws_endpoint: Option<Url>,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,
    #[arg(long)]
    poly_ws_endpoint: Option<Url>,

    #[arg(long, default_value = "cross-arb")]
    strategy_id: String,

    #[arg(long, default_value_t = 100)]
    max_contracts_per_side: u32,
    #[arg(long, default_value_t = 5_000)]
    max_notional_cents_per_side: u64,
    #[arg(long, default_value_t = 50_000)]
    max_account_notional_cents: u64,
    #[arg(long, default_value_t = 5_000)]
    max_daily_loss_cents: u64,
    #[arg(long, default_value_t = 20)]
    max_orders_per_window: u32,
    #[arg(long, default_value_t = 1_000)]
    rate_window_ms: u64,

    #[arg(long, default_value_t = 2)]
    min_edge_cents: u32,
    #[arg(long, default_value_t = 25)]
    max_size: u32,
    #[arg(long, default_value_t = 500)]
    cooldown_ms: u64,
    #[arg(long, default_value_t = 500)]
    fills_poll_ms: u64,

    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// File path for durable cid storage. Strongly recommended in
    /// production; without it, cids restart from 0 on each run.
    #[arg(long)]
    cid_store: Option<PathBuf>,

    /// Path to a JSON file the OMS snapshots its state to. See
    /// `arb-trader --help` for details.
    #[arg(long)]
    oms_state: Option<PathBuf>,

    /// Out-of-process kill switch. Path to a flag file the daemon
    /// polls every 2 s; when its content starts with `armed`, the
    /// OMS rejects new submits. Set to `""` to disable.
    #[arg(long, default_value = "~/.config/predigy/kill-switch.flag")]
    kill_flag: PathBuf,
}

fn parse_pair(s: &str) -> std::result::Result<(MarketTicker, String), String> {
    let (kalshi, poly) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KALSHI_TICKER=POLY_ASSET_ID, got {s:?}"))?;
    if kalshi.is_empty() || poly.is_empty() {
        return Err(format!("empty side in pair {s:?}"));
    }
    Ok((MarketTicker::new(kalshi), poly.to_string()))
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;
    let rest_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("REST signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(rest_signer))
    } else {
        RestClient::authed(rest_signer)
    }
    .map_err(|e| anyhow!("build REST client: {e}"))?;

    let ws_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("WS signer: {e}"))?;
    let kalshi_client = if let Some(endpoint) = &args.kalshi_ws_endpoint {
        MdClient::with_endpoint(endpoint.clone(), Some(ws_signer))
    } else {
        MdClient::new(ws_signer).map_err(|e| anyhow!("build kalshi WS client: {e}"))?
    };
    let mut kalshi_md = kalshi_client.connect();

    let poly_client = if let Some(endpoint) = &args.poly_ws_endpoint {
        PolyClient::with_endpoint(endpoint.clone())
    } else {
        PolyClient::new().map_err(|e| anyhow!("build poly WS client: {e}"))?
    };
    let mut poly_md = poly_client.connect();

    let limits = Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: args.max_contracts_per_side,
            max_notional_cents_per_side: args.max_notional_cents_per_side,
        },
        per_market_overrides: HashMap::new(),
        account: AccountLimits {
            max_gross_notional_cents: args.max_account_notional_cents,
            max_daily_loss_cents: args.max_daily_loss_cents,
        },
        rate: RateLimits {
            max_orders_per_window: args.max_orders_per_window,
            window: Duration::from_millis(args.rate_window_ms),
        },
    };
    info!(?limits, "risk limits");

    let state_backing = if let Some(path) = &args.oms_state {
        predigy_oms::StateBacking::Persistent { path: path.clone() }
    } else {
        warn!("no --oms-state; daily-pnl + kill-switch + orders reset on every run");
        predigy_oms::StateBacking::InMemory
    };
    let cid_backing = if let Some(path) = &args.cid_store {
        CidBacking::Persistent {
            store_path: path.clone(),
            chunk_size: 1_000,
        }
    } else {
        warn!(
            "no --cid-store; cids will restart from 0 each run. Use --cid-store \
             in production to avoid venue duplicate-id rejects across restarts."
        );
        CidBacking::InMemory { start_seq: 0 }
    };

    let (executor, reports) = RestExecutor::spawn(
        rest,
        PollerConfig {
            interval: Duration::from_millis(args.fills_poll_ms),
            initial_lookback: Duration::from_mins(1),
        },
    );

    let mut oms = Oms::try_spawn(
        OmsConfig {
            strategy_id: args.strategy_id.clone(),
            cid_backing,
            state_backing,
        },
        RiskEngine::new(limits),
        executor,
        reports,
    )
    .map_err(|e| anyhow!("oms init: {e}"))?;

    let kill_flag = expand_tilde(&args.kill_flag);
    if !kill_flag.as_os_str().is_empty() {
        info!(kill_flag = %kill_flag.display(), "kill-flag watcher armed");
        spawn_kill_watcher(oms.control(), kill_flag, Duration::from_secs(2));
    }

    let pair_file_path = args.pair_file.as_ref().map(|p| expand_tilde(p));
    let initial_map = build_initial_pair_map(&args.pairs, pair_file_path.as_deref())?;
    if initial_map.is_empty() {
        return Err(anyhow!(
            "no pairs configured: pass --pair or --pair-file with at least one entry"
        ));
    }
    let mut strategy = CrossArbStrategy::new(
        CrossArbConfig {
            min_edge_cents: args.min_edge_cents,
            max_size: args.max_size,
            cooldown: Duration::from_millis(args.cooldown_ms),
        },
        initial_map.clone(),
    );

    // Track current pair set for the file watcher's diff logic.
    let mut active_pairs: HashMap<MarketTicker, String> = initial_map;
    let mut pair_file_mtime: Option<std::time::SystemTime> = pair_file_path
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    let kalshi_market_strs: Vec<String> = strategy
        .kalshi_markets()
        .map(|m| m.as_str().to_string())
        .collect();
    let poly_assets: Vec<String> = strategy.poly_assets().map(String::from).collect();

    let kalshi_req_id = kalshi_md
        .subscribe(
            &[
                KalshiChannel::OrderbookDelta,
                KalshiChannel::Ticker,
                KalshiChannel::Trade,
            ],
            &kalshi_market_strs,
        )
        .await
        .map_err(|e| anyhow!("kalshi subscribe: {e}"))?;
    poly_md
        .subscribe(&poly_assets)
        .await
        .map_err(|e| anyhow!("poly subscribe: {e}"))?;
    info!(
        kalshi_req_id,
        kalshi_markets = ?kalshi_market_strs,
        poly_assets = ?poly_assets,
        dry_run = args.dry_run,
        "cross-arb subscribed"
    );

    let mut books: HashMap<MarketTicker, predigy_book::OrderBook> = kalshi_market_strs
        .iter()
        .map(|m| {
            (
                MarketTicker::new(m),
                predigy_book::OrderBook::new(m.clone()),
            )
        })
        .collect();

    let stop = wait_for_ctrl_c();
    tokio::pin!(stop);

    let mut pair_tick = tokio::time::interval(Duration::from_secs(args.pair_file_poll_secs));
    pair_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Burn the immediate first tick so we don't re-read on startup.
    pair_tick.tick().await;

    loop {
        tokio::select! {
            () = &mut stop => {
                info!("cross-arb received stop");
                break;
            }
            ev = kalshi_md.next_event() => {
                let Some(ev) = ev else { break; };
                handle_kalshi(ev, &mut books, &mut strategy, &oms, args.dry_run).await;
            }
            ev = poly_md.next_event() => {
                let Some(ev) = ev else { break; };
                handle_poly(ev, &mut strategy);
            }
            ev = oms.next_event() => {
                let Some(ev) = ev else { break; };
                log_oms_event(&ev);
            }
            _ = pair_tick.tick(), if pair_file_path.is_some() => {
                let path = pair_file_path.as_deref().expect("guarded by select condition");
                match maybe_reload_pairs(path, &mut pair_file_mtime) {
                    Ok(Some(next_map)) => {
                        apply_pair_diff(
                            &active_pairs,
                            &next_map,
                            &mut strategy,
                            &mut books,
                            &mut kalshi_md,
                            &mut poly_md,
                        ).await;
                        active_pairs = next_map;
                    }
                    Ok(None) => { /* no change */ }
                    Err(e) => warn!(error = %e, "pair-file reload failed; keeping current set"),
                }
            }
        }
    }
    oms.close().await;
    Ok(())
}

fn build_initial_pair_map(
    cli: &[(MarketTicker, String)],
    pair_file: Option<&std::path::Path>,
) -> Result<HashMap<MarketTicker, String>> {
    if let Some(path) = pair_file {
        let parsed =
            cross_arb_trader::pair_file::read(path).context("read --pair-file at startup")?;
        let mut map: HashMap<MarketTicker, String> = parsed
            .into_iter()
            .map(|(k, p)| (MarketTicker::new(k), p))
            .collect();
        // CLI --pair entries are merged on top, useful for hand-
        // testing additions without editing the file.
        for (k, p) in cli {
            map.insert(k.clone(), p.clone());
        }
        Ok(map)
    } else {
        Ok(cli.iter().cloned().collect())
    }
}

/// Returns `Some(map)` if the file changed since `last_mtime` and
/// parsed cleanly; `None` if there was no change. Updates
/// `last_mtime` on a successful read.
fn maybe_reload_pairs(
    path: &std::path::Path,
    last_mtime: &mut Option<std::time::SystemTime>,
) -> Result<Option<HashMap<MarketTicker, String>>> {
    let meta = std::fs::metadata(path).context("stat pair-file")?;
    let mtime = meta.modified().context("pair-file mtime")?;
    if Some(mtime) == *last_mtime {
        return Ok(None);
    }
    let parsed = cross_arb_trader::pair_file::read(path).context("re-read pair-file")?;
    let map: HashMap<MarketTicker, String> = parsed
        .into_iter()
        .map(|(k, p)| (MarketTicker::new(k), p))
        .collect();
    *last_mtime = Some(mtime);
    Ok(Some(map))
}

/// Apply the diff between the previous and new pair set: subscribe
/// new pairs to both venues, remove gone pairs from the strategy.
/// Note: we don't unsubscribe gone pairs from the WS feeds — the
/// strategy's `evaluate()` is keyed by the active pair map and
/// silently no-ops on a market that's no longer paired, so the
/// only cost is bandwidth on a stale subscription. Acceptable for
/// the low-churn cross-arb workload.
async fn apply_pair_diff(
    prev: &HashMap<MarketTicker, String>,
    next: &HashMap<MarketTicker, String>,
    strategy: &mut CrossArbStrategy,
    books: &mut HashMap<MarketTicker, predigy_book::OrderBook>,
    kalshi_md: &mut predigy_kalshi_md::Connection,
    poly_md: &mut predigy_poly_md::Connection,
) {
    let added: Vec<(MarketTicker, String)> = next
        .iter()
        .filter(|(k, _)| !prev.contains_key(*k))
        .map(|(k, p)| (k.clone(), p.clone()))
        .collect();
    let removed: Vec<MarketTicker> = prev
        .keys()
        .filter(|k| !next.contains_key(*k))
        .cloned()
        .collect();
    if added.is_empty() && removed.is_empty() {
        return;
    }
    info!(
        added = added.len(),
        removed = removed.len(),
        active_now = next.len(),
        "pair-file change detected; applying diff"
    );
    for (k, _) in &added {
        strategy.add_pair(k.clone(), next[k].clone());
        books
            .entry(k.clone())
            .or_insert_with(|| predigy_book::OrderBook::new(k.as_str().to_string()));
    }
    if !added.is_empty() {
        let kalshi_strs: Vec<String> = added.iter().map(|(k, _)| k.as_str().to_string()).collect();
        let poly_strs: Vec<String> = added.iter().map(|(_, p)| p.clone()).collect();
        if let Err(e) = kalshi_md
            .subscribe(
                &[
                    KalshiChannel::OrderbookDelta,
                    KalshiChannel::Ticker,
                    KalshiChannel::Trade,
                ],
                &kalshi_strs,
            )
            .await
        {
            warn!(error = %e, ?kalshi_strs, "kalshi subscribe (reload) failed");
        }
        if let Err(e) = poly_md.subscribe(&poly_strs).await {
            warn!(error = %e, ?poly_strs, "poly subscribe (reload) failed");
        }
        info!(?kalshi_strs, ?poly_strs, "subscribed new pairs");
    }
    for k in removed {
        if let Some(asset) = strategy.remove_pair(&k) {
            books.remove(&k);
            info!(market = %k, asset, "removed pair (still subscribed; ignored by strategy)");
        }
    }
}

async fn handle_kalshi(
    ev: predigy_kalshi_md::Event,
    books: &mut HashMap<MarketTicker, predigy_book::OrderBook>,
    strategy: &mut CrossArbStrategy,
    oms: &predigy_oms::OmsHandle,
    dry_run: bool,
) {
    use predigy_book::ApplyOutcome;
    use predigy_kalshi_md::Event as KEvent;
    let market = match ev {
        KEvent::Snapshot {
            market, snapshot, ..
        } => {
            let key = MarketTicker::new(&market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| predigy_book::OrderBook::new(market));
            book.apply_snapshot(snapshot);
            Some(key)
        }
        KEvent::Delta { delta, .. } => {
            let key = MarketTicker::new(&delta.market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| predigy_book::OrderBook::new(delta.market.clone()));
            match book.apply_delta(&delta) {
                ApplyOutcome::Ok => Some(key),
                ApplyOutcome::Gap { expected, got } => {
                    warn!(
                        market = %delta.market, expected, got,
                        "kalshi sequence gap; awaiting fresh snapshot"
                    );
                    books.remove(&key);
                    None
                }
                ApplyOutcome::WrongMarket => None,
            }
        }
        _ => None,
    };
    let Some(market) = market else { return };
    let Some(book) = books.get(&market) else {
        return;
    };
    let intents = strategy.evaluate(&market, book, Instant::now());
    if intents.is_empty() {
        return;
    }
    if dry_run {
        info!(
            market = %market,
            count = intents.len(),
            "cross-arb (dry run) would submit"
        );
        return;
    }
    for intent in intents {
        match oms.submit(intent.clone()).await {
            Ok(cid) => info!(
                cid = %cid,
                market = %intent.market,
                side = ?intent.side,
                price = intent.price.cents(),
                "cross-arb submitted"
            ),
            Err(e) => warn!(%e, market = %intent.market, "cross-arb submit rejected"),
        }
    }
}

fn handle_poly(ev: PolyEvent, strategy: &mut CrossArbStrategy) {
    match ev {
        PolyEvent::Book(b) => {
            let bid = b.bids.first().and_then(|l| l.price.parse::<f64>().ok());
            let ask = b.asks.first().and_then(|l| l.price.parse::<f64>().ok());
            strategy.update_poly(&b.asset_id, bid, ask);
        }
        PolyEvent::PriceChange(p) => {
            for change in &p.price_changes {
                let bid = change
                    .best_bid
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok());
                let ask = change
                    .best_ask
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok());
                strategy.update_poly(&change.asset_id, bid, ask);
            }
        }
        PolyEvent::LastTradePrice(_) | PolyEvent::TickSizeChange(_) => {}
        PolyEvent::Disconnected { attempt, reason } => {
            warn!(attempt, reason, "poly md disconnected");
        }
        PolyEvent::Reconnected => info!("poly md reconnected; awaiting fresh book"),
        PolyEvent::Malformed { error, .. } => warn!(%error, "poly malformed frame; ignored"),
    }
}

fn log_oms_event(ev: &OmsEvent) {
    match ev {
        OmsEvent::Filled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => info!(
            cid = %cid, cumulative_qty, fill_price = fill_price.cents(),
            "oms: filled"
        ),
        OmsEvent::PartiallyFilled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => info!(
            cid = %cid, cumulative_qty, fill_price = fill_price.cents(),
            "oms: partial fill"
        ),
        OmsEvent::Rejected { cid, reason } => warn!(cid = %cid, reason, "oms: rejected"),
        OmsEvent::PositionUpdated {
            market,
            side,
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
        } => info!(
            market = %market, side = ?side, new_qty, new_avg_entry_cents,
            realized_pnl_delta_cents,
            "oms: position updated"
        ),
        OmsEvent::KillSwitchArmed => warn!("oms: kill switch ARMED"),
        _ => {}
    }
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

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}
