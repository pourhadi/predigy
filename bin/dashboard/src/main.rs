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
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json},
    routing::{get, post},
};
use clap::Parser;
use predigy_kalshi_rest::types::{MarketDetail, MarketPosition};
use predigy_kalshi_rest::{Client as RestClient, Signer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const MARKET_DETAIL_CACHE_TTL: Duration = Duration::from_secs(60);
const MARKET_DETAIL_FETCH_SPACING: Duration = Duration::from_millis(75);
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

    /// Postgres connection string. Dashboard prefers DB-derived
    /// state when available, falling back to JSON file reads
    /// during the migration.
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///predigy")]
    database_url: String,
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
    /// Sum of `unrealized_pnl_cents` across active positions.
    /// `None` if any position couldn't be marked (e.g. market
    /// detail fetch failed).
    total_unrealized_pnl_cents: Option<i64>,
    /// Any strategy currently armed.
    any_kill_switch: bool,
    /// Shared kill-flag file currently armed.
    kill_flag_armed: bool,
    last_refresh_error: Option<String>,

    // Phase 6 — engine-side surfaces. Populated only when the
    // Postgres pool is available; empty otherwise (legacy
    // daemons don't write to the engine's `positions` /
    // `intents` tables in their JSON state files).
    /// Per-strategy open positions from the engine's
    /// `positions` table. Distinct from `open_positions` above
    /// (which is Kalshi's account-wide REST view); engine
    /// positions are scoped per strategy and include the
    /// idempotent client_id chain that produced them.
    engine_positions: Vec<EnginePositionRow>,
    /// Recent intents whose client_id matches an exit pattern
    /// (`*-exit:*` or `*-flat:*`) — Phase 6 take-profit,
    /// stop-loss, and force-flat fires. Newest first.
    recent_exits: Vec<ExitRow>,
    /// **Audit I5** — fill-latency telemetry.
    /// Aggregated submit-to-fill duration per strategy across
    /// the last hour. `None` if no fills landed in the window.
    fill_latency: Vec<FillLatencyRow>,
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
    /// Average cost per contract in cents. Always positive — for
    /// a NO position this is the price paid for NO contracts.
    avg_cost_cents: Option<f64>,
    /// Mark-to-market price per contract in cents. For a YES
    /// position: the current YES bid (price we'd receive on a
    /// market sell). For a NO position: 100 - yes_ask (price we'd
    /// receive on a market sell of NO ≡ buy YES at the ask).
    /// `None` if we couldn't fetch the market detail or the book
    /// has no quote on the relevant side.
    mark_cents: Option<i64>,
    /// (mark - avg_cost) × |contracts|, signed. `None` if mark
    /// couldn't be computed.
    unrealized_pnl_cents: Option<i64>,
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

/// Phase 6 — one open position from the engine's `positions`
/// table, with derived age. The engine writes these on every
/// fill cascade; the dashboard surfaces them so the operator
/// can see what each strategy currently holds.
#[derive(Debug, Clone, Serialize)]
struct EnginePositionRow {
    strategy: String,
    ticker: String,
    side: String,
    /// Signed: positive = long; negative = short.
    current_qty: i32,
    avg_entry_cents: i32,
    /// Seconds since the position opened.
    age_secs: i64,
    realized_pnl_cents: i64,
    fees_paid_cents: i64,
    /// **Audit I4** — current mark in cents. For a long YES,
    /// the YES bid we'd unwind into; for a long NO, the
    /// complement of the YES ask. `None` when the dashboard
    /// hasn't fetched a `MarketDetail` for this ticker.
    mark_cents: Option<i32>,
    /// (mark - avg_entry) × |qty|, signed. `None` if mark is
    /// missing.
    unrealized_pnl_cents: Option<i64>,
}

/// **Audit I5** — submit-to-fill latency per strategy, last
/// hour. Surfaces the round-trip time the engine sees from
/// the moment it persists `intents.submitted_at` to the
/// moment the WS push delivers a fill (`fills.ts`). Useful for
/// detecting venue / network / queue degradation.
#[derive(Debug, Clone, Serialize)]
struct FillLatencyRow {
    strategy: String,
    n_fills: i64,
    mean_ms: i64,
    p50_ms: i64,
    p95_ms: i64,
    max_ms: i64,
}

/// Phase 6 — one recent exit fire (TP/SL/force-flat). Pulled
/// from `intents` filtered by client_id pattern.
#[derive(Debug, Clone, Serialize)]
struct ExitRow {
    /// Unix seconds.
    ts: i64,
    strategy: String,
    ticker: String,
    side: String,
    /// `tp` (take profit) | `sl` (stop loss) | `flat` (latency
    /// time-based force-flat) | `unknown` (didn't match a known
    /// pattern).
    kind: String,
    qty: i32,
    price_cents: Option<i32>,
    /// Current intent status — `shadow` / `submitted` / `acked`
    /// / `filled` / `rejected` / etc.
    status: String,
    /// The strategy's per-fire reason (entry/mark/pnl summary).
    reason: Option<String>,
}

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Snapshot>>,
    market_details: Arc<Mutex<MarketDetailCache>>,
    kill_flag: Arc<PathBuf>,
    /// Optional Postgres pool. `None` if DB connection failed at
    /// startup (dashboard still works against JSON-only). Logs
    /// the degradation at WARN.
    db: Option<sqlx::PgPool>,
}

