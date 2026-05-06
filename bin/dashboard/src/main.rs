// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `predigy-dashboard`: mobile-friendly HTTP dashboard for the
//! whole strategy fleet.
//!
//! Serves a single-page HTML view at `/` that auto-refreshes every
//! 15 s, plus JSON API at `/api/state`. Backed by a periodically-
//! refreshed in-memory snapshot that pulls from:
//!
//! - Kalshi REST (`/portfolio/balance`, `/portfolio/positions`)
//! - Each strategy's OMS state file (positions, daily P&L,
//!   kill-switch, in-flight order count)
//! - Each strategy's stderr log (recent rule fires, fills, errors)
//! - The shared kill-flag file at `~/.config/predigy/kill-switch.flag`
//!   (POST `/api/kill` writes/clears this; the strategy daemons
//!   poll it via [`predigy_oms::spawn_kill_watcher`]).
//!
//! ```text
//! predigy-dashboard \
//!     --kalshi-key-id $KALSHI_KEY_ID \
//!     --kalshi-pem    /path/to/kalshi.pem \
//!     --strategy "weather=~/.config/predigy/oms-state.json:~/Library/Logs/predigy/latency-trader.stderr.log" \
//!     --strategy "settlement=~/.config/predigy/oms-state-settlement.json:~/Library/Logs/predigy/settlement.stderr.log" \
//!     --strategy "cross-arb=~/.config/predigy/oms-state-cross-arb.json:~/Library/Logs/predigy/cross-arb.stderr.log" \
//!     --kill-flag ~/.config/predigy/kill-switch.flag \
//!     --bind 0.0.0.0:8080
//! ```
//!
//! Bind `0.0.0.0:8080` for LAN/Tailscale access from a phone;
//! `127.0.0.1:8080` (default) for local-only.

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Router,
    extract::State,
    http::header,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
};
use clap::Parser;
use predigy_kalshi_rest::types::MarketPosition;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const RECENT_EVENTS_KEEP: usize = 30;
const HTML: &str = include_str!("../static/index.html");

#[derive(Debug, Parser)]
#[command(
    name = "predigy-dashboard",
    about = "Mobile dashboard for the predigy fleet."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// Repeatable strategy spec: `NAME=OMS_STATE_PATH:LOG_PATH`.
    /// Pass once per strategy (weather, settlement, cross-arb, …).
    /// Tilde expansion supported in both paths.
    #[arg(long = "strategy", value_parser = parse_strategy_spec)]
    strategies: Vec<StrategySpec>,

    /// Path to the shared kill-switch flag file. Written by
    /// `POST /api/kill`. Polled by each strategy daemon.
    #[arg(long, default_value = "~/.config/predigy/kill-switch.flag")]
    kill_flag: PathBuf,

    /// Bind address. `127.0.0.1:8080` (default) restricts to local;
    /// use `0.0.0.0:8080` for LAN/Tailscale.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,
}

#[derive(Debug, Clone)]
struct StrategySpec {
    name: String,
    oms_state: PathBuf,
    log_file: PathBuf,
}

fn parse_strategy_spec(s: &str) -> std::result::Result<StrategySpec, String> {
    let (name, rest) = s
        .split_once('=')
        .ok_or_else(|| format!("expected NAME=OMS_PATH:LOG_PATH, got {s:?}"))?;
    let (oms, log) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("expected OMS_PATH:LOG_PATH, got {rest:?}"))?;
    Ok(StrategySpec {
        name: name.trim().to_string(),
        oms_state: PathBuf::from(oms.trim()),
        log_file: PathBuf::from(log.trim()),
    })
}

