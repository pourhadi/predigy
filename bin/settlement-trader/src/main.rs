// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `settlement-trader`: lift the touch on near-locked sports
//! markets in the final minutes before settlement.
//!
//! Subscribes to Kalshi WS for the operator-supplied market list,
//! evaluates each book update against [`SettlementStrategy`] (heavy-
//! bid / thin-ask asymmetry inside `close_window`), submits IOC
//! orders to Kalshi when the rule fires.
//!
//! ```text
//! settlement-trader \
//!     --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./key.pem \
//!     --market KXNBASERIES-26PHINYKR2-NYK \
//!     --market KXNBASERIES-26LALOKCR2-OKC \
//!     --close-window-secs 600 \
//!     --max-account-notional-cents 300
//! ```
//!
//! Pass each Kalshi market ticker via `--market` once. The binary
//! pulls the `close_time` for each from `Client::market_detail`
//! at startup; markets with `close_time` already in the past are
//! skipped with a warning.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_book::{ApplyOutcome, OrderBook};
use predigy_core::market::MarketTicker;
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::{Channel as KalshiChannel, Client as MdClient};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use settlement_trader::{SettlementConfig, SettlementStrategy};
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

    /// Kalshi market tickers to watch. Pass once per market.
    #[arg(long = "market", required = true)]
    markets: Vec<String>,

    #[arg(long, default_value = "settlement")]
    strategy_id: String,

    /// Time-to-close window. Strategy fires only when
    /// `close_time - now < close_window`. Default 10 min.
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

    let ws_signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("ws signer: {e}"))?;
    let ws_client = if let Some(endpoint) = &args.kalshi_ws_endpoint {
        MdClient::with_endpoint(endpoint.clone(), Some(ws_signer))
    } else {
        MdClient::new(ws_signer).map_err(|e| anyhow!("ws: {e}"))?
    };
    let mut md = ws_client.connect();

    // Build strategy and pre-load each market's close_time via
    // REST. Markets that already closed are dropped with a warning.
    let mut strategy = SettlementStrategy::new(SettlementConfig {
        close_window: Duration::from_secs(args.close_window_secs),
        min_price_cents: args.min_price_cents,
        max_price_cents: args.max_price_cents,
        bid_to_ask_ratio: args.bid_to_ask_ratio,
        size: args.size,
        cooldown: Duration::from_millis(args.cooldown_ms),
    });
    let now_unix = current_unix();
    let mut subscribe_markets: Vec<String> = Vec::with_capacity(args.markets.len());
    for ticker in &args.markets {
        match rest.market_detail(ticker).await {
            Ok(detail) => {
                let Some(close_unix) = parse_iso8601_to_unix(&detail.market.close_time) else {
                    warn!(market = %ticker, close_time = %detail.market.close_time,
                        "couldn't parse close_time; skipping");
                    continue;
                };
                if close_unix <= now_unix {
                    warn!(market = %ticker, close_time = %detail.market.close_time,
                        "market already closed; skipping");
                    continue;
                }
                strategy.set_close_time(MarketTicker::new(ticker), close_unix);
                subscribe_markets.push(ticker.clone());
                info!(market = %ticker, secs_to_close = close_unix - now_unix,
                    "settlement-trader: armed");
            }
            Err(e) => {
                warn!(market = %ticker, error = %e, "couldn't fetch market_detail; skipping");
            }
        }
    }
    if subscribe_markets.is_empty() {
        return Err(anyhow!(
            "no actionable markets — every --market was unparseable, missing, or closed"
        ));
    }

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

    let req_id = md
        .subscribe(
            &[KalshiChannel::OrderbookDelta, KalshiChannel::Ticker],
            &subscribe_markets,
        )
        .await
        .map_err(|e| anyhow!("subscribe: {e}"))?;
    info!(req_id, dry_run = args.dry_run, markets = ?subscribe_markets, "settlement-trader: subscribed");

    let mut books: HashMap<MarketTicker, OrderBook> = subscribe_markets
        .iter()
        .map(|m| (MarketTicker::new(m), OrderBook::new(m.clone())))
        .collect();

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
    strategy: &mut SettlementStrategy,
    oms: &predigy_oms::OmsHandle,
    dry_run: bool,
) {
    use predigy_kalshi_md::Event as MdEvent;
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

/// Tiny ISO-8601 → unix-seconds parser. RFC3339 is what Kalshi
/// emits; assumes `Z` suffix (UTC). Returns `None` on any
/// malformed input — caller should warn-log and skip the market.
fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    // Format: `2026-05-19T00:00:00Z` or `2026-05-19T00:00:00.123456Z`.
    // Take the first 19 chars — `YYYY-MM-DDTHH:MM:SS` — and parse
    // each component. Reject anything else.
    let part = s.get(..19)?;
    let bytes = part.as_bytes();
    if bytes.len() != 19 || bytes[4] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let atoi = |slice: &[u8]| -> Option<i64> {
        let mut n = 0i64;
        for &c in slice {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + i64::from(c - b'0');
        }
        Some(n)
    };
    let (y, m, d, hh, mm, ss) = (
        atoi(&bytes[0..4])?,
        atoi(&bytes[5..7])?,
        atoi(&bytes[8..10])?,
        atoi(&bytes[11..13])?,
        atoi(&bytes[14..16])?,
        atoi(&bytes[17..19])?,
    );
    // Civil-from-days (Howard Hinnant). UTC, no DST.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 {
        y_adj / 400
    } else {
        (y_adj - 399) / 400
    };
    let yoe = y_adj - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_parser_basic() {
        // Sanity-check via a self-consistent epoch round-trip rather
        // than a hand-calculated day count: parse two timestamps a
        // known number of seconds apart.
        let a = parse_iso8601_to_unix("2026-05-19T00:00:00Z").unwrap();
        let b = parse_iso8601_to_unix("2026-05-20T00:00:00Z").unwrap();
        assert_eq!(b - a, 86_400);
        // And one round-the-year jump from 2025-01-01 to 2026-01-01
        // (365 days; 2025 is not a leap year).
        let y2025 = parse_iso8601_to_unix("2025-01-01T00:00:00Z").unwrap();
        let y2026 = parse_iso8601_to_unix("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(y2026 - y2025, 365 * 86_400);
        // 2024 was a leap year; 2024-01-01 → 2025-01-01 = 366d.
        let y2024 = parse_iso8601_to_unix("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(y2025 - y2024, 366 * 86_400);
    }

    #[test]
    fn iso8601_parser_with_fractional_secs() {
        let base = parse_iso8601_to_unix("2026-05-19T00:00:00Z").unwrap();
        let withtime = parse_iso8601_to_unix("2026-05-19T12:34:56.789Z").unwrap();
        assert_eq!(withtime - base, 12 * 3600 + 34 * 60 + 56);
    }

    #[test]
    fn iso8601_parser_rejects_malformed() {
        assert!(parse_iso8601_to_unix("not-a-date").is_none());
        assert!(parse_iso8601_to_unix("2026/05/19").is_none());
        assert!(parse_iso8601_to_unix("").is_none());
    }
}
