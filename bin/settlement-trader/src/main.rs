// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `settlement-trader`: lift the touch on near-locked sports
//! markets in the final minutes before settlement.
//!
//! Discovers Kalshi sports markets dynamically — no static markets
//! file. Every `--discovery-interval-secs` (default 60s), polls
//! REST for open markets in the configured `--series`, filters to
//! markets whose `expected_expiration_time` is within
//! `--max-secs-to-settle` from now, and (un)subscribes the WS feed
//! accordingly. The strategy fires within `--close-window-secs` of
//! each market's per-event settlement.
//!
//! ```text
//! settlement-trader \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --close-window-secs 600 \
//!     --max-secs-to-settle 1800 \
//!     --discovery-interval-secs 60 \
//!     --max-account-notional-cents 300
//! ```
//!
//! `--series` repeats; defaults to a curated set of per-event
//! Kalshi sports series. `--market` still works as a manual seed
//! that bypasses discovery filtering — useful for hand-picking a
//! specific game.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_book::{ApplyOutcome, OrderBook};
use predigy_core::market::MarketTicker;
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::{Channel as KalshiChannel, Client as MdClient};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent, spawn_kill_watcher};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use settlement_trader::discovery::{self, DiscoveryConfig, DiscoveryDelta};
use settlement_trader::{DEFAULT_SERIES, SettlementConfig, SettlementStrategy};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "settlement-trader",
    about = "Take Kalshi quotes near settlement on heavy-bid markets."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,
    #[arg(long)]
    kalshi_ws_endpoint: Option<Url>,

    /// Manually seed specific Kalshi market tickers. Optional —
    /// the discovery loop normally finds them automatically. Use
    /// for hand-picked games or when testing.
    #[arg(long = "market")]
    markets: Vec<String>,

    /// Kalshi series to scan for live sports markets. Defaults to
    /// a curated set of per-event series — see DEFAULT_SERIES.
    #[arg(long = "series")]
    series: Vec<String>,

    /// How often to re-scan Kalshi for new tickers entering the
    /// settlement window. Should be << close_window_secs so games
    /// get discovered with head room before the strategy fires.
    #[arg(long, default_value_t = 60)]
    discovery_interval_secs: u64,

    /// Only watch markets whose expected settlement is within this
    /// many seconds of now. Default 30 min — gives the strategy
    /// 20 min of book observation before the close_window opens.
    #[arg(long, default_value_t = 1800)]
    max_secs_to_settle: i64,

    #[arg(long, default_value = "settlement")]
    strategy_id: String,

    /// Time-to-close window. Strategy fires only when
    /// `settle_time - now < close_window`. Default 10 min.
    #[arg(long, default_value_t = 600)]
    close_window_secs: u64,
    #[arg(long, default_value_t = 88)]
    min_price_cents: u8,
    #[arg(long, default_value_t = 96)]
    max_price_cents: u8,
    #[arg(long, default_value_t = 5)]
    bid_to_ask_ratio: u32,
    #[arg(long, default_value_t = 1)]
    size: u32,
    #[arg(long, default_value_t = 60_000)]
    cooldown_ms: u64,

    #[arg(long, default_value_t = 5)]
    max_contracts_per_side: u32,
    #[arg(long, default_value_t = 200)]
    max_notional_cents_per_side: u64,
    #[arg(long, default_value_t = 300)]
    max_account_notional_cents: u64,
    #[arg(long, default_value_t = 200)]
    max_daily_loss_cents: u64,
    #[arg(long, default_value_t = 5)]
    max_orders_per_window: u32,
    #[arg(long, default_value_t = 1_000)]
    rate_window_ms: u64,
    #[arg(long, default_value_t = 500)]
    fills_poll_ms: u64,

    #[arg(long, default_value_t = false)]
    dry_run: bool,

    #[arg(long)]
    cid_store: Option<PathBuf>,

    #[arg(long)]
    oms_state: Option<PathBuf>,

    /// Out-of-process kill switch flag file. Polled every 2 s; when
    /// content starts with `armed` the OMS rejects new submits.
    #[arg(long, default_value = "~/.config/predigy/kill-switch.flag")]
    kill_flag: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;

    let rest_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("rest signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(rest_signer))
    } else {
        RestClient::authed(rest_signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;

    // Discovery needs its own REST client because the executor takes
    // `rest` by move and Client isn't Clone (RSA signer state).
    let discovery_signer = Signer::from_pem(&args.kalshi_key_id, &pem)
        .map_err(|e| anyhow!("discovery signer: {e}"))?;
    let discovery_rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(discovery_signer))
    } else {
        RestClient::authed(discovery_signer)
    }
    .map_err(|e| anyhow!("discovery rest: {e}"))?;

    let ws_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("ws signer: {e}"))?;
    let ws_client = if let Some(endpoint) = &args.kalshi_ws_endpoint {
        MdClient::with_endpoint(endpoint.clone(), Some(ws_signer))
    } else {
        MdClient::new(ws_signer).map_err(|e| anyhow!("ws: {e}"))?
    };
    let mut md = ws_client.connect();

    let mut strategy = SettlementStrategy::new(SettlementConfig {
        close_window: Duration::from_secs(args.close_window_secs),
        min_price_cents: args.min_price_cents,
        max_price_cents: args.max_price_cents,
        bid_to_ask_ratio: args.bid_to_ask_ratio,
        size: args.size,
        cooldown: Duration::from_millis(args.cooldown_ms),
    });

    let series = if args.series.is_empty() {
        DEFAULT_SERIES.iter().map(|s| (*s).to_string()).collect()
    } else {
        args.series.clone()
    };
    info!(
        series = ?series,
        manual_seeds = args.markets.len(),
        discovery_interval_secs = args.discovery_interval_secs,
        max_secs_to_settle = args.max_secs_to_settle,
        "settlement-trader: discovery config"
    );
    let mut discovery_rx = discovery::spawn(
        discovery_rest,
        DiscoveryConfig {
            series,
            interval: Duration::from_secs(args.discovery_interval_secs),
            max_secs_to_settle: args.max_secs_to_settle,
            require_quote: false,
        },
        args.markets.clone(),
    );

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
    let cid_backing = if let Some(path) = &args.cid_store {
        CidBacking::Persistent {
            store_path: path.clone(),
            chunk_size: 1_000,
        }
    } else {
        warn!("no --cid-store; cids reset on every run");
        CidBacking::InMemory { start_seq: 0 }
    };
    let state_backing = if let Some(path) = &args.oms_state {
        predigy_oms::StateBacking::Persistent { path: path.clone() }
    } else {
        warn!("no --oms-state; daily P&L + kill-switch + orders reset every run");
        predigy_oms::StateBacking::InMemory
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
    .map_err(|e| anyhow!("oms: {e}"))?;

    let kill_flag = expand_tilde(&args.kill_flag);
    if !kill_flag.as_os_str().is_empty() {
        info!(kill_flag = %kill_flag.display(), "kill-flag watcher armed");
        spawn_kill_watcher(oms.control(), kill_flag, Duration::from_secs(2));
    }

    let mut books: HashMap<MarketTicker, OrderBook> = HashMap::new();
    // Per-ticker subscription bookkeeping. We subscribe one ticker
    // at a time so each gets its own pair of (orderbook_delta,
    // ticker) sids, which makes per-ticker unsubscribe trivial.
    // `pending_subs` maps the subscribe req_id → ticker until the
    // server's Subscribed message confirms the sid.
    let mut pending_subs: HashMap<u64, MarketTicker> = HashMap::new();
    let mut sids_by_ticker: HashMap<MarketTicker, Vec<u64>> = HashMap::new();
    let channels = [KalshiChannel::OrderbookDelta, KalshiChannel::Ticker];

    let stop = wait_for_ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => {
                info!("settlement-trader: stop");
                break;
            }
            ev = md.next_event() => {
                let Some(ev) = ev else { break; };
                handle_md(
                    ev,
                    &mut books,
                    &mut strategy,
                    &oms,
                    args.dry_run,
                    &mut pending_subs,
                    &mut sids_by_ticker,
                ).await;
            }
            ev = oms.next_event() => {
                let Some(ev) = ev else { break; };
                log_oms_event(&ev);
            }
            delta = discovery_rx.recv() => {
                let Some(delta) = delta else {
                    warn!("discovery channel closed; exiting");
                    break;
                };
                apply_discovery_delta(
                    delta,
                    &mut md,
                    &channels,
                    &mut strategy,
                    &mut books,
                    &mut pending_subs,
                    &mut sids_by_ticker,
                ).await;
            }
        }
    }
    oms.close().await;
    Ok(())
}

