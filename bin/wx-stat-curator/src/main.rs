// Vendor / product names appear throughout the doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `wx-stat-curator`: forecast-driven curator for Kalshi temperature
//! markets. Outputs a `StatRule[]` JSON file consumable by the
//! existing `stat-trader` binary.
//!
//! ```text
//! wx-stat-curator \
//!   --kalshi-key-id $KALSHI_KEY_ID --kalshi-pem ./kalshi.pem \
//!   --user-agent "$NWS_USER_AGENT" \
//!   --output ./wx-stat-rules.json \
//!   --min-edge-cents 5 \
//!   --min-margin-f 5 \
//!   --write
//! ```
//!
//! Without `--write` the proposed rules are printed to stdout —
//! useful for eyeballing the forecast→price comparisons before
//! committing them to the rule file.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_ext_feeds::nbm::NbmClient;
use predigy_ext_feeds::nws_forecast::NwsForecastClient;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use stat_trader::StatRule;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wx_stat_curator::calibration::Calibration;
use wx_stat_curator::nbm_curate::{NbmCurateOutcome, curate_via_nbm};
use wx_stat_curator::nbm_path::recent_qmd_cycle;
use wx_stat_curator::{
    Airport, ForecastDecision, ProbabilityConfig, TempMarket, TempStrikeKind, lookup_airport,
    parse_temp_market, scan_temp_markets,
};

#[derive(Debug, Parser)]
#[command(
    name = "wx-stat-curator",
    about = "Curate stat-trader rules for Kalshi temperature markets via NWS forecast."
)]
struct Args {
    #[arg(long, env = "KALSHI_KEY_ID")]
    kalshi_key_id: String,
    #[arg(long, env = "KALSHI_PEM")]
    kalshi_pem: PathBuf,
    #[arg(long)]
    kalshi_rest_endpoint: Option<String>,

    /// User-Agent for NWS requests. NWS rejects anonymous traffic;
    /// format suggestion is `"(myapp.com, contact@example.com)"`.
    #[arg(long, env = "NWS_USER_AGENT")]
    user_agent: String,

    /// Output path for the rule JSON. Format matches what
    /// `stat-trader --rule-file` reads.
    #[arg(long, default_value = "wx-stat-rules.json")]
    output: PathBuf,

    /// Minimum after-fee per-contract edge (cents) for an emitted
    /// StatRule. Stat-trader fires when the market quote diverges
    /// from `model_p` by at least this much.
    #[arg(long, default_value_t = 5)]
    min_edge_cents: u32,

    /// Minimum forecast-to-threshold margin (degrees F) to consider
    /// a forecast decisive. Markets within this margin are skipped.
    /// Phase 1 conviction-zone gate; replaced in Phase 2 by NBM
    /// probabilistic data.
    #[arg(long, default_value_t = 5.0)]
    min_margin_f: f64,

    /// Write the curated rules to `--output`. Without this, prints
    /// to stdout (dry-run).
    #[arg(long, default_value_t = false)]
    write: bool,

    /// Restart the named launchd job after a successful write.
    #[arg(long)]
    restart_job: Option<String>,

    /// Use the NBM probabilistic forecast (Phase 2) instead of the
    /// NWS hourly point forecast (Phase 1). NBM gives a calibrated
    /// CDF interpolation at the Kalshi threshold rather than the
    /// blunt 0.97/0.03 conviction-zone label. Default off until
    /// the user has validated NBM-side results against historical
    /// market outcomes.
    #[arg(long, default_value_t = false)]
    nbm: bool,

    /// Cache root for NBM-extracted per-airport quantile vectors.
    /// Cache reads make the second invocation within a 6h cycle
    /// effectively free.
    #[arg(long, default_value = "data/nbm_cache")]
    nbm_cache: PathBuf,

    /// Path to a Phase-2E Platt-scaling calibration JSON file. If
    /// absent, calibration falls back to the identity (raw NBM
    /// quantile probabilities). Produced by `wx-stat-fit-calibration`
    /// from accumulated (forecast, outcome) pairs.
    #[arg(long, default_value = "data/wx_stat_calibration.json")]
    nbm_calibration: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let pem = tokio::fs::read_to_string(&args.kalshi_pem)
        .await
        .with_context(|| format!("read PEM at {}", args.kalshi_pem.display()))?;
    let signer = Signer::from_pem(&args.kalshi_key_id, &pem).map_err(|e| anyhow!("signer: {e}"))?;
    let rest = if let Some(base) = &args.kalshi_rest_endpoint {
        RestClient::with_base(base, Some(signer))
    } else {
        RestClient::authed(signer)
    }
    .map_err(|e| anyhow!("rest: {e}"))?;