#[derive(Debug, Default)]
struct MarketDetailCache {
    entries: HashMap<String, CachedMarketDetail>,
}

#[derive(Debug, Clone)]
struct CachedMarketDetail {
    detail: MarketDetail,
    fetched_at: Instant,
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

    // Postgres pool — degraded-mode tolerant. If the DB is down or
    // the schema isn't migrated, we still serve the dashboard from
    // JSON state files; the operator gets a WARN at startup and
    // any DB-only fields render as "—".
    let db = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(&args.database_url)
        .await
    {
        Ok(p) => {
            info!(url = %args.database_url, "dashboard: postgres connected");
            Some(p)
        }
        Err(e) => {
            warn!(error = %e, url = %args.database_url, "dashboard: postgres connect failed; falling back to JSON-only");
            None
        }
    };

    let state = AppState {
        snapshot: Arc::new(RwLock::new(Snapshot::default())),
        market_details: Arc::new(Mutex::new(MarketDetailCache::default())),
        kill_flag: Arc::new(kill_flag.clone()),
        db: db.clone(),
    };

    let initial = build_snapshot(
        &rest,
        &state.market_details,
        &strategies,
        &kill_flag,
        db.as_ref(),
    )
    .await;
    *state.snapshot.write().await = initial;

    let refresh_state = state.clone();
    let refresh_rest = rest.clone();
    let refresh_strats = strategies.clone();
    let refresh_kill_flag = kill_flag.clone();
    let refresh_db = db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let snap = build_snapshot(
                &refresh_rest,
                &refresh_state.market_details,
                &refresh_strats,
                &refresh_kill_flag,
                refresh_db.as_ref(),
            )
            .await;
            *refresh_state.snapshot.write().await = snap;
        }
    });

    let app = Router::new()
        .route("/", get(serve_html))
        .route("/api/state", get(serve_state))
        .route("/api/kill", post(serve_kill))
        .route("/healthz", get(|| async { "ok" }))
        // Audit: strategy evaluation framework JSON endpoints. The
        // dashboard's static `/eval/` page (eval.html) calls these
        // for the strategy table + per-strategy drill-down.
        .route("/eval", get(serve_eval_html))
        .route("/eval/summary.json", get(serve_eval_summary))
        .route("/eval/ledger/{strategy}", get(serve_eval_ledger))
        .route("/eval/diagnose/{strategy}", get(serve_eval_diagnose))
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

    // Write the file FIRST (legacy daemons watch it), then mirror
    // to the DB scope='global' kill switch (engine watches DB).
    // Both signals are belt-and-suspenders — either alone arms.
    let file_result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    })();

    if let Some(pool) = state.db.as_ref() {
        let db_result = sqlx::query(
            "INSERT INTO kill_switches (scope, armed, set_at, set_by, reason)
             VALUES ('global', $1, now(), 'dashboard', $2)
             ON CONFLICT (scope) DO UPDATE
             SET armed = EXCLUDED.armed,
                 set_at = now(),
                 set_by = 'dashboard',
                 reason = EXCLUDED.reason",
        )
        .bind(req.armed)
        .bind(if req.armed {
            "manual: dashboard arm"
        } else {
            "manual: dashboard clear"
        })
        .execute(pool)
        .await;
        if let Err(e) = db_result {
            warn!(error = %e, "kill switch DB write failed (file write still applied)");
        }
    }

    match file_result {
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

async fn fetch_market_details(
    rest: &RestClient,
    cache: &Arc<Mutex<MarketDetailCache>>,
    tickers: Vec<String>,
) -> Vec<(String, Option<MarketDetail>)> {
    let mut unique = tickers
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    unique.sort();

    let mut out = Vec::with_capacity(unique.len());
    for ticker in unique {
        let now = Instant::now();
        if let Some(detail) = cached_market_detail(cache, &ticker, now).await {
            out.push((ticker, Some(detail)));
            continue;
        }

        match rest.market_detail(&ticker).await {
            Ok(resp) => {
                let detail = resp.market;
                cache_market_detail(cache, ticker.clone(), detail.clone(), Instant::now()).await;
                out.push((ticker, Some(detail)));
            }
            Err(e) => {
                warn!(ticker, error = %e, "refresh: market_detail failed");
                out.push((ticker, None));
            }
        }
        tokio::time::sleep(MARKET_DETAIL_FETCH_SPACING).await;
    }
    out
}

async fn cached_market_detail(
    cache: &Arc<Mutex<MarketDetailCache>>,
    ticker: &str,
    now: Instant,
) -> Option<MarketDetail> {
    let cache = cache.lock().await;
    cache.entries.get(ticker).and_then(|entry| {
        if now.duration_since(entry.fetched_at) <= MARKET_DETAIL_CACHE_TTL {
            Some(entry.detail.clone())
        } else {
            None
        }
    })
}

async fn cache_market_detail(
    cache: &Arc<Mutex<MarketDetailCache>>,
    ticker: String,
    detail: MarketDetail,
    fetched_at: Instant,
) {
    let mut cache = cache.lock().await;
    cache
        .entries
        .insert(ticker, CachedMarketDetail { detail, fetched_at });
}

async fn build_snapshot(
    rest: &RestClient,
    market_details: &Arc<Mutex<MarketDetailCache>>,
    strategies: &[StrategySpec],
    kill_flag: &std::path::Path,
    db: Option<&sqlx::PgPool>,
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

    // Mark map shared between the Kalshi-account positions table
    // (PositionRow) and the engine-positions table
    // (EnginePositionRow / I4 enrichment below). Built once,
    // consulted by both consumers.
    let mut mark_map: HashMap<String, MarketDetail> = HashMap::new();

    match rest.positions().await {
        Ok(positions_resp) => {
            let active: Vec<MarketPosition> = positions_resp
                .market_positions
                .into_iter()
                .filter(has_activity)
                .collect();
            // Mark each open position to the current touch without bursting
            // Kalshi REST. Dashboard marks tolerate a short cache TTL; rate
            // limits do not tolerate dozens of concurrent detail requests.
            let active_tickers: Vec<String> = active.iter().map(|p| p.ticker.clone()).collect();
            let marks = fetch_market_details(rest, market_details, active_tickers).await;
            for (t, d) in marks {
                if let Some(detail) = d {
                    mark_map.insert(t, detail);
                }
            }
            let mut total_unrealized: i64 = 0;
            let mut all_marked = true;
            let rows: Vec<PositionRow> = active
                .into_iter()
                .map(|p| {
                    let row = position_row_with_mark(&p, mark_map.get(&p.ticker));
                    if let Some(u) = row.unrealized_pnl_cents {
                        total_unrealized += u;
                    } else {
                        all_marked = false;
                    }
                    row
                })
                .collect();
            snap.open_positions = rows;
            snap.total_unrealized_pnl_cents = if all_marked {
                Some(total_unrealized)
            } else {
                // Surface partial sum even when some positions
                // failed — useful for the dashboard, but flag the
                // gap via the existing `last_refresh_error`.
                Some(total_unrealized)
            };
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

        // Phase 2 cutover: prefer DB-derived state. If the DB
        // returned anything for this strategy at all we treat it
        // as authoritative. Falls through to JSON on connection
        // failure or empty result.
        let mut db_used = false;
        if let Some(pool) = db
            && let Ok(state) = db_strategy_state(pool, &strat.name).await
        {
            row.oms_state_present = true;
            row.oms_daily_realized_pnl_cents = state.daily_realized_pnl_cents;
            row.oms_kill_switch = state.kill_switch_armed;
            row.oms_in_flight_orders = state.in_flight_orders;
            db_used = true;
        }

        if !db_used {
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

    // Phase 6 — engine-side surfaces. DB-only; legacy daemons
    // don't write to engine positions/intents tables. If the
    // pool is unavailable or queries fail, the dashboard
    // continues to render with empty Phase 6 sections rather
    // than failing the whole snapshot.
    if let Some(pool) = db {
        match db_engine_positions(pool).await {
            Ok(mut rows) => {
                // I4 — enrich each engine position with mark +
                // unrealized PnL using the shared mark_map. For
                // tickers not in the map (engine has a position
                // but Kalshi-side hasn't been fetched), fetch
                // their market_detail in parallel and add to
                // the map.
                let missing: Vec<String> = rows
                    .iter()
                    .map(|r| r.ticker.clone())
                    .filter(|t| !mark_map.contains_key(t))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                if !missing.is_empty() {
                    let extras = fetch_market_details(rest, market_details, missing).await;
                    for (t, d) in extras {
                        if let Some(detail) = d {
                            mark_map.insert(t, detail);
                        }
                    }
                }
                for r in &mut rows {
                    if let Some(detail) = mark_map.get(&r.ticker) {
                        let mark = engine_position_mark(r, detail);
                        if let Some(m) = mark {
                            r.mark_cents = Some(m);
                            // PnL per contract × |qty|; signed.
                            let pnl_per = if r.current_qty > 0 {
                                m - r.avg_entry_cents
                            } else {
                                r.avg_entry_cents - m
                            };
                            let abs_qty = i64::from(r.current_qty.unsigned_abs());
                            r.unrealized_pnl_cents = Some(i64::from(pnl_per) * abs_qty);
                        }
                    }
                }
                snap.engine_positions = rows;
            }
            Err(e) => {
                warn!(error = %e, "refresh: engine_positions failed");
                snap.last_refresh_error = Some(format!("engine_positions: {e}"));
            }
        }
        match db_recent_exits(pool).await {
            Ok(rows) => snap.recent_exits = rows,
            Err(e) => {
                warn!(error = %e, "refresh: recent_exits failed");
                snap.last_refresh_error = Some(format!("recent_exits: {e}"));
            }
        }
        match db_fill_latency(pool).await {
            Ok(rows) => snap.fill_latency = rows,
            Err(e) => {
                warn!(error = %e, "refresh: fill_latency failed");
                snap.last_refresh_error = Some(format!("fill_latency: {e}"));
            }
        }
    }

    snap
}

/// I4 — compute the unwind mark for a single engine position
/// from a Kalshi MarketDetail. For long YES we'd sell into the
/// YES bid; for long NO we'd sell NO ≡ buy YES at ask, so the
/// unwind price is `100 - yes_ask`. Symmetric on short side.
fn engine_position_mark(
    row: &EnginePositionRow,
    detail: &predigy_kalshi_rest::types::MarketDetail,
) -> Option<i32> {
    let is_yes = row.side == "yes";
    let is_long = row.current_qty > 0;
    let cents = match (is_yes, is_long) {
        (true, true) => detail.yes_bid_dollars.map(|v| (v * 100.0).round() as i32),
        (true, false) => detail
            .yes_ask_dollars
            .map(|v| 100i32 - (v * 100.0).round() as i32),
        (false, true) => detail
            .yes_ask_dollars
            .map(|v| 100i32 - (v * 100.0).round() as i32),
        (false, false) => detail.yes_bid_dollars.map(|v| (v * 100.0).round() as i32),
    };
    cents.map(|c| c.clamp(0, 100))
}

fn has_activity(p: &MarketPosition) -> bool {
    p.position_contracts.unwrap_or(0.0).abs() > 1e-9
        || p.fees_paid_dollars.unwrap_or(0.0).abs() > 1e-9
        || p.resting_orders_count.unwrap_or(0) > 0
}

fn position_row_with_mark(
    p: &MarketPosition,
    detail: Option<&predigy_kalshi_rest::types::MarketDetail>,
) -> PositionRow {
    let contracts = p.position_contracts.unwrap_or(0.0);
    let total_traded = p.total_traded_dollars.unwrap_or(0.0);
    let abs_contracts = contracts.abs();
    // avg_cost_cents is per-contract in the contract's own price
    // space (YES-cents for long YES, NO-cents for long NO).
    let avg_cost_cents = if abs_contracts > 1e-9 {
        Some((total_traded / abs_contracts) * 100.0)
    } else {
        None
    };
    let mark_cents = detail.and_then(|d| match contracts {
        c if c > 0.0 => d.yes_bid_dollars.map(|v| (v * 100.0).round() as i64),
        c if c < 0.0 => d
            .yes_ask_dollars
            .map(|v| (100.0 - v * 100.0).round() as i64),
        _ => None,
    });
    let unrealized_pnl_cents = match (avg_cost_cents, mark_cents) {
        (Some(cost), Some(mark)) => {
            let pnl = (f64::from(mark as i32) - cost) * abs_contracts;
            Some(pnl.round() as i64)
        }
        _ => None,
    };
    PositionRow {
        ticker: p.ticker.clone(),
        contracts,
        exposure_dollars: p.market_exposure_dollars.unwrap_or(0.0),
        realized_pnl_dollars: p.realized_pnl_dollars.unwrap_or(0.0),
        fees_paid_dollars: p.fees_paid_dollars.unwrap_or(0.0),
        resting_orders: p.resting_orders_count.unwrap_or(0),
        avg_cost_cents,
        mark_cents,
        unrealized_pnl_cents,
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

/// One-call snapshot of a strategy's live state from Postgres.
/// Composes three queries (positions PnL, kill switches,
/// in-flight intents) — all small, all indexed, all should land
/// in well under 50ms total.
#[derive(Debug, Default)]
struct DbStrategyState {
    daily_realized_pnl_cents: i64,
    kill_switch_armed: bool,
    in_flight_orders: usize,
}

/// Phase 6 — pull every currently-open engine position from
/// Postgres, regardless of strategy. Returned newest-first by
/// `opened_at`. Bounded at 200 rows so a runaway engine doesn't
/// produce a 10MB JSON payload.
async fn db_engine_positions(pool: &sqlx::PgPool) -> Result<Vec<EnginePositionRow>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        strategy: String,
        ticker: String,
        side: String,
        current_qty: i32,
        avg_entry_cents: i32,
        opened_at: chrono::DateTime<chrono::Utc>,
        realized_pnl_cents: i64,
        fees_paid_cents: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT strategy, ticker, side, current_qty, avg_entry_cents,
                opened_at, realized_pnl_cents, fees_paid_cents
           FROM positions
          WHERE closed_at IS NULL
          ORDER BY opened_at DESC
          LIMIT 200",
    )
    .fetch_all(pool)
    .await?;
    let now = chrono::Utc::now();
    Ok(rows
        .into_iter()
        .map(|r| EnginePositionRow {
            strategy: r.strategy,
            ticker: r.ticker,
            side: r.side,
            current_qty: r.current_qty,
            avg_entry_cents: r.avg_entry_cents,
            age_secs: (now - r.opened_at).num_seconds().max(0),
            realized_pnl_cents: r.realized_pnl_cents,
            fees_paid_cents: r.fees_paid_cents,
            // I4: filled in by build_snapshot from the
            // MarketDetail map shared with Kalshi-side positions.
            mark_cents: None,
            unrealized_pnl_cents: None,
        })
        .collect())
}

/// Phase 6 — recent exit fires (TP / SL / force-flat). Matches
/// the deterministic client_id patterns each strategy uses for
/// its closing intents:
///
/// - `stat-exit:{ticker}:{side}:{tp|sl}:...`
/// - `cross-arb-exit:{ticker}:{side}:{tp|sl}:...`
/// - `latency-flat:{ticker}:{side}:...`
///
/// Bounded at 50 rows; client-side filterable in the dashboard
/// UI by `kind` if needed.
async fn db_recent_exits(pool: &sqlx::PgPool) -> Result<Vec<ExitRow>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        submitted_at: chrono::DateTime<chrono::Utc>,
        strategy: String,
        ticker: String,
        side: String,
        client_id: String,
        qty: i32,
        price_cents: Option<i32>,
        status: String,
        reason: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT submitted_at, strategy, ticker, side, client_id,
                qty, price_cents, status, reason
           FROM intents
          WHERE client_id LIKE '%-exit:%'
             OR client_id LIKE '%-flat:%'
          ORDER BY submitted_at DESC
          LIMIT 50",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ExitRow {
            ts: r.submitted_at.timestamp(),
            strategy: r.strategy,
            ticker: r.ticker,
            side: r.side,
            kind: classify_exit_kind(&r.client_id),
            qty: r.qty,
            price_cents: r.price_cents,
            status: r.status,
            reason: r.reason,
        })
        .collect())
}