async fn apply_discovery_delta(
    delta: DiscoveryDelta,
    md: &mut predigy_kalshi_md::Connection,
    channels: &[KalshiChannel],
    strategy: &mut SettlementStrategy,
    books: &mut HashMap<MarketTicker, OrderBook>,
    pending_subs: &mut HashMap<u64, MarketTicker>,
    sids_by_ticker: &mut HashMap<MarketTicker, Vec<u64>>,
) {
    for (ticker, settle_unix) in delta.add {
        let key = MarketTicker::new(&ticker);
        // Strategy-side state: the close-time table and the book.
        // Set/overwrite even on re-subscribe — the discovery loop
        // is idempotent on the strategy.
        strategy.set_close_time(key.clone(), settle_unix);
        books
            .entry(key.clone())
            .or_insert_with(|| OrderBook::new(ticker.clone()));
        if sids_by_ticker.contains_key(&key) {
            // Already subscribed — discovery emitted a re-add (e.g.
            // after a settlement-time update). Strategy state is
            // refreshed above; nothing more to do.
            continue;
        }
        match md.subscribe(channels, std::slice::from_ref(&ticker)).await {
            Ok(req_id) => {
                pending_subs.insert(req_id, key.clone());
                sids_by_ticker.insert(key, Vec::new());
                info!(
                    market = %ticker,
                    secs_to_settle = settle_unix.saturating_sub(current_unix()),
                    "settlement-trader: subscribing"
                );
            }
            Err(e) => warn!(market = %ticker, error = %e, "subscribe failed"),
        }
    }
    for ticker in delta.remove {
        let key = MarketTicker::new(&ticker);
        let Some(sids) = sids_by_ticker.remove(&key) else {
            books.remove(&key);
            continue;
        };
        if !sids.is_empty() {
            if let Err(e) = md.unsubscribe(&sids).await {
                warn!(market = %ticker, error = %e, "unsubscribe failed");
            } else {
                info!(market = %ticker, sids = ?sids, "settlement-trader: unsubscribed");
            }
        }
        books.remove(&key);
    }
}

