//! Compute `model_p` from an NWS hourly forecast given a
//! [`TempMarketSpec`].
//!
//! ## Phase 1 (this module): deterministic point forecast
//!
//! NWS hourly forecast is a single number per hour, not a
//! distribution. So `model_p` is binary 0/1 in the conviction-zone
//! and `None` (skip) outside it:
//!
//! - For a `Greater { threshold }` market on date `D`: take the max
//!   of the forecast over hours falling in `D` (local time);
//!   compare to `threshold`. Margin = `forecast_max - threshold`.
//! - For a `Less { threshold }` market: max forecast vs threshold
//!   the same way (max < threshold ⇒ YES; max > threshold ⇒ NO).
//! - For a `Between { lower, upper }` market: skip in Phase 1 — a
//!   point forecast can't sensibly probability-band a range market.
//!
//! ## The conviction-zone gate
//!
//! Without a calibrated distribution we have no idea whether a
//! 1-degree margin means 60% confidence or 95%. So Phase 1 only
//! emits a rule when the forecast margin to the threshold is
//! **at least `min_margin_f` degrees** AND the market price still
//! disagrees enough to clear `min_edge_cents`. Inside the conviction
//! zone we treat `model_p = 1.0 - epsilon` (or `epsilon` for the
//! losing side) — a near-certainty bet.
//!
//! Phase 2 replaces this with NBM probabilistic data + per-airport
//! Platt calibration; the conviction-zone gate goes away then.

use crate::ticker_parse::{TempMarketSpec, TempMeasurement, TempStrikeKind};
use predigy_ext_feeds::nws_forecast::HourlyForecast;

/// Lower-bound of conviction-zone probability — a 1-epsilon belief
/// that the side wins. Choosing 0.97 rather than 0.99 leaves
/// headroom for NWS forecast misses (the historical 24h-out hourly
/// max-temp error has a long tail).
pub const CONVICTION_P: f64 = 0.97;

#[derive(Debug, Clone)]
pub struct ProbabilityConfig {
    /// Minimum forecast margin (degrees) to threshold required to
    /// emit a rule. Markets within this margin are skipped.
    pub min_margin_f: f64,
}

