//! One-shot REST order submit. Used to flatten orphan positions and
//! to smoke-test the V2 wire shape during live shake-down without
//! the OMS in the loop.
//!
//!     KALSHI_KEY_ID=... KALSHI_PEM=/path/to/key.pem \
//!       cargo run -p predigy-kalshi-rest --example close_position -- \
//!       <market> <price_cents> <qty> [buy|sell]
//!
//! `action` defaults to `sell` (close-position semantics). Pass `buy`
//! at a stub price (e.g. 1¢) to verify wire shape without filling.

use predigy_kalshi_rest::types::{
    CreateOrderRequest, OrderAction, OrderSideV2, SelfTradePreventionV2, TimeInForceV2,
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
    let action = match env::args().nth(4).as_deref() {
        Some("buy") => OrderAction::Buy,
        _ => OrderAction::Sell,
    };

    let signer = Signer::from_pem(&key_id, &pem)?;
    let client = Client::authed(signer)?;

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let cid = format!("close-{nanos}");
    let req = CreateOrderRequest {
        ticker: market.clone(),
        client_order_id: cid.clone(),
        // Sell YES → ask side; Buy YES → bid side (matches the
        // mapping in kalshi-exec).
        side: match action {
            OrderAction::Buy => OrderSideV2::Bid,
            OrderAction::Sell => OrderSideV2::Ask,
        },
        action,
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
