//! Parse Kalshi temperature-market metadata into the structured
//! shape `wx-stat-curator` actually needs:
//! `(airport_code, strike_kind, threshold(s), settlement_date)`.
//!
//! ## Where the fields come from
//!
//! Kalshi exposes `floor_strike` / `cap_strike` / `strike_type` /
//! `occurrence_datetime` directly in market metadata. For the local
//! settlement day we use the event-ticker date suffix (`26MAY07`),
//! because Kalshi's occurrence timestamp is UTC and can land on the
//! following UTC date for US local-day temperature markets. We do NOT
//! parse the threshold from the ticker
//! letter prefix (`-T68` vs `-B65.5`); the ticker is shape-stable
//! but the meaning of `T` flips between "greater than" and "less
//! than" depending on `strike_type`.
//!
//! What we DO parse from the ticker: the airport code, embedded in
//! the **series ticker** (`KXHIGHDEN` → `DEN`).

use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub enum TempStrikeKind {
    /// YES if observed > `threshold`.
    Greater { threshold: f64 },
    /// YES if observed < `threshold`.
    Less { threshold: f64 },
    /// YES if `lower < observed < upper`. Range markets like
    /// `KXHIGHDEN-26MAY07-B65.5`.
    Between { lower: f64, upper: f64 },
}

/// Parsed Kalshi temperature market.
#[derive(Debug, Clone, PartialEq)]
pub struct TempMarketSpec {
    /// 3-letter airport / location code as it appears in
    /// `airports::Airport::code`.
    pub airport_code: String,
    /// Whether the market is for the daily HIGH or daily LOW.
    pub measurement: TempMeasurement,
    pub kind: TempStrikeKind,
    /// Local-time calendar date the settlement value is observed
    /// on. `YYYY-MM-DD`. Derived from the event-ticker date suffix.
    pub settlement_date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempMeasurement {
    DailyHigh,
    DailyLow,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("ticker {0:?} doesn't match a known temperature-market shape")]
    UnrecognizedTicker(String),
    #[error("strike_type {0:?} not supported by wx-stat (only greater/less/between)")]
    UnsupportedStrikeType(String),
    #[error("missing required strike field for strike_type={0:?}")]
    MissingStrike(String),
    #[error(
        "can't derive settlement date from occurrence_datetime={odt:?} or event_ticker={evt:?}"
    )]
    NoSettlementDate { odt: Option<String>, evt: String },
}

/// Parse a Kalshi temperature market.
///
/// `event_ticker` is the series-date prefix (`"KXHIGHDEN-26MAY07"`).
/// `strike_type` / `floor_strike` / `cap_strike` come from the
/// market's metadata fields (now part of `MarketSummary`).
/// `occurrence_datetime` is the RFC3339 settlement target time; it is
/// only a fallback for malformed legacy event tickers.
pub fn parse_temp_market(
    event_ticker: &str,
    strike_type: Option<&str>,
    floor_strike: Option<f64>,
    cap_strike: Option<f64>,
    occurrence_datetime: Option<&str>,
) -> Result<TempMarketSpec, ParseError> {
    let (measurement, airport_code) = parse_event_ticker(event_ticker)
        .ok_or_else(|| ParseError::UnrecognizedTicker(event_ticker.to_string()))?;

    let strike_type =
        strike_type.ok_or_else(|| ParseError::UnsupportedStrikeType(String::new()))?;
    let kind = match strike_type {
        "greater" => {
            let threshold =
                floor_strike.ok_or_else(|| ParseError::MissingStrike("greater".into()))?;
            TempStrikeKind::Greater { threshold }
        }
        "less" => {
            // Kalshi sets `floor_strike` for less-than markets too —
            // it's the threshold the observed value must be below.
            // (Verified empirically: `KXHIGHDEN-26MAY07-T61` =
            // "<61°" carries `floor_strike=61` not `cap_strike=61`.)
            let threshold = floor_strike
                .or(cap_strike)
                .ok_or_else(|| ParseError::MissingStrike("less".into()))?;
            TempStrikeKind::Less { threshold }
        }
        "between" => {
            let lower = floor_strike.ok_or_else(|| ParseError::MissingStrike("between".into()))?;
            let upper = cap_strike.ok_or_else(|| ParseError::MissingStrike("between".into()))?;
            TempStrikeKind::Between { lower, upper }
        }
        other => return Err(ParseError::UnsupportedStrikeType(other.to_string())),
    };

    let settlement_date =
        derive_settlement_date(occurrence_datetime, event_ticker).ok_or_else(|| {
            ParseError::NoSettlementDate {
                odt: occurrence_datetime.map(str::to_string),
                evt: event_ticker.to_string(),
            }
        })?;

    Ok(TempMarketSpec {
        airport_code,
        measurement,
        kind,
        settlement_date,
    })
}