async fn handle_md(
    ev: predigy_kalshi_md::Event,
    books: &mut HashMap<MarketTicker, OrderBook>,
    strategy: &mut SettlementStrategy,
    oms: &predigy_oms::OmsHandle,
    dry_run: bool,
    pending_subs: &mut HashMap<u64, MarketTicker>,
    sids_by_ticker: &mut HashMap<MarketTicker, Vec<u64>>,
) {
    use predigy_kalshi_md::Event as MdEvent;
    if let MdEvent::Subscribed { req_id, sid, .. } = &ev {
        if let Some(req_id) = req_id
            && let Some(ticker) = pending_subs.get(req_id).cloned()
        {
            sids_by_ticker.entry(ticker).or_default().push(*sid);
        }
        return;
    }
    let updated_market = match ev {
        MdEvent::Snapshot {
            market, snapshot, ..
        } => {
            let key = MarketTicker::new(&market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| OrderBook::new(market));
            book.apply_snapshot(snapshot);
            Some(key)
        }
        MdEvent::Delta { delta, .. } => {
            let key = MarketTicker::new(&delta.market);
            let book = books
                .entry(key.clone())
                .or_insert_with(|| OrderBook::new(delta.market.clone()));
            match book.apply_delta(&delta) {
                ApplyOutcome::Ok => Some(key),
                ApplyOutcome::Gap { expected, got } => {
                    warn!(market = %delta.market, expected, got,
                        "settlement-trader: gap; awaiting fresh snapshot");
                    books.remove(&key);
                    None
                }
                ApplyOutcome::WrongMarket => None,
            }
        }
        _ => None,
    };
    let Some(market) = updated_market else { return };
    let Some(book) = books.get(&market) else {
        return;
    };
    let intent = strategy.evaluate(&market, book, current_unix(), Instant::now());
    let Some(intent) = intent else { return };
    info!(
        market = %intent.market,
        side = ?intent.side,
        price = intent.price.cents(),
        size = intent.qty.get(),
        dry_run,
        "settlement-trader: rule fired"
    );
    if !dry_run {
        match oms.submit(intent).await {
            Ok(cid) => info!(cid = %cid, "settlement-trader: submitted"),
            Err(e) => warn!(%e, "settlement-trader: submit rejected"),
        }
    }
}

fn log_oms_event(ev: &OmsEvent) {
    match ev {
        OmsEvent::Filled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => {
            info!(cid = %cid, cumulative_qty, fill_price = fill_price.cents(), "filled");
        }
        OmsEvent::PartiallyFilled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => {
            info!(cid = %cid, cumulative_qty, fill_price = fill_price.cents(), "partial");
        }
        OmsEvent::Rejected { cid, reason } => warn!(cid = %cid, reason, "rejected"),
        OmsEvent::Cancelled { cid, reason } => info!(cid = %cid, reason, "cancelled"),
        OmsEvent::PositionUpdated {
            market,
            side,
            new_qty,
            new_avg_entry_cents,
            realized_pnl_delta_cents,
        } => info!(
            market = %market, side = ?side, new_qty, new_avg_entry_cents,
            realized_pnl_delta_cents, "position"
        ),
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

fn current_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
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
