//! Live integration tests against NOAA's NBM S3 bucket.
//!
//! These hit the network and depend on NOAA's bucket, so they're
//! `#[ignore]` by default. Run explicitly with:
//!
//! ```text
//! cargo test -p predigy-ext-feeds --test nbm_live -- --ignored --nocapture
//! ```
//!
//! What they validate:
//!
//! 1. The bucket is reachable + range requests work.
//! 2. The `.grib2.idx` parser handles real NBM files.
//! 3. The `grib` crate decodes NBM JPEG2000-packed temperature
//!    quantile messages correctly on this Mac (with openjpeg from
//!    Homebrew).
//! 4. The decoded lat/lon grid + values match real-world
//!    expectations at known city coordinates.
//!
//! Together these confirm the Phase 2 NBM pipeline (per
//! `docs/WX_STAT_NBM_PHASE2.md`).
//!
//! Cycle-selection note: NBM **qmd** runs only at 00 / 06 / 12 /
//! 18 UTC (the *core* product runs hourly but quantile data
//! doesn't). f024 from cycle CC publishes ~5–6 hours after CC, so
//! we walk back ~7 h from "now", round down to the most recent
//! 6 h boundary, and use that cycle. Verified empirically against
//! the live bucket on 2026-05-06.

use predigy_ext_feeds::nbm::{
    NbmClient, NbmCycle, locate_quantile_messages, locate_threshold_message,
};
use predigy_ext_feeds::nbm_decode::decode_message;
use predigy_ext_feeds::nbm_extract::{NamedPoint, extract_tmp_quantiles_at_points};
use std::time::{SystemTime, UNIX_EPOCH};

const TEST_USER_AGENT: &str = "(predigy-ext-feeds tests, dan@pourhadi.com)";

/// Pick a recent NBM **qmd** cycle that should already have
/// published f024. Walk back 7 hours from `now`, round down to
/// the most recent 6h boundary (00/06/12/18 UTC).
#[allow(clippy::cast_possible_wrap)]
fn recent_cycle(now_unix: i64) -> NbmCycle {
    let lookback_secs: i64 = 7 * 3600;
    let target = now_unix - lookback_secs;
    let secs = target.max(0) as u64;
    let days = (secs / 86_400) as i64;
    let raw_hour = ((secs / 3600) % 24) as u8;
    // Round down to the nearest 6h cycle boundary.
    let cycle_hour = (raw_hour / 6) * 6;
    let (year, month, day) = days_to_ymd(days);
    NbmCycle {
        year,
        month,
        day,
        hour: cycle_hour,
    }
}

