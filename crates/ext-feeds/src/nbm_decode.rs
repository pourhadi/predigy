//! GRIB2 decode for NBM probabilistic messages.
//!
//! Wraps the `grib` crate so the rest of the project doesn't have
//! to know about GRIB2's deeply nested API. Surface:
//!
//! - [`decode_message`]: take a single-message byte buffer (as
//!   returned by [`crate::nbm::NbmClient::fetch_message`]) and
//!   decode it into an [`NbmField`] with raw f32 values.
//! - [`NbmField::sample_nearest`]: nearest-grid-point sampling at
//!   a given lat/lon — the operation airport extraction needs.
//!
//! NBM CONUS messages use Lambert Conformal Conic grid + JPEG2000
//! compression (Template 5.40). The `grib` crate handles both via
//! the `jpeg2000-unpack-with-openjpeg` feature; build-time C dep
//! `openjpeg` is supplied by Homebrew on macOS.

use crate::error::Error;
use grib::LatLons as _;
use std::io::Cursor;

/// One decoded GRIB2 message: the f32 grid + per-point lat/lon.
/// Stored as flat row-major arrays so the consumer can index by
/// linear position. `lats[i]`, `lons[i]`, and `values[i]` line up.
///
/// The `grib` crate's latlons iterator yields f32; that's fine for
/// our use (5-decimal-digit precision is well within f32 range and
/// the airport map is 4dp anyway).
#[derive(Debug, Clone)]
pub struct NbmField {
    /// Per-point latitudes (degrees north).
    pub lats: Vec<f32>,
    /// Per-point longitudes (degrees east; may be 0..360 or
    /// -180..180 depending on grid).
    pub lons: Vec<f32>,
    /// Per-point values, decoded f32 in the GRIB-native unit
    /// (Kelvin for TMP at 2m).
    pub values: Vec<f32>,
}

impl NbmField {
    /// Sample the value at the grid point closest to `(lat, lon)`
    /// (degrees). Returns `(value, point_index, distance_km_approx)`.
    /// Distance is a simple haversine; good enough for picking the
    /// nearest cell among the ~250k-point CONUS grid.
    ///
    /// Panics if the field is empty.
    pub fn sample_nearest(&self, lat: f64, lon: f64) -> (f32, usize, f64) {
        assert!(
            !self.values.is_empty(),
            "sample_nearest on empty NbmField"
        );
        let target_lon = normalize_lon(lon);
        let mut best_i = 0usize;
        let mut best_d2 = f64::MAX;
        for (i, (&plat, &plon)) in self.lats.iter().zip(self.lons.iter()).enumerate() {
            // Use squared planar distance for the inner loop — way
            // faster than haversine; latitude/longitude near a
            // single airport is local enough that the small-angle
            // approximation is fine for nearest-neighbour
            // selection.
            let dlat = f64::from(plat) - lat;
            let dlon = wrap_diff_deg(normalize_lon(f64::from(plon)), target_lon);
            let d2 = dlat * dlat + dlon * dlon;
            if d2 < best_d2 {
                best_d2 = d2;
                best_i = i;
            }
        }
        let value = self.values[best_i];
        // Approx haversine for telemetry only — the (lat, lon) of
        // the chosen cell is also returned so the caller can sanity-
        // check the snap.
        let approx_km = approx_distance_km(
            lat,
            lon,
            f64::from(self.lats[best_i]),
            f64::from(self.lons[best_i]),
        );
        (value, best_i, approx_km)
    }

    /// Number of grid points in this field.
    pub fn point_count(&self) -> usize {
        self.values.len()
    }
}