impl Default for ProbabilityConfig {
    fn default() -> Self {
        Self { min_margin_f: 5.0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForecastDecision {
    /// Forecast is decisive enough to emit a rule.
    Decisive {
        /// Calibrated probability YES will resolve true. In Phase 1
        /// always either `CONVICTION_P` or `1 - CONVICTION_P`.
        model_p: f64,
        /// Forecast aggregate value used for the decision (max for
        /// daily-high markets, min for daily-low markets), in F.
        forecast_value_f: f64,
        /// Hours of forecast data considered for the aggregate.
        hours_considered: usize,
    },
    /// Skip — either no overlapping hours, no convicting margin, or
    /// strike-kind not supported in Phase 1.
    Skip { reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// Forecast doesn't include any hour for the requested date.
    NoOverlappingHours,
    /// Forecast margin to threshold smaller than `min_margin_f`.
    InsideConvictionZone {
        forecast_value_f: f64,
        margin_f: f64,
    },
    /// `Between` markets aren't priceable from a point forecast.
    UnsupportedStrikeKind,
    /// Forecast contained Celsius — Phase 1 only handles F.
    /// Could be relaxed to convert; punt for now since all US
    /// hourly point forecasts come back F.
    NonFahrenheitForecast,
}

/// Compute the forecast aggregate (max for high markets, min for
/// low markets) over the hours that fall in the market's settlement
/// date, then decide what to emit.
pub fn derive_model_p(
    spec: &TempMarketSpec,
    forecast: &HourlyForecast,
    cfg: &ProbabilityConfig,
) -> ForecastDecision {
    let threshold = match spec.kind {
        TempStrikeKind::Greater { threshold } | TempStrikeKind::Less { threshold } => threshold,
        TempStrikeKind::Between { .. } => {
            return ForecastDecision::Skip {
                reason: SkipReason::UnsupportedStrikeKind,
            };
        }
    };

    // Filter to hours whose `start_time` local-date equals the
    // settlement date. NWS hourly periods carry a local-time offset
    // already (e.g. `2026-05-07T14:00:00-06:00`), so the date prefix
    // of `start_time` IS the local date — no tz conversion needed.
    let mut values: Vec<f64> = Vec::new();
    for p in &forecast.periods {
        if !p.temperature_unit.eq_ignore_ascii_case("F") {
            return ForecastDecision::Skip {
                reason: SkipReason::NonFahrenheitForecast,
            };
        }
        let Some(local_date) = p.start_time.get(..10) else {
            continue;
        };
        if local_date == spec.settlement_date {
            values.push(p.temperature);
        }
    }
    if values.is_empty() {
        return ForecastDecision::Skip {
            reason: SkipReason::NoOverlappingHours,
        };
    }
    let hours_considered = values.len();
    let aggregate = match spec.measurement {
        TempMeasurement::DailyHigh => values.iter().copied().fold(f64::MIN, f64::max),
        TempMeasurement::DailyLow => values.iter().copied().fold(f64::MAX, f64::min),
    };

    // Margin = signed distance toward YES. For a `Greater {68}`
    // market (YES if observed > 68), `forecast_high - threshold`
    // is the margin: positive ⇒ favour YES, negative ⇒ favour NO.
    // For `Less {61}` (YES if observed < 61), it's the negation:
    // `threshold - forecast_high`.
    let margin = match spec.kind {
        TempStrikeKind::Greater { .. } => aggregate - threshold,
        TempStrikeKind::Less { .. } => threshold - aggregate,
        TempStrikeKind::Between { .. } => unreachable!("guarded above"),
    };

    if margin.abs() < cfg.min_margin_f {
        return ForecastDecision::Skip {
            reason: SkipReason::InsideConvictionZone {
                forecast_value_f: aggregate,
                margin_f: margin,
            },
        };
    }

    let model_p = if margin >= 0.0 {
        CONVICTION_P
    } else {
        1.0 - CONVICTION_P
    };
    ForecastDecision::Decisive {
        model_p,
        forecast_value_f: aggregate,
        hours_considered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_ext_feeds::nws_forecast::{HourlyForecast, HourlyForecastEntry};

    fn forecast_with(temps_f: &[(&str, f64)]) -> HourlyForecast {
        HourlyForecast {
            generated_at: Some("2026-05-05T18:00:00Z".into()),
            periods: temps_f
                .iter()
                .map(|(start, t)| HourlyForecastEntry {
                    start_time: (*start).into(),
                    end_time: format!("{start}+1h"), // unused in tests
                    temperature: *t,
                    temperature_unit: "F".into(),
                })
                .collect(),
        }
    }

    fn spec_high_gt(threshold: f64, date: &str) -> TempMarketSpec {
        TempMarketSpec {
            airport_code: "DEN".into(),
            measurement: TempMeasurement::DailyHigh,
            kind: TempStrikeKind::Greater { threshold },
            settlement_date: date.into(),
        }
    }

    fn spec_high_lt(threshold: f64, date: &str) -> TempMarketSpec {
        TempMarketSpec {
            airport_code: "DEN".into(),
            measurement: TempMeasurement::DailyHigh,
            kind: TempStrikeKind::Less { threshold },
            settlement_date: date.into(),
        }
    }

    #[test]
    fn decisive_yes_when_forecast_max_clears_threshold_by_margin() {
        // Forecast hits 78F on 2026-05-07. Threshold 68F. Margin 10F > 5F.
        let f = forecast_with(&[
            ("2026-05-07T13:00:00-06:00", 70.0),
            ("2026-05-07T14:00:00-06:00", 78.0),
            ("2026-05-07T15:00:00-06:00", 76.0),
        ]);
        let d = derive_model_p(&spec_high_gt(68.0, "2026-05-07"), &f, &ProbabilityConfig::default());
        match d {
            ForecastDecision::Decisive {
                model_p,
                forecast_value_f,
                hours_considered,
            } => {
                assert!((model_p - CONVICTION_P).abs() < 1e-9);
                assert!((forecast_value_f - 78.0).abs() < 1e-9);
                assert_eq!(hours_considered, 3);
            }
            other @ ForecastDecision::Skip { .. } => panic!("expected Decisive, got {other:?}"),
        }
    }

    #[test]
    fn decisive_no_when_forecast_max_below_threshold_by_margin() {
        // Forecast tops out 60F. Threshold 68F. Margin -8F. Favours NO.
        let f = forecast_with(&[
            ("2026-05-07T14:00:00-06:00", 58.0),
            ("2026-05-07T15:00:00-06:00", 60.0),
        ]);
        let d = derive_model_p(&spec_high_gt(68.0, "2026-05-07"), &f, &ProbabilityConfig::default());
        match d {
            ForecastDecision::Decisive { model_p, .. } => {
                assert!((model_p - (1.0 - CONVICTION_P)).abs() < 1e-9);
            }
            other @ ForecastDecision::Skip { .. } => panic!("expected Decisive, got {other:?}"),
        }
    }

    #[test]
    fn skip_when_inside_conviction_zone() {
        // Forecast 71F vs threshold 68F → margin only 3F. Skip.
        let f = forecast_with(&[("2026-05-07T14:00:00-06:00", 71.0)]);
        let d = derive_model_p(&spec_high_gt(68.0, "2026-05-07"), &f, &ProbabilityConfig::default());
        match d {
            ForecastDecision::Skip {
                reason: SkipReason::InsideConvictionZone { .. },
            } => {}
            other => panic!("expected Skip(InsideConvictionZone), got {other:?}"),
        }
    }

    #[test]
    fn skip_when_no_hours_overlap_settlement_date() {
        let f = forecast_with(&[("2026-05-08T14:00:00-06:00", 90.0)]);
        let d = derive_model_p(&spec_high_gt(68.0, "2026-05-07"), &f, &ProbabilityConfig::default());
        assert_eq!(
            d,
            ForecastDecision::Skip {
                reason: SkipReason::NoOverlappingHours,
            }
        );
    }

    #[test]
    fn skip_between_markets() {
        let spec = TempMarketSpec {
            airport_code: "DEN".into(),
            measurement: TempMeasurement::DailyHigh,
            kind: TempStrikeKind::Between {
                lower: 65.0,
                upper: 67.0,
            },
            settlement_date: "2026-05-07".into(),
        };
        let f = forecast_with(&[("2026-05-07T14:00:00-06:00", 66.0)]);
        let d = derive_model_p(&spec, &f, &ProbabilityConfig::default());
        assert_eq!(
            d,
            ForecastDecision::Skip {
                reason: SkipReason::UnsupportedStrikeKind,
            }
        );
    }

    #[test]
    fn less_than_market_handles_direction_correctly() {
        // YES if observed < 60. Forecast tops out at 50 → favours YES.
        let f = forecast_with(&[("2026-05-07T14:00:00-06:00", 50.0)]);
        let d = derive_model_p(&spec_high_lt(60.0, "2026-05-07"), &f, &ProbabilityConfig::default());
        match d {
            ForecastDecision::Decisive { model_p, .. } => {
                assert!((model_p - CONVICTION_P).abs() < 1e-9);
            }
            other @ ForecastDecision::Skip { .. } => panic!("expected Decisive YES, got {other:?}"),
        }
    }
}
