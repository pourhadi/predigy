//! Authed read-only smoke test. Verifies the RSA-PSS signer
//! handshake against prod. Reads `KALSHI_KEY_ID` + `KALSHI_PEM`
//! from env. Hits `GET /portfolio/positions` which is auth-only
//! and read-only — no orders submitted, no state mutated.

use predigy_kalshi_rest::{Client, Signer};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_id = env::var("KALSHI_KEY_ID")?;
    let pem_path = env::var("KALSHI_PEM")?;
    let pem = std::fs::read_to_string(&pem_path)?;

    let signer = Signer::from_pem(&key_id, &pem)?;
    let client = Client::authed(signer)?;

    let resp = client.positions().await?;
    println!(
        "auth_smoke: signer accepted; {} market positions",
        resp.market_positions.len()
    );
    for p in resp.market_positions.iter().take(5) {
        println!(
            "  {} pos={} realized_pnl={:?} fees_paid={:?}",
            p.ticker, p.position, p.realized_pnl_dollars, p.fees_paid_dollars
        );
    }
    Ok(())
}
