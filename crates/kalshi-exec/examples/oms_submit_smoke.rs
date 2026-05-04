//! End-to-end OMS + REST executor submit-then-cancel smoke test
//! against Kalshi prod. Submits a 1¢ resting BUY YES on a market
//! whose top-of-book is well above 1¢ (so the order rests, doesn't
//! fill), waits for the venue Acked, then cancels.
//!
//! Worst-case capital at risk: 1¢ (1 contract × 1¢) for the
//! ~5 seconds the order is alive. The order rests at the bottom of
//! the book and is unlikely to fill.
//!
//!     KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!       cargo run -p predigy-kalshi-exec --example oms_submit_smoke -- \
//!       KXNBASERIES-26PHINYKR2-PHI

#![allow(clippy::too_many_lines)]

use anyhow::{Result, anyhow};
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_kalshi_exec::{PollerConfig, RestExecutor};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use predigy_oms::{CidBacking, Oms, OmsConfig, OmsEvent};
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
        .unwrap_or_else(|| "KXNBASERIES-26PHINYKR2-PHI".to_string());

    let signer = Signer::from_pem(&key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = RestClient::authed(signer).map_err(|e| anyhow!("rest: {e}"))?;

    let (executor, reports) = RestExecutor::spawn(
        rest,
        PollerConfig {
            interval: Duration::from_millis(500),
            initial_lookback: Duration::from_mins(1),
        },
    );

    let limits = Limits {
        per_market: PerMarketLimits {
            max_contracts_per_side: 5,
            max_notional_cents_per_side: 50,
        },
        per_market_overrides: HashMap::default(),
        account: AccountLimits {
            max_gross_notional_cents: 50,
            max_daily_loss_cents: 50,
        },
        rate: RateLimits {
            max_orders_per_window: 10,
            window: Duration::from_secs(1),
        },
    };
    let mut oms = Oms::try_spawn(
        OmsConfig {
            strategy_id: "oms-smoke".into(),
            cid_backing: CidBacking::InMemory { start_seq: 0 },
            state_backing: predigy_oms::StateBacking::InMemory,
        },
        RiskEngine::new(limits),
        executor,
        reports,
    )
    .map_err(|e| anyhow!("oms init: {e}"))?;

    // Submit a 1¢ resting BUY YES limit. Top of book is above this
    // for any liquid market, so the order will rest, not fill.
    let intent = Intent::limit(
        MarketTicker::new(&market),
        Side::Yes,
        Action::Buy,
        Price::from_cents(1).unwrap(),
        Qty::new(1).unwrap(),
    );
    let cid = oms
        .submit(intent)
        .await
        .map_err(|e| anyhow!("oms.submit: {e}"))?;
    eprintln!("submitted cid={cid}");

    // Drain events for up to 10 seconds; bail when we see Acked.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut acked = false;
    while std::time::Instant::now() < deadline {
        let remaining = deadline - std::time::Instant::now();
        match tokio::time::timeout(remaining, oms.next_event()).await {
            Ok(Some(ev)) => {
                eprintln!("oms event: {ev:?}");
                if matches!(ev, OmsEvent::Acked { .. }) {
                    acked = true;
                    break;
                }
                if let OmsEvent::Rejected { reason, .. } = &ev {
                    return Err(anyhow!("submit rejected: {reason}"));
                }
            }
            Ok(None) => return Err(anyhow!("oms event stream ended early")),
            Err(_) => break, // timeout
        }
    }
    if !acked {
        return Err(anyhow!("never received Acked within 10s"));
    }
    eprintln!("✔ Acked. Sleeping 2s before cancel.");
    tokio::time::sleep(Duration::from_secs(2)).await;

    oms.cancel(cid.clone())
        .await
        .map_err(|e| anyhow!("oms.cancel: {e}"))?;
    eprintln!("cancel requested cid={cid}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut cancelled = false;
    while std::time::Instant::now() < deadline {
        let remaining = deadline - std::time::Instant::now();
        match tokio::time::timeout(remaining, oms.next_event()).await {
            Ok(Some(ev)) => {
                eprintln!("oms event: {ev:?}");
                if matches!(ev, OmsEvent::Cancelled { .. }) {
                    cancelled = true;
                    break;
                }
                if matches!(ev, OmsEvent::Filled { .. }) {
                    eprintln!(
                        "WARNING: order filled before we could cancel — that means the \
                         book had liquidity at 1¢ which is unusual on this market"
                    );
                    cancelled = true;
                    break;
                }
            }
            Ok(None) => return Err(anyhow!("oms event stream ended early")),
            Err(_) => break,
        }
    }
    if !cancelled {
        return Err(anyhow!("never received Cancelled within 10s"));
    }
    eprintln!("✔ Cancelled. Closing OMS.");
    oms.close().await;
    println!("oms_submit_smoke: SUBMIT + CANCEL round-trip succeeded");
    Ok(())
}
