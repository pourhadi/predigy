//! Historical observed-temperature fetcher — Iowa State Mesonet
//! ASOS archive, free, no auth.
//!
//! Endpoint: `mesonet.agron.iastate.edu/cgi-bin/request/asos.py`
//! returns minute-resolution surface observations for any ASOS
//! station and date range, in CSV form. We slice it to one
//! station-day at a time and compute the daily maximum / minimum
//! `tmpf` value. Live trading uses a local-day window derived from
//! the airport UTC offset; calibration backfills can still use UTC-day
//! aggregates where exact local settlement semantics are not required.
//!
//! ## Disk cache
//!
//! Observations don't change once they've been recorded, so we
//! cache the daily extreme per (station, date) under
//! `<cache_root>/asos/<station>/<YYYY-MM-DD>.json`. The fit
//! binary's second pass over a backfill window is therefore zero
//! network — exactly the same shape as the NBM cache.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const ENDPOINT: &str = "https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py";

/// Iowa Mesonet rate-limits aggressively; back-off after a 429.
/// 2 seconds between requests has empirically been enough to
/// avoid being blocked over a multi-hundred-request backfill.
const MIN_REQ_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RETRIES_ON_429: u32 = 4;

#[derive(Debug, Clone)]
pub struct AsosClient {
    http: reqwest::Client,
    endpoint: String,
    /// Last-request-time gate so concurrent + rapid-sequential
    /// callers all serialise through the throttle.
    last_request: Arc<Mutex<Instant>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AsosError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("no observations for station {station} on {date}")]
    NoObservations { station: String, date: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// One day's observation summary at one station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyExtremes {
    pub station: String,
    /// Date label the observations were aggregated over. Historical
    /// calibration paths use UTC dates; live trading uses the market-local
    /// Kalshi settlement date.
    pub date_utc: String,
    /// Maximum observed temperature in °F (the same unit Iowa
    /// Mesonet's `tmpf` returns).
    pub tmax_f: f64,
    /// Minimum observed temperature in °F.
    pub tmin_f: f64,
    /// Number of valid (station, time, tmpf) rows the aggregate
    /// was built from. <30 means a sparse day; the fit binary may
    /// drop those.
    pub n_obs: u32,
}

