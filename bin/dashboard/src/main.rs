// Vendor names appear in docs.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `predigy-dashboard`: mobile-friendly HTTP dashboard for the
//! weather strategy daemon.
//!
//! Serves a single-page HTML view at `/` that auto-refreshes every
//! 15 seconds, plus a JSON API at `/api/state` for programmatic
//! consumers. Backed by a periodically-refreshed in-memory snapshot
//! that pulls from:
//!
//! - Kalshi REST (`/portfolio/balance`, `/portfolio/positions`) for
//!   the venue's authoritative view of cash + positions.
//! - The OMS state snapshot file (positions, daily P&L, kill-switch).
//! - The `latency-trader` log file (parsed for `rule fired` and
//!   `rule_fired`-derived fill events).
//!
//! ```text
//! predigy-dashboard \
//!     --kalshi-key-id $KALSHI_KEY_ID \
//!     --kalshi-pem    /path/to/kalshi.pem \
//!     --oms-state     ~/.config/predigy/oms-state.json \
//!     --log-file      ~/Library/Logs/predigy/latency-trader.stderr.log \
//!     --bind          0.0.0.0:8080
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
    routing::get,
};
use clap::Parser;
use predigy_kalshi_rest::types::MarketPosition;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const RECENT_FIRES_KEEP: usize = 30;
const HTML: &str = include_str!("../static/index.html");

#[derive(Debug, Parser)]
#[command(
    name = "predigy-dashboard",
    about = "Mobile dashboard for the predigy weather strategy."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// OMS state snapshot file (the same path latency-trader writes).
    #[arg(long, default_value = "~/.config/predigy/oms-state.json")]
    oms_state: PathBuf,

    /// latency-trader log file (the dashboard tails the last N
    /// lines for recent fires/fills).
    #[arg(
        long,
        default_value = "~/Library/Logs/predigy/latency-trader.stderr.log"
    )]
    log_file: PathBuf,

    /// Bind address. `127.0.0.1:8080` (default) restricts to local;
    /// use `0.0.0.0:8080` for LAN/Tailscale.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,
}

#[derive(Debug, Default, Clone, Serialize)]
struct Snapshot {
    /// Wall-clock unix-seconds when this snapshot was taken.
    refreshed_at: i64,
    /// Cents of settled cash on Kalshi.
    balance_cents: i64,
    /// Cents of mark-to-market open positions.
    portfolio_cents: i64,
    /// Per-market venue position rows (active, non-zero only).
    open_positions: Vec<PositionRow>,
    /// OMS-side daily realized P&L, cents (signed).
    oms_daily_realized_pnl_cents: i64,
    /// OMS kill-switch armed?
    oms_kill_switch: bool,
    /// In-flight order count from the OMS snapshot.
    oms_in_flight_orders: usize,
    /// Most recent rule fires parsed from the log (newest first).
    recent_fires: Vec<FireRow>,
    /// Latency-trader daemon health: how long since the log file
    /// was last written, in seconds. `None` if the log is missing.
    log_age_secs: Option<i64>,
    /// Last error / warning encountered when refreshing.
    last_refresh_error: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
struct FireRow {
    /// Unix seconds.
    ts: i64,
    market: String,
    event: String,
    severity: String,
    side: String,
    price_cents: u8,
    size: u32,
    dry_run: bool,
}

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Snapshot>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let oms_state = expand_tilde(&args.oms_state);
    let log_file = expand_tilde(&args.log_file);

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
    };

    // Seed the snapshot once before opening the port so the first
    // request doesn't see a zero-state placeholder.
    let initial = build_snapshot(&rest, &oms_state, &log_file).await;
    *state.snapshot.write().await = initial;

    // Background refresher.
    let refresh_state = state.clone();
    let refresh_rest = rest.clone();
    let refresh_oms = oms_state.clone();
    let refresh_log = log_file.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let snap = build_snapshot(&refresh_rest, &refresh_oms, &refresh_log).await;
            *refresh_state.snapshot.write().await = snap;
        }
    });

    let app = Router::new()
        .route("/", get(serve_html))
        .route("/api/state", get(serve_state))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("bind {}", args.bind))?;
    let bound = listener.local_addr()?;
    info!(%bound, "predigy-dashboard listening");
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

