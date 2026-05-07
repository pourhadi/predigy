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
use crate::kalshi_scan::TempMarket;
use crate::nbm_path::{
    DAILY_HIGH_LOCAL_HOURS, DAILY_LOW_LOCAL_HOURS, approx_utc_offset_hours, forecast_hour_window,
};
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
        match score_plan(&plan, &fetched) {
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
    let want_above = matches!(plan.spec.kind, TempStrikeKind::Greater { .. });
    let mut best_p: f64 = 0.0;
    let mut best_h: Option<u16> = None;
    let mut peak_value_k: Option<f32> = None;
    for h in plan.window_start..=plan.window_end {
        let Some(q) = fetched.get(&(h, plan.airport.code)) else {
            continue;
        };
        let cdf = q.cdf_at(threshold_k);
        let p = if want_above { 1.0 - cdf } else { cdf };
        if p > best_p {
            best_p = p;
            best_h = Some(h);
            // Use the median (50% level) as the peak-hour reference
            // value for the inspection table.
            peak_value_k = q.temps_k.get(10).copied();
        }
    }
    if best_h.is_none() {
        return Err("no NBM quantiles available in forecast window".into());
    }
    // Clamp away from {0, 1}.  A raw 100% from NBM means "all 21
    // ensemble quantiles bracket below the threshold" — it does NOT
    // mean "physically impossible to exceed the threshold". A black-
    // swan front, a measurement-source quirk, or a settlement-feed
    // mismatch can still flip the outcome. Capping at [0.02, 0.98]
    // keeps stat-trader's Kelly sizing finite — without it, a 100%
    // belief at price 5¢ would size to ~max contracts.  Phase 2E
    // calibration replaces this hard cap with per-airport Platt
    // scaling against historical observations.
    let model_p = best_p.clamp(0.02, 0.98);
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
    let forecast_value_f = peak_value_k
        .map(|k| (f64::from(k) - 273.15) * 9.0 / 5.0 + 32.0)
        .unwrap_or(0.0);
    let audit = format!(
        "ticker={ticker} airport={code}({city}) kind={threshold} fcst_h={fh} fcst_50pct={fcst:.1}F model_p={mp:.3} side={side:?} ask={ask}c edge={edge:+}c",
        ticker = plan.market.ticker,
        code = plan.airport.code,
        city = plan.airport.city,
        threshold = threshold_str,
        fh = best_h.unwrap_or(0),
        fcst = forecast_value_f,
        mp = model_p,
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
    };
    Ok(NbmRuleOut {
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
    })
}

fn f_to_k(fahrenheit: f64) -> f64 {
    (fahrenheit - 32.0) * 5.0 / 9.0 + 273.15
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fahrenheit_to_kelvin_known_values() {
        // 32 F = 273.15 K
        assert!((f_to_k(32.0) - 273.15).abs() < 1e-9);
        // 80 F = 299.817 K (matches NBM threshold)
        assert!((f_to_k(80.0) - 299.817).abs() < 0.01);
        // 100 F = 310.928 K
        assert!((f_to_k(100.0) - 310.928).abs() < 0.01);
    }
}