/// **Audit I5** — fill-latency telemetry. Pulls submit-to-fill
/// latency for every fill in the last hour, aggregates per
/// strategy. PERCENTILE_DISC is the bog-standard distribution
/// quantile in Postgres.
async fn db_fill_latency(pool: &sqlx::PgPool) -> Result<Vec<FillLatencyRow>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        strategy: String,
        n_fills: i64,
        // f64s because the sqlx driver maps DOUBLE PRECISION to
        // f64; conversion to ms (i64) happens in the mapper below.
        mean_secs: Option<f64>,
        p50_secs: Option<f64>,
        p95_secs: Option<f64>,
        max_secs: Option<f64>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT
             i.strategy,
             COUNT(*)::BIGINT AS n_fills,
             AVG(EXTRACT(EPOCH FROM (f.ts - i.submitted_at)))::DOUBLE PRECISION AS mean_secs,
             PERCENTILE_DISC(0.5) WITHIN GROUP (
                 ORDER BY EXTRACT(EPOCH FROM (f.ts - i.submitted_at))::DOUBLE PRECISION
             ) AS p50_secs,
             PERCENTILE_DISC(0.95) WITHIN GROUP (
                 ORDER BY EXTRACT(EPOCH FROM (f.ts - i.submitted_at))::DOUBLE PRECISION
             ) AS p95_secs,
             MAX(EXTRACT(EPOCH FROM (f.ts - i.submitted_at)))::DOUBLE PRECISION AS max_secs
           FROM fills f
           JOIN intents i ON i.client_id = f.client_id
          WHERE f.ts >= now() - interval '1 hour'
          GROUP BY i.strategy
          ORDER BY i.strategy",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FillLatencyRow {
            strategy: r.strategy,
            n_fills: r.n_fills,
            mean_ms: (r.mean_secs.unwrap_or(0.0) * 1000.0).round() as i64,
            p50_ms: (r.p50_secs.unwrap_or(0.0) * 1000.0).round() as i64,
            p95_ms: (r.p95_secs.unwrap_or(0.0) * 1000.0).round() as i64,
            max_ms: (r.max_secs.unwrap_or(0.0) * 1000.0).round() as i64,
        })
        .collect())
}

