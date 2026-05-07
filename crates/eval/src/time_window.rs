//! Time window parsing — the operator-facing shorthand
//! `1h / 24h / 7d / 30d / all` plus RFC3339 endpoints.

use chrono::{DateTime, Duration, Utc};

/// Right-open window `[start, end)`. `end` defaults to now.
#[derive(Debug, Clone, Copy)]
pub struct TimeWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl TimeWindow {
    #[must_use]
    pub fn last(duration: Duration) -> Self {
        let end = Utc::now();
        Self {
            start: end - duration,
            end,
        }
    }

    /// "All time" — Kalshi launched in 2021. Start at 2020 to
    /// be safe; end at now.
    #[must_use]
    pub fn all() -> Self {
        Self {
            start: chrono::DateTime::from_naive_utc_and_offset(
                chrono::NaiveDate::from_ymd_opt(2020, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
                Utc,
            ),
            end: Utc::now(),
        }
    }

    /// Parse the operator shorthand. Recognizes:
    ///   `1h`, `24h`, `7d`, `30d`, `all`
    /// or any RFC3339 timestamp pair separated by `..` (e.g.
    /// `2026-05-01T00:00:00Z..2026-05-07T00:00:00Z`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "all" {
            return Ok(Self::all());
        }
        // Range like `start..end`.
        if let Some((a, b)) = s.split_once("..") {
            let start = a
                .parse::<DateTime<Utc>>()
                .map_err(|e| format!("parse start `{a}`: {e}"))?;
            let end = b
                .parse::<DateTime<Utc>>()
                .map_err(|e| format!("parse end `{b}`: {e}"))?;
            return Ok(Self { start, end });
        }
        // Bare duration: `Nh` / `Nd` / `Nm`.
        let (digits, unit) = s.split_at(s.len() - 1);
        let n: i64 = digits
            .parse()
            .map_err(|e| format!("parse duration `{s}`: {e}"))?;
        let dur = match unit {
            "h" => Duration::hours(n),
            "d" => Duration::days(n),
            "m" => Duration::minutes(n),
            other => return Err(format!("unknown unit `{other}`; want h/d/m or `all`")),
        };
        Ok(Self::last(dur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_24h() {
        let w = TimeWindow::parse("24h").unwrap();
        let dur = w.end - w.start;
        assert_eq!(dur.num_hours(), 24);
    }

    #[test]
    fn parses_7d() {
        let w = TimeWindow::parse("7d").unwrap();
        assert_eq!((w.end - w.start).num_days(), 7);
    }

    #[test]
    fn parses_all() {
        let w = TimeWindow::parse("all").unwrap();
        // Sanity — start is well before now.
        assert!((w.end - w.start).num_days() > 365);
    }

    #[test]
    fn parses_range() {
        let w = TimeWindow::parse("2026-05-01T00:00:00Z..2026-05-07T00:00:00Z").unwrap();
        assert_eq!((w.end - w.start).num_days(), 6);
    }

    #[test]
    fn rejects_bad_unit() {
        assert!(TimeWindow::parse("5x").is_err());
    }
}
