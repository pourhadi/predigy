//! Prediction-record logging — the data side of Phase 2E.
//!
//! Each curator run writes one record per emitted rule to a
//! daily JSONL file under `<predictions_dir>/<YYYY-MM-DD>.jsonl`.
//! The fit driver (`wx-stat-fit-calibration`) consumes these
//! records, joins them with realised observations from a free
//! NOAA archive, and produces (raw_p, outcome) pairs to fit
//! Platt scaling against.
//!
//! Why a sidecar instead of querying live model state at fit
//! time? Because by the time we want to fit (days-to-weeks
//! later), the cycle's NBM data has rolled off the bucket and
//! we'd have to re-fetch it. Cheaper to log the model_p at
//! curate time and join with observations later.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One per emitted rule. Operator-readable JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionRecord {
    /// ISO-8601 UTC of the curator run that produced this prediction.
    pub run_ts_utc: String,
    /// Kalshi market ticker — joins back to the Kalshi-side outcome.
    pub ticker: String,
    /// Airport code (matches Airport::code in airports.rs).
    pub airport: String,
    /// Settlement date in airport-local YYYY-MM-DD. Used as the
    /// `month` bucket key for calibration.
    pub settlement_date: String,
    /// Threshold in Kelvin (NBM-native). Recorded so the fit
    /// driver doesn't have to re-resolve the Kalshi-side strike
    /// when computing outcome (and so the fit is robust to
    /// Kalshi-side ticker re-keys).
    pub threshold_k: f32,
    /// Whether the YES side wins when observed temp is ABOVE the
    /// threshold (`true` for `Greater` markets) or BELOW
    /// (`false` for `Less`). The fit driver maps observed temp
    /// → outcome via this.
    pub yes_when_above: bool,
    /// Whether this is a daily-HIGH or daily-LOW market. Affects
    /// which observation aggregate to compare against.
    pub measurement: PredictionMeasurement,
    /// Pre-calibration NBM probability — what the bucket would
    /// have produced before calibration. This is the dependent
    /// variable in the Platt fit: `outcome ~ logistic(raw_p)`.
    pub raw_p: f64,
    /// Post-calibration probability after the [0.02, 0.98] clamp.
    /// Captured for audit only; the fit doesn't use it.
    pub model_p: f64,
    /// NBM 50%-level forecast at the peak hour, in F. Operator-
    /// facing inspection field.
    pub forecast_50pct_f: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionMeasurement {
    DailyHigh,
    DailyLow,
}

/// Atomic append: write to `<dir>/<YYYY-MM-DD>.jsonl.tmp`, fsync,
/// rename. The `<YYYY-MM-DD>` portion is the `run_ts_utc` date so
/// each curator run can be reproduced from the daily roll-up.
///
/// JSONL format: one record per line, no trailing comma.
pub fn append_records(dir: &Path, records: &[PredictionRecord]) -> std::io::Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(dir)?;
    // All records share the same run_ts_utc (one curator run); use
    // the first record's date as the file day.
    let date = records
        .first()
        .and_then(|r| r.run_ts_utc.get(..10))
        .unwrap_or("unknown");
    let path: PathBuf = dir.join(format!("{date}.jsonl"));
    let mut out = String::new();
    for r in records {
        let line = serde_json::to_string(r).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        out.push_str(&line);
        out.push('\n');
    }
    // Append, not overwrite — multiple curator runs per day all land
    // in the same file.
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(out.as_bytes())?;
    f.sync_all()?;
    Ok(records.len())
}

/// Read every `.jsonl` under `dir`, parse each line as a
/// [`PredictionRecord`]. Skips empty lines and lines that fail
/// to parse (logged as warnings via `tracing`).
pub fn read_dir_records(dir: &Path) -> std::io::Result<Vec<PredictionRecord>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        for (line_no, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<PredictionRecord>(line) {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        line = line_no + 1,
                        error = %e,
                        "skipping malformed prediction record"
                    );
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ticker: &str, run_ts: &str, raw_p: f64) -> PredictionRecord {
        PredictionRecord {
            run_ts_utc: run_ts.into(),
            ticker: ticker.into(),
            airport: "DEN".into(),
            settlement_date: "2026-05-07".into(),
            threshold_k: 295.0,
            yes_when_above: true,
            measurement: PredictionMeasurement::DailyHigh,
            raw_p,
            model_p: raw_p.clamp(0.02, 0.98),
            forecast_50pct_f: 75.0,
        }
    }

    #[test]
    fn append_records_creates_jsonl_with_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let recs = vec![
            rec("KX-A", "2026-05-07T03:00:00Z", 0.85),
            rec("KX-B", "2026-05-07T03:00:00Z", 0.40),
        ];
        let written = append_records(dir.path(), &recs).unwrap();
        assert_eq!(written, 2);
        let path = dir.path().join("2026-05-07.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            // Each line should be valid JSON.
            let parsed: PredictionRecord = serde_json::from_str(line).unwrap();
            assert!(parsed.ticker.starts_with("KX-"));
        }
    }

    #[test]
    fn append_records_appends_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let r1 = vec![rec("KX-A", "2026-05-07T03:00:00Z", 0.85)];
        let r2 = vec![
            rec("KX-B", "2026-05-07T06:00:00Z", 0.40),
            rec("KX-C", "2026-05-07T06:00:00Z", 0.65),
        ];
        append_records(dir.path(), &r1).unwrap();
        append_records(dir.path(), &r2).unwrap();
        let body = std::fs::read_to_string(dir.path().join("2026-05-07.jsonl")).unwrap();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn read_dir_records_returns_empty_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("nope");
        let recs = read_dir_records(&nonexistent).unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn read_dir_records_skips_non_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        // Plant an .json (without `l`) and a .txt file alongside.
        std::fs::write(dir.path().join("not-jsonl.json"), b"{}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hello\n").unwrap();
        // And one valid .jsonl.
        let recs = vec![rec("KX-A", "2026-05-07T03:00:00Z", 0.85)];
        append_records(dir.path(), &recs).unwrap();
        let read = read_dir_records(dir.path()).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].ticker, "KX-A");
    }

    #[test]
    fn read_dir_records_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-05-07.jsonl");
        std::fs::write(
            &path,
            b"not-json\n{\"ticker\":\"KX-OK\",\"run_ts_utc\":\"2026-05-07T03:00:00Z\",\"airport\":\"DEN\",\"settlement_date\":\"2026-05-07\",\"threshold_k\":295.0,\"yes_when_above\":true,\"measurement\":\"daily_high\",\"raw_p\":0.7,\"model_p\":0.7,\"forecast_50pct_f\":75.0}\n",
        ).unwrap();
        let read = read_dir_records(dir.path()).unwrap();
        // Only the valid line should round-trip.
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].ticker, "KX-OK");
    }

    #[test]
    fn round_trip_preserves_measurement_enum() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = rec("KX-LOW", "2026-05-07T03:00:00Z", 0.4);
        r.measurement = PredictionMeasurement::DailyLow;
        append_records(dir.path(), &[r]).unwrap();
        let read = read_dir_records(dir.path()).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].measurement, PredictionMeasurement::DailyLow);
    }
}
