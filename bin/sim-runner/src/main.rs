//! `sim-runner` — offline backtester wrapper.
//!
//! Loads an `md-recorder` NDJSON file, runs the configured strategy
//! (today: `arb-trader::ArbStrategy`) through the same OMS path that
//! production uses, and prints a summary of fills and realised P&L.
//!
//! ```text
//! sim-runner \
//!     --input ./data/2026-05-04.ndjson \
//!     --market FED-23DEC-T3.00 \
//!     --min-edge-cents 2 \
//!     --size-per-pair 25
//! ```
//!
//! Acceptance: replays the input file end-to-end, asserts no panics,
//! and prints a deterministic summary that an operator can diff
//! across config changes.

use anyhow::{Context as _, Result, anyhow};
use arb_trader::strategy::{ArbConfig, ArbStrategy};
use clap::Parser;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent, OmsHandle};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use predigy_sim::{BookStore, Replay, ReplayUpdate, SimExecutor};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "sim-runner",
    about = "Offline backtester for predigy strategies. Replays an md-recorder NDJSON file."
)]
struct Args {
    /// Path to an NDJSON file produced by `md-recorder`.
    #[arg(long)]
    input: PathBuf,

    /// Markets to consider for the strategy. Pass multiple times.
    /// Events for other markets are still replayed (so the book
    /// stays in sync) but the strategy only evaluates these.
    #[arg(long = "market", required = true)]
    markets: Vec<String>,

    /// Per-market position cap, contracts per side.
    #[arg(long, default_value_t = 100)]
    max_contracts_per_side: u32,

    /// Per-market notional cap, cents per side. 0 disables.
    #[arg(long, default_value_t = 5_000)]
    max_notional_cents_per_side: u64,

    /// Account-wide gross notional cap, cents. 0 disables.
    #[arg(long, default_value_t = 50_000)]
    max_account_notional_cents: u64,

    /// Daily realised loss breaker (cents). 0 disables.
    #[arg(long, default_value_t = 0)]
    max_daily_loss_cents: u64,

    /// Min edge per arb pair (cents) after fees.
    #[arg(long, default_value_t = 2)]
    min_edge_cents: u32,

    /// Cap on contracts per arb pair.
    #[arg(long, default_value_t = 25)]
    size_per_pair: u32,

    /// Cooldown between submits per market (ms).
    #[arg(long, default_value_t = 100)]
    cooldown_ms: u64,
}

#[derive(Debug, Default)]
struct Stats {
    submitted: u64,
    rejected: u64,
    acked: u64,
    filled: u64,
    partials: u64,
    cancelled: u64,
    total_filled_qty: u64,
    realized_pnl_cents: i64,
    final_yes_qty: u32,
    final_no_qty: u32,
    final_yes_avg: u16,
    final_no_avg: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

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
            max_orders_per_window: 0,
            window: Duration::from_secs(1),
        },
    };
    info!(?limits, "sim risk limits");

    let store = BookStore::new();
    let (executor, reports) = SimExecutor::spawn(store.clone());
    let oms = Oms::spawn(
        OmsConfig {
            strategy_id: "sim-arb".into(),
            cid_backing: CidBacking::InMemory { start_seq: 0 },
            state_backing: predigy_oms::StateBacking::InMemory,
        },
        RiskEngine::new(limits),
        executor,
        reports,
    );
    let oms_arc = Arc::new(Mutex::new(oms));

    let allowed: std::collections::HashSet<MarketTicker> =
        args.markets.iter().map(MarketTicker::new).collect();
    let strategy = Arc::new(std::sync::Mutex::new(ArbStrategy::new(ArbConfig {
        min_edge_cents: args.min_edge_cents,
        max_size_per_pair: args.size_per_pair,
        cooldown: Duration::from_millis(args.cooldown_ms),
    })));

    let replay = Replay::new(store.clone());
    let strategy_cb = strategy.clone();
    let store_cb = store.clone();
    let oms_cb = oms_arc.clone();
    let allowed_cb = Arc::new(allowed);
    replay
        .drive_file(&args.input, move |update| {
            let strategy_cb = strategy_cb.clone();
            let store_cb = store_cb.clone();
            let oms_cb = oms_cb.clone();
            let allowed_cb = allowed_cb.clone();
            Box::pin(async move {
                let ReplayUpdate::BookUpdated(market) = update else {
                    return;
                };
                if !allowed_cb.contains(&market) {
                    return;
                }
                let intents = {
                    let mut s = strategy_cb.lock().unwrap();
                    store_cb
                        .with_book(&market, |book| {
                            book.map(|b| s.evaluate(&market, b, Instant::now()))
                        })
                        .map(|ev| ev.intents)
                        .unwrap_or_default()
                };
                if intents.is_empty() {
                    return;
                }
                let oms_g = oms_cb.lock().await;
                for intent in intents {
                    let _ = oms_g.submit(intent).await;
                }
            }) as Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        })
        .await
        .with_context(|| format!("replay {}", args.input.display()))?;

    let mut oms = Arc::try_unwrap(oms_arc)
        .map_err(|_| anyhow!("oms handle leaked beyond replay"))?
        .into_inner();
    let stats = drain_stats(&mut oms).await;
    oms.close().await;
    print_stats(&stats);
    Ok(())
}

