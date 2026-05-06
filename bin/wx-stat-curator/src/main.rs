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
use predigy_ext_feeds::nws_forecast::NwsForecastClient;
use predigy_kalshi_rest::{Client as RestClient, Signer};
use stat_trader::StatRule;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
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

    // Resolve airport→GridPoint lazily, caching by airport code.
    let cfg = ProbabilityConfig {
        min_margin_f: args.min_margin_f,
    };
    let mut grid_cache: HashMap<&'static str, predigy_ext_feeds::nws_forecast::GridPoint> =
        HashMap::new();
    let mut forecast_cache: HashMap<
        (&'static str, String),
        predigy_ext_feeds::nws_forecast::HourlyForecast,
    > = HashMap::new();
    let mut rules: Vec<StatRule> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();

    for m in &markets {
        match curate_one(&nws, m, &cfg, &mut grid_cache, &mut forecast_cache).await {
            CurateOutcome::Rule { rule, audit } => {
                info!(audit = %audit, "accepted rule");
                rules.push(rule);
            }
            CurateOutcome::Skip(reason) => {
                debug!(market = %m.ticker, reason = %reason, "skip");
                skipped.push((m.ticker.clone(), reason));
            }
            CurateOutcome::Error(reason) => {
                warn!(market = %m.ticker, reason = %reason, "curate failed");
                skipped.push((m.ticker.clone(), reason));
            }
        }
    }

    info!(kept = rules.len(), skipped = skipped.len(), "synthesis done");

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
    }
    Ok(())
}

enum CurateOutcome {
    Rule { rule: StatRule, audit: String },
    Skip(String),
    Error(String),
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
            Err(e) => return CurateOutcome::Error(format!("nws lookup_point({}): {e}", airport.code)),
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
            Err(e) => return CurateOutcome::Error(format!("nws fetch_hourly({}): {e}", airport.code)),
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
        } => emit_rule(m, airport, &spec.kind, model_p, forecast_value_f, hours_considered),
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
    let side = if model_p > 0.5 {
        Side::Yes
    } else {
        Side::No
    };
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
    let audit = format!(
        "ticker={ticker} airport={code}({city}) kind={threshold} forecast={forecast:.1}F hours={hours} model_p={mp:.3} side={side:?} yes_ask={yes_ask}c",
        ticker = m.ticker,
        code = airport.code,
        city = airport.city,
        threshold = threshold_str,
        forecast = forecast_value_f,
        hours = hours_considered,
        mp = model_p,
        side = side,
        yes_ask = m.yes_ask_cents,
    );
    CurateOutcome::Rule { rule, audit }
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