impl AsosClient {
    pub fn new(user_agent: &str) -> Result<Self, AsosError> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent.to_string())
            .timeout(Duration::from_mins(2))
            .build()?;
        // Initialise the last-request gate to the past so the
        // first call doesn't sleep.
        let past = Instant::now()
            .checked_sub(MIN_REQ_INTERVAL * 2)
            .unwrap_or_else(Instant::now);
        Ok(Self {
            http,
            endpoint: ENDPOINT.to_string(),
            last_request: Arc::new(Mutex::new(past)),
        })
    }

    /// Override endpoint for testing against a local mock.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Pull `(date_utc, station)` daily extremes from cache, or
    /// fetch + parse + persist if cache miss.
    pub async fn fetch_daily_extremes(
        &self,
        cache_root: &Path,
        station: &str,
        date_utc: &str,
    ) -> Result<DailyExtremes, AsosError> {
        let cache_path = build_cache_path(cache_root, station, date_utc);
        if let Ok(bytes) = std::fs::read(&cache_path) {
            if let Ok(cached) = serde_json::from_slice::<DailyExtremes>(&bytes) {
                debug!(?cache_path, station, date_utc, "asos: cache hit");
                return Ok(cached);
            }
        }
        let csv = self.fetch_csv(station, date_utc).await?;
        let extremes = parse_csv_daily_extremes(&csv, station, date_utc)?;
        // Persist atomically.
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&extremes)?;
        let tmp = cache_path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &cache_path)?;
        Ok(extremes)
    }

    /// Pull observed extremes for a market-local date. `utc_offset_hours`
    /// is the airport's local offset approximation (negative in the US).
    /// Current-day callers should set `use_cache=false` so partial same-day
    /// observations refresh instead of reusing stale early-day extrema.
    pub async fn fetch_local_day_extremes(
        &self,
        cache_root: &Path,
        station: &str,
        local_date: &str,
        utc_offset_hours: i32,
        use_cache: bool,
    ) -> Result<DailyExtremes, AsosError> {
        let cache_path = build_local_cache_path(cache_root, station, local_date, utc_offset_hours);
        if use_cache && let Ok(bytes) = std::fs::read(&cache_path) {
            if let Ok(cached) = serde_json::from_slice::<DailyExtremes>(&bytes) {
                debug!(
                    ?cache_path,
                    station, local_date, utc_offset_hours, "asos: local cache hit"
                );
                return Ok(cached);
            }
        }

        let local_start = parse_iso_date_to_unix(local_date).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad date format: {local_date}"),
        })?;
        let start_utc = local_start - i64::from(utc_offset_hours) * 3600;
        let end_utc = start_utc + 24 * 3600;
        let start_date = unix_to_iso_date(start_utc).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad local start for {local_date}"),
        })?;
        let end_date = unix_to_iso_date(end_utc - 1).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad local end for {local_date}"),
        })?;
        let csv = self
            .fetch_csv_range(station, &start_date, &end_date)
            .await?;
        let extremes = parse_csv_extremes_between(&csv, station, local_date, start_utc, end_utc)?;

        if use_cache {
            if let Some(parent) = cache_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_vec_pretty(&extremes)?;
            let tmp = cache_path.with_extension("tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, &cache_path)?;
        }
        Ok(extremes)
    }

    async fn fetch_csv(&self, station: &str, date_utc: &str) -> Result<String, AsosError> {
        split_iso_date(date_utc).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad date format: {date_utc}"),
        })?;
        self.fetch_csv_range(station, date_utc, date_utc).await
    }

    async fn fetch_csv_range(
        &self,
        station: &str,
        start_date_utc: &str,
        end_date_utc: &str,
    ) -> Result<String, AsosError> {
        let (y1, m1, d1) = split_iso_date(start_date_utc).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad date format: {start_date_utc}"),
        })?;
        let (y2, m2, d2) = split_iso_date(end_date_utc).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad date format: {end_date_utc}"),
        })?;
        let url = format!(
            "{base}?station={station}&data=tmpf&year1={y1}&month1={m1}&day1={d1}&year2={y2}&month2={m2}&day2={d2}&tz=Etc%2FUTC&format=onlycomma&latlon=no&missing=empty&trace=empty",
            base = self.endpoint,
        );
        debug!(%url, "asos: fetch_csv");

        // Retry-on-429 with exponential backoff. Each attempt
        // serialises through the throttle gate so we never burst
        // multiple requests within MIN_REQ_INTERVAL.
        let mut backoff = MIN_REQ_INTERVAL;
        for attempt in 0..=MAX_RETRIES_ON_429 {
            self.throttle().await;
            let resp = self.http.get(&url).send().await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.text().await?);
            }
            if status.as_u16() == 429 && attempt < MAX_RETRIES_ON_429 {
                warn!(
                    station,
                    start_date = %start_date_utc,
                    end_date = %end_date_utc,
                    attempt = attempt + 1,
                    backoff_ms = backoff.as_millis(),
                    "asos: 429; backing off"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(AsosError::Api {
                status: status.as_u16(),
                body,
            });
        }
        // Exhausted retries.
        Err(AsosError::Api {
            status: 429,
            body: "asos rate-limit exhausted retries".into(),
        })
    }

    /// Block until at least [`MIN_REQ_INTERVAL`] has elapsed since
    /// the last request, then mark "now" as the new last-request
    /// time. Serialised across concurrent callers via the mutex so
    /// many tasks all behave like a single polite consumer.
    async fn throttle(&self) {
        let mut guard = self.last_request.lock().await;
        let elapsed = guard.elapsed();
        if elapsed < MIN_REQ_INTERVAL {
            tokio::time::sleep(MIN_REQ_INTERVAL - elapsed).await;
        }
        *guard = Instant::now();
    }
}

fn build_cache_path(cache_root: &Path, station: &str, date_utc: &str) -> PathBuf {
    let mut p = cache_root.to_path_buf();
    p.push("asos");
    p.push(station);
    p.push(format!("{date_utc}.json"));
    p
}

fn build_local_cache_path(
    cache_root: &Path,
    station: &str,
    local_date: &str,
    utc_offset_hours: i32,
) -> PathBuf {
    let mut p = cache_root.to_path_buf();
    p.push("asos_local");
    p.push(station);
    p.push(format!("{local_date}_utc{utc_offset_hours:+}.json"));
    p
}