    let nws = NwsForecastClient::new(&args.user_agent).map_err(|e| anyhow!("nws client: {e}"))?;

    info!("scanning Kalshi for daily-temperature markets");
    let markets = scan_temp_markets(&rest)
        .await
        .map_err(|e| anyhow!("scan: {e}"))?;
    info!(found = markets.len(), "actionable temp markets discovered");
    if markets.is_empty() {
        warn!("no actionable markets found — writing empty rule file");
        if args.write {
            write_rules(&[], &args.output).await?;
        } else {
            println!("[]");
        }
        return Ok(());
    }

    let mut rules: Vec<StatRule> = Vec::new();
    let mut inspections: Vec<RuleInspection> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut skip_counts: HashMap<&'static str, u32> = HashMap::new();

    if args.nbm {
        // ---- Phase 2 NBM probabilistic path ----
        let nbm_client =
            NbmClient::new(&args.user_agent).map_err(|e| anyhow!("nbm client: {e}"))?;
        let cycle = recent_qmd_cycle(now_unix());
        info!(?cycle, "nbm: using cycle");
        std::fs::create_dir_all(&args.nbm_cache).ok();
        let calibration = match Calibration::load(&args.nbm_calibration) {
            Ok(Some(cal)) => {
                info!(
                    path = %args.nbm_calibration.display(),
                    n_buckets = cal.buckets.len(),
                    fitted_at = cal.fitted_at_iso.as_deref().unwrap_or("?"),
                    "nbm: loaded calibration"
                );
                Some(cal)
            }
            Ok(None) => {
                info!(
                    path = %args.nbm_calibration.display(),
                    "nbm: no calibration file present; using identity (raw NBM probabilities)"
                );
                None
            }
            Err(e) => {
                warn!(
                    path = %args.nbm_calibration.display(),
                    error = %e,
                    "nbm: calibration load failed; using identity"
                );
                None
            }
        };
        let outcomes =
            curate_via_nbm(&nbm_client, &args.nbm_cache, cycle, &markets, calibration.as_ref())
                .await;
        for (m, outcome) in markets.iter().zip(outcomes.into_iter()) {
            match outcome {
                NbmCurateOutcome::Rule(out) => {
                    info!(audit = %out.audit, "accepted rule (nbm)");
                    rules.push(out.rule);
                    inspections.push(RuleInspection {
                        ticker: out.ticker,
                        title: out.title,
                        airport: out.airport,
                        threshold: out.threshold,
                        forecast_value_f: out.forecast_value_f,
                        model_p: out.model_p,
                        side: out.side,
                        quoted_ask_cents: out.quoted_ask_cents,
                        apparent_edge_cents: out.apparent_edge_cents,
                    });
                }
                NbmCurateOutcome::Skip { reason } => {
                    debug!(market = %m.ticker, reason = %reason, "skip");
                    *skip_counts.entry(skip_category(&reason)).or_insert(0) += 1;
                    skipped.push((m.ticker.clone(), reason));
                }
                NbmCurateOutcome::Error { reason } => {
                    warn!(market = %m.ticker, reason = %reason, "curate failed");
                    *skip_counts.entry("error").or_insert(0) += 1;
                    skipped.push((m.ticker.clone(), reason));
                }
            }
        }
    } else {
        // ---- Phase 1 NWS deterministic path ----
        let cfg = ProbabilityConfig {
            min_margin_f: args.min_margin_f,
        };
        let mut grid_cache: HashMap<&'static str, predigy_ext_feeds::nws_forecast::GridPoint> =
            HashMap::new();
        let mut forecast_cache: HashMap<
            (&'static str, String),
            predigy_ext_feeds::nws_forecast::HourlyForecast,
        > = HashMap::new();
        for m in &markets {
            match curate_one(&nws, m, &cfg, &mut grid_cache, &mut forecast_cache).await {
                CurateOutcome::Rule {
                    rule,
                    audit,
                    inspection,
                } => {
                    info!(audit = %audit, "accepted rule");
                    rules.push(rule);
                    inspections.push(inspection);
                }
                CurateOutcome::Skip(reason) => {
                    debug!(market = %m.ticker, reason = %reason, "skip");
                    *skip_counts.entry(skip_category(&reason)).or_insert(0) += 1;
                    skipped.push((m.ticker.clone(), reason));
                }
                CurateOutcome::Error(reason) => {
                    warn!(market = %m.ticker, reason = %reason, "curate failed");
                    *skip_counts.entry("error").or_insert(0) += 1;
                    skipped.push((m.ticker.clone(), reason));
                }
            }
        }
    }

    info!(
        kept = rules.len(),
        skipped = skipped.len(),
        "synthesis done"
    );
    // Surface skip-category histogram so the operator can see at a
    // glance whether the conviction-zone gate is the dominant reason
    // (expected) or whether something more concerning is happening
    // (e.g. unmapped airports / unsupported strike kinds dominating
    // the skips → coverage gap that needs filling).
    if !skip_counts.is_empty() {
        let mut categories: Vec<(&&str, &u32)> = skip_counts.iter().collect();
        categories.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, n) in categories {
            info!(category = *cat, count = *n, "skip");
        }
    }

