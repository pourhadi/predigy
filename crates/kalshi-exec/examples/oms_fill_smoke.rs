//! Live-fill smoke test: round-trip a single contract through the
//! OMS and verify the fills-poller → `Filled` → `PositionUpdated`
//! path against Kalshi prod.
//!
//! Step 1: REST fetch the orderbook for the target market and assert
//! the best YES ask is ≤ a safety ceiling (default 15¢). This caps
//! capital at risk before any submit.
//!
//! Step 2: Submit a BUY YES IOC at `best_yes_ask` for 1 contract.
//! Drain the OMS event stream until we see `Filled` (or
//! `PartiallyFilled` then `Cancelled` from the IOC tail) and
//! `PositionUpdated { new_qty: 1, ... }`.
//!
//! Step 3: Submit a SELL YES IOC at `best_yes_bid` for 1 contract.
//! Drain until the second `Filled` and `PositionUpdated { new_qty:
//! 0, ... }`. Realized P&L delta on this event is `(sell_px -
//! buy_px) * 1` minus venue fees.
//!
//! Worst-case capital at risk: `safety_ceiling_cents` (default 15)
//! contracts of $1 per contract = $0.15. In practice the buy fills
//! at ~ask, sell at ~bid, net round-trip cost ≈ 1-3¢ + fees.
//!
//!     KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!       cargo run -p predigy-kalshi-exec --example oms_fill_smoke -- \
//!       KXNBASERIES-26LALOKCR2-LAL
//!
//! Optional second arg: safety ceiling in cents (default 15).

#![allow(clippy::too_many_lines)]

use anyhow::{Result, anyhow};
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_md::Client as MdClient;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent, OmsHandle};
use predigy_risk::{AccountLimits, Limits, PerMarketLimits, RateLimits, RiskEngine};
use std::collections::HashMap;
use std::env;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let key_id = env::var("KALSHI_KEY_ID")?;
    let pem_path = env::var("KALSHI_PEM")?;
    let pem = std::fs::read_to_string(&pem_path)?;
    let market = env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: oms_fill_smoke <market-ticker> [safety-ceiling-cents]"))?;
    let safety_ceiling: u8 = env::args()
        .nth(2)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(15);

    let use_ws_fills = std::env::var("KALSHI_FILLS_WS").as_deref() == Ok("1");
    let signer = Signer::from_pem(&key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = RestClient::authed(signer).map_err(|e| anyhow!("rest: {e}"))?;

    // Step 1: pre-flight check the touch.
    let snap = rest
        .orderbook_snapshot(&market)
        .await
        .map_err(|e| anyhow!("orderbook_snapshot: {e}"))?;
    let best_yes_bid = snap
        .yes_bids
        .iter()
        .max_by_key(|(p, _)| p.cents())
        .copied()
        .ok_or_else(|| anyhow!("no YES bids on {market}"))?;
    let best_no_bid = snap
        .no_bids
        .iter()
        .max_by_key(|(p, _)| p.cents())
        .copied()
        .ok_or_else(|| anyhow!("no NO bids on {market}"))?;
    let yes_ask_cents = 100u8.saturating_sub(best_no_bid.0.cents());
    let yes_ask = Price::from_cents(yes_ask_cents).map_err(|e| anyhow!("derive yes_ask: {e}"))?;

    eprintln!(
        "preflight: market={market} yes_bid={}¢×{} yes_ask={}¢ (from no_bid={}¢×{})",
        best_yes_bid.0.cents(),
        best_yes_bid.1,
        yes_ask.cents(),
        best_no_bid.0.cents(),
        best_no_bid.1,
    );
    if yes_ask.cents() > safety_ceiling {
        return Err(anyhow!(
            "yes_ask {}¢ exceeds safety ceiling {}¢; aborting before any submit",
            yes_ask.cents(),
            safety_ceiling
        ));
    }
    if best_yes_bid.1 < 1 || best_no_bid.1 < 1 {
        return Err(anyhow!("best touch < 1 contract deep; aborting"));
    }

    // Step 2: spin up OMS with caps that limit blast radius.
    let cap = u64::from(safety_ceiling);
    let (executor, reports) = if use_ws_fills {
        eprintln!("using WS-pushed fills (KALSHI_FILLS_WS=1)");
        let ws_signer = Signer::from_pem(&key_id, &pem).map_err(|e| anyhow!("ws signer: {e}"))?;
        let ws_client = MdClient::new(ws_signer).map_err(|e| anyhow!("ws client: {e}"))?;
        RestExecutor::spawn_with_ws_fills(
            rest,
            ws_client,
            // Slow REST poll as catch-up safety net for WS gaps.
            PollerConfig {
                interval: Duration::from_secs(5),
                initial_lookback: Duration::from_mins(1),
            },
        )
    } else {
        RestExecutor::spawn(
            rest,
            PollerConfig {
                interval: Duration::from_millis(500),
                initial_lookback: Duration::from_mins(1),
            },
        )
    };
    let limits = Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: 2,
            max_notional_cents_per_side: cap,
        },
        per_market_overrides: HashMap::default(),
        account: AccountLimits {
            max_gross_notional_cents: cap,
            max_daily_loss_cents: 100,
        },
        rate: RateLimits {
            max_orders_per_window: 10,
            window: Duration::from_secs(1),
        },
    };
    // Seed the cid sequence with a monotonic timestamp so reruns
    // don't collide with prior runs' cids (Kalshi remembers them
    // across the whole account history → submitting a duplicate
    // returns 409 `order_already_exists`).
    let start_seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut oms = Oms::try_spawn(
        OmsConfig {
            strategy_id: "oms-fill".into(),
            cid_backing: CidBacking::InMemory { start_seq },
        },
        RiskEngine::new(limits),
        executor,
        reports,
    )
    .map_err(|e| anyhow!("oms init: {e}"))?;

    // Step 3: BUY 1 YES IOC at the ask.
    let buy_intent = Intent::limit(
        MarketTicker::new(&market),
        Side::Yes,
        Action::Buy,
        yes_ask,
        Qty::new(1).unwrap(),
    )
    .with_tif(TimeInForce::Ioc);
    let buy_cid = oms
        .submit(buy_intent)
        .await
        .map_err(|e| anyhow!("buy submit: {e}"))?;
    eprintln!("BUY submitted cid={buy_cid} px={}¢", yes_ask.cents());

    let buy_result = drain_until_terminal(&mut oms, &market, /* expect_qty */ 1).await?;
    eprintln!(
        "BUY terminal: filled_qty={} fill_avg={}¢ realized_pnl_delta={}¢",
        buy_result.cumulative_qty, buy_result.fill_price_cents, buy_result.realized_pnl_delta_cents
    );
    if buy_result.cumulative_qty == 0 {
        oms.close().await;
        return Err(anyhow!(
            "BUY filled 0 contracts; nothing to sell. Touch may have moved between preflight \
             and submit, leaving the IOC limit non-marketable."
        ));
    }

    // Step 4: SELL N YES IOC at the bid (where N = buy_result.cumulative_qty).
    let sell_qty = buy_result.cumulative_qty;
    let sell_intent = Intent::limit(
        MarketTicker::new(&market),
        Side::Yes,
        Action::Sell,
        best_yes_bid.0,
        Qty::new(sell_qty).unwrap(),
    )
    .with_tif(TimeInForce::Ioc);
    let sell_cid = oms
        .submit(sell_intent)
        .await
        .map_err(|e| anyhow!("sell submit: {e}"))?;
    eprintln!(
        "SELL submitted cid={sell_cid} px={}¢ qty={sell_qty}",
        best_yes_bid.0.cents()
    );

    let sell_result = drain_until_terminal(&mut oms, &market, /* expect_qty */ sell_qty).await?;
    eprintln!(
        "SELL terminal: filled_qty={} fill_avg={}¢ realized_pnl_delta={}¢ \
         new_position_qty={}",
        sell_result.cumulative_qty,
        sell_result.fill_price_cents,
        sell_result.realized_pnl_delta_cents,
        sell_result.new_qty,
    );

    let net_pnl_cents = buy_result.realized_pnl_delta_cents + sell_result.realized_pnl_delta_cents;
    println!(
        "round-trip done: buy_fill={}¢ sell_fill={}¢ realized_pnl_total={net_pnl_cents}¢ (pre-fees) \
         final_position={}",
        buy_result.fill_price_cents, sell_result.fill_price_cents, sell_result.new_qty,
    );
    if sell_result.new_qty != 0 {
        oms.close().await;
        return Err(anyhow!(
            "expected final position = 0 contracts, got {}",
            sell_result.new_qty
        ));
    }
    oms.close().await;
    Ok(())
}

