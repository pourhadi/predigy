//! Per-airport quantile-temperature extraction from NBM, with disk
//! caching so that we decode each ~5MB GRIB2 quantile message only
//! once per (cycle × forecast hour).
//!
//! The interesting piece is [`extract_tmp_quantiles_at_points`]:
//! given an NBM cycle + forecast hour + a set of named (lat, lon)
//! points (the curator's airports), it decodes each TMP quantile
//! message exactly once and samples all points against the same
//! decoded field. That makes per-airport queries effectively free
//! after the first cache miss.
//!
//! Cache layout:
//! ```text
//! <cache_root>/
//!   <YYYYMMDD>/<CC>/<fcst_hour:03>/
//!     tmp_2m/<airport_name>.json
//! ```
//! Each per-airport file is ~200 bytes (21 f32 quantiles + lat/lon
//! sanity bytes). With ~30 airports × 168 forecast hours × 4 cycles
//! per day, lifetime cache footprint is ~1.7 MB/day — trivial.

use crate::error::Error;
use crate::nbm::{MessageRange, NbmClient, NbmCycle, locate_quantile_messages};
use crate::nbm_decode::{NbmField, decode_message};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// One airport (or any named point) whose quantile vector we want.
#[derive(Debug, Clone)]
pub struct NamedPoint {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// Quantile temperatures (Kelvin) for one point at one (cycle ×
/// forecast hour). The 21-element layout is `[0%, 5%, ..., 100%]`,
/// so index `i` corresponds to quantile `5 * i`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirportQuantiles {
    /// Cycle the data was sampled from. Stored for cache-key
    /// safety + debugging.
    pub cycle_prefix: String,
    pub fcst_hour: u16,
    /// Caller-provided point name (typically the airport code).
    pub name: String,
    pub query_lat: f64,
    pub query_lon: f64,
    /// (lat, lon) of the actual NBM grid cell the value came from.
    pub snapped_lat: f32,
    pub snapped_lon: f32,
    /// Approximate haversine distance from query → snapped cell
    /// (km). Sanity check; should be < ~2.5 km on the NBM CONUS
    /// grid.
    pub snap_distance_km: f64,
    /// 21 quantile values in Kelvin. `temps_k[i]` is the
    /// `5 * i` percentile.
    pub temps_k: Vec<f32>,
}

impl AirportQuantiles {
    /// Linear-interpolate the CDF at `threshold_k` (Kelvin).
    /// Returns `P(T <= threshold_k)`. Handy for converting a
    /// Kalshi-side `>X` market into `model_p = 1 - cdf_at(X)`.
    pub fn cdf_at(&self, threshold_k: f32) -> f64 {
        // Edge cases: below the 0% quantile → CDF=0 (model says
        // certain not to happen). Above the 100% quantile → CDF=1.
        if self.temps_k.is_empty() {
            return 0.0;
        }
        if threshold_k <= self.temps_k[0] {
            return 0.0;
        }
        let last_idx = self.temps_k.len() - 1;
        if threshold_k >= self.temps_k[last_idx] {
            return 1.0;
        }
        // Find the two adjacent quantile values bracketing
        // threshold_k. Quantile percentages are 0, 5, 10, ..., 100.
        for i in 0..last_idx {
            let lo = self.temps_k[i];
            let hi = self.temps_k[i + 1];
            if threshold_k >= lo && threshold_k <= hi {
                // i ≤ 20 by construction, so the cast is exact.
                #[allow(clippy::cast_precision_loss)]
                let q_lo = (i as f64) * 5.0 / 100.0;
                #[allow(clippy::cast_precision_loss)]
                let q_hi = ((i + 1) as f64) * 5.0 / 100.0;
                if (hi - lo).abs() < f32::EPSILON {
                    // Two adjacent quantiles tied — return the lower
                    // bound (conservative).
                    return q_lo;
                }
                let frac = f64::from(threshold_k - lo) / f64::from(hi - lo);
                return q_lo + (q_hi - q_lo) * frac;
            }
        }
        // Should be unreachable given the bracket check above; fall
        // back to 1.0 conservatively.
        1.0
    }
}