    if args.write {
        write_rules(&rules, &args.output).await?;
        println!("wrote {} rules to {}", rules.len(), args.output.display());
        if let Some(job) = &args.restart_job {
            kickstart_job(job);
        }
    } else {
        let json = serde_json::to_string_pretty(&rules)?;
        println!("{json}");
        eprintln!(
            "dry-run: {} rules proposed, {} skipped. Use --write to commit to {}",
            rules.len(),
            skipped.len(),
            args.output.display()
        );
        if !skip_counts.is_empty() {
            let mut categories: Vec<(&&str, &u32)> = skip_counts.iter().collect();
            categories.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!("skip categories:");
            for (cat, n) in categories {
                eprintln!("  {n:>4}  {cat}");
            }
        }
        print_inspection_table(&inspections);
    }
    Ok(())
}

enum CurateOutcome {
    Rule {
        rule: StatRule,
        audit: String,
        inspection: RuleInspection,
    },
    Skip(String),
    Error(String),
}

/// Per-rule details surfaced in the dry-run inspection table.
/// Lets the operator scan the proposed rules sorted by apparent
/// edge before promoting them.
#[derive(Debug, Clone)]
struct RuleInspection {
    ticker: String,
    title: String,
    airport: String,
    threshold: String,
    forecast_value_f: f64,
    /// Calibrated probability YES will resolve true (0.97 or 0.03
    /// in Phase 1). Surfaced in the table so the operator sees the
    /// belief side, not just the price.
    model_p: f64,
    side: Side,
    /// Quote of the side this rule will bet on (YES ask if side=Yes,
    /// NO ask if side=No), in cents.
    quoted_ask_cents: u8,
    /// `(model_p_in_cents - quoted_ask_cents)` for the bet side.
    /// Positive = apparent edge in our favour. Stat-trader recomputes
    /// this against live prices at fire time; this is the curator-
    /// time snapshot for ranking only.
    apparent_edge_cents: i32,
}

