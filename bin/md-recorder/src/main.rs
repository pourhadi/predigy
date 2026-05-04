//! `md-recorder` binary: long-running NDJSON recorder of Kalshi
//! market-data events with REST resync on sequence gap.
//!
//! Usage:
//!
//! ```text
//! md-recorder \
//!     --output ./data/2026-05-03.ndjson \
//!     --market FED-23DEC-T3.00 --market FED-23DEC-T3.25 \
//!     --kalshi-key-id $KALSHI_KEY_ID \
//!     --kalshi-pem    /path/to/kalshi.pem
//! ```
//!
//! `--ws-endpoint` and `--rest-endpoint` are overridable for sandbox /
//! test runs. SIGINT triggers a graceful shutdown that flushes the
//! NDJSON file before exit.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use md_recorder::{Recorder, SnapshotProvider};
use predigy_book::Snapshot;
use predigy_kalshi_md::{Channel, Client as MdClient};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "md-recorder",
    about = "Record Kalshi market data to NDJSON with REST resync on gaps."
)]
struct Args {
    /// Output NDJSON file. Created if missing, appended otherwise.
    #[arg(long)]
    output: PathBuf,

    /// Kalshi market tickers to subscribe. Pass multiple times for
    /// multiple markets.
    #[arg(long = "market", required = true)]
    markets: Vec<String>,

    /// Kalshi API key id (UUID-like). Required — the WS upgrade is
    /// authenticated even for public channels.
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,

    /// Path to the PEM-encoded Kalshi private key (PKCS#1 or PKCS#8).
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,

    /// Override the WS endpoint. Default:
    /// `wss://api.elections.kalshi.com/trade-api/ws/v2`.
    #[arg(long)]
    ws_endpoint: Option<Url>,

    /// Override the REST base URL. Default:
    /// `https://api.elections.kalshi.com/trade-api/v2`.
    #[arg(long)]
    rest_endpoint: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;
    let signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("build signer: {e}"))?;

    let rest = build_rest_client(&args)?;
    let ws = build_ws_client(&args, signer)?;

    let mut conn = ws.connect();
    let req_id = conn
        .subscribe(
            &[Channel::OrderbookDelta, Channel::Ticker, Channel::Trade],
            &args.markets,
        )
        .await
        .map_err(|e| anyhow!("subscribe: {e}"))?;
    info!(req_id, markets = ?args.markets, "subscribed");

    let provider = RestSnapshotProvider {
        client: Arc::new(rest),
    };
    let mut recorder = Recorder::new(args.output.clone(), provider);

    let stop = wait_for_ctrl_c();
    recorder.run(conn, stop).await?;
    info!(path = %args.output.display(), "recorder exited cleanly");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn build_rest_client(args: &Args) -> Result<RestClient> {
    let pem = std::fs::read_to_string(&args.kalshi_pem)
        .with_context(|| format!("re-read PEM at {}", args.kalshi_pem.display()))?;
    let signer =
        Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("REST signer: {e}"))?;
    let client = if let Some(base) = &args.rest_endpoint {
        RestClient::with_base(base, Some(signer))
    } else {
        RestClient::authed(signer)
    }
    .map_err(|e| anyhow!("build REST client: {e}"))?;
    Ok(client)
}

fn build_ws_client(args: &Args, signer: Signer) -> Result<MdClient> {
    if let Some(endpoint) = &args.ws_endpoint {
        Ok(MdClient::with_endpoint(endpoint.clone(), Some(signer)))
    } else {
        MdClient::new(signer).map_err(|e| anyhow!("build WS client: {e}"))
    }
}

async fn wait_for_ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %e, "ctrl_c handler failed; recorder will run until killed");
        // Park so the caller's select! doesn't fire spuriously.
        loop {
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
    }
}

/// Production [`SnapshotProvider`] backed by a `predigy_kalshi_rest::Client`.
struct RestSnapshotProvider {
    client: Arc<RestClient>,
}

impl SnapshotProvider for RestSnapshotProvider {
    async fn fresh_snapshot(&self, market: &str) -> Result<Snapshot> {
        self.client
            .orderbook_snapshot(market)
            .await
            .map_err(|e| anyhow!("kalshi REST orderbook_snapshot({market}): {e}"))
    }
}
