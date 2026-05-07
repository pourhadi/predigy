//! Same-day observed-extreme gate for Kalshi daily-temperature markets.
//!
//! Daily highs are monotonic upward during the local day and daily lows are
//! monotonic downward. Once an observed extreme crosses a market threshold,
//! that market's YES outcome is already decided for the rest of the day. The
//! curator must use that fact before consulting forecast probabilities.

use crate::airports::Airport;
use crate::observations::DailyExtremes;
use crate::ticker_parse::{TempMarketSpec, TempMeasurement, TempStrikeKind};
use predigy_core::side::Side;

pub type ObservedKey = (String, String);
pub type ObservedMap = std::collections::HashMap<ObservedKey, Result<DailyExtremes, String>>;

#[derive(Debug, Clone, PartialEq)]
pub enum ObservedGateDecision {
    Decided {
        yes_wins: bool,
        model_p: f64,
        side: Side,
        observed_f: f64,
        reason: String,
    },
    Undecided {
        observed_f: f64,
        reason: String,
    },
}

pub fn observations_required(settlement_date: &str, run_local_date: &str) -> bool {
    settlement_date <= run_local_date
}

pub fn observed_key(airport: &Airport, spec: &TempMarketSpec) -> ObservedKey {
    (
        airport.asos_station_or_code().to_string(),
        spec.settlement_date.clone(),
    )
}

pub fn decide_from_observed(
    spec: &TempMarketSpec,
    extremes: &DailyExtremes,
) -> Option<ObservedGateDecision> {
    match (&spec.measurement, &spec.kind) {
        (TempMeasurement::DailyHigh, TempStrikeKind::Greater { threshold }) => {
            if extremes.tmax_f > *threshold {
                Some(decided(
                    true,
                    extremes.tmax_f,
                    format!(
                        "observed high {:.1}F already > threshold {:.1}F",
                        extremes.tmax_f, threshold
                    ),
                ))
            } else {
                Some(ObservedGateDecision::Undecided {
                    observed_f: extremes.tmax_f,
                    reason: format!(
                        "observed high {:.1}F has not exceeded threshold {:.1}F",
                        extremes.tmax_f, threshold
                    ),
                })
            }
        }
        (TempMeasurement::DailyHigh, TempStrikeKind::Less { threshold }) => {
            if extremes.tmax_f >= *threshold {
                Some(decided(
                    false,
                    extremes.tmax_f,
                    format!(
                        "observed high {:.1}F already >= less-than threshold {:.1}F",
                        extremes.tmax_f, threshold
                    ),
                ))
            } else {
                Some(ObservedGateDecision::Undecided {
                    observed_f: extremes.tmax_f,
                    reason: format!(
                        "observed high {:.1}F remains below less-than threshold {:.1}F",
                        extremes.tmax_f, threshold
                    ),
                })
            }
        }
        (TempMeasurement::DailyLow, TempStrikeKind::Less { threshold }) => {
            if extremes.tmin_f < *threshold {
                Some(decided(
                    true,
                    extremes.tmin_f,
                    format!(
                        "observed low {:.1}F already < threshold {:.1}F",
                        extremes.tmin_f, threshold
                    ),
                ))
            } else {
                Some(ObservedGateDecision::Undecided {
                    observed_f: extremes.tmin_f,
                    reason: format!(
                        "observed low {:.1}F has not fallen below threshold {:.1}F",
                        extremes.tmin_f, threshold
                    ),
                })
            }
        }
        (TempMeasurement::DailyLow, TempStrikeKind::Greater { threshold }) => {
            if extremes.tmin_f <= *threshold {
                Some(decided(
                    false,
                    extremes.tmin_f,
                    format!(
                        "observed low {:.1}F already <= greater-than threshold {:.1}F",
                        extremes.tmin_f, threshold
                    ),
                ))
            } else {
                Some(ObservedGateDecision::Undecided {
                    observed_f: extremes.tmin_f,
                    reason: format!(
                        "observed low {:.1}F remains above greater-than threshold {:.1}F",
                        extremes.tmin_f, threshold
                    ),
                })
            }
        }
        (_, TempStrikeKind::Between { .. }) => None,
    }
}

fn decided(yes_wins: bool, observed_f: f64, reason: String) -> ObservedGateDecision {
    let model_p = if yes_wins { 0.98 } else { 0.02 };
    let side = if yes_wins { Side::Yes } else { Side::No };
    ObservedGateDecision::Decided {
        yes_wins,
        model_p,
        side,
        observed_f,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extremes(tmax_f: f64, tmin_f: f64) -> DailyExtremes {
        DailyExtremes {
            station: "SFO".into(),
            date_utc: "2026-05-07".into(),
            tmax_f,
            tmin_f,
            n_obs: 100,
        }
    }

    fn spec(measurement: TempMeasurement, kind: TempStrikeKind) -> TempMarketSpec {
        TempMarketSpec {
            airport_code: "SFO".into(),
            measurement,
            kind,
            settlement_date: "2026-05-07".into(),
        }
    }

    #[test]
    fn high_less_is_false_once_observed_high_reaches_threshold() {
        let d = decide_from_observed(
            &spec(
                TempMeasurement::DailyHigh,
                TempStrikeKind::Less { threshold: 62.0 },
            ),
            &extremes(64.0, 51.0),
        )
        .unwrap();
        assert_eq!(
            d,
            ObservedGateDecision::Decided {
                yes_wins: false,
                model_p: 0.02,
                side: Side::No,
                observed_f: 64.0,
                reason: "observed high 64.0F already >= less-than threshold 62.0F".into(),
            }
        );
    }

    #[test]
    fn high_greater_is_true_once_observed_high_exceeds_threshold() {
        let d = decide_from_observed(
            &spec(
                TempMeasurement::DailyHigh,
                TempStrikeKind::Greater { threshold: 62.0 },
            ),
            &extremes(64.0, 51.0),
        )
        .unwrap();
        assert!(matches!(
            d,
            ObservedGateDecision::Decided { yes_wins: true, .. }
        ));
    }

    #[test]
    fn low_less_is_true_once_observed_low_falls_below_threshold() {
        let d = decide_from_observed(
            &spec(
                TempMeasurement::DailyLow,
                TempStrikeKind::Less { threshold: 50.0 },
            ),
            &extremes(70.0, 47.0),
        )
        .unwrap();
        assert!(matches!(
            d,
            ObservedGateDecision::Decided { yes_wins: true, .. }
        ));
    }

    #[test]
    fn low_greater_is_false_once_observed_low_reaches_threshold() {
        let d = decide_from_observed(
            &spec(
                TempMeasurement::DailyLow,
                TempStrikeKind::Greater { threshold: 50.0 },
            ),
            &extremes(70.0, 50.0),
        )
        .unwrap();
        assert!(matches!(
            d,
            ObservedGateDecision::Decided {
                yes_wins: false,
                ..
            }
        ));
    }

    #[test]
    fn undecided_when_monotonic_extreme_has_not_crossed() {
        let d = decide_from_observed(
            &spec(
                TempMeasurement::DailyHigh,
                TempStrikeKind::Less { threshold: 62.0 },
            ),
            &extremes(59.0, 51.0),
        )
        .unwrap();
        assert!(matches!(d, ObservedGateDecision::Undecided { .. }));
    }
}