// Order matters: longer prefixes first so `KXHIGHT` doesn't match
// `KXHIGH` and leave `T...` as the location code.
static PREFIXES: LazyLock<Vec<(&'static str, TempMeasurement)>> = LazyLock::new(|| {
    vec![
        ("KXHIGHT", TempMeasurement::DailyHigh),
        ("KXLOWT", TempMeasurement::DailyLow),
        ("KXHIGH", TempMeasurement::DailyHigh),
        ("KXLOW", TempMeasurement::DailyLow),
    ]
});

/// Pull (measurement, airport_code) from a series-date event ticker
/// like `"KXHIGHDEN-26MAY07"` → `(DailyHigh, "DEN")`.
fn parse_event_ticker(event_ticker: &str) -> Option<(TempMeasurement, String)> {
    let series = event_ticker.split('-').next()?;
    for (prefix, measurement) in PREFIXES.iter() {
        if let Some(rest) = series.strip_prefix(prefix) {
            // Airport code is variable-length: 2 letters for some
            // (KXLOWNY = NYC), 3 for most (KXHIGHDEN = DEN), 4 for
            // a few (KXHIGHPHIL = Philadelphia, KXLOWTNOLA = New
            // Orleans). Cap at 5 to bound exotic cases.
            let n = rest.len();
            if (2..=5).contains(&n) {
                let code = rest.to_ascii_uppercase();
                if code.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Some((*measurement, code));
                }
            }
        }
    }
    None
}

/// Derive the local calendar date the market settles on.
///
/// Preference order: event-ticker date suffix `26MAY07` →
/// `occurrence_datetime` fallback → fail.
fn derive_settlement_date(occurrence_datetime: Option<&str>, event_ticker: &str) -> Option<String> {
    if let Some(suffix) = event_ticker.split('-').nth(1)
        && let Some(date) = parse_yymmmdd_to_iso(suffix)
    {
        return Some(date);
    }
    if let Some(odt) = occurrence_datetime {
        // RFC3339, take the date part. `2026-05-07T14:00:00Z` →
        // `2026-05-07`. This fallback is not used for canonical
        // temperature event tickers because UTC can cross the local
        // settlement-day boundary.
        if let Some(date) = odt.get(..10)
            && date.len() == 10
            && date.chars().nth(4) == Some('-')
        {
            return Some(date.to_string());
        }
    }
    None
}