async fn curate_one(
    nws: &NwsForecastClient,
    m: &TempMarket,
    cfg: &ProbabilityConfig,
    grid_cache: &mut HashMap<&'static str, predigy_ext_feeds::nws_forecast::GridPoint>,
    forecast_cache: &mut HashMap<
        (&'static str, String),
        predigy_ext_feeds::nws_forecast::HourlyForecast,
    >,
) -> CurateOutcome {
    // 1) Parse market metadata into a structured spec.
    let spec = match parse_temp_market(
        &m.event_ticker,
        m.strike_type.as_deref(),
        m.floor_strike,
        m.cap_strike,
        m.occurrence_datetime.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => return CurateOutcome::Skip(format!("parse: {e}")),
    };

    // 2) Resolve the airport.
    let Some(airport) = lookup_airport(&spec.airport_code) else {
        return CurateOutcome::Skip(format!("unmapped airport {}", spec.airport_code));
    };

    // 3) Resolve airport → grid cell (cached).
    let grid = match grid_cache.get(airport.code) {
        Some(g) => g.clone(),
        None => match nws.lookup_point(airport.lat, airport.lon).await {
            Ok(g) => {
                debug!(airport = airport.code, ?g, "grid resolved");
                grid_cache.insert(airport.code, g.clone());
                g
            }
            Err(e) => {
                return CurateOutcome::Error(format!("nws lookup_point({}): {e}", airport.code));
            }
        },
    };

    // 4) Fetch the hourly forecast for this grid cell, keyed on
    //    `(airport, settlement_date)` so we don't refetch when many
    //    markets share the same airport-day.
    let key = (airport.code, spec.settlement_date.clone());
    let forecast = match forecast_cache.get(&key) {
        Some(f) => f.clone(),
        None => match nws.fetch_hourly(&grid).await {
            Ok(f) => {
                forecast_cache.insert(key, f.clone());
                f
            }
            Err(e) => {
                return CurateOutcome::Error(format!("nws fetch_hourly({}): {e}", airport.code));
            }
        },
    };

    // 5) Derive `model_p` from the forecast.
    let decision = wx_stat_curator::derive_model_p(&spec, &forecast, cfg);
    match decision {
        ForecastDecision::Skip { reason } => CurateOutcome::Skip(format!("forecast: {reason:?}")),
        ForecastDecision::Decisive {
            model_p,
            forecast_value_f,
            hours_considered,
        } => emit_rule(
            m,
            airport,
            &spec.kind,
            model_p,
            forecast_value_f,
            hours_considered,
        ),
    }
}

fn emit_rule(
    m: &TempMarket,
    airport: &Airport,
    kind: &TempStrikeKind,
    model_p: f64,
    forecast_value_f: f64,
    hours_considered: usize,
) -> CurateOutcome {
    if m.ticker.is_empty() {
        return CurateOutcome::Error("empty market ticker".into());
    }
    let kalshi_market = MarketTicker::new(&m.ticker);
    // Decide which side stat-trader should bet. The conviction
    // direction (model_p > 0.5 ⇒ favour YES; < 0.5 ⇒ favour NO)
    // tells us this. Stat-trader's `evaluate()` will additionally
    // check that the market price is far enough off model_p to
    // clear `min_edge_cents`.
    let side = if model_p > 0.5 { Side::Yes } else { Side::No };
    let rule = StatRule {
        kalshi_market,
        model_p,
        side,
        // The actual edge floor — stat-trader handles this. Use a
        // fixed value here rather than threading the CLI flag
        // because the rule file is the contract; bumping the floor
        // requires re-curating.
        min_edge_cents: 5,
    };
    let threshold_str = match kind {
        TempStrikeKind::Greater { threshold } => format!(">{threshold}"),
        TempStrikeKind::Less { threshold } => format!("<{threshold}"),
        TempStrikeKind::Between { lower, upper } => format!("[{lower},{upper}]"),
    };
    // The quote of the side we'd bet on. For Yes we cross at yes_ask;
    // for No we cross at no_ask. Edge is `(model_p_in_cents - ask)`.
    let model_p_cents = (model_p * 100.0).round() as i32;
    let (quoted_ask_cents, apparent_edge_cents) = match side {
        Side::Yes => (m.yes_ask_cents, model_p_cents - i32::from(m.yes_ask_cents)),
        Side::No => (
            m.no_ask_cents,
            // For a No bet the relevant model probability is 1 - model_p.
            (100 - model_p_cents) - i32::from(m.no_ask_cents),
        ),
    };
    let audit = format!(
        "ticker={ticker} airport={code}({city}) kind={threshold} forecast={forecast:.1}F hours={hours} model_p={mp:.3} side={side:?} ask={ask}c edge={edge:+}c",
        ticker = m.ticker,
        code = airport.code,
        city = airport.city,
        threshold = threshold_str,
        forecast = forecast_value_f,
        hours = hours_considered,
        mp = model_p,
        side = side,
        ask = quoted_ask_cents,
        edge = apparent_edge_cents,
    );
    let inspection = RuleInspection {
        ticker: m.ticker.clone(),
        title: m.title.clone(),
        airport: airport.code.to_string(),
        threshold: threshold_str,
        forecast_value_f,
        model_p,
        side,
        quoted_ask_cents,
        apparent_edge_cents,
    };
    CurateOutcome::Rule {
        rule,
        audit,
        inspection,
    }
}

async fn write_rules(rules: &[StatRule], output: &std::path::Path) -> Result<()> {
    let json = serde_json::to_string_pretty(rules)?;
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let tmp = output.with_extension("tmp");
    tokio::fs::write(&tmp, &json)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, output)
        .await
        .with_context(|| format!("rename to {}", output.display()))?;
    Ok(())
}