#[derive(Debug, Default, Clone, Serialize)]
struct Snapshot {
    refreshed_at: i64,
    /// Cents of settled cash on Kalshi.
    balance_cents: i64,
    /// Cents of mark-to-market open positions.
    portfolio_cents: i64,
    open_positions: Vec<PositionRow>,
    /// Per-strategy P&L + status rolled up from each OMS state file.
    strategies: Vec<StrategyRow>,
    /// Recent rule fires/fills aggregated across all strategy logs,
    /// newest first.
    recent_events: Vec<EventRow>,
    /// Sum of `oms_daily_realized_pnl_cents` across strategies.
    total_daily_realized_pnl_cents: i64,
    /// Any strategy currently armed.
    any_kill_switch: bool,
    /// Shared kill-flag file currently armed.
    kill_flag_armed: bool,
    last_refresh_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StrategyRow {
    name: String,
    oms_daily_realized_pnl_cents: i64,
    oms_kill_switch: bool,
    oms_in_flight_orders: usize,
    /// How long since the strategy log was last written, in
    /// seconds. `None` if the log is missing.
    log_age_secs: Option<i64>,
    /// `None` if the OMS state file doesn't exist yet.
    oms_state_present: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PositionRow {
    ticker: String,
    contracts: f64,
    exposure_dollars: f64,
    realized_pnl_dollars: f64,
    fees_paid_dollars: f64,
    resting_orders: u32,
}

/// Generic strategy event extracted from a stderr log: fires,
/// submits, fills, rejects.
#[derive(Debug, Clone, Serialize)]
struct EventRow {
    ts: i64,
    strategy: String,
    kind: String,
    /// Free-form summary the UI displays as the body of the card.
    summary: String,
}

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Snapshot>>,
    kill_flag: Arc<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct KillRequest {
    armed: bool,
}

#[derive(Debug, Serialize)]
struct KillResponse {
    armed: bool,
    flag_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let strategies: Vec<StrategySpec> = args
        .strategies
        .into_iter()
        .map(|s| StrategySpec {
            name: s.name,
            oms_state: expand_tilde(&s.oms_state),
            log_file: expand_tilde(&s.log_file),
        })
        .collect();
    if strategies.is_empty() {
        return Err(anyhow!(
            "no --strategy provided; pass at least one NAME=OMS_PATH:LOG_PATH"
        ));
    }
    let kill_flag = expand_tilde(&args.kill_flag);

    let pem = std::fs::read_to_string(expand_tilde(&args.kalshi_pem))
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;
    let signer = Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(signer))
    } else {
        RestClient::authed(signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;
    let rest = Arc::new(rest);

    let state = AppState {
        snapshot: Arc::new(RwLock::new(Snapshot::default())),
        kill_flag: Arc::new(kill_flag.clone()),
    };

    let initial = build_snapshot(&rest, &strategies, &kill_flag).await;
    *state.snapshot.write().await = initial;

    let refresh_state = state.clone();
    let refresh_rest = rest.clone();
    let refresh_strats = strategies.clone();
    let refresh_kill_flag = kill_flag.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let snap = build_snapshot(&refresh_rest, &refresh_strats, &refresh_kill_flag).await;
            *refresh_state.snapshot.write().await = snap;
        }
    });

    let app = Router::new()
        .route("/", get(serve_html))
        .route("/api/state", get(serve_state))
        .route("/api/kill", post(serve_kill))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("bind {}", args.bind))?;
    let bound = listener.local_addr()?;
    info!(%bound, strategies = strategies.len(), "predigy-dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(HTML),
    )
}

async fn serve_state(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.snapshot.read().await.clone();
    Json(snap)
}

async fn serve_kill(
    State(state): State<AppState>,
    Json(req): Json<KillRequest>,
) -> impl IntoResponse {
    let path = state.kill_flag.as_ref().clone();
    let body = if req.armed { "armed\n" } else { "" };
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    })();
    match result {
        Ok(()) => {
            info!(armed = req.armed, path = %path.display(), "kill flag updated");
            Json(KillResponse {
                armed: req.armed,
                flag_path: path.display().to_string(),
            })
            .into_response()
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "kill flag write failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("kill flag write failed: {e}"),
            )
                .into_response()
        }
    }
}

