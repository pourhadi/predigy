//! Probe the authed WS channels (`fill`, `order_state`,
//! `market_positions`). Subscribes, then prints every raw message
//! envelope (after JSON re-serialisation) for ~30 s. Used to capture
//! wire shapes for the typed decoder.
//!
//! ```text
//! KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!   cargo run -p predigy-kalshi-md --example authed_channels_probe
//! ```
//!
//! While this is running, place a small order via `oms_submit_smoke`
//! or `close_position` in another shell to trigger fill /
//! `order_state` / `market_positions` messages.

use predigy_kalshi_md::{Channel, Client};
use predigy_kalshi_rest::Signer;
use std::env;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let key_id = env::var("KALSHI_KEY_ID")?;
    let pem_path = env::var("KALSHI_PEM")?;
    let pem = std::fs::read_to_string(&pem_path)?;
    let duration_secs: u64 = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let signer = Signer::from_pem(&key_id, &pem)?;
    let client = Client::new(signer)?;
    let mut conn = client.connect();

    // Subscribe to all three authed channels at once. No
    // market_ticker filter (we want every event for this account).
    let req_id = conn
        .subscribe(
            &[Channel::Fill, Channel::OrderState, Channel::MarketPositions],
            &[],
        )
        .await?;
    eprintln!("authed_channels_probe: subscribed req_id={req_id}, listening for {duration_secs}s");

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        match tokio::time::timeout(remaining, conn.next_event()).await {
            Ok(Some(ev)) => {
                println!("event: {ev:?}");
            }
            Ok(None) => {
                eprintln!("stream ended");
                break;
            }
            Err(_) => break,
        }
    }
    conn.close().await;
    Ok(())
}
