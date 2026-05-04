//! Authed WS smoke test: connect to Kalshi prod with the signer,
//! subscribe to a single liquid market's `orderbook_delta` and
//! `ticker` channels, print every event for ~30 s, then close.
//!
//!     KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!       cargo run -p predigy-kalshi-md --example ws_smoke -- \
//!       KXNBASERIES-26PHINYKR2-PHI

use predigy_kalshi_md::{Channel, Client, Event};
use predigy_kalshi_rest::Signer;
use std::env;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_id = env::var("KALSHI_KEY_ID")?;
    let pem_path = env::var("KALSHI_PEM")?;
    let pem = std::fs::read_to_string(&pem_path)?;
    let market = env::args()
        .nth(1)
        .unwrap_or_else(|| "KXNBASERIES-26PHINYKR2-PHI".to_string());

    let signer = Signer::from_pem(&key_id, &pem)?;
    let client = Client::new(signer)?;
    let mut conn = client.connect();

    let req_id = conn
        .subscribe(
            &[Channel::OrderbookDelta, Channel::Ticker],
            std::slice::from_ref(&market),
        )
        .await?;
    eprintln!("ws_smoke: subscribed market={market} req_id={req_id}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut snapshots = 0u64;
    let mut deltas = 0u64;
    let mut tickers = 0u64;
    let mut other = 0u64;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, conn.next_event()).await {
            Ok(Some(Event::Snapshot { snapshot, .. })) => {
                snapshots += 1;
                eprintln!(
                    "snapshot seq={} yes_levels={} no_levels={}",
                    snapshot.seq,
                    snapshot.yes_bids.len(),
                    snapshot.no_bids.len()
                );
            }
            Ok(Some(Event::Delta { delta, .. })) => {
                deltas += 1;
                if deltas <= 5 {
                    eprintln!(
                        "delta seq={} side={:?} price={} qty_delta={}",
                        delta.seq,
                        delta.side,
                        delta.price.cents(),
                        delta.qty_delta
                    );
                }
            }
            Ok(Some(Event::Ticker { body, .. })) => {
                tickers += 1;
                if tickers <= 3 {
                    eprintln!("ticker {body:?}");
                }
            }
            Ok(Some(Event::Subscribed { sid, channel, .. })) => {
                eprintln!("subscribed sid={sid} channel={channel:?}");
            }
            Ok(Some(Event::ServerError { code, msg, .. })) => {
                eprintln!("server error code={code} msg={msg}");
                break;
            }
            Ok(Some(Event::Disconnected { attempt, reason })) => {
                eprintln!("disconnected attempt={attempt} reason={reason}");
            }
            Ok(Some(Event::Reconnected)) => eprintln!("reconnected"),
            Ok(Some(Event::Malformed { error, .. })) => {
                eprintln!("malformed: {error}");
                other += 1;
            }
            Ok(Some(_)) => other += 1,
            Ok(None) => {
                eprintln!("ws_smoke: stream ended");
                break;
            }
            Err(_) => break, // timeout
        }
    }

    eprintln!(
        "ws_smoke done: snapshots={snapshots} deltas={deltas} tickers={tickers} other={other}"
    );
    conn.close().await;
    Ok(())
}