async fn drain_stats(oms: &mut OmsHandle) -> Stats {
    let mut stats = Stats::default();
    // Drain whatever the OMS already has buffered. After a 100ms
    // quiet period we assume the stream is idle.
    let idle_timeout = Duration::from_millis(100);
    loop {
        let Ok(Some(ev)) = tokio::time::timeout(idle_timeout, oms.next_event()).await else {
            break;
        };
        match ev {
            OmsEvent::Submitted { .. } => stats.submitted += 1,
            OmsEvent::Rejected { .. } => stats.rejected += 1,
            OmsEvent::Acked { .. } => stats.acked += 1,
            OmsEvent::Filled { delta_qty, .. } => {
                stats.filled += 1;
                stats.total_filled_qty += u64::from(delta_qty);
            }
            OmsEvent::PartiallyFilled { delta_qty, .. } => {
                stats.partials += 1;
                stats.total_filled_qty += u64::from(delta_qty);
            }
            OmsEvent::Cancelled { .. } => stats.cancelled += 1,
            OmsEvent::PositionUpdated {
                side,
                new_qty,
                new_avg_entry_cents,
                realized_pnl_delta_cents,
                ..
            } => {
                stats.realized_pnl_cents += realized_pnl_delta_cents;
                match side {
                    Side::Yes => {
                        stats.final_yes_qty = new_qty;
                        stats.final_yes_avg = new_avg_entry_cents;
                    }
                    Side::No => {
                        stats.final_no_qty = new_qty;
                        stats.final_no_avg = new_avg_entry_cents;
                    }
                }
            }
            OmsEvent::Reconciled { .. }
            | OmsEvent::KillSwitchArmed
            | OmsEvent::KillSwitchDisarmed => {}
        }
    }
    stats
}

fn print_stats(s: &Stats) {
    println!("\n=== sim-runner summary ===");
    println!("submitted          : {}", s.submitted);
    println!("rejected           : {}", s.rejected);
    println!("acked              : {}", s.acked);
    println!("filled (terminal)  : {}", s.filled);
    println!("partial fills      : {}", s.partials);
    println!("cancelled          : {}", s.cancelled);
    println!("total filled qty   : {}", s.total_filled_qty);
    println!("realised P&L cents : {}", s.realized_pnl_cents);
    println!(
        "final YES position : {} @ {}¢",
        s.final_yes_qty, s.final_yes_avg
    );
    println!(
        "final NO position  : {} @ {}¢",
        s.final_no_qty, s.final_no_avg
    );
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