fn split_iso_date(s: &str) -> Option<(u16, u8, u8)> {
    let mut parts = s.splitn(3, '-');
    let y: u16 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// Parse the Iowa Mesonet `format=onlycomma` CSV. Header row
/// looks like `station,valid,tmpf`. We compute max/min over the
/// `tmpf` column and skip rows whose value is empty or
/// non-numeric (Iowa Mesonet uses empty for missing rather than
/// `M` when `missing=empty` is in the URL).
pub fn parse_csv_daily_extremes(
    csv: &str,
    station: &str,
    date_utc: &str,
) -> Result<DailyExtremes, AsosError> {
    parse_csv_extremes(csv, station, date_utc, None)
}

pub fn parse_csv_extremes_between(
    csv: &str,
    station: &str,
    date_label: &str,
    start_unix: i64,
    end_unix: i64,
) -> Result<DailyExtremes, AsosError> {
    parse_csv_extremes(csv, station, date_label, Some((start_unix, end_unix)))
}

fn parse_csv_extremes(
    csv: &str,
    station: &str,
    date_label: &str,
    window_unix: Option<(i64, i64)>,
) -> Result<DailyExtremes, AsosError> {
    let mut tmax = f64::MIN;
    let mut tmin = f64::MAX;
    let mut n: u32 = 0;
    let mut header_seen = false;
    for raw in csv.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Skip header.
        if !header_seen {
            header_seen = true;
            // First non-empty line is the header; some Mesonet
            // responses also start with a comment line beginning
            // with `#`. Continue past comment-style lines.
            if line.starts_with('#') {
                header_seen = false;
            }
            continue;
        }
        // CSV columns: station,valid,tmpf
        let mut cols = line.split(',');
        let _station_col = cols.next();
        let valid_col = cols.next().map(str::trim);
        let tmpf_col = match cols.next() {
            Some(s) => s.trim(),
            None => continue,
        };
        if let Some((start, end)) = window_unix {
            let Some(valid) = valid_col else {
                continue;
            };
            let Some(ts) = parse_valid_utc_to_unix(valid) else {
                continue;
            };
            if ts < start || ts >= end {
                continue;
            }
        }
        if tmpf_col.is_empty() {
            continue;
        }
        let v: f64 = match tmpf_col.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Iowa Mesonet typically uses 0.0 for missing-but-not-flagged
        // rows. -9999 also appears occasionally. Drop both.
        if !(-100.0..=150.0).contains(&v) {
            continue;
        }
        if v > tmax {
            tmax = v;
        }
        if v < tmin {
            tmin = v;
        }
        n += 1;
    }
    if n == 0 {
        return Err(AsosError::NoObservations {
            station: station.to_string(),
            date: date_label.to_string(),
        });
    }
    Ok(DailyExtremes {
        station: station.to_string(),
        date_utc: date_label.to_string(),
        tmax_f: tmax,
        tmin_f: tmin,
        n_obs: n,
    })
}

