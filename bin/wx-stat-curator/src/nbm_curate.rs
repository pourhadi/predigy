//! Phase 2 NBM curation flow.
//!
//! Two-pass design over the scanned Kalshi markets:
//!
//! 1. **Plan**: parse each market into `(airport, kind, window)`,
//!    skipping ones we can't price (Between markets, unmapped
//!    airports, etc.).
//! 2. **Batch fetch**: for each unique (cycle × forecast hour),
//!    extract quantile temperatures for ALL airports needed at
//!    that hour in one call. The single bucket-side cost amortises
//!    across every market sharing the airport-day.
//! 3. **Emit**: per market, walk the window's per-hour quantile
//!    vectors, compute `P_h(side wins)` via CDF interpolation,
//!    take the max as `model_p`. Emit a `StatRule` if model_p is
//!    far enough from 50% to clear `min_edge_cents` against the
//!    quoted ask.
//!
//! Caching makes the second run within a 6h cycle window
//! effectively free — only file reads, no decode.

use crate::airports::{Airport, lookup_airport};
use crate::airports::{airport_utc_offset_hours, local_date_for_unix};
use crate::calibration::{BucketKey, Calibration};
use crate::kalshi_scan::TempMarket;
use crate::nbm_path::{
    DAILY_HIGH_LOCAL_HOURS, DAILY_LOW_LOCAL_HOURS, approx_utc_offset_hours, forecast_hour_window,
};
use crate::observed_gate::{
    ObservedGateDecision, ObservedMap, decide_from_observed, observations_required, observed_key,
};
use crate::predictions::{PredictionMeasurement, PredictionRecord};
use crate::ticker_parse::{TempMarketSpec, TempMeasurement, TempStrikeKind, parse_temp_market};
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_ext_feeds::nbm::{NbmClient, NbmCycle};
use predigy_ext_feeds::nbm_extract::{
    AirportQuantiles, NamedPoint, extract_tmp_quantiles_at_points,
};
use stat_trader::StatRule;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::{info, warn};

/// One NBM-curated rule plus inspection details. Mirrors the shape
/// `RuleInspection` in `main.rs` so the dry-run table can render
/// it identically.
#[derive(Debug, Clone)]
pub struct NbmRuleOut {
    pub rule: StatRule,
    pub audit: String,
    pub ticker: String,
    pub title: String,
    pub airport: String,
    pub threshold: String,
    /// Forecast value in F at the *peak hour* — the hour where
    /// the per-hour P_h(T > threshold) was largest (for high
    /// markets) / smallest (for low markets). Operator-facing
    /// only — used in the inspection table.
    pub forecast_value_f: f64,
    pub model_p: f64,
    pub side: Side,
    pub quoted_ask_cents: u8,
    pub apparent_edge_cents: i32,
    /// Sidecar prediction record for Phase 2E calibration. Forecast-priced
    /// rules emit one; observed-deterministic rules intentionally do not, to
    /// avoid leaking realised observations back into NBM calibration samples.
    pub prediction: Option<PredictionRecord>,
}

/// Outcome for one market — either a rule or a structured skip.
#[derive(Debug, Clone)]
pub enum NbmCurateOutcome {
    Rule(NbmRuleOut),
    Skip { reason: String },
    Error { reason: String },
}