async fn build_snapshot(
    rest: &RestClient,
    strategies: &[StrategySpec],
    kill_flag: &std::path::Path,
) -> Snapshot {
    let mut snap = Snapshot {
        refreshed_at: now_unix(),
        ..Snapshot::default()
    };

    match rest.balance().await {
        Ok(b) => {
            snap.balance_cents = b.balance;
            snap.portfolio_cents = b.portfolio_value;
        }
        Err(e) => {
            snap.last_refresh_error = Some(format!("balance: {e}"));
            warn!(error = %e, "refresh: balance failed");
        }
    }

    match rest.positions().await {
        Ok(positions_resp) => {
            snap.open_positions = positions_resp
                .market_positions
                .into_iter()
                .filter(has_activity)
                .map(position_row)
                .collect();
        }
        Err(e) => {
            snap.last_refresh_error = Some(format!("positions: {e}"));
            warn!(error = %e, "refresh: positions failed");
        }
    }

    let mut total_pnl: i64 = 0;
    let mut any_kill = false;
    let mut all_events: Vec<EventRow> = Vec::new();
    for strat in strategies {
        let mut row = StrategyRow {
            name: strat.name.clone(),
            oms_daily_realized_pnl_cents: 0,
            oms_kill_switch: false,
            oms_in_flight_orders: 0,
            log_age_secs: log_age_secs(&strat.log_file),
            oms_state_present: false,
        };
        match read_oms_state(&strat.oms_state) {
            Ok(Some(oms)) => {
                row.oms_state_present = true;
                row.oms_daily_realized_pnl_cents = oms
                    .get("account")
                    .and_then(|a| a.get("daily_realized_pnl_cents"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                row.oms_kill_switch = oms
                    .get("account")
                    .and_then(|a| a.get("kill_switch"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                row.oms_in_flight_orders = oms
                    .get("orders")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, std::vec::Vec::len);
            }
            Ok(None) => { /* file not yet written */ }
            Err(e) => {
                snap.last_refresh_error = Some(format!("oms-state {}: {e}", strat.name));
            }
        }
        total_pnl += row.oms_daily_realized_pnl_cents;
        any_kill = any_kill || row.oms_kill_switch;
        all_events.extend(parse_recent_events(&strat.name, &strat.log_file));
        snap.strategies.push(row);
    }
    snap.total_daily_realized_pnl_cents = total_pnl;
    snap.any_kill_switch = any_kill;
    snap.kill_flag_armed = read_kill_flag(kill_flag);

    all_events.sort_by_key(|e| std::cmp::Reverse(e.ts));
    all_events.truncate(RECENT_EVENTS_KEEP);
    snap.recent_events = all_events;

    snap
}

fn has_activity(p: &MarketPosition) -> bool {
    p.position_contracts.unwrap_or(0.0).abs() > 1e-9
        || p.fees_paid_dollars.unwrap_or(0.0).abs() > 1e-9
        || p.resting_orders_count.unwrap_or(0) > 0
}

fn position_row(p: MarketPosition) -> PositionRow {
    PositionRow {
        ticker: p.ticker,
        contracts: p.position_contracts.unwrap_or(0.0),
        exposure_dollars: p.market_exposure_dollars.unwrap_or(0.0),
        realized_pnl_dollars: p.realized_pnl_dollars.unwrap_or(0.0),
        fees_paid_dollars: p.fees_paid_dollars.unwrap_or(0.0),
        resting_orders: p.resting_orders_count.unwrap_or(0),
    }
}

fn read_oms_state(path: &std::path::Path) -> Result<Option<serde_json::Value>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn read_kill_flag(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| s.trim().to_ascii_lowercase().starts_with("armed"))
}

fn log_age_secs(path: &std::path::Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let now = SystemTime::now();
    let dur = now.duration_since(mtime).ok()?;
    i64::try_from(dur.as_secs()).ok()
}

/// Parse the last N kind-of-interest lines from a strategy log.
/// We pick up `rule fired`, `submitted`, `filled`, `partial`,
/// `rejected`, and any line with a `cid=` plus `position`.
fn parse_recent_events(strategy: &str, path: &std::path::Path) -> Vec<EventRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<EventRow> = Vec::new();
    for raw in text.lines().rev() {
        let line = strip_ansi(raw);
        let lower = line.to_ascii_lowercase();
        let kind = if lower.contains("rule fired") {
            "fire"
        } else if lower.contains(" filled ") || lower.contains(" partial ") {
            "fill"
        } else if lower.contains("submit rejected") || lower.contains("rejected ") {
            "reject"
        } else if lower.contains(" submitted ") {
            "submit"
        } else if lower.contains("position ") && lower.contains("cid=") {
            "position"
        } else {
            continue;
        };
        let ts = parse_iso_timestamp(&line);
        let summary = extract_summary(&line);
        out.push(EventRow {
            ts,
            strategy: strategy.to_string(),
            kind: kind.to_string(),
            summary,
        });
        if out.len() >= RECENT_EVENTS_KEEP {
            break;
        }
    }
    out
}

/// Pull the structured-fields tail off a tracing log line.
/// Tracing's `key=value` tail starts after the message; we keep
/// it short and human-readable for the dashboard card.
fn extract_summary(line: &str) -> String {
    // Drop the leading timestamp + level + module prefix if any.
    // Best-effort: split on the first occurrence of double-space
    // after the initial timestamp and keep everything past it.
    let trimmed = line.trim();
    let after_ts = trimmed.get(20..).unwrap_or(trimmed);
    let after_level = after_ts
        .split_once("INFO ")
        .map(|x| x.1)
        .or_else(|| after_ts.split_once("WARN ").map(|x| x.1))
        .or_else(|| after_ts.split_once("ERROR").map(|x| x.1))
        .unwrap_or(after_ts);
    after_level.trim().chars().take(200).collect()
}

fn parse_iso_timestamp(line: &str) -> i64 {
    let part = line.get(..19).unwrap_or("");
    let bytes = part.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[10] != b'T' {
        return 0;
    }
    let (year, mon, day, hh, mm, ss) = (
        atoi(&bytes[0..4]),
        atoi(&bytes[5..7]),
        atoi(&bytes[8..10]),
        atoi(&bytes[11..13]),
        atoi(&bytes[14..16]),
        atoi(&bytes[17..19]),
    );
    if year == 0 {
        return 0;
    }
    days_from_epoch_utc(year, mon, day) * 86_400 + hh * 3_600 + mm * 60 + ss
}

fn atoi(b: &[u8]) -> i64 {
    let mut n = 0i64;
    for &c in b {
        if !c.is_ascii_digit() {
            return 0;
        }
        n = n * 10 + i64::from(c - b'0');
    }
    n
}

fn days_from_epoch_utc(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
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
    fn parse_strategy_spec_ok() {
        let s = parse_strategy_spec("weather=/tmp/oms.json:/tmp/wx.log").unwrap();
        assert_eq!(s.name, "weather");
        assert_eq!(s.oms_state, PathBuf::from("/tmp/oms.json"));
        assert_eq!(s.log_file, PathBuf::from("/tmp/wx.log"));
    }

    #[test]
    fn parse_strategy_spec_with_tilde() {
        let s = parse_strategy_spec("x=~/oms.json:~/log").unwrap();
        assert_eq!(s.oms_state, PathBuf::from("~/oms.json"));
    }

    #[test]
    fn parse_strategy_spec_rejects_malformed() {
        assert!(parse_strategy_spec("bad").is_err());
        assert!(parse_strategy_spec("name=/path-without-colon").is_err());
    }

    #[test]
    fn parse_iso_timestamp_basic() {
        let s = "2026-05-05T19:33:51.236748Z INFO ...";
        let t = parse_iso_timestamp(s);
        assert!(t > 1_700_000_000, "got {t}");
    }

    #[test]
    fn strip_ansi_drops_sgr_sequences() {
        let s = "\x1b[2m2026-05-05\x1b[0m INFO";
        assert_eq!(strip_ansi(s), "2026-05-05 INFO");
    }
}