async fn build_snapshot(
    rest: &RestClient,
    oms_state: &std::path::Path,
    log_file: &std::path::Path,
) -> Snapshot {
    let mut snap = Snapshot {
        refreshed_at: now_unix(),
        ..Snapshot::default()
    };
    // Balance + portfolio value.
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
    // Positions (filter to non-zero).
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
    // OMS state snapshot — best-effort.
    match read_oms_state(oms_state) {
        Ok(Some(oms)) => {
            snap.oms_daily_realized_pnl_cents = oms
                .get("account")
                .and_then(|a| a.get("daily_realized_pnl_cents"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            snap.oms_kill_switch = oms
                .get("account")
                .and_then(|a| a.get("kill_switch"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            snap.oms_in_flight_orders = oms
                .get("orders")
                .and_then(serde_json::Value::as_array)
                .map_or(0, std::vec::Vec::len);
        }
        Ok(None) => {
            // First-run case — file not yet written.
        }
        Err(e) => {
            snap.last_refresh_error = Some(format!("oms-state: {e}"));
        }
    }
    // Tail log for fires + age.
    snap.log_age_secs = log_age_secs(log_file);
    snap.recent_fires = parse_recent_fires(log_file);
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

fn log_age_secs(path: &std::path::Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let now = SystemTime::now();
    let dur = now.duration_since(mtime).ok()?;
    i64::try_from(dur.as_secs()).ok()
}

/// Tails the log file looking for the structured "rule fired" line
/// emitted by latency-trader. The line is roughly:
///
///   2026-05-05T... INFO latency-trader: rule fired event=...
///       area=... severity=... market=... side=... price=... size=...
///       dry_run=... rule_idx=...
///
/// We do a brittle but cheap regex parse — production-grade would
/// switch latency-trader to JSON logs and consume that. For v1 the
/// goal is "show the operator what's happening" and a fuzzy parser
/// gets us 95% of the way there with no dependency.
fn parse_recent_fires(path: &std::path::Path) -> Vec<FireRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut fires: Vec<FireRow> = Vec::new();
    for raw in text.lines().rev() {
        if !raw.contains("rule fired") {
            continue;
        }
        // Strip ANSI colour codes that tracing emits when not piped
        // through a tty stripper.
        let line = strip_ansi(raw);
        let Some(fire) = parse_fire_line(&line) else {
            continue;
        };
        fires.push(fire);
        if fires.len() >= RECENT_FIRES_KEEP {
            break;
        }
    }
    fires
}

fn parse_fire_line(line: &str) -> Option<FireRow> {
    let ts = parse_iso_timestamp(line);
    // Each known key's value runs from `<key>=` up to the next
    // ` <known-key>=` boundary. Without the leading space, a value
    // like `area=Lincoln, Lyon ...` would swallow the whole tail.
    let event = field(line, "event=", &[" area=", " severity="]).unwrap_or_default();
    let severity = field(line, "severity=", &[" market=", " rule_idx="]).unwrap_or_default();
    let market = field(line, "market=", &[" side="]).unwrap_or_default();
    let side = field(line, "side=", &[" price="]).unwrap_or_default();
    let price_cents = field(line, "price=", &[" size="])
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0);
    let qty = field(line, "size=", &[" dry_run="])
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let dry_run = field(line, "dry_run=", &[" rule_idx="])
        .or_else(|| field(line, "dry_run=", &[]))
        .as_deref()
        == Some("true");
    if market.is_empty() || event.is_empty() {
        return None;
    }
    Some(FireRow {
        ts,
        market,
        event,
        severity,
        side,
        price_cents,
        size: qty,
        dry_run,
    })
}

/// Extract `<start>VALUE<end-marker>` from `line`. Picks the
/// nearest `end_markers` match (so multiple candidates work). If
/// none match, runs to end of line.
fn field(line: &str, start: &str, end_markers: &[&str]) -> Option<String> {
    let i = line.find(start)? + start.len();
    let tail = &line[i..];
    let j = end_markers
        .iter()
        .filter_map(|m| tail.find(m))
        .min()
        .unwrap_or(tail.len());
    Some(tail[..j].trim().to_string())
}

/// Best-effort: pull the leading ISO timestamp out of the log line.
/// Returns 0 if the line doesn't start with one.
fn parse_iso_timestamp(line: &str) -> i64 {
    // Lines look like `2026-05-05T19:33:51.236748Z INFO ...`. Keep
    // the date+time part, drop fractional seconds + `Z`, parse to
    // unix-seconds via a tiny manual scan (no chrono dep).
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

/// Days from 1970-01-01 to (year, month, day) UTC. Civil-from-days
/// algorithm (Howard Hinnant). No locale, no DST — log timestamps
/// are emitted as UTC `Z` by tracing's default fmt.
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
    // Trivial state machine — sufficient for the `\x1b[...m` SGR
    // sequences `tracing-subscriber` emits. Doesn't bother with
    // full ANSI; nothing else appears in our logs.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // skip until 'm'
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
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        i64::try_from(d.as_secs()).unwrap_or(0)
    })
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

// Keep `Instant` referenced so the lints don't warn on a stale
// import if the periodic task signature changes.
const _UNUSED_INSTANT: Option<Instant> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fire_extracts_fields() {
        let raw = "2026-05-05T19:33:51.236748Z  INFO latency-trader: rule fired \
                   rule_idx=44 event=Winter Storm Warning area=South CO severity=Severe \
                   market=KXHIGHDEN-26MAY05-T48 side=Yes price=52 size=1 dry_run=true";
        let f = parse_fire_line(raw).expect("parsed");
        assert_eq!(f.market, "KXHIGHDEN-26MAY05-T48");
        assert_eq!(f.severity, "Severe");
        assert_eq!(f.price_cents, 52);
        assert_eq!(f.size, 1);
        assert!(f.dry_run);
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