/// Run the full NBM curation flow against a batch of Kalshi
/// temperature markets.
///
/// `cycle` is the NBM cycle we'll fetch from (typically chosen via
/// [`crate::nbm_path::recent_qmd_cycle`]). `cache_dir` is the
/// per-cycle on-disk cache root used by
/// [`predigy_ext_feeds::nbm_extract::extract_tmp_quantiles_at_points`].
pub async fn curate_via_nbm(
    nbm_client: &NbmClient,
    cache_dir: &Path,
    cycle: NbmCycle,
    markets: &[TempMarket],
    calibration: Option<&Calibration>,
    run_ts_utc: &str,
    run_unix: i64,
    observed: &ObservedMap,
) -> Vec<NbmCurateOutcome> {
    // ---- Pass 1: plan ----
    let mut plans: Vec<Plan> = Vec::new();
    let mut outcomes: Vec<NbmCurateOutcome> = Vec::with_capacity(markets.len());
    for m in markets {
        match plan_market(m, cycle) {
            Ok(plan) => plans.push(plan),
            Err(reason) => outcomes.push(NbmCurateOutcome::Skip { reason }),
        }
    }
    if plans.is_empty() {
        return outcomes;
    }

    // ---- Pass 2: collect needed (fcst_hour → airports) ----
    let mut needed_by_hour: HashMap<u16, HashSet<&'static str>> = HashMap::new();
    for plan in &plans {
        for h in plan.window_start..=plan.window_end {
            needed_by_hour
                .entry(h)
                .or_default()
                .insert(plan.airport.code);
        }
    }
    info!(
        markets_planned = plans.len(),
        forecast_hours_to_fetch = needed_by_hour.len(),
        "nbm: extracting quantiles"
    );

    let mut fetched: HashMap<(u16, &'static str), AirportQuantiles> = HashMap::new();
    let mut hours: Vec<u16> = needed_by_hour.keys().copied().collect();
    hours.sort_unstable();
    for fcst_hour in hours {
        let codes: Vec<&'static str> = needed_by_hour
            .get(&fcst_hour)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        let points: Vec<NamedPoint> = codes
            .iter()
            .filter_map(|code| {
                lookup_airport(code).map(|a| NamedPoint {
                    name: a.code.to_string(),
                    lat: a.lat,
                    lon: a.lon,
                })
            })
            .collect();
        match extract_tmp_quantiles_at_points(nbm_client, cache_dir, cycle, fcst_hour, &points)
            .await
        {
            Ok(qs) => {
                for q in qs {
                    // Find the matching &'static str airport code.
                    if let Some(code) = codes.iter().find(|c| **c == q.name.as_str()).copied() {
                        fetched.insert((fcst_hour, code), q);
                    }
                }
            }
            Err(e) => {
                warn!(fcst_hour, error = %e, "nbm: extract failed; skipping hour");
            }
        }
    }

    // ---- Pass 3: emit per market ----
    for plan in plans {
        match score_plan(&plan, &fetched, calibration, run_ts_utc, run_unix, observed) {
            Ok(out) => outcomes.push(NbmCurateOutcome::Rule(out)),
            Err(reason) => outcomes.push(NbmCurateOutcome::Skip { reason }),
        }
    }
    outcomes
}

#[derive(Debug, Clone)]
struct Plan {
    market: TempMarket,
    spec: TempMarketSpec,
    airport: &'static Airport,
    window_start: u16,
    window_end: u16,
}

fn plan_market(m: &TempMarket, cycle: NbmCycle) -> Result<Plan, String> {
    let spec = parse_temp_market(
        &m.event_ticker,
        m.strike_type.as_deref(),
        m.floor_strike,
        m.cap_strike,
        m.occurrence_datetime.as_deref(),
    )
    .map_err(|e| format!("parse: {e}"))?;
    let airport = lookup_airport(&spec.airport_code)
        .ok_or_else(|| format!("unmapped airport {}", spec.airport_code))?;
    if matches!(spec.kind, TempStrikeKind::Between { .. }) {
        return Err("unsupported strike kind: between".into());
    }
    let local_hours = match spec.measurement {
        TempMeasurement::DailyHigh => DAILY_HIGH_LOCAL_HOURS,
        TempMeasurement::DailyLow => DAILY_LOW_LOCAL_HOURS,
    };
    let utc_offset = approx_utc_offset_hours(airport.lon);
    let (start, end) = forecast_hour_window(cycle, &spec.settlement_date, local_hours, utc_offset)
        .ok_or_else(|| {
            format!(
                "forecast window unreachable for cycle {cycle:?} settlement {} offset {utc_offset}",
                spec.settlement_date
            )
        })?;
    if start == 0 {
        // f000 is the analysis cycle; quantiles may not be
        // published. Bump start to 1.
        let start = 1u16;
        return Ok(Plan {
            market: m.clone(),
            spec,
            airport,
            window_start: start,
            window_end: end.max(start),
        });
    }
    Ok(Plan {
        market: m.clone(),
        spec,
        airport,
        window_start: start,
        window_end: end,
    })
}

fn score_plan(
    plan: &Plan,
    fetched: &HashMap<(u16, &'static str), AirportQuantiles>,
    calibration: Option<&Calibration>,
    run_ts_utc: &str,
    run_unix: i64,
    observed: &ObservedMap,
) -> Result<NbmRuleOut, String> {
    // Threshold in Kelvin (NBM is K).
    let threshold_k = match plan.spec.kind {
        TempStrikeKind::Greater { threshold } | TempStrikeKind::Less { threshold } => {
            f_to_k(threshold) as f32
        }
        TempStrikeKind::Between { .. } => {
            return Err("unsupported strike kind: between".into());
        }
    };

    let observed_utc_offset = airport_utc_offset_hours(plan.airport, &plan.spec.settlement_date)
        .ok_or_else(|| {
            format!(
                "observed: missing UTC offset mapping for {}",
                plan.airport.code
            )
        })?;
    let run_local_date = local_date_for_unix(run_unix, observed_utc_offset)
        .ok_or_else(|| "observed: invalid run timestamp".to_string())?;
    if observations_required(&plan.spec.settlement_date, &run_local_date) {
        let key = observed_key(plan.airport, &plan.spec);
        let extremes = observed
            .get(&key)
            .ok_or_else(|| format!("observed: missing required ASOS cache for {key:?}"))?
            .as_ref()
            .map_err(|e| format!("observed: {e}"))?;
        if let Some(decision) = decide_from_observed(&plan.spec, extremes) {
            match decision {
                ObservedGateDecision::Decided {
                    model_p,
                    observed_f,
                    reason,
                    ..
                } => {
                    return Ok(build_rule_out(
                        plan,
                        model_p,
                        None,
                        observed_f,
                        run_ts_utc,
                        Some(&reason),
                        None,
                    ));
                }
                ObservedGateDecision::Undecided { .. } => {}
            }
        }
    }

    let aggregation = NbmAggregation::for_spec(&plan.spec);
    let mut best_p: f64 = aggregation.initial_probability();
    let mut best_h: Option<u16> = None;
    let mut peak_value_k: Option<f32> = None;
    for h in plan.window_start..=plan.window_end {
        let Some(q) = fetched.get(&(h, plan.airport.code)) else {
            continue;
        };
        let cdf = q.cdf_at(threshold_k);
        let p = aggregation.hour_yes_probability(cdf);
        if aggregation.should_replace(p, best_p) {
            best_p = p;
            best_h = Some(h);
            // Use the median (50% level) as the peak-hour reference
            // value for the inspection table. For all-hours markets
            // this becomes the constraining hour, not necessarily the
            // peak/coldest label implied by the field name.
            peak_value_k = q.temps_k.get(10).copied();
        }
    }
    if best_h.is_none() {
        return Err("no NBM quantiles available in forecast window".into());
    }
    // Phase 2E calibration: if a fitted (airport, month) bucket
    // exists, apply Platt scaling to the raw NBM probability before
    // emitting. The bucket's coefficients shift saturated/biased
    // model probabilities toward observed reality.
    let raw_p = best_p;
    let calibrated_p = match calibration {
        Some(cal) => {
            let month = parse_settlement_month(&plan.spec.settlement_date).unwrap_or(0);
            if month == 0 {
                raw_p
            } else {
                cal.apply_or_identity(&BucketKey::new(plan.airport.code, month), raw_p)
            }
        }
        None => raw_p,
    };
    // Hard clamp [0.02, 0.98] independent of whether calibration
    // was applied. A calibration fit on noisy data could itself
    // produce 99.9% beliefs we don't trust to size against; the
    // clamp is the floor of confidence, not a substitute for
    // calibration.
    let model_p = calibrated_p.clamp(0.02, 0.98);
    let forecast_value_f = peak_value_k
        .map(|k| (f64::from(k) - 273.15) * 9.0 / 5.0 + 32.0)
        .unwrap_or(0.0);
    let prediction = PredictionRecord {
        run_ts_utc: run_ts_utc.to_string(),
        ticker: plan.market.ticker.clone(),
        airport: plan.airport.code.to_string(),
        settlement_date: plan.spec.settlement_date.clone(),
        threshold_k,
        yes_when_above: matches!(plan.spec.kind, TempStrikeKind::Greater { .. }),
        measurement: match plan.spec.measurement {
            TempMeasurement::DailyHigh => PredictionMeasurement::DailyHigh,
            TempMeasurement::DailyLow => PredictionMeasurement::DailyLow,
        },
        raw_p,
        model_p,
        forecast_50pct_f: forecast_value_f,
    };

    Ok(build_rule_out(
        plan,
        model_p,
        best_h,
        forecast_value_f,
        run_ts_utc,
        calibration_note(&raw_p, &calibrated_p).as_deref(),
        Some(prediction),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NbmAggregation {
    AnyHourAbove,
    AllHoursBelow,
    AnyHourBelow,
    AllHoursAbove,
}

impl NbmAggregation {
    fn for_spec(spec: &TempMarketSpec) -> Self {
        match (spec.measurement, &spec.kind) {
            (TempMeasurement::DailyHigh, TempStrikeKind::Greater { .. }) => Self::AnyHourAbove,
            (TempMeasurement::DailyHigh, TempStrikeKind::Less { .. }) => Self::AllHoursBelow,
            (TempMeasurement::DailyLow, TempStrikeKind::Less { .. }) => Self::AnyHourBelow,
            (TempMeasurement::DailyLow, TempStrikeKind::Greater { .. }) => Self::AllHoursAbove,
            (_, TempStrikeKind::Between { .. }) => {
                unreachable!("between guarded before NBM scoring")
            }
        }
    }

    fn initial_probability(self) -> f64 {
        match self {
            Self::AnyHourAbove | Self::AnyHourBelow => 0.0,
            Self::AllHoursBelow | Self::AllHoursAbove => 1.0,
        }
    }

    fn hour_yes_probability(self, cdf: f64) -> f64 {
        match self {
            Self::AnyHourAbove | Self::AllHoursAbove => 1.0 - cdf,
            Self::AnyHourBelow | Self::AllHoursBelow => cdf,
        }
    }

    fn should_replace(self, candidate: f64, current: f64) -> bool {
        match self {
            Self::AnyHourAbove | Self::AnyHourBelow => candidate > current,
            Self::AllHoursBelow | Self::AllHoursAbove => candidate < current,
        }
    }
}

fn build_rule_out(
    plan: &Plan,
    model_p: f64,
    best_h: Option<u16>,
    forecast_value_f: f64,
    generated_at_utc: &str,
    note: Option<&str>,
    prediction: Option<PredictionRecord>,
) -> NbmRuleOut {
    let side = if model_p > 0.5 { Side::Yes } else { Side::No };

    // Edge in cents at curator time.
    let model_p_cents = (model_p * 100.0).round() as i32;
    let (quoted_ask_cents, apparent_edge_cents) = match side {
        Side::Yes => (
            plan.market.yes_ask_cents,
            model_p_cents - i32::from(plan.market.yes_ask_cents),
        ),
        Side::No => (
            plan.market.no_ask_cents,
            (100 - model_p_cents) - i32::from(plan.market.no_ask_cents),
        ),
    };

    let threshold_str = match plan.spec.kind {
        TempStrikeKind::Greater { threshold } => format!(">{threshold}"),
        TempStrikeKind::Less { threshold } => format!("<{threshold}"),
        TempStrikeKind::Between { lower, upper } => format!("[{lower},{upper}]"),
    };
    let note = note.map_or_else(String::new, |s| format!(" {s}"));
    let audit = format!(
        "ticker={ticker} airport={code}({city}) kind={threshold} fcst_h={fh} fcst_50pct={fcst:.1}F model_p={mp:.3}{note} side={side:?} ask={ask}c edge={edge:+}c",
        ticker = plan.market.ticker,
        code = plan.airport.code,
        city = plan.airport.city,
        threshold = threshold_str,
        fh = best_h.unwrap_or(0),
        fcst = forecast_value_f,
        mp = model_p,
        note = note,
        side = side,
        ask = quoted_ask_cents,
        edge = apparent_edge_cents,
    );

    let kalshi_market = MarketTicker::new(&plan.market.ticker);
    let rule = StatRule {
        kalshi_market,
        model_p,
        side,
        min_edge_cents: 5,
        settlement_date: Some(plan.spec.settlement_date.clone()),
        generated_at_utc: Some(generated_at_utc.to_string()),
    };
    NbmRuleOut {
        rule,
        audit,
        ticker: plan.market.ticker.clone(),
        title: plan.market.title.clone(),
        airport: plan.airport.code.to_string(),
        threshold: threshold_str,
        forecast_value_f,
        model_p,
        side,
        quoted_ask_cents,
        apparent_edge_cents,
        prediction,
    }
}

fn calibration_note(raw_p: &f64, calibrated_p: &f64) -> Option<String> {
    if (raw_p - calibrated_p).abs() > 1e-6 {
        Some(format!("raw_p={raw_p:.3}"))
    } else {
        None
    }
}

fn f_to_k(fahrenheit: f64) -> f64 {
    (fahrenheit - 32.0) * 5.0 / 9.0 + 273.15
}

/// `"2026-05-07"` → `Some(5)`. Returns `None` if the date doesn't
/// parse — caller treats that as "no calibration bucket".
fn parse_settlement_month(iso_date: &str) -> Option<u8> {
    let mut parts = iso_date.splitn(3, '-');
    parts.next()?; // year
    let month: u8 = parts.next()?.parse().ok()?;
    if (1..=12).contains(&month) {
        Some(month)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observations::DailyExtremes;

    #[test]
    fn parse_month_canonical() {
        assert_eq!(parse_settlement_month("2026-05-07"), Some(5));
        assert_eq!(parse_settlement_month("2026-12-31"), Some(12));
    }

    #[test]
    fn parse_month_rejects_invalid() {
        assert_eq!(parse_settlement_month("2026-13-01"), None);
        assert_eq!(parse_settlement_month("2026-00-01"), None);
        assert_eq!(parse_settlement_month("not-a-date"), None);
        assert_eq!(parse_settlement_month("2026"), None);
    }

    #[test]
    fn fahrenheit_to_kelvin_known_values() {
        // 32 F = 273.15 K
        assert!((f_to_k(32.0) - 273.15).abs() < 1e-9);
        // 80 F = 299.817 K (matches NBM threshold)
        assert!((f_to_k(80.0) - 299.817).abs() < 0.01);
        // 100 F = 310.928 K
        assert!((f_to_k(100.0) - 310.928).abs() < 0.01);
    }

    #[test]
    fn observed_high_less_market_forces_no_side_before_nbm() {
        let airport = lookup_airport("SFO").unwrap();
        let plan = Plan {
            market: TempMarket {
                ticker: "KXHIGHTSFO-26MAY07-T62".into(),
                event_ticker: "KXHIGHTSFO-26MAY07".into(),
                series_ticker: "KXHIGHTSFO".into(),
                title: "SFO high below 62".into(),
                close_time: "2026-05-08T06:59:00Z".into(),
                yes_ask_cents: 1,
                no_ask_cents: 3,
                strike_type: Some("less".into()),
                floor_strike: Some(62.0),
                cap_strike: Some(62.0),
                occurrence_datetime: Some("2026-05-07T14:00:00Z".into()),
            },
            spec: TempMarketSpec {
                airport_code: "SFO".into(),
                measurement: TempMeasurement::DailyHigh,
                kind: TempStrikeKind::Less { threshold: 62.0 },
                settlement_date: "2026-05-07".into(),
            },
            airport,
            window_start: 1,
            window_end: 1,
        };
        let mut observed = ObservedMap::new();
        observed.insert(
            ("SFO".into(), "2026-05-07".into()),
            Ok(DailyExtremes {
                station: "SFO".into(),
                date_utc: "2026-05-07".into(),
                tmax_f: 64.0,
                tmin_f: 51.0,
                n_obs: 100,
            }),
        );

        let out = score_plan(
            &plan,
            &HashMap::new(),
            None,
            "2026-05-07T20:00:00Z",
            1_778_184_000,
            &observed,
        )
        .unwrap();

        assert_eq!(out.rule.side, Side::No);
        assert_eq!(out.rule.model_p, 0.02);
        assert_eq!(out.side, Side::No);
        assert_eq!(out.quoted_ask_cents, 3);
        assert!(out.prediction.is_none());
        assert!(
            out.audit
                .contains("observed high 64.0F already >= less-than threshold 62.0F")
        );
    }

    #[test]
    fn daily_high_less_uses_constraining_hot_hour_not_coolest_hour() {
        let airport = lookup_airport("PHX").unwrap();
        let plan = Plan {
            market: TempMarket {
                ticker: "KXHIGHTPHX-26MAY08-T98".into(),
                event_ticker: "KXHIGHTPHX-26MAY08".into(),
                series_ticker: "KXHIGHTPHX".into(),
                title: "PHX high below 98".into(),
                close_time: "2026-05-09T06:59:00Z".into(),
                yes_ask_cents: 4,
                no_ask_cents: 97,
                strike_type: Some("less".into()),
                floor_strike: Some(98.0),
                cap_strike: Some(98.0),
                occurrence_datetime: Some("2026-05-08T14:00:00Z".into()),
            },
            spec: TempMarketSpec {
                airport_code: "PHX".into(),
                measurement: TempMeasurement::DailyHigh,
                kind: TempStrikeKind::Less { threshold: 98.0 },
                settlement_date: "2026-05-08".into(),
            },
            airport,
            window_start: 66,
            window_end: 72,
        };
        let mut fetched = HashMap::new();
        fetched.insert((66, "PHX"), quantiles_at_f("PHX", 66, 101.2));
        fetched.insert((72, "PHX"), quantiles_at_f("PHX", 72, 86.4));

        let out = score_plan(
            &plan,
            &fetched,
            None,
            "2026-05-07T20:00:00Z",
            1_778_184_000,
            &ObservedMap::new(),
        )
        .unwrap();

        assert_eq!(out.rule.side, Side::No);
        assert_eq!(out.rule.model_p, 0.02);
        assert_eq!(out.forecast_value_f.round() as i32, 101);
        assert!(out.audit.contains("fcst_h=66"));
    }

    #[test]
    fn daily_low_greater_uses_constraining_cold_hour_not_warmest_hour() {
        let airport = lookup_airport("PHX").unwrap();
        let plan = Plan {
            market: TempMarket {
                ticker: "KXLOWTPHX-26MAY08-T69".into(),
                event_ticker: "KXLOWTPHX-26MAY08".into(),
                series_ticker: "KXLOWTPHX".into(),
                title: "PHX low above 69".into(),
                close_time: "2026-05-09T06:59:00Z".into(),
                yes_ask_cents: 57,
                no_ask_cents: 44,
                strike_type: Some("greater".into()),
                floor_strike: Some(69.0),
                cap_strike: None,
                occurrence_datetime: Some("2026-05-08T14:00:00Z".into()),
            },
            spec: TempMarketSpec {
                airport_code: "PHX".into(),
                measurement: TempMeasurement::DailyLow,
                kind: TempStrikeKind::Greater { threshold: 69.0 },
                settlement_date: "2026-05-08".into(),
            },
            airport,
            window_start: 50,
            window_end: 57,
        };
        let mut fetched = HashMap::new();
        fetched.insert((50, "PHX"), quantiles_at_f("PHX", 50, 65.0));
        fetched.insert((57, "PHX"), quantiles_at_f("PHX", 57, 78.0));

        let out = score_plan(
            &plan,
            &fetched,
            None,
            "2026-05-07T20:00:00Z",
            1_778_184_000,
            &ObservedMap::new(),
        )
        .unwrap();

        assert_eq!(out.rule.side, Side::No);
        assert_eq!(out.rule.model_p, 0.02);
        assert_eq!(out.forecast_value_f.round() as i32, 65);
        assert!(out.audit.contains("fcst_h=50"));
    }

    fn quantiles_at_f(name: &str, fcst_hour: u16, f: f64) -> AirportQuantiles {
        let k = f_to_k(f) as f32;
        AirportQuantiles {
            cycle_prefix: "blend.20260507/06".into(),
            fcst_hour,
            name: name.into(),
            query_lat: 33.4342,
            query_lon: -112.0117,
            snapped_lat: 33.4342,
            snapped_lon: -112.0117,
            snap_distance_km: 0.0,
            temps_k: vec![k; 21],
        }
    }
}
