//! One-shot REST close: sell 1 YES contract IOC on a market we have
//! a long position on. Uses the raw REST client (no OMS / no
//! tracking) so it's safe to run even when the OMS thinks there's no
//! position. Used during the live shake-down to flatten an
//! orphaned position.
//!
//!     KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!       cargo run -p predigy-kalshi-rest --example close_position -- \
//!       KXNBASERIES-26LALOKCR2-LAL 7 1
//!
//! Args: <market> <price_cents> <qty>

use predigy_kalshi_rest::types::{
    CreateOrderRequest, OrderSideV2, SelfTradePreventionV2, TimeInForceV2,
};
use predigy_kalshi_rest::{Client, Signer};
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_id = env::var("KALSHI_KEY_ID")?;
    let pem_path = env::var("KALSHI_PEM")?;
    let pem = std::fs::read_to_string(&pem_path)?;
    let market = env::args()
        .nth(1)
        .expect("usage: close_position <market> <price_cents> <qty>");
    let price_cents: u8 = env::args().nth(2).expect("price").parse()?;
    let qty: u32 = env::args().nth(3).expect("qty").parse()?;

    let signer = Signer::from_pem(&key_id, &pem)?;
    let client = Client::authed(signer)?;

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let cid = format!("close-{nanos}");
    let req = CreateOrderRequest {
        ticker: market.clone(),
        client_order_id: cid.clone(),
        side: OrderSideV2::Ask, // SELL YES
        count: format!("{qty}.00"),
        price: format!("{:.4}", f64::from(price_cents) / 100.0),
        time_in_force: TimeInForceV2::ImmediateOrCancel,
        self_trade_prevention_type: SelfTradePreventionV2::TakerAtCross,
        post_only: None,
        reduce_only: None,
    };
    println!("submitting close: cid={cid} req={req:?}");
    let resp = client.create_order(&req).await?;
    println!("response: {resp:?}");
    Ok(())
}