/// Extract TMP 2m quantile vectors for many points in one call.
///
/// Strategy: hit the bucket once per quantile message (21 round-
/// trips total per cycle/fcst_hour), decode each, sample all
/// requested points against the decoded field. That keeps
/// per-airport cost linear in the *number of airports* rather than
/// the *number of GRIB messages × airports*.
///
/// Per-point results are persisted under
/// `<cache_root>/<cycle.YYYYMMDD>/<CC>/<fcst_hour:03>/tmp_2m/<name>.json`
/// for fast subsequent reads.
///
/// On cache hit (all requested points already cached), this
/// function does zero network or decode work — just N file reads.
pub async fn extract_tmp_quantiles_at_points(
    client: &NbmClient,
    cache_root: &Path,
    cycle: NbmCycle,
    fcst_hour: u16,
    points: &[NamedPoint],
) -> Result<Vec<AirportQuantiles>, Error> {
    if points.is_empty() {
        return Ok(Vec::new());
    }

    let cycle_dir = build_cycle_dir(cache_root, cycle, fcst_hour);

    // Cache hit short-circuit: if every requested point already
    // has a cached file, just read them.
    if let Some(cached) = try_read_all(&cycle_dir, points)? {
        debug!(?cycle_dir, n = cached.len(), "nbm_extract: cache hit");
        return Ok(cached);
    }

    // Cache miss path: fetch idx + decode every quantile message.
    info!(
        ?cycle,
        fcst_hour,
        n_points = points.len(),
        "nbm_extract: cache miss; pulling quantile fields"
    );
    let idx = client.fetch_index(cycle, fcst_hour, "co", "qmd").await?;
    let quantiles = locate_quantile_messages(&idx, "TMP", "2 m above ground");
    if quantiles.len() != 21 {
        return Err(Error::Invalid(format!(
            "NBM TMP 2m had {} quantile messages, expected 21",
            quantiles.len()
        )));
    }

    // Per-point accumulators of (snap_lat, snap_lon, dist_km, [21 vals]).
    let mut accum: Vec<PointAccum> = points.iter().map(|p| PointAccum::new(p.clone())).collect();

    for (pct, range) in &quantiles {
        let temps = decode_quantile_for_points(client, cycle, fcst_hour, range, points).await?;
        // Quantile percentage → array index (0% → 0, 5% → 1, …, 100% → 20).
        let q_idx = (*pct / 5) as usize;
        for (acc, sample) in accum.iter_mut().zip(&temps) {
            acc.set_quantile(q_idx, sample);
        }
    }

    let cycle_prefix = cycle.prefix();
    let out: Vec<AirportQuantiles> = accum
        .into_iter()
        .map(|acc| acc.finalise(&cycle_prefix, fcst_hour))
        .collect::<Result<Vec<_>, _>>()?;

    // Persist each point.
    if let Err(e) = std::fs::create_dir_all(&cycle_dir) {
        warn!(?cycle_dir, error = %e, "nbm_extract: cache mkdir failed; skipping write");
    } else {
        for q in &out {
            let path = point_cache_path(&cycle_dir, &q.name);
            if let Err(e) = write_atomic(&path, q) {
                warn!(?path, error = %e, "nbm_extract: cache write failed");
            }
        }
    }
    Ok(out)
}

/// Per-point sample of one quantile field. Carries the snap
/// distance + snapped (lat, lon) so the *first* quantile fetched
/// can populate them in [`PointAccum`]; subsequent quantiles all
/// snap to the same grid cell so we don't need to re-record.
struct QuantileSample {
    value_k: f32,
    snap_lat: f32,
    snap_lon: f32,
    dist_km: f64,
}

async fn decode_quantile_for_points(
    client: &NbmClient,
    cycle: NbmCycle,
    fcst_hour: u16,
    range: &MessageRange,
    points: &[NamedPoint],
) -> Result<Vec<QuantileSample>, Error> {
    // Plain helper — no `&mut` consumers downstream; iterator-style
    // borrow only.
    let bytes = client
        .fetch_message(cycle, fcst_hour, "co", "qmd", range)
        .await?;
    let field = decode_message(&bytes)?;
    let samples = points
        .iter()
        .map(|p| sample_one(&field, p))
        .collect::<Vec<_>>();
    Ok(samples)
}

fn sample_one(field: &NbmField, p: &NamedPoint) -> QuantileSample {
    let (val, idx, dist_km) = field.sample_nearest(p.lat, p.lon);
    QuantileSample {
        value_k: val,
        snap_lat: field.lats[idx],
        snap_lon: field.lons[idx],
        dist_km,
    }
}

struct PointAccum {
    point: NamedPoint,
    /// Snap info from the FIRST quantile message we processed.
    /// All quantile messages cover the same grid so the snap
    /// is identical; we capture it once and reuse.
    snap_lat: Option<f32>,
    snap_lon: Option<f32>,
    snap_dist_km: Option<f64>,
    temps: [Option<f32>; 21],
}

impl PointAccum {
    fn new(point: NamedPoint) -> Self {
        Self {
            point,
            snap_lat: None,
            snap_lon: None,
            snap_dist_km: None,
            temps: [None; 21],
        }
    }