/// Map an exit-cid to its kind for UI grouping. Pattern is
/// `<strategy>-{exit|flat}:{ticker}:{side}:{tag}:...`.
/// Tags:
///   tp   — take-profit
///   sl   — stop-loss
///   bd   — belief-drift (stat: rule's model_p moved through us)
///   conv — convergence (cross-arb: poly mid caught up to kalshi)
///   inv  — thesis inversion (cross-arb: poly moved below kalshi)
///   flat — time-based force-flat (latency)
fn classify_exit_kind(cid: &str) -> String {
    if cid.contains("-flat:") {
        return "flat".to_string();
    }
    if cid.contains("-exit:") {
        for (needle, tag) in [
            (":tp:", "tp"),
            (":sl:", "sl"),
            (":ts:", "ts"),
            (":bd:", "bd"),
            (":conv:", "conv"),
            (":inv:", "inv"),
        ] {
            if cid.contains(needle) {
                return tag.to_string();
            }
        }
    }
    "unknown".to_string()
}

async fn db_strategy_state(
    pool: &sqlx::PgPool,
    strategy: &str,
) -> Result<DbStrategyState, sqlx::Error> {
    // Today's realised PnL summed across positions that closed
    // today. Excludes still-open positions (those count as
    // unrealised).
    let pnl: (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(realized_pnl_cents)::BIGINT
           FROM positions
          WHERE strategy = $1
            AND closed_at >= date_trunc('day', now())",
    )
    .bind(strategy)
    .fetch_one(pool)
    .await?;
    let daily_realized_pnl_cents = pnl.0.unwrap_or(0);

    // Per-strategy kill switch OR global kill switch.
    let killed: Option<(bool,)> = sqlx::query_as(
        "SELECT armed FROM kill_switches
          WHERE scope IN ('global', $1) AND armed = true LIMIT 1",
    )
    .bind(strategy)
    .fetch_optional(pool)
    .await?;
    let kill_switch_armed = killed.is_some();

    // Currently-open intents (any non-terminal status).
    // 'shadow' means the engine wrote it but never sent to the
    // venue (Shadow mode during the migration); excluded from
    // in-flight because there's no real venue exposure.
    let in_flight: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM intents
          WHERE strategy = $1
            AND status NOT IN ('filled','cancelled','rejected','expired','shadow')",
    )
    .bind(strategy)
    .fetch_one(pool)
    .await?;

    Ok(DbStrategyState {
        daily_realized_pnl_cents,
        kill_switch_armed,
        in_flight_orders: usize::try_from(in_flight.0).unwrap_or(0),
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

// ─── Strategy evaluation framework JSON endpoints ────────────────

const EVAL_HTML: &str = include_str!("../static/eval.html");

async fn serve_eval_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(EVAL_HTML),
    )
}

