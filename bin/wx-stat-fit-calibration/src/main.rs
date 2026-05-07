// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! `wx-stat-fit-calibration`: fit Platt-scaling calibration for
//! the wx-stat-curator pipeline against accumulated prediction
//! records and realised airport observations.
//!
//! ```text
//! wx-stat-fit-calibration \
//!   --predictions-dir data/wx_stat_predictions \
//!   --asos-cache data/asos_cache \
//!   --user-agent "$NWS_USER_AGENT" \
//!   --calibration-out data/wx_stat_calibration.json \
//!   --min-samples-per-bucket 10
//! ```
//!
//! Pipeline:
//!
//! 1. Read every `*.jsonl` under `--predictions-dir` as
//!    `PredictionRecord`s.
//! 2. Filter to records whose `settlement_date` is strictly in
//!    the past (UTC) — we can't observe outcomes for the future.
//! 3. For each surviving record, fetch the airport's ASOS daily
//!    extreme on the settlement date (cached on disk).
//! 4. Compute `outcome ∈ {0.0, 1.0}` from the observed extreme
//!    versus the threshold + side direction.
//! 5. Group `(raw_p, outcome)` pairs by (airport, month-of-
//!    settlement).
//! 6. Fit Platt scaling per bucket where sample count meets the
//!    floor. Write `calibration.json`.

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wx_stat_curator::airports::lookup_airport;
use wx_stat_curator::calibration::{BucketKey, Calibration, fit_platt};
use wx_stat_curator::observations::AsosClient;
use wx_stat_curator::predictions::{PredictionMeasurement, PredictionRecord, read_dir_records};

#[derive(Debug, Parser)]
#[command(
    name = "wx-stat-fit-calibration",
    about = "Fit Platt-scaling calibration for wx-stat-curator from accumulated predictions + realised observations."
)]
struct Args {
    /// Directory of prediction-record JSONL files written by
    /// `wx-stat-curator --nbm`.
    #[arg(long, default_value = "data/wx_stat_predictions")]
    predictions_dir: PathBuf,

    /// Cache root for fetched ASOS daily extremes. Re-fits over
    /// the same prediction window are zero network after the
    /// first pass.
    #[arg(long, default_value = "data/asos_cache")]
    asos_cache: PathBuf,

    /// User-Agent for HTTP requests. Iowa Mesonet doesn't strictly
    /// require it but it's polite to identify the caller.
    #[arg(long, env = "NWS_USER_AGENT")]
    user_agent: String,

    /// Output path for the fitted calibration JSON. Loaded by
    /// `wx-stat-curator --nbm-calibration` at the next run.
    #[arg(long, default_value = "data/wx_stat_calibration.json")]
    calibration_out: PathBuf,

    /// Minimum (raw_p, outcome) samples per bucket required for a
    /// bucket fit to ship. Buckets below the floor stay
    /// uncalibrated (identity at inference); the operator sees
    /// the per-bucket sample histogram in the run summary.
    #[arg(long, default_value_t = 10)]
    min_samples_per_bucket: u32,

    /// Number of full days of slack to wait after a settlement
    /// date before trying to fetch the observation. ASOS publishes
    /// the day's data near-real-time but we leave a buffer for
    /// late-arriving rows. Default 1 — a settlement_date strictly
    /// at least 1 full UTC day ago.
    #[arg(long, default_value_t = 1)]
    settlement_lag_days: i64,

    /// Skip the actual calibration write — only print the
    /// histogram and per-bucket fits to stderr. Useful for
    /// eyeballing before committing.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    let predictions = read_dir_records(&args.predictions_dir)
        .with_context(|| format!("read predictions {}", args.predictions_dir.display()))?;
    info!(
        n_predictions = predictions.len(),
        dir = %args.predictions_dir.display(),
        "loaded prediction records"
    );
    if predictions.is_empty() {
        return Err(anyhow!(
            "no predictions found under {} — run wx-stat-curator --nbm first to accumulate them",
            args.predictions_dir.display()
        ));
    }

    let cutoff_unix = now_unix() - args.settlement_lag_days.saturating_mul(86_400);
    let eligible: Vec<&PredictionRecord> = predictions
        .iter()
        .filter(|p| {
            settlement_unix(&p.settlement_date)
                .is_some_and(|t| t <= cutoff_unix)
        })
        .collect();
    info!(
        n_eligible = eligible.len(),
        n_total = predictions.len(),
        "filtered to predictions whose settlement is past the lag cutoff"
    );

    let asos = AsosClient::new(&args.user_agent).map_err(|e| anyhow!("asos client: {e}"))?;

    // Collect (airport_code → settlement_date → daily extremes)
    // by fetching once per (station, date) pair.
    let mut obs_cache: HashMap<(String, String), wx_stat_curator::observations::DailyExtremes> =
        HashMap::new();
    let mut buckets: HashMap<BucketKey, Vec<(f64, f64)>> = HashMap::new();
    let mut dropped = 0u32;

