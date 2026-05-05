// Vendor names (NWS, Kalshi) appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `latency-trader`: race the Kalshi book on news/data events.
//!
//! Subscribes to one or more external feeds (today: NWS active
//! alerts) and, when an alert matches a configured trigger, lifts a
//! pre-decided trade on the corresponding Kalshi market. IOC at the
//! configured `max_price_cents` — won't pay above the rule's ceiling
//! even if the book has rolled.
//!
//! ```text
//! latency-trader \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --nws-states TX --nws-states OK \
//!     --nws-user-agent "(latency-trader, ops@example.com)" \
//!     --rule-file ./rules.json
//! ```
//!
//! `rules.json` is a JSON array of [`latency_trader::LatencyRule`].

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use latency_trader::LatencyStrategy;
use predigy_core::market::MarketTicker;
use predigy_ext_feeds::{MIN_POLL_INTERVAL, NwsAlertsConfig, spawn_nws};
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::Client as MdClient;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "latency-trader",
    about = "Race the Kalshi book on NWS / news events."
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

    /// JSON file containing an array of `LatencyRule`s.
    #[arg(long)]
    rule_file: PathBuf,

    /// 2-letter US state codes for the NWS alerts subscription.
    /// Pass multiple times. Empty = all states (heavy traffic).
    #[arg(long = "nws-states")]
    nws_states: Vec<String>,

    /// User-Agent string NWS requires; format `"(app, contact)"`.
    #[arg(long, env = "NWS_USER_AGENT")]
    nws_user_agent: String,

    /// Poll interval (ms). Floored at NWS recommended minimum.
    #[arg(long, default_value_t = 30_000)]
    nws_poll_ms: u64,

    #[arg(long, default_value = "latency")]
    strategy_id: String,

    #[arg(long, default_value_t = 100)]
    max_contracts_per_side: u32,
    #[arg(long, default_value_t = 5_000)]
    max_notional_cents_per_side: u64,
    #[arg(long, default_value_t = 50_000)]
    max_account_notional_cents: u64,
    #[arg(long, default_value_t = 10_000)]
    max_daily_loss_cents: u64,
    #[arg(long, default_value_t = 10)]
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

    /// Path to a JSON file storing the NWS seen-alert-id set.
    /// Required for any 24/7 deployment that restarts: without
    /// it, a restart re-emits every currently-active NWS alert
    /// to the strategy, which then re-fires every rule that
    /// matched in the prior session. With it, alert ids persist
    /// across restarts and the strategy only sees genuinely-new
    /// alerts.
    #[arg(long)]
    nws_seen: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;

    let rules: Vec<latency_trader::LatencyRule> = {
        let raw = tokio::fs::read(&args.rule_file)
            .await
            .with_context(|| format!("read rules at {}", args.rule_file.display()))?;
        serde_json::from_slice(&raw).context("parse rule file")?
    };
    if rules.is_empty() {
        return Err(anyhow!("rule file is empty"));
    }
    let mut strategy = LatencyStrategy::new(rules);
    info!(
        rules = strategy.rule_count(),
        "latency-trader: loaded rules"
    );

    // OMS + executor.
    let rest_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(rest_signer))
    } else {
        RestClient::authed(rest_signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;

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

    // The latency strategy doesn't read Kalshi books — the rule
    // file already encodes "what to lift" — so we don't subscribe
    // to the WS feed. The MdClient import + ws_endpoint flag are
    // kept for future revisions that might quote-shade based on
    // book state at fire time.
    let _ = (&args.kalshi_ws_endpoint, std::any::type_name::<MdClient>());

    if args.nws_seen.is_none() {
        warn!(
            "no --nws-seen; restarts will re-emit every active alert and re-fire any rule \
             that already fired in the prior session"
        );
    }
    let nws_config = NwsAlertsConfig {
        states: args.nws_states.clone(),
        poll_interval: Duration::from_millis(args.nws_poll_ms).max(MIN_POLL_INTERVAL),
        user_agent: args.nws_user_agent.clone(),
        base_url: None,
        seen_path: args.nws_seen.clone(),
    };
    let (mut nws_rx, _nws_task) = spawn_nws(nws_config).map_err(|e| anyhow!("nws spawn: {e}"))?;
    info!(states = ?args.nws_states, dry_run = args.dry_run, "latency-trader: nws subscribed");

    let stop = wait_for_ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => {
                info!("latency-trader: stop");
                break;
            }
            alert = nws_rx.recv() => {
                let Some(alert) = alert else { break; };
                if let Some((idx, intent)) = strategy.evaluate(&alert) {
                    info!(
                        rule_idx = idx,
                        event = %alert.event_type,
                        area = %alert.area_desc,
                        severity = %alert.severity,
                        market = %intent.market,
                        side = ?intent.side,
                        price = intent.price.cents(),
                        size = intent.qty.get(),
                        dry_run = args.dry_run,
                        "latency-trader: rule fired"
                    );
                    if !args.dry_run {
                        match oms.submit(intent.clone()).await {
                            Ok(cid) => info!(cid = %cid, "latency-trader: submitted"),
                            Err(e) => warn!(%e, "latency-trader: submit rejected"),
                        }
                    }
                } else {
                    // Visibility for dry-run shake-downs: knowing
                    // *what* arrived even when nothing fired tells
                    // the operator whether the rule set is too
                    // narrow vs the feed is dry.
                    info!(
                        event = %alert.event_type,
                        area = %alert.area_desc,
                        severity = %alert.severity,
                        "latency-trader: alert ignored (no matching rule)"
                    );
                }
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

// `predigy_core::market::MarketTicker` is referenced by the rule
// loader through serde — keep the use here so static analysers see
// the crate is meaningfully imported even when the file is being
// linted in isolation.
const _: Option<MarketTicker> = None;