#[derive(Debug, Deserialize)]
struct EvalQuery {
    /// `1h | 24h | 7d | 30d | all`. Defaults to `24h`.
    #[serde(default)]
    since: Option<String>,
}

fn parse_window(q: Option<&str>) -> Result<predigy_eval_lib::TimeWindow, String> {
    let s = q.unwrap_or("24h");
    predigy_eval_lib::TimeWindow::parse(s)
}

async fn serve_eval_summary(
    State(state): State<AppState>,
    Query(q): Query<EvalQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no DB pool".into()))?;
    let win = parse_window(q.since.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let db = predigy_engine_core::Db::from_pool(pool.clone());
    let trades = predigy_eval_lib::load_trades(&db, win, None)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load_trades: {e}"),
            )
        })?;
    let activity = predigy_eval_lib::ledger::load_intent_activity(&db, win, None)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load_intent_activity: {e}"),
            )
        })?;
    let metrics = predigy_eval_lib::compute_metrics(&trades, &activity, win.start, win.end);
    let mut diagnoses: HashMap<String, Vec<predigy_eval_lib::Diagnosis>> = HashMap::new();
    for (s, m) in &metrics {
        diagnoses.insert(s.clone(), predigy_eval_lib::diagnose(m, &trades));
    }
    Ok(Json(serde_json::json!({
        "metrics": metrics,
        "diagnoses": diagnoses,
        "window": { "start": win.start, "end": win.end },
    })))
}

