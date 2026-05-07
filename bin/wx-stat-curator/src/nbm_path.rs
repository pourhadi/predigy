//! Phase 2 NBM path for `wx-stat-curator`.
//!
//! Replaces Phase 1's deterministic point forecast + conviction-zone
//! gate with NBM probabilistic quantile data and CDF interpolation
//! at the Kalshi-side threshold.
//!
//! ## Core math
//!
//! For a Kalshi daily-high market `(>X)` settling on local-date D
//! at airport A:
//!
//! 1. Pick NBM cycle C = most recent 6-hourly cycle that has
//!    published f024 (~7h before now).
//! 2. Compute A's UTC offset from longitude (sufficient for
//!    deciding which forecast hours cover A's local daytime).
//! 3. The forecast-hour *window* covering A's local 14:00–23:00
//!    on date D — that's the hourly span in which the daily max
//!    typically realises.
//! 4. For each hour h in the window, NBM gives 21 quantile
//!    temperatures. Compute `P_h(T > X)` by interpolating the
//!    quantile CDF.
//! 5. `model_p = max_h P_h(T > X)`.
//!
//! Why max? The "any hour above" semantics: P(daily_max > X) =
//! P(∃h: T_h > X). Under independence the right answer is
//! `1 - ∏(1 - P_h(T > X))`; under perfect correlation across
//! hours it's `max P_h(T > X)`. Real hourly temperatures are
//! strongly correlated, so max is the better approximation —
//! and conservative (smaller probability than independence
//! assumption gives).
//!
//! For daily-low markets `(<X)` the symmetric calculation:
//! `model_p = max_h P_h(T < X)`.

use predigy_ext_feeds::nbm::NbmCycle;

/// Local-time window during which the daily HIGH temp typically
/// realises. Pacific to Atlantic, this spans 14:00–23:00 local
/// for most US locations in most seasons.
pub const DAILY_HIGH_LOCAL_HOURS: std::ops::RangeInclusive<u8> = 14..=23;

/// Local-time window during which the daily LOW temp typically
/// realises. Often dawn-adjacent: 02:00–08:00 local.
pub const DAILY_LOW_LOCAL_HOURS: std::ops::RangeInclusive<u8> = 2..=8;

/// Round-down to the most recent 6h NBM cycle boundary that is
/// almost certainly fully published. NBM **qmd** publishes only at
/// 00/06/12/18 UTC, and the far-out forecast hours (f036, f048,
/// f060) for cycle CC don't appear until ~8-10h after CC. Using a
/// 13h lookback guarantees we land in the *previous* 6h cycle from
/// "now" — fully published for the entire 168h horizon.
///
/// Verified empirically 2026-05-07: at 02:17 UTC, today's 12Z
/// cycle has all f024+ hours published; today's 18Z cycle (only
/// ~8h old) returns 404 for f024. With a 7h lookback we'd
/// incorrectly pick 18Z; 13h lookback correctly picks 12Z.
pub fn recent_qmd_cycle(now_unix: i64) -> NbmCycle {
    let lookback_secs: i64 = 13 * 3600;
    let target = now_unix - lookback_secs;
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    let secs = target.max(0) as u64;
    let days = (secs / 86_400) as i64;
    let raw_hour = u8::try_from((secs / 3600) % 24).unwrap_or(0);
    let cycle_hour = (raw_hour / 6) * 6;
    let (year, month, day) = days_to_ymd(days);
    NbmCycle {
        year,
        month,
        day,
        hour: cycle_hour,
    }
}

/// Compute an airport's UTC offset (hours, signed) from longitude.
/// `lon ÷ 15.0` rounded to nearest hour — accurate enough for
/// deciding forecast-hour windows. Doesn't model DST: the daily
/// max/min windows are robust to ±1h DST drift.
pub fn approx_utc_offset_hours(lon_deg: f64) -> i32 {
    let raw = lon_deg / 15.0;
    raw.round() as i32
}

/// Forecast hours from `cycle` that cover the local-time window
/// `local_hours` on local date `target_date_iso` (`YYYY-MM-DD`)
/// at airport with UTC offset `utc_offset_hours`.
///
/// Returns the (start, end) inclusive forecast-hour offsets from
/// the cycle's start, suitable for stepping `for h in start..=end`.
/// Returns `None` if the local-date window is in the past relative
/// to the cycle (so we never request f000 or negative offsets) or
/// outside NBM's typical 168h horizon.
pub fn forecast_hour_window(
    cycle: NbmCycle,
    target_date_iso: &str,
    local_hours: std::ops::RangeInclusive<u8>,
    utc_offset_hours: i32,
) -> Option<(u16, u16)> {
    // Derive cycle start as a unix epoch, then derive target date's
    // local-window start/end as unix epochs, take the offset.
    let cycle_start_unix = cycle_start_unix(cycle)?;

    let target_unix_at_local_zero = parse_iso_date_to_unix_utc(target_date_iso)?;
    // Local hour H at airport with offset O happens at UTC hour
    // (H - O) on the same local date — convert to unix.
    let lo_local = i64::from(*local_hours.start());
    let hi_local = i64::from(*local_hours.end());
    let lo_utc_offset_secs = (lo_local - i64::from(utc_offset_hours)) * 3600;
    let hi_utc_offset_secs = (hi_local - i64::from(utc_offset_hours)) * 3600;
    let window_start_unix = target_unix_at_local_zero + lo_utc_offset_secs;
    let window_end_unix = target_unix_at_local_zero + hi_utc_offset_secs;

    if window_end_unix <= cycle_start_unix {
        // The whole window has already occurred; can't forecast.
        return None;
    }

    let start_secs_from_cycle = (window_start_unix - cycle_start_unix).max(0);
    let end_secs_from_cycle = (window_end_unix - cycle_start_unix).max(0);
    // Round each to integer hour offsets. NBM qmd has hourly
    // granularity for the first 36 hours then 3-hourly out to 60+;
    // we round-to-nearest and let the caller skip missing hours.
    let start_h = u16::try_from(start_secs_from_cycle.div_euclid(3600)).ok()?;
    let end_h = u16::try_from(end_secs_from_cycle.div_euclid(3600)).ok()?;
    if end_h > 168 {
        // Beyond NBM forecast horizon.
        return None;
    }
    if start_h == 0 {
        // f000 is the analysis cycle itself; NBM may not have
        // quantiles published. Skip start_h = 0; caller iterates.
    }
    Some((start_h, end_h))
}