/// `"26MAY07"` → `"2026-05-07"`. Returns None on shape mismatch.
fn parse_yymmmdd_to_iso(s: &str) -> Option<String> {
    if s.len() != 7 {
        return None;
    }
    let yy: u32 = s.get(..2)?.parse().ok()?;
    let mon = s.get(2..5)?;
    let dd: u32 = s.get(5..7)?.parse().ok()?;
    let mm = match mon.to_ascii_uppercase().as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    };
    if !(1..=31).contains(&dd) {
        return None;
    }
    // Two-digit year: assume 20xx (2000-2099). Kalshi has been
    // around since 2021, so anything < 70 is post-2000; >= 70 would
    // be 1970..2000 which we don't expect. Be strict.
    let year = 2000 + yy;
    Some(format!("{year:04}-{mm:02}-{dd:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_greater_than_high_temp_market() {
        let spec = parse_temp_market(
            "KXHIGHDEN-26MAY07",
            Some("greater"),
            Some(68.0),
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(spec.airport_code, "DEN");
        assert_eq!(spec.measurement, TempMeasurement::DailyHigh);
        assert_eq!(spec.kind, TempStrikeKind::Greater { threshold: 68.0 });
        assert_eq!(spec.settlement_date, "2026-05-07");
    }

    #[test]
    fn parses_less_than_high_temp_market() {
        // Empirically Kalshi puts the threshold in floor_strike for
        // both >X and <X markets. Verify the parser respects that.
        let spec = parse_temp_market(
            "KXHIGHDEN-26MAY07",
            Some("less"),
            Some(61.0),
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(spec.kind, TempStrikeKind::Less { threshold: 61.0 });
    }

    #[test]
    fn parses_between_range_market() {
        let spec = parse_temp_market(
            "KXHIGHDEN-26MAY07",
            Some("between"),
            Some(65.5),
            Some(66.5),
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            spec.kind,
            TempStrikeKind::Between {
                lower: 65.5,
                upper: 66.5
            }
        );
    }

    #[test]
    fn parses_low_temp_market() {
        let spec = parse_temp_market(
            "KXLOWNY-26MAY07",
            Some("less"),
            Some(50.0),
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(spec.airport_code, "NY");
        assert_eq!(spec.measurement, TempMeasurement::DailyLow);
    }

    #[test]
    fn falls_back_to_event_ticker_date_when_odt_missing() {
        let spec = parse_temp_market("KXHIGHDEN-26MAY07", Some("greater"), Some(68.0), None, None)
            .unwrap();
        assert_eq!(spec.settlement_date, "2026-05-07");
    }

    #[test]
    fn rejects_unsupported_strike_type() {
        let err = parse_temp_market(
            "KXHIGHDEN-26MAY07",
            Some("functional"),
            None,
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap_err();
        matches!(err, ParseError::UnsupportedStrikeType(_));
    }

    #[test]
    fn rejects_missing_strike() {
        let err = parse_temp_market(
            "KXHIGHDEN-26MAY07",
            Some("greater"),
            None, // missing
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap_err();
        matches!(err, ParseError::MissingStrike(_));
    }

    #[test]
    fn rejects_unrecognized_ticker_shape() {
        let err = parse_temp_market(
            "WEIRDSERIES-26MAY07",
            Some("greater"),
            Some(68.0),
            None,
            None,
        )
        .unwrap_err();
        matches!(err, ParseError::UnrecognizedTicker(_));
    }

    #[test]
    fn rejects_unparseable_ticker_date_when_odt_missing() {
        let err = parse_temp_market("KXHIGHDEN-FOOBAR", Some("greater"), Some(68.0), None, None)
            .unwrap_err();
        matches!(err, ParseError::NoSettlementDate { .. });
    }

    #[test]
    fn parses_4_letter_location_codes() {
        // Kalshi uses 4-letter codes for some cities
        // (KXHIGHPHIL → Philadelphia, KXLOWTNOLA → New Orleans).
        // Regression: the original 2-or-3 length cap silently
        // skipped these as parse errors instead of letting the
        // airport-lookup decide.
        let spec = parse_temp_market(
            "KXHIGHPHIL-26MAY07",
            Some("greater"),
            Some(75.0),
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(spec.airport_code, "PHIL");
        assert_eq!(spec.measurement, TempMeasurement::DailyHigh);

        let spec2 = parse_temp_market(
            "KXLOWTNOLA-26MAY07",
            Some("less"),
            Some(72.0),
            None,
            Some("2026-05-07T14:00:00Z"),
        )
        .unwrap();
        assert_eq!(spec2.airport_code, "NOLA");
        assert_eq!(spec2.measurement, TempMeasurement::DailyLow);
    }

    #[test]
    fn yymmmdd_round_trips() {
        assert_eq!(
            parse_yymmmdd_to_iso("26MAY07").as_deref(),
            Some("2026-05-07")
        );
        assert_eq!(
            parse_yymmmdd_to_iso("25DEC25").as_deref(),
            Some("2025-12-25")
        );
        assert_eq!(parse_yymmmdd_to_iso("26ZZZ07"), None);
        assert_eq!(parse_yymmmdd_to_iso("26MAY"), None);
    }
}