/// Decode one NBM GRIB2 message into a flat `NbmField`.
/// The message bytes are assumed to be a single GRIB2 message —
/// not a concatenation of multiple — i.e. exactly the byte range
/// produced by [`crate::nbm::IdxEntry::range_with`].
pub fn decode_message(bytes: &[u8]) -> Result<NbmField, Error> {
    let grib2 = grib::from_reader(Cursor::new(bytes))
        .map_err(|e| Error::Invalid(format!("grib2 parse failed: {e}")))?;
    // A single-message buffer should yield exactly one submessage
    // when we iterate. Take the first one; if there are zero, the
    // file is malformed.
    let (_idx, submessage) = grib2
        .iter()
        .next()
        .ok_or_else(|| Error::Invalid("nbm grib2 contained no submessages".into()))?;

    let latlons_iter = submessage
        .latlons()
        .map_err(|e| Error::Invalid(format!("grib2 latlons: {e}")))?;
    let decoder = grib::Grib2SubmessageDecoder::from(submessage)
        .map_err(|e| Error::Invalid(format!("grib2 decoder build: {e}")))?;
    let values = decoder
        .dispatch()
        .map_err(|e| Error::Invalid(format!("grib2 dispatch: {e}")))?;

    // Materialize both iterators in lock-step. Both yield the same
    // grid-point count by construction.
    let mut lats = Vec::new();
    let mut lons = Vec::new();
    let mut vals = Vec::new();
    for ((lat, lon), v) in latlons_iter.zip(values) {
        lats.push(lat);
        lons.push(lon);
        vals.push(v);
    }

    if vals.is_empty() {
        return Err(Error::Invalid("nbm grib2 decoded zero values".into()));
    }

    Ok(NbmField {
        lats,
        lons,
        values: vals,
    })
}

/// Bring a longitude into the canonical -180..=180 range so two
/// values in different conventions (0..360 vs ±180) can be compared.
fn normalize_lon(lon: f64) -> f64 {
    let mut x = lon;
    while x > 180.0 {
        x -= 360.0;
    }
    while x < -180.0 {
        x += 360.0;
    }
    x
}

/// Smallest absolute longitude difference (degrees) between two
/// already-normalized values, accounting for the ±180 wrap.
fn wrap_diff_deg(a: f64, b: f64) -> f64 {
    let mut d = (a - b).abs();
    if d > 180.0 {
        d = 360.0 - d;
    }
    d
}

/// Quick haversine for telemetry. Earth radius 6371 km.
fn approx_distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r_km = 6371.0;
    let to_rad = std::f64::consts::PI / 180.0;
    let phi1 = lat1 * to_rad;
    let phi2 = lat2 * to_rad;
    let dphi = (lat2 - lat1) * to_rad;
    let dl = (lon2 - lon1) * to_rad;
    let a = (dphi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dl / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    r_km * c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lon_round_trip() {
        assert!((normalize_lon(0.0) - 0.0).abs() < 1e-9);
        assert!((normalize_lon(180.0) - 180.0).abs() < 1e-9);
        assert!((normalize_lon(-180.0) - -180.0).abs() < 1e-9);
        // 200 east is the same as -160 east.
        assert!((normalize_lon(200.0) - -160.0).abs() < 1e-9);
        // -200 east is the same as 160 east.
        assert!((normalize_lon(-200.0) - 160.0).abs() < 1e-9);
    }

    #[test]
    fn wrap_diff_handles_meridian_wrap() {
        // 179 vs -179: short way is 2°, not 358°.
        assert!((wrap_diff_deg(179.0, -179.0) - 2.0).abs() < 1e-9);
        // 10 vs 5: 5°.
        assert!((wrap_diff_deg(10.0, 5.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn approx_distance_km_zero_for_same_point() {
        let d = approx_distance_km(39.86, -104.67, 39.86, -104.67);
        assert!(d < 1e-3);
    }

    #[test]
    fn approx_distance_km_denver_to_la() {
        // Real-world reference: Denver KDEN (39.86, -104.67) to
        // Los Angeles KLAX (33.94, -118.41) is ~1338 km. Allow 20%
        // tolerance — this is for telemetry, not navigation.
        let d = approx_distance_km(39.8617, -104.6731, 33.9416, -118.4085);
        assert!((1100.0..1500.0).contains(&d), "got {d}");
    }
}