/// Convert unix-epoch days to (year, month, day) UTC, no chrono.
fn days_to_ymd(days: i64) -> (u16, u8, u8) {
    let mut d = days;
    let mut year: u16 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        year += 1;
    }
    let mut month: u8 = 1;
    loop {
        let dm: i64 = days_in_month(year, month).into();
        if d < dm {
            break;
        }
        d -= dm;
        month += 1;
    }
    let day = u8::try_from(d + 1).unwrap_or(1);
    (year, month, day)
}

fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn days_in_month(y: u16, m: u8) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// `(year, month, day, hour)` → unix seconds (UTC).
fn unix_utc(year: u16, month: u8, day: u8, hour: u8) -> Option<i64> {
    if !(1970..=2100).contains(&year) {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    let dim = days_in_month(year, month);
    if day == 0 || u32::from(day) > dim {
        return None;
    }
    if hour > 23 {
        return None;
    }
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += i64::from(days_in_month(year, m));
    }
    days += i64::from(day - 1);
    let secs = days * 86_400 + i64::from(hour) * 3600;
    Some(secs)
}

fn cycle_start_unix(cycle: NbmCycle) -> Option<i64> {
    unix_utc(cycle.year, cycle.month, cycle.day, cycle.hour)
}

fn parse_iso_date_to_unix_utc(s: &str) -> Option<i64> {
    // YYYY-MM-DD only.
    let mut parts = s.splitn(3, '-');
    let year: u16 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    unix_utc(year, month, day, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_offset_denver_is_minus_seven() {
        // Denver longitude -104.67 → -6.98 → -7 (MST).
        assert_eq!(approx_utc_offset_hours(-104.67), -7);
    }

    #[test]
    fn approx_offset_los_angeles_is_minus_eight() {
        assert_eq!(approx_utc_offset_hours(-118.41), -8);
    }

    #[test]
    fn approx_offset_new_york_is_minus_five() {
        assert_eq!(approx_utc_offset_hours(-73.88), -5);
    }

    #[test]
    fn parse_iso_date_handles_canonical_format() {
        let u = parse_iso_date_to_unix_utc("2026-05-07").unwrap();
        // 2026-05-07 00:00:00 UTC = 1778112000.
        assert_eq!(u, 1_778_112_000);
    }

    #[test]
    fn forecast_window_for_denver_high_from_12z_cycle() {
        // Cycle 12Z 2026-05-06. Target: Denver 2026-05-07 daily
        // high (local 14-23 = UTC 21-06+1). Cycle start = May 6
        // 12Z. Window start = 2026-05-07 21Z = 33h after cycle.
        // Window end = 2026-05-08 06Z = 42h after cycle.
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 12,
        };
        let (start_h, end_h) =
            forecast_hour_window(cycle, "2026-05-07", DAILY_HIGH_LOCAL_HOURS, -7).unwrap();
        assert_eq!(start_h, 33);
        assert_eq!(end_h, 42);
    }

    #[test]
    fn forecast_window_for_la_high_from_06z_cycle() {
        // Cycle 06Z 2026-05-06. Target: LA 2026-05-07 daily high
        // (local 14-23 = UTC 22-07+1, since LA is UTC-8).
        // Cycle start = May 6 06Z. Window start = May 7 22Z =
        // 40h. Window end = May 8 07Z = 49h.
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 6,
        };
        let (start_h, end_h) =
            forecast_hour_window(cycle, "2026-05-07", DAILY_HIGH_LOCAL_HOURS, -8).unwrap();
        assert_eq!(start_h, 40);
        assert_eq!(end_h, 49);
    }

    #[test]
    fn forecast_window_returns_none_for_past_window() {
        // Cycle 18Z 2026-05-08, asking for May 6 high → in past.
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 8,
            hour: 18,
        };
        assert!(forecast_hour_window(cycle, "2026-05-06", DAILY_HIGH_LOCAL_HOURS, -7).is_none());
    }

    #[test]
    fn recent_qmd_cycle_rounds_to_six_hour_boundary() {
        // 2026-05-06 23:54 UTC → minus 13h = 10:54 → round down
        // to 06Z on May 6.
        let now = unix_utc(2026, 5, 6, 23).unwrap() + 54 * 60;
        let c = recent_qmd_cycle(now);
        assert_eq!(c.year, 2026);
        assert_eq!(c.month, 5);
        assert_eq!(c.day, 6);
        assert_eq!(c.hour, 6);
    }

    #[test]
    fn recent_qmd_cycle_rolls_over_day_boundary() {
        // 2026-05-07 04:00 UTC → minus 13h = 2026-05-06 15:00 →
        // round down to 12Z on May 6.
        let now = unix_utc(2026, 5, 7, 4).unwrap();
        let c = recent_qmd_cycle(now);
        assert_eq!(c.year, 2026);
        assert_eq!(c.month, 5);
        assert_eq!(c.day, 6);
        assert_eq!(c.hour, 12);
    }
}
