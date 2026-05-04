use predigy_kalshi_rest::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let c = Client::public()?;
    let resp = c.list_markets(Some("open"), None, Some(3), None).await?;
    println!(
        "got {} markets, cursor={:?}",
        resp.markets.len(),
        resp.cursor
    );
    for m in resp.markets.iter().take(3) {
        println!(
            "  {} | {} | yes_ask={:?}",
            m.ticker, m.status, m.yes_ask_dollars
        );
        let snap = c.orderbook_snapshot(&m.ticker).await?;
        println!(
            "    snapshot: yes_levels={} no_levels={}",
            snap.yes_bids.len(),
            snap.no_bids.len()
        );
    }
    Ok(())
}
