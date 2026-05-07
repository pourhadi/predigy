//! Historical observed-temperature fetcher — Iowa State Mesonet
//! ASOS archive, free, no auth.
//!
//! Endpoint: `mesonet.agron.iastate.edu/cgi-bin/request/asos.py`
//! returns minute-resolution surface observations for any ASOS
//! station and date range, in CSV form. We slice it to one
//! station-day at a time and compute the daily maximum / minimum
//! `tmpf` value.
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
    /// Local-date the observations were aggregated over (UTC).
    /// We use UTC date for now — the Kalshi settlement source is
    /// the NWS Climatological Report which is strictly local-day,
    /// but matching that exactly would require timezone-aware
    /// aggregation per station; UTC-day is a defensible
    /// approximation that costs at most ~6 hours of overlap.
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

    async fn fetch_csv(&self, station: &str, date_utc: &str) -> Result<String, AsosError> {
        // Parse YYYY-MM-DD into year/month/day.
        let (y, m, d) = split_iso_date(date_utc).ok_or_else(|| AsosError::Api {
            status: 0,
            body: format!("bad date format: {date_utc}"),
        })?;
        // The Iowa Mesonet endpoint takes start/end as separate
        // year/month/day query params and returns CSV per station.
        let url = format!(
            "{base}?station={station}&data=tmpf&year1={y}&month1={m}&day1={d}&year2={y}&month2={m}&day2={d}&tz=Etc%2FUTC&format=onlycomma&latlon=no&missing=empty&trace=empty",
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
                    date = %date_utc,
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
        let _valid_col = cols.next();
        let tmpf_col = match cols.next() {
            Some(s) => s.trim(),
            None => continue,
        };
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
            date: date_utc.to_string(),
        });
    }
    Ok(DailyExtremes {
        station: station.to_string(),
        date_utc: date_utc.to_string(),
        tmax_f: tmax,
        tmin_f: tmin,
        n_obs: n,
    })
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
}