/// Convert days-since-1970-01-01 (Unix epoch) to (year, month,
/// day) in the proleptic Gregorian calendar. Inverse of the
/// chrono-lite UTC date used elsewhere in the crate.
fn days_to_ymd(days: i64) -> (u16, u8, u8) {
    let mut d = days;
    // Start at 1970, walk forward.
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
    let day = (d + 1) as u8;
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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

#[tokio::test]
#[ignore = "hits live NOAA S3 bucket"]
async fn fetch_index_returns_real_quantile_entries() {
    let client = NbmClient::new(TEST_USER_AGENT).unwrap();
    let cycle = recent_cycle(now_unix());
    eprintln!("test cycle: {cycle:?}");
    let idx = client
        .fetch_index(cycle, 24, "co", "qmd")
        .await
        .expect("fetch_index");
    eprintln!("fetched idx with {} entries", idx.len());
    assert!(idx.len() > 100, "got only {} idx entries", idx.len());

    let q = locate_quantile_messages(&idx, "TMP", "2 m above ground");
    eprintln!("found {} TMP 2m quantile messages", q.len());
    // Should have all 21 levels (0%, 5%, ..., 100%).
    assert_eq!(q.len(), 21);
    let pcts: Vec<u8> = q.iter().map(|(p, _)| *p).collect();
    assert_eq!(pcts[0], 0);
    assert_eq!(pcts[10], 50);
    assert_eq!(pcts[20], 100);

    // Cross-check: the prob >299.817K (=80°F) threshold message
    // should also exist alongside the quantiles.
    let prob80 = locate_threshold_message(&idx, "TMP", "2 m above ground", "prob >299.817");
    assert!(
        prob80.is_some(),
        "TMP 2m prob>80F threshold message not found"
    );
}

#[tokio::test]
#[ignore = "hits live NOAA S3 bucket and JPEG2000-decodes a ~5MB GRIB message"]
async fn decode_tmp_50pct_quantile_at_denver() {
    // The full integration: fetch idx → find the TMP 50% (median)
    // message → range-fetch → decode → sample at Denver coords.
    // Sanity-check the value is in a plausible Earth-temperature
    // band (200..330 K = -73 .. 57 °C).
    let client = NbmClient::new(TEST_USER_AGENT).unwrap();
    let cycle = recent_cycle(now_unix());
    eprintln!("test cycle: {cycle:?}");

    let idx = client
        .fetch_index(cycle, 24, "co", "qmd")
        .await
        .expect("fetch_index");
    let quantiles = locate_quantile_messages(&idx, "TMP", "2 m above ground");
    let (pct, range) = quantiles
        .iter()
        .find(|(p, _)| *p == 50)
        .expect("TMP 50% level message");
    assert_eq!(*pct, 50);
    eprintln!("TMP 50% range: {range:?}");

    let bytes = client
        .fetch_message(cycle, 24, "co", "qmd", range)
        .await
        .expect("fetch_message");
    eprintln!("fetched {} bytes for TMP 50%", bytes.len());

    let field = decode_message(&bytes).expect("decode");
    eprintln!("decoded field: {} grid points", field.point_count());
    assert!(
        field.point_count() > 100_000,
        "expected NBM CONUS to have >100k grid points, got {}",
        field.point_count()
    );

    // Denver airport.
    let (val_kelvin, idx, dist_km) = field.sample_nearest(39.8617, -104.6731);
    let val_celsius = val_kelvin - 273.15;
    let val_fahrenheit = val_celsius * 9.0 / 5.0 + 32.0;
    eprintln!(
        "Denver TMP 50% level @ f024: {val_kelvin:.2} K = {val_celsius:.1} C = {val_fahrenheit:.1} F (cell idx {idx}, ~{dist_km:.1} km away)"
    );
    assert!(
        (200.0..330.0).contains(&val_kelvin),
        "Denver TMP 50% level looks unphysical: {val_kelvin} K"
    );
    // Snap distance should be very small — NBM CONUS grid is 2.5km
    // resolution. Allow up to 5 km for slop.
    assert!(
        dist_km < 5.0,
        "nearest grid point at Denver was {dist_km:.1} km away — grid lookup likely wrong"
    );
}

#[tokio::test]
#[ignore = "hits live NOAA S3 + decodes 21 GRIB messages (~100MB)"]
async fn extract_quantiles_for_multiple_airports_round_trip() {
    // The full Phase 2C pipeline: pull ALL 21 TMP quantile messages
    // for one (cycle, fcst_hour), sample at multiple airports in
    // one pass, validate the cache file written and the CDF
    // interpolation makes sense.
    let client = NbmClient::new(TEST_USER_AGENT).unwrap();
    let cycle = recent_cycle(now_unix());
    eprintln!("test cycle: {cycle:?}");

    let cache_dir = tempfile::tempdir().unwrap();

    let points = vec![
        NamedPoint {
            name: "DEN".into(),
            lat: 39.8617,
            lon: -104.6731,
        },
        NamedPoint {
            name: "LAX".into(),
            lat: 33.9416,
            lon: -118.4085,
        },
        NamedPoint {
            name: "NYC".into(),
            lat: 40.7794,
            lon: -73.8803,
        },
    ];

    let started = SystemTime::now();
    let qs = extract_tmp_quantiles_at_points(&client, cache_dir.path(), cycle, 24, &points)
        .await
        .expect("extract");
    let cold_secs = started.elapsed().unwrap().as_secs_f64();
    eprintln!("cold extract: {cold_secs:.1}s for {} points", qs.len());
    assert_eq!(qs.len(), 3);

    for q in &qs {
        eprintln!(
            "  {}: 21 quantile temps Kelvin, 50%-level={:.2}K ({:.1}F), snap {:.2} km",
            q.name,
            q.temps_k[10],
            (q.temps_k[10] - 273.15) * 9.0 / 5.0 + 32.0,
            q.snap_distance_km,
        );
        assert_eq!(q.temps_k.len(), 21);
        // Quantiles must be monotonic non-decreasing.
        for w in q.temps_k.windows(2) {
            assert!(
                w[0] <= w[1] + 0.01,
                "quantile non-monotone for {}: {} > {}",
                q.name,
                w[0],
                w[1]
            );
        }
        // 50%-level should look earth-like.
        assert!((200.0..330.0).contains(&q.temps_k[10]));
        assert!(q.snap_distance_km < 5.0);
    }

    // Cache should now be populated; second call must be fast.
    let started = SystemTime::now();
    let qs2 = extract_tmp_quantiles_at_points(&client, cache_dir.path(), cycle, 24, &points)
        .await
        .expect("extract from cache");
    let warm_secs = started.elapsed().unwrap().as_secs_f64();
    eprintln!("warm extract (cache hit): {warm_secs:.3}s");
    assert_eq!(qs2.len(), 3);
    // Cache hit should be <1s (just file reads).
    assert!(
        warm_secs < 1.0,
        "warm extract took {warm_secs}s (cache miss?)"
    );

    // CDF interpolation sanity: the median quantile (50%) should
    // map to ~0.5; a value below the 0% quantile to 0; above 100%
    // to 1.
    let first = &qs[0];
    let median = first.temps_k[10];
    let p_at_median = first.cdf_at(median);
    eprintln!(
        "{}: cdf_at(median={:.1}K) = {:.3}",
        first.name, median, p_at_median
    );
    assert!((0.45..=0.55).contains(&p_at_median));
    assert!((first.cdf_at(100.0) - 0.0).abs() < 1e-9);
    assert!((first.cdf_at(400.0) - 1.0).abs() < 1e-9);
}