    for p in &eligible {
        let Some(airport) = lookup_airport(&p.airport) else {
            warn!(airport = %p.airport, "no airport entry; dropping prediction");
            dropped += 1;
            continue;
        };
        let station = airport.asos_station_or_code().to_string();
        let key = (station.clone(), p.settlement_date.clone());
        let extremes = match obs_cache.get(&key) {
            Some(e) => e.clone(),
            None => match asos
                .fetch_daily_extremes(&args.asos_cache, &station, &p.settlement_date)
                .await
            {
                Ok(e) => {
                    obs_cache.insert(key, e.clone());
                    e
                }
                Err(e) => {
                    warn!(
                        airport = %p.airport,
                        station = %station,
                        date = %p.settlement_date,
                        error = %e,
                        "asos fetch failed; dropping prediction"
                    );
                    dropped += 1;
                    continue;
                }
            },
        };
        // Pick the right extreme based on the prediction's
        // measurement intent.
        let observed_f = match p.measurement {
            PredictionMeasurement::DailyHigh => extremes.tmax_f,
            PredictionMeasurement::DailyLow => extremes.tmin_f,
        };
        let observed_k = (observed_f - 32.0) * 5.0 / 9.0 + 273.15;
        let exceeded = (observed_k as f32) > p.threshold_k;
        let outcome = if (exceeded && p.yes_when_above)
            || (!exceeded && !p.yes_when_above)
        {
            1.0_f64
        } else {
            0.0_f64
        };
        let month = settlement_month(&p.settlement_date).unwrap_or(0);
        if month == 0 {
            dropped += 1;
            continue;
        }
        let bucket_key = BucketKey::new(airport.code, month);
        buckets
            .entry(bucket_key)
            .or_default()
            .push((p.raw_p, outcome));
    }

    info!(
        n_buckets = buckets.len(),
        n_obs_cached = obs_cache.len(),
        n_dropped = dropped,
        "joined predictions with observations"
    );

    // Fit per bucket where sample count meets floor.
    let mut cal = Calibration::empty();
    let mut fit_summary: Vec<(BucketKey, u32, Option<wx_stat_curator::calibration::PlattCoeffs>)> =
        Vec::new();
    for (bucket, samples) in &buckets {
        let n: u32 = u32::try_from(samples.len()).unwrap_or(u32::MAX);
        if n < args.min_samples_per_bucket {
            fit_summary.push((bucket.clone(), n, None));
            continue;
        }
        match fit_platt(samples) {
            Some(coeffs) => {
                cal.set(bucket.clone(), coeffs, n);
                fit_summary.push((bucket.clone(), n, Some(coeffs)));
            }
            None => fit_summary.push((bucket.clone(), n, None)),
        }
    }

    cal.fitted_at_iso = Some(format_now_utc_iso());
    cal.source = Some(format!(
        "wx-stat-fit-calibration: {} eligible predictions, {} buckets fitted",
        eligible.len(),
        cal.buckets.len()
    ));

    print_summary(&fit_summary);

    if args.dry_run {
        eprintln!("(dry-run — calibration not written)");
        return Ok(());
    }
    cal.save(&args.calibration_out)
        .with_context(|| format!("write {}", args.calibration_out.display()))?;
    println!(
        "wrote calibration with {} fitted buckets to {}",
        cal.buckets.len(),
        args.calibration_out.display()
    );
    Ok(())
}

fn print_summary(
    summary: &[(BucketKey, u32, Option<wx_stat_curator::calibration::PlattCoeffs>)],
) {
    let mut rows: Vec<&(BucketKey, u32, Option<wx_stat_curator::calibration::PlattCoeffs>)> =
        summary.iter().collect();
    rows.sort_by(|a, b| (a.0.airport.as_str(), a.0.month).cmp(&(b.0.airport.as_str(), b.0.month)));
    eprintln!();
    eprintln!(
        "  {airport:<5}  {month:>3}  {n:>4}  {a:>7}  {b:>7}",
        airport = "ap",
        month = "mo",
        n = "n",
        a = "a",
        b = "b"
    );
    for (bucket, n, coeffs) in rows {
        match coeffs {
            Some(c) => eprintln!(
                "  {airport:<5}  {month:>3}  {n:>4}  {a:>7.3}  {b:>7.3}",
                airport = bucket.airport,
                month = bucket.month,
                n = n,
                a = c.a,
                b = c.b,
            ),
            None => eprintln!(
                "  {airport:<5}  {month:>3}  {n:>4}  {a:>7}  {b:>7}",
                airport = bucket.airport,
                month = bucket.month,
                n = n,
                a = "—",
                b = "—",
            ),
        }
    }
    eprintln!();
}

fn settlement_month(iso_date: &str) -> Option<u8> {
    let mut parts = iso_date.splitn(3, '-');
    parts.next()?;
    let m: u8 = parts.next()?.parse().ok()?;
    if (1..=12).contains(&m) { Some(m) } else { None }
}

fn settlement_unix(iso_date: &str) -> Option<i64> {
    let mut parts = iso_date.splitn(3, '-');
    let y: u16 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let is_leap = |yr: u16| (yr.is_multiple_of(4) && !yr.is_multiple_of(100)) || yr.is_multiple_of(400);
    let dim = |yr: u16, mo: u8| -> u32 {
        match mo {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap(yr) {
                    29
                } else {
                    28
                }
            }
            _ => 31,
        }
    };
    let mut days: i64 = 0;
    for yr in 1970..y {
        days += if is_leap(yr) { 366 } else { 365 };
    }
    for mo in 1..m {
        days += i64::from(dim(y, mo));
    }
    days += i64::from(d - 1);
    Some(days * 86_400)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn format_now_utc_iso() -> String {
    let secs = now_unix().max(0) as u64;
    let total_secs = secs;
    let hour = ((total_secs / 3600) % 24) as u8;
    let minute = ((total_secs / 60) % 60) as u8;
    let second = (total_secs % 60) as u8;
    let days_since_epoch = (total_secs / 86_400) as u32;
    let mut year: u16 = 1970;
    let mut remaining = days_since_epoch;
    let is_leap = |y: u16| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if remaining < dy {
            break;
        }
        remaining -= dy;
        year += 1;
    }
    let mut month: u8 = 1;
    loop {
        let dim: u32 = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if is_leap(year) {
                    29
                } else {
                    28
                }
            }
            _ => 31,
        };
        if remaining < dim {
            break;
        }
        remaining -= dim;
        month += 1;
    }
    let day = u8::try_from(remaining + 1).unwrap_or(1);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