/// Best-effort launchctl kickstart. Same pattern as stat-curator.
fn kickstart_job(job: &str) {
    let Some(uid) = current_uid() else {
        warn!(job = %job, "couldn't resolve uid; skipping kickstart");
        return;
    };
    let target = format!("gui/{uid}/{job}");
    let status = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status();
    match status {
        Ok(s) if s.success() => info!(job = %job, "kickstarted launchd job"),
        Ok(s) => warn!(job = %job, exit = ?s.code(), "launchctl kickstart non-zero exit"),
        Err(e) => warn!(job = %job, error = %e, "launchctl kickstart failed"),
    }
}

fn current_uid() -> Option<u32> {
    let out = std::process::Command::new("id").arg("-u").output().ok()?;
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    s.parse().ok()
}

/// Render the proposed rules to stderr as a sorted table — biggest
/// apparent edge first. The operator scans this to pick which rules
/// are worth promoting from `wx-stat-rules.json` to the live
/// `stat-rules.json`. Apparent edge is the curator-time snapshot
/// (model_p_cents − quoted_ask_cents); stat-trader re-evaluates
/// against live prices at fire time.
fn print_inspection_table(inspections: &[RuleInspection]) {
    if inspections.is_empty() {
        return;
    }
    let mut rows: Vec<&RuleInspection> = inspections.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.apparent_edge_cents));

    eprintln!();
    eprintln!("inspection (sorted by apparent edge desc):");
    eprintln!(
        "  {edge:>5}  {ticker:<32}  {airport:<5}  {threshold:<8}  {fcst:>5}  {model:>5}  {side:>4}  {ask:>5}  title",
        edge = "edge",
        ticker = "ticker",
        airport = "airpt",
        threshold = "thresh",
        fcst = "fcst",
        model = "model",
        side = "side",
        ask = "ask",
    );
    for r in &rows {
        let side_str = match r.side {
            Side::Yes => "YES",
            Side::No => "NO",
        };
        let title_short: String = r.title.chars().take(60).collect();
        eprintln!(
            "  {edge:>+5}  {ticker:<32}  {airport:<5}  {threshold:<8}  {fcst:>5.1}  {model:>4.0}%  {side:>4}  {ask:>4}c  {title}",
            edge = r.apparent_edge_cents,
            ticker = r.ticker,
            airport = r.airport,
            threshold = r.threshold,
            fcst = r.forecast_value_f,
            model = r.model_p * 100.0,
            side = side_str,
            ask = r.quoted_ask_cents,
            title = title_short,
        );
    }
    eprintln!();
}

/// Bucket a skip reason string into a coarse category label for
/// the per-run histogram. The reason strings are produced by
/// `curate_one` — keep this categorizer in lockstep with the
/// match arms there. When a new skip path is added, return a new
/// category name here so the operator-facing histogram surfaces
/// it.
fn skip_category(reason: &str) -> &'static str {
    // Phase 1 (NWS deterministic) reasons:
    if reason.contains("InsideConvictionZone") {
        "inside_conviction_zone"
    } else if reason.contains("NoOverlappingHours") {
        "no_overlapping_forecast_hours"
    } else if reason.contains("NonFahrenheitForecast") {
        "non_fahrenheit_forecast"
    // Strike-kind skip (shared between Phase 1 and Phase 2):
    } else if reason.contains("UnsupportedStrikeKind") || reason.contains("between") {
        "unsupported_strike_kind_between"
    // Phase 2 (NBM probabilistic) reasons:
    } else if reason.contains("no NBM quantiles available") {
        "nbm_window_unreachable"
    } else if reason.contains("forecast window unreachable") {
        "settlement_window_in_past"
    // Shared reasons:
    } else if reason.starts_with("unmapped airport") {
        "unmapped_airport"
    } else if reason.starts_with("parse:") {
        "parse_error"
    } else {
        "other"
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