fn parse_valid_utc_to_unix(s: &str) -> Option<i64> {
    let date = s.get(..10)?;
    let time = s.get(11..16)?;
    let base = parse_iso_date_to_unix(date)?;
    let mut parts = time.splitn(2, ':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    Some(base + hour * 3600 + minute * 60)
}

fn parse_iso_date_to_unix(s: &str) -> Option<i64> {
    let (year, month, day) = split_iso_date(s)?;
    unix_utc(year, month, day, 0)
}

fn unix_to_iso_date(ts: i64) -> Option<String> {
    if ts < 0 {
        return None;
    }
    let days = ts / 86_400;
    let (year, month, day) = days_to_ymd(days);
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn unix_utc(year: u16, month: u8, day: u8, hour: u8) -> Option<i64> {
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || hour > 23 {
        return None;
    }
    let dim = days_in_month(year, month);
    if day == 0 || u32::from(day) > dim {
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
    Some(days * 86_400 + i64::from(hour) * 3600)
}

fn days_to_ymd(days: i64) -> (u16, u8, u8) {
    let mut d = days;
    let mut year = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        year += 1;
    }
    let mut month = 1;
    loop {
        let dm = i64::from(days_in_month(year, month));
        if d < dm {
            break;
        }
        d -= dm;
        month += 1;
    }
    (year, month, u8::try_from(d + 1).unwrap_or(1))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_canonical_csv_returns_correct_extremes() {
        let csv = "station,valid,tmpf\nDEN,2024-05-01 00:00,42.5\nDEN,2024-05-01 06:00,38.0\nDEN,2024-05-01 12:00,55.5\nDEN,2024-05-01 18:00,72.3\nDEN,2024-05-01 23:00,60.0";
        let e = parse_csv_daily_extremes(csv, "DEN", "2024-05-01").unwrap();
        assert!((e.tmax_f - 72.3).abs() < 1e-9);
        assert!((e.tmin_f - 38.0).abs() < 1e-9);
        assert_eq!(e.n_obs, 5);
    }

    #[test]
    fn parse_handles_empty_tmpf_columns() {
        // Mesonet emits empty column when missing=empty is set.
        let csv = "station,valid,tmpf\nDEN,2024-05-01 00:00,\nDEN,2024-05-01 06:00,38.0\nDEN,2024-05-01 12:00,";
        let e = parse_csv_daily_extremes(csv, "DEN", "2024-05-01").unwrap();
        assert_eq!(e.n_obs, 1);
        assert!((e.tmax_f - 38.0).abs() < 1e-9);
        assert!((e.tmin_f - 38.0).abs() < 1e-9);
    }

    #[test]
    fn parse_skips_out_of_range_sentinels() {
        let csv = "station,valid,tmpf\nDEN,2024-05-01 00:00,-9999.0\nDEN,2024-05-01 06:00,72.0\nDEN,2024-05-01 12:00,200.0";
        let e = parse_csv_daily_extremes(csv, "DEN", "2024-05-01").unwrap();
        // 72.0 is the only valid row; -9999 and 200 are dropped.
        assert_eq!(e.n_obs, 1);
        assert!((e.tmax_f - 72.0).abs() < 1e-9);
    }

    #[test]
    fn parse_returns_no_observations_for_empty_csv() {
        let csv = "station,valid,tmpf\n";
        assert!(parse_csv_daily_extremes(csv, "DEN", "2024-05-01").is_err());
    }

    #[test]
    fn parse_skips_leading_comment_line() {
        let csv = "# this is a comment\nstation,valid,tmpf\nDEN,2024-05-01 12:00,72.0";
        let e = parse_csv_daily_extremes(csv, "DEN", "2024-05-01").unwrap();
        assert_eq!(e.n_obs, 1);
    }

    #[test]
    fn build_cache_path_layout() {
        let p = build_cache_path(Path::new("/tmp/x"), "DEN", "2024-05-01");
        assert!(p.ends_with("asos/DEN/2024-05-01.json"));
    }

    #[test]
    fn parse_local_day_window_for_west_coast_dst() {
        let csv = "station,valid,tmpf\nSFO,2026-05-07 06:59,80.0\nSFO,2026-05-07 07:00,50.0\nSFO,2026-05-07 22:00,64.0\nSFO,2026-05-08 06:59,58.0\nSFO,2026-05-08 07:00,20.0";
        let local_start = parse_iso_date_to_unix("2026-05-07").unwrap();
        let start_utc = local_start + 7 * 3600;
        let end_utc = start_utc + 24 * 3600;

        let e = parse_csv_extremes_between(csv, "SFO", "2026-05-07", start_utc, end_utc).unwrap();

        assert_eq!(e.n_obs, 3);
        assert_eq!(e.tmax_f, 64.0);
        assert_eq!(e.tmin_f, 50.0);
    }

    #[test]
    fn parse_local_day_window_for_east_coast_dst() {
        let csv = "station,valid,tmpf\nLGA,2026-05-07 03:59,40.0\nLGA,2026-05-07 04:00,61.0\nLGA,2026-05-07 18:00,74.0\nLGA,2026-05-08 03:59,66.0\nLGA,2026-05-08 04:00,10.0";
        let local_start = parse_iso_date_to_unix("2026-05-07").unwrap();
        let start_utc = local_start + 4 * 3600;
        let end_utc = start_utc + 24 * 3600;

        let e = parse_csv_extremes_between(csv, "LGA", "2026-05-07", start_utc, end_utc).unwrap();

        assert_eq!(e.n_obs, 3);
        assert_eq!(e.tmax_f, 74.0);
        assert_eq!(e.tmin_f, 61.0);
    }

    #[tokio::test]
    async fn same_day_local_fetch_bypasses_stale_cache_when_disabled() {
        let cache = tempfile::tempdir().unwrap();
        let cache_path = build_local_cache_path(cache.path(), "SFO", "2026-05-07", -7);
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cache_path,
            serde_json::to_vec(&DailyExtremes {
                station: "SFO".into(),
                date_utc: "2026-05-07".into(),
                tmax_f: 55.0,
                tmin_f: 50.0,
                n_obs: 2,
            })
            .unwrap(),
        )
        .unwrap();

        let body = "station,valid,tmpf\nSFO,2026-05-07 07:00,51.0\nSFO,2026-05-07 22:00,64.0";
        let endpoint = spawn_one_shot_http_server(body);
        let client = AsosClient::new("wx-stat-curator-test")
            .unwrap()
            .with_endpoint(endpoint);

        let e = client
            .fetch_local_day_extremes(cache.path(), "SFO", "2026-05-07", -7, false)
            .await
            .unwrap();

        assert_eq!(e.tmax_f, 64.0);
        assert_eq!(e.tmin_f, 51.0);
        assert_eq!(e.n_obs, 2);
    }

    #[test]
    fn split_iso_date_canonical() {
        assert_eq!(split_iso_date("2024-05-01"), Some((2024, 5, 1)));
        assert_eq!(split_iso_date("2024-12-31"), Some((2024, 12, 31)));
    }

    #[test]
    fn split_iso_date_rejects_invalid() {
        assert_eq!(split_iso_date("2024-13-01"), None);
        assert_eq!(split_iso_date("2024-00-01"), None);
        assert_eq!(split_iso_date("not-a-date"), None);
    }

    fn spawn_one_shot_http_server(body: &'static str) -> String {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{addr}/asos.py")
    }
}
