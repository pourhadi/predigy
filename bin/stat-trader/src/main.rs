// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `stat-trader`: take Kalshi quotes when an operator-supplied
//! model probability differs from the market by more than a
//! configured threshold.
//!
//! ```text
//! stat-trader \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --rule-file ./stat-rules.json \
//!     --bankroll-cents 50000 --kelly-factor 0.25
//! ```

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_book::{ApplyOutcome, OrderBook};
use predigy_core::market::MarketTicker;
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::{Channel as KalshiChannel, Client as MdClient};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use stat_trader::{StatConfig, StatRule, StatStrategy};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "stat-trader",
    about = "Take Kalshi quotes when model probability differs from market."
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

    /// JSON file containing an array of `StatRule`s.
    #[arg(long)]
    rule_file: PathBuf,

    /// Bankroll for Kelly sizing, in cents.
    #[arg(long, default_value_t = 50_000)]
    bankroll_cents: u64,

    /// Fractional Kelly modifier in `(0, 1]`. Quarter-Kelly (0.25)
    /// is the default — robust to model error.
    #[arg(long, default_value_t = 0.25)]
    kelly_factor: f64,

    #[arg(long, default_value_t = 100)]
    max_size: u32,
    #[arg(long, default_value_t = 500)]
    cooldown_ms: u64,

    #[arg(long, default_value = "stat")]
    strategy_id: String,

    #[arg(long, default_value_t = 100)]
    max_contracts_per_side: u32,
    #[arg(long, default_value_t = 5_000)]
    max_notional_cents_per_side: u64,
    #[arg(long, default_value_t = 50_000)]
    max_account_notional_cents: u64,
    #[arg(long, default_value_t = 10_000)]
    max_daily_loss_cents: u64,
    #[arg(long, default_value_t = 20)]
    max_orders_per_window: u32,
    #[arg(long, default_value_t = 1_000)]
    rate_window_ms: u64,
    #[arg(long, default_value_t = 500)]
    fills_poll_ms: u64,

    #[arg(long, default_value_t = false)]
    dry_run: bool,

    #[arg(long)]
    cid_store: Option<PathBuf>,

    /// Path to a JSON file the OMS snapshots its state to. See
    /// `arb-trader --help` for details.
    #[arg(long)]
    oms_state: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;

    let rules: Vec<StatRule> = {
        let raw = tokio::fs::read(&args.rule_file)
            .await
            .with_context(|| format!("read rules at {}", args.rule_file.display()))?;
        serde_json::from_slice(&raw).context("parse rule file")?
    };
    if rules.is_empty() {
        return Err(anyhow!("rule file is empty"));
    }

    let mut strategy = StatStrategy::new(
        StatConfig {
            bankroll_cents: args.bankroll_cents,
            kelly_factor: args.kelly_factor,
            max_size: args.max_size,
            cooldown: Duration::from_millis(args.cooldown_ms),
        },
        rules,
    );
    let market_strs: Vec<String> = strategy.markets().map(|m| m.as_str().to_string()).collect();
    info!(rules = market_strs.len(), markets = ?market_strs, "stat-trader: loaded rules");

    let rest_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("rest signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(rest_signer))
    } else {
        RestClient::authed(rest_signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;

    let ws_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("ws signer: {e}"))?;
    let ws_client = if let Some(endpoint) = &args.kalshi_ws_endpoint {
        MdClient::with_endpoint(endpoint.clone(), Some(ws_signer))
    } else {
        MdClient::new(ws_signer).map_err(|e| anyhow!("ws: {e}"))?
    };
    let mut md = ws_client.connect();

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
        warn!("no --oms-state; daily-pnl + kill-switch + orders reset on every run");
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

    let req_id = md
        .subscribe(
            &[KalshiChannel::OrderbookDelta, KalshiChannel::Ticker],
            &market_strs,
        )
        .await
        .map_err(|e| anyhow!("subscribe: {e}"))?;
    info!(req_id, dry_run = args.dry_run, "stat-trader: subscribed");

    let mut books: HashMap<MarketTicker, OrderBook> = market_strs
        .iter()
        .map(|m| (MarketTicker::new(m), OrderBook::new(m.clone())))
        .collect();

    let stop = wait_for_ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            ev = md.next_event() => {
                let Some(ev) = ev else { break; };
                handle_md(ev, &mut books, &mut strategy, &oms, args.dry_run).await;
            }
            ev = oms.next_event() => {
                let Some(ev) = ev else { break; };
                log_oms_event(&ev);
            }
        }
    }
    oms.close().await;
    Ok(())
}

async fn handle_md(
    ev: predigy_kalshi_md::Event,
    books: &mut HashMap<MarketTicker, OrderBook>,
    strategy: &mut StatStrategy,
    oms: &predigy_oms::OmsHandle,
    dry_run: bool,
) {
    use predigy_kalshi_md::Event as MdEvent;
    let market = match ev {
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
                    warn!(
                        market = %delta.market, expected, got,
                        "stat-trader: gap; awaiting snapshot"
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
    let Some(intent) = strategy.evaluate(&market, book, Instant::now()) else {
        return;
    };
    info!(
        market = %market,
        side = ?intent.side,
        price = intent.price.cents(),
        size = intent.qty.get(),
        dry_run,
        "stat-trader: rule fired"
    );
    if !dry_run {
        match oms.submit(intent).await {
            Ok(cid) => info!(cid = %cid, "stat-trader: submitted"),
            Err(e) => warn!(%e, "stat-trader: submit rejected"),
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
        } => info!(cid = %cid, cumulative_qty, fill_price = fill_price.cents(), "filled"),
        OmsEvent::PartiallyFilled {
            cid,
            cumulative_qty,
            fill_price,
            ..
        } => info!(cid = %cid, cumulative_qty, fill_price = fill_price.cents(), "partial"),
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
            realized_pnl_delta_cents,
            "position"
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