    fn set_quantile(&mut self, q_idx: usize, sample: &QuantileSample) {
        if self.snap_lat.is_none() {
            self.snap_lat = Some(sample.snap_lat);
            self.snap_lon = Some(sample.snap_lon);
            self.snap_dist_km = Some(sample.dist_km);
        }
        self.temps[q_idx] = Some(sample.value_k);
    }

    fn finalise(self, cycle_prefix: &str, fcst_hour: u16) -> Result<AirportQuantiles, Error> {
        let temps: Vec<f32> = self
            .temps
            .iter()
            .enumerate()
            .map(|(i, t)| {
                t.ok_or_else(|| {
                    Error::Invalid(format!(
                        "missing quantile {}% for point {}",
                        i * 5,
                        self.point.name
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AirportQuantiles {
            cycle_prefix: cycle_prefix.to_string(),
            fcst_hour,
            name: self.point.name,
            query_lat: self.point.lat,
            query_lon: self.point.lon,
            snapped_lat: self.snap_lat.unwrap_or_default(),
            snapped_lon: self.snap_lon.unwrap_or_default(),
            snap_distance_km: self.snap_dist_km.unwrap_or_default(),
            temps_k: temps,
        })
    }
}

/// On-disk path: `<cache_root>/blend.YYYYMMDD/CC/<fcst:03>/tmp_2m/`.
fn build_cycle_dir(cache_root: &Path, cycle: NbmCycle, fcst_hour: u16) -> PathBuf {
    let mut p = cache_root.to_path_buf();
    p.push(format!(
        "blend.{:04}{:02}{:02}",
        cycle.year, cycle.month, cycle.day
    ));
    p.push(format!("{:02}", cycle.hour));
    p.push(format!("{fcst_hour:03}"));
    p.push("tmp_2m");
    p
}

fn point_cache_path(cycle_dir: &Path, name: &str) -> PathBuf {
    let mut p = cycle_dir.to_path_buf();
    p.push(format!("{name}.json"));
    p
}

/// Try to read every requested point from cache. Returns `Some(_)`
/// only if all requested points are present (so the caller doesn't
/// have to do partial fetches; on partial cache, just refresh).
fn try_read_all(
    cycle_dir: &Path,
    points: &[NamedPoint],
) -> Result<Option<Vec<AirportQuantiles>>, Error> {
    let mut out = Vec::with_capacity(points.len());
    for p in points {
        let path = point_cache_path(cycle_dir, &p.name);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<AirportQuantiles>(&bytes) {
                Ok(q) => out.push(q),
                Err(e) => {
                    warn!(?path, error = %e, "nbm_extract: corrupt cache file; treating as miss");
                    return Ok(None);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::Invalid(format!(
                    "cache read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Ok(Some(out))
}

fn write_atomic(path: &Path, q: &AirportQuantiles) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(q)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantiles_with(temps: Vec<f32>) -> AirportQuantiles {
        AirportQuantiles {
            cycle_prefix: "blend.20260506/12".into(),
            fcst_hour: 24,
            name: "DEN".into(),
            query_lat: 39.86,
            query_lon: -104.67,
            snapped_lat: 39.86,
            snapped_lon: -104.67,
            snap_distance_km: 0.0,
            temps_k: temps,
        }
    }

    #[test]
    fn cdf_at_below_min_returns_zero() {
        let q = quantiles_with((0u16..21).map(|i| 290.0 + f32::from(i)).collect());
        // 280 K is below the 0% quantile (290 K).
        assert!((q.cdf_at(280.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn cdf_at_above_max_returns_one() {
        let q = quantiles_with((0u16..21).map(|i| 290.0 + f32::from(i)).collect());
        // 320 K is above the 100% quantile (310 K).
        assert!((q.cdf_at(320.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cdf_at_midpoint_interpolates_linearly() {
        // Quantiles step 290..310 in 1K increments; 50% level is at
        // 300K (index 10). Half-way between 50% (300K) and 55%
        // (301K) → 300.5K should map to halfway between 0.50 and
        // 0.55 = 0.525.
        let q = quantiles_with((0u16..21).map(|i| 290.0 + f32::from(i)).collect());
        let p = q.cdf_at(300.5);
        assert!((p - 0.525).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn cdf_at_exact_quantile_value_returns_quantile() {
        let q = quantiles_with((0u16..21).map(|i| 290.0 + f32::from(i)).collect());
        // 300 K = 50% level; should return 0.50 (within
        // floating-point slop on the bracket boundary).
        let p = q.cdf_at(300.0);
        assert!((p - 0.50).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn cycle_dir_layout_matches_doc() {
        let cycle = NbmCycle {
            year: 2026,
            month: 5,
            day: 6,
            hour: 12,
        };
        let dir = build_cycle_dir(Path::new("/tmp/cache"), cycle, 24);
        assert!(dir.ends_with("blend.20260506/12/024/tmp_2m"));
    }
}
