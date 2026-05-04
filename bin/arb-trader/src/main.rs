//! `arb-trader` binary: static intra-venue arb on Kalshi.
//!
//! The strategy fires when `best_yes_ask + best_no_ask < $1` minus
//! taker fees on both legs. Each opportunity submits a pair of IOC
//! orders (buy YES, buy NO) at the touch; settlement guarantees one
//! contract pays $1 per pair, locking in the difference.
//!
//! ```text
//! arb-trader \
//!     --market FED-23DEC-T3.00 --market FED-23DEC-T3.25 \
//!     --kalshi-key-id $KALSHI_KEY_ID \
//!     --kalshi-pem    /path/to/kalshi.pem \
//!     --max-account-notional-cents 50000 \
//!     --max-daily-loss-cents 5000 \
//!     --min-edge-cents 2 \
//!     --size-per-pair 25
//! ```

use anyhow::{Context as _, Result, anyhow};
use arb_trader::{ArbConfig, Runner, RunnerConfig};
use clap::Parser;
use predigy_core::market::MarketTicker;
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::Client as MdClient;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "arb-trader",
    about = "Static intra-venue arb on Kalshi: lift YES+NO when the touch totals < $1 - fees."
)]
struct Args {
    /// Kalshi market tickers to scan. Pass multiple times.
    #[arg(long = "market", required = true)]
    markets: Vec<String>,

    /// Kalshi API key id. Required — both WS and REST authenticate.
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,

    /// Path to the PEM-encoded Kalshi private key (PKCS#1 or PKCS#8).
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,

    /// Override the WS endpoint (default: production Kalshi).
    #[arg(long)]
    ws_endpoint: Option<Url>,

    /// Override the REST base URL (default: production Kalshi).
    #[arg(long)]
    rest_endpoint: Option<String>,

    /// Strategy id embedded in client order ids and structured logs.
    #[arg(long, default_value = "arb")]
    strategy_id: String,

    /// Per-market position cap (contracts, per side).
    #[arg(long, default_value_t = 100)]
    max_contracts_per_side: u32,

    /// Per-market notional cap (cents, per side). 0 disables.
    #[arg(long, default_value_t = 5_000)]
    max_notional_cents_per_side: u64,

    /// Account-wide gross notional cap (cents). 0 disables.
    #[arg(long, default_value_t = 50_000)]
    max_account_notional_cents: u64,

    /// Daily realised loss breaker (cents). Once realised loss
    /// reaches this, the OMS rejects further submits. 0 disables.
    #[arg(long, default_value_t = 5_000)]
    max_daily_loss_cents: u64,

    /// Order-rate cap: max submits per `--rate-window-ms`.
    /// 0 disables.
    #[arg(long, default_value_t = 20)]
    max_orders_per_window: u32,

    /// Order-rate window in milliseconds.
    #[arg(long, default_value_t = 1_000)]
    rate_window_ms: u64,

    /// Minimum net edge per pair (cents, after both taker fees) to
    /// fire the trade.
    #[arg(long, default_value_t = 2)]
    min_edge_cents: u32,

    /// Cap on contracts per arb pair (the strategy auto-shrinks to
    /// what's available at the touch).
    #[arg(long, default_value_t = 25)]
    size_per_pair: u32,

    /// Minimum interval between submits on the same market.
    #[arg(long, default_value_t = 500)]
    cooldown_ms: u64,

    /// Fills-poll interval (milliseconds).
    #[arg(long, default_value_t = 500)]
    fills_poll_ms: u64,

    /// Don't actually submit — log proposed pairs only. Useful for
    /// shaking down the wiring against live data.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Path to a file the OMS uses to persist cid sequence numbers
    /// across restarts. Strongly recommended for production runs —
    /// without it, cids restart from 0 on each run and the venue
    /// will reject duplicates from a prior session.
    #[arg(long)]
    cid_store: Option<PathBuf>,

    /// How many cids to pre-allocate per persistence write. Higher
    /// = fewer fsyncs at the cost of more wasted ids on a crash.
    #[arg(long, default_value_t = 1_000)]
    cid_chunk_size: u64,

    /// Path to a JSON file the OMS snapshots its state to (positions,
    /// daily realised P&L, kill-switch flag, in-flight orders) after
    /// every mutation. On restart the file is loaded if present so
    /// the daily-loss breaker, kill-switch, and orders ledger
    /// survive a crash. Without this, those reset to zero on every
    /// run.
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

    let rest_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("REST signer: {e}"))?;
    let rest = if let Some(base) = &args.rest_endpoint {
        RestClient::with_base(base, Some(rest_signer))
    } else {
        RestClient::authed(rest_signer)
    }
    .map_err(|e| anyhow!("build REST client: {e}"))?;

    let ws_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("WS signer: {e}"))?;
    let ws_client = if let Some(endpoint) = &args.ws_endpoint {
        MdClient::with_endpoint(endpoint.clone(), Some(ws_signer))
    } else {
        MdClient::new(ws_signer).map_err(|e| anyhow!("build WS client: {e}"))?
    };
    let md_conn = ws_client.connect();

    let limits = Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: args.max_contracts_per_side,
            max_notional_cents_per_side: args.max_notional_cents_per_side,
        },
        per_market_overrides: HashMap::default(),
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

    let (executor, reports) = RestExecutor::spawn(
        rest,
        PollerConfig {
            interval: Duration::from_millis(args.fills_poll_ms),
            initial_lookback: Duration::from_mins(1),
        },
    );

    let cid_backing = if let Some(path) = &args.cid_store {
        CidBacking::Persistent {
            store_path: path.clone(),
            chunk_size: args.cid_chunk_size,
        }
    } else {
        tracing::warn!(
            "no --cid-store; cids will restart from 0 each run. Use --cid-store \
             in production to avoid venue duplicate-id rejects across restarts."
        );
        CidBacking::InMemory { start_seq: 0 }
    };
    let state_backing = if let Some(path) = &args.oms_state {
        predigy_oms::StateBacking::Persistent { path: path.clone() }
    } else {
        tracing::warn!(
            "no --oms-state; positions/daily-pnl/kill-switch/orders reset on every run. \
             Use --oms-state in production so the daily-loss breaker survives restarts."
        );
        predigy_oms::StateBacking::InMemory
    };
    let oms = Oms::try_spawn(
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

    let runner_config = RunnerConfig {
        markets: args.markets.iter().map(MarketTicker::new).collect(),
        arb: ArbConfig {
            min_edge_cents: args.min_edge_cents,
            max_size_per_pair: args.size_per_pair,
            cooldown: Duration::from_millis(args.cooldown_ms),
        },
        dry_run: args.dry_run,
    };

    let runner = Runner::new(runner_config);
    let stop = wait_for_ctrl_c();
    runner.run(md_conn, oms, stop).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

async fn wait_for_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %e, "ctrl_c handler failed; will run until killed");
        loop {
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
    }
}