#[derive(Debug, Default)]
struct LegOutcome {
    cumulative_qty: u32,
    fill_price_cents: u8,
    realized_pnl_delta_cents: i64,
    new_qty: u32,
}

/// Drain OMS events until we see *both* a terminal report
/// (Filled / Cancelled / Rejected) for the most recent submit *and*
/// a `PositionUpdated` for `market`. Returns the combined outcome.
/// Bails after 30 seconds with whatever we've seen — the fills
/// poller is on a 500 ms cycle so 30 s is plenty.
async fn drain_until_terminal(
    oms: &mut OmsHandle,
    market: &str,
    _expect_qty: u32,
) -> Result<LegOutcome> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut out = LegOutcome::default();
    let mut got_terminal = false;
    let mut got_position = false;
    while std::time::Instant::now() < deadline && !(got_terminal && got_position) {
        let remaining = deadline - std::time::Instant::now();
        match tokio::time::timeout(remaining, oms.next_event()).await {
            Ok(Some(ev)) => {
                eprintln!("oms event: {ev:?}");
                match ev {
                    OmsEvent::Filled {
                        cumulative_qty,
                        fill_price,
                        ..
                    } => {
                        out.cumulative_qty = cumulative_qty;
                        out.fill_price_cents = fill_price.cents();
                        got_terminal = true;
                    }
                    OmsEvent::PartiallyFilled {
                        cumulative_qty,
                        fill_price,
                        ..
                    } => {
                        out.cumulative_qty = cumulative_qty;
                        out.fill_price_cents = fill_price.cents();
                        // not terminal — IOC tail will follow as Cancelled
                    }
                    OmsEvent::Cancelled { .. } => got_terminal = true,
                    OmsEvent::Rejected { reason, .. } => {
                        return Err(anyhow!("rejected: {reason}"));
                    }
                    OmsEvent::PositionUpdated {
                        market: m,
                        new_qty,
                        realized_pnl_delta_cents,
                        ..
                    } if m.as_str() == market => {
                        out.new_qty = new_qty;
                        out.realized_pnl_delta_cents = realized_pnl_delta_cents;
                        got_position = true;
                    }
                    _ => {}
                }
            }
            Ok(None) => return Err(anyhow!("oms event stream ended early")),
            Err(_) => break,
        }
    }
    if !got_terminal {
        return Err(anyhow!(
            "no terminal report (Filled/Cancelled/Rejected) within 30 s"
        ));
    }
    Ok(out)
}