async fn serve_eval_ledger(
    State(state): State<AppState>,
    Path(strategy): Path<String>,
    Query(q): Query<EvalQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no DB pool".into()))?;
    let win = parse_window(q.since.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let db = predigy_engine_core::Db::from_pool(pool.clone());
    let trades = predigy_eval_lib::load_trades(&db, win, Some(&strategy))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load_trades: {e}"),
            )
        })?;
    Ok(Json(serde_json::json!({ "trades": trades })))
}

async fn serve_eval_diagnose(
    State(state): State<AppState>,
    Path(strategy): Path<String>,
    Query(q): Query<EvalQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no DB pool".into()))?;
    let win = parse_window(q.since.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let db = predigy_engine_core::Db::from_pool(pool.clone());
    let trades = predigy_eval_lib::load_trades(&db, win, Some(&strategy))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load_trades: {e}"),
            )
        })?;
    let activity = predigy_eval_lib::ledger::load_intent_activity(&db, win, Some(&strategy))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load_intent_activity: {e}"),
            )
        })?;
    let metrics = predigy_eval_lib::compute_metrics(&trades, &activity, win.start, win.end);
    let m = metrics.get(&strategy);
    let diags = m
        .map(|m| predigy_eval_lib::diagnose(m, &trades))
        .unwrap_or_default();
    Ok(Json(serde_json::json!({
        "strategy": strategy,
        "metrics": m,
        "diagnoses": diags,
    })))
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

    #[test]
    fn classify_exit_kind_recognises_take_profit() {
        assert_eq!(classify_exit_kind("stat-exit:KX-A:Y:tp:00abcdef"), "tp");
        assert_eq!(classify_exit_kind("cross-arb-exit:KX-B:N:tp:50:0010"), "tp");
    }

    #[test]
    fn classify_exit_kind_recognises_stop_loss() {
        assert_eq!(classify_exit_kind("stat-exit:KX-A:Y:sl:00abcdef"), "sl");
    }

    #[test]
    fn classify_exit_kind_recognises_force_flat() {
        assert_eq!(classify_exit_kind("latency-flat:WX-A:Y:001a2b3c"), "flat");
    }

    #[test]
    fn classify_exit_kind_falls_back_to_unknown() {
        // Entry cids should not be classified as exits.
        assert_eq!(classify_exit_kind("stat:KX-A:50:0001:00abcdef"), "unknown");
        assert_eq!(classify_exit_kind("anything-else"), "unknown");
    }

    #[test]
    fn classify_exit_kind_recognises_phase_a_tags() {
        // A1 belief-drift, A2 convergence + inversion, A3 trailing.
        assert_eq!(classify_exit_kind("stat-exit:KX-A:Y:bd:00abcdef"), "bd");
        assert_eq!(classify_exit_kind("stat-exit:KX-A:Y:ts:00abcdef"), "ts");
        assert_eq!(
            classify_exit_kind("cross-arb-exit:KX-B:Y:conv:53:0004"),
            "conv"
        );
        assert_eq!(
            classify_exit_kind("cross-arb-exit:KX-C:Y:inv:52:0004"),
            "inv"
        );
        assert_eq!(classify_exit_kind("cross-arb-exit:KX-D:Y:ts:52:0004"), "ts");
    }
}
