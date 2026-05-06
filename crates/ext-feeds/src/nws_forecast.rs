//! NWS hourly point forecast client.
//!
//! Different from the [`crate::nws`] alerts module: that one polls
//! the active-alerts feed for severe-weather warnings and emits
//! `NwsAlert` events as they fire. This one is a *pull-on-demand*
//! forecast client used by `wx-stat-curator` to compute
//! `model_p` for Kalshi temperature markets.
//!
//! ## API shape
//!
//! NWS uses a two-step lookup:
//!
//! 1. `GET /points/{lat},{lon}` returns a JSON envelope describing
//!    which Weather Forecast Office (WFO) covers that lat/lon and
//!    the grid cell index `(grid_x, grid_y)` within that office.
//! 2. `GET /gridpoints/{office}/{grid_x},{grid_y}/forecast/hourly`
//!    returns up to 168 hours of hourly forecast for that cell.
//!
//! Both endpoints require a `User-Agent` header identifying the
//! caller; NWS rejects anonymous traffic.
//!
//! Phase 1 surfaces only the fields needed for temperature-market
//! probability computation: `start_time`, `end_time`, `temperature`,
//! `temperature_unit`. NBM probabilistic data is Phase 2 in
//! [`docs/WX_STAT_PLAN.md`].

use crate::error::Error;
use serde::Deserialize;
use std::time::Duration;
use tracing::debug;

const DEFAULT_BASE: &str = "https://api.weather.gov";

/// Resolved grid-point handle returned by [`lookup_point`].
#[derive(Debug, Clone, PartialEq)]
pub struct GridPoint {
    /// 3-letter Weather Forecast Office id (e.g. `"BOU"` for Boulder,
    /// CO; `"OKX"` for New York City).
    pub office: String,
    pub grid_x: u32,
    pub grid_y: u32,
    /// Approximate observation point used for the grid cell —
    /// useful for sanity-checking that the lookup landed in the
    /// right place. May not exactly match the input lat/lon
    /// because NWS snaps to the cell center.
    pub forecast_lat: Option<f64>,
    pub forecast_lon: Option<f64>,
    /// City+state (`"Denver, CO"`) for the cell, when available.
    pub relative_location: Option<String>,
}

/// One hour of forecast.
#[derive(Debug, Clone, PartialEq)]
pub struct HourlyForecastEntry {
    /// ISO-8601 start of the forecast hour.
    pub start_time: String,
    /// ISO-8601 end (exclusive) of the forecast hour.
    pub end_time: String,
    /// Temperature value at the hour, in the unit named by
    /// `temperature_unit`. NWS hourly forecast is point-valued
    /// (deterministic), not probabilistic.
    pub temperature: f64,
    /// `"F"` or `"C"`. Almost always `"F"` for US points but the
    /// API does return Celsius for some products.
    pub temperature_unit: String,
}

/// Up to 168 hours of forecast plus the issuance timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct HourlyForecast {
    /// When NWS produced this forecast (ISO-8601). Earlier issuance
    /// = staler — useful when deciding whether to skip a market
    /// because the forecast hasn't been refreshed in too long.
    pub generated_at: Option<String>,
    /// Hours, in chronological order. Length up to 168 (7 days × 24h).
    pub periods: Vec<HourlyForecastEntry>,
}

#[derive(Debug, Clone)]
pub struct NwsForecastClient {
    http: reqwest::Client,
    base: String,
}

impl NwsForecastClient {
    /// Build a client. `user_agent` is required — NWS rejects
    /// anonymous traffic. Format suggestion from the API docs:
    /// `"(myapp.com, contact@example.com)"`.
    pub fn new(user_agent: &str) -> Result<Self, Error> {
        if user_agent.trim().is_empty() {
            return Err(Error::Invalid(
                "NWS requires a non-empty User-Agent identifying the caller".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent.to_string())
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            base: DEFAULT_BASE.into(),
        })
    }

    /// Override the base URL — used by tests against a local mock.
    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Resolve a (lat, lon) to its NWS grid cell. NWS expects
    /// 4-decimal-place coordinates; longer precision returns 301.
    pub async fn lookup_point(&self, lat: f64, lon: f64) -> Result<GridPoint, Error> {
        let url = format!("{base}/points/{lat:.4},{lon:.4}", base = self.base);
        debug!(%url, "nws-forecast: lookup_point");
        let bytes = self.fetch_geojson(&url).await?;
        let env: PointResponse = serde_json::from_slice(&bytes)?;
        let p = env.properties;
        let office = p
            .grid_id
            .ok_or_else(|| Error::Invalid("NWS /points response missing gridId".into()))?;
        let grid_x = p
            .grid_x
            .ok_or_else(|| Error::Invalid("NWS /points response missing gridX".into()))?;
        let grid_y = p
            .grid_y
            .ok_or_else(|| Error::Invalid("NWS /points response missing gridY".into()))?;
        let (forecast_lat, forecast_lon) = match env.geometry {
            Some(Geometry { ty, coordinates }) if ty == "Point" && coordinates.len() == 2 => {
                // GeoJSON convention: [lon, lat].
                (Some(coordinates[1]), Some(coordinates[0]))
            }
            _ => (None, None),
        };
        let relative_location = p.relative_location.and_then(|loc| {
            let city = loc.properties.city?;
            let state = loc.properties.state?;
            Some(format!("{city}, {state}"))
        });
        Ok(GridPoint {
            office,
            grid_x,
            grid_y,
            forecast_lat,
            forecast_lon,
            relative_location,
        })
    }

    /// Fetch the hourly forecast for a previously-resolved grid cell.
    pub async fn fetch_hourly(&self, gp: &GridPoint) -> Result<HourlyForecast, Error> {
        let url = format!(
            "{base}/gridpoints/{office}/{x},{y}/forecast/hourly",
            base = self.base,
            office = gp.office,
            x = gp.grid_x,
            y = gp.grid_y,
        );
        debug!(%url, "nws-forecast: fetch_hourly");
        let bytes = self.fetch_geojson(&url).await?;
        parse_hourly(&bytes)
    }

    async fn fetch_geojson(&self, url: &str) -> Result<Vec<u8>, Error> {
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/geo+json")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

// ---- response shapes ----

#[derive(Debug, Deserialize)]
struct PointResponse {
    #[serde(default)]
    properties: PointProperties,
    #[serde(default)]
    geometry: Option<Geometry>,
}

#[derive(Debug, Deserialize, Default)]
struct PointProperties {
    #[serde(default, rename = "gridId")]
    grid_id: Option<String>,
    #[serde(default, rename = "gridX")]
    grid_x: Option<u32>,
    #[serde(default, rename = "gridY")]
    grid_y: Option<u32>,
    #[serde(default, rename = "relativeLocation")]
    relative_location: Option<RelativeLocation>,
}

#[derive(Debug, Deserialize)]
struct RelativeLocation {
    #[serde(default)]
    properties: RelativeLocationProps,
}

#[derive(Debug, Deserialize, Default)]
struct RelativeLocationProps {
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Geometry {
    #[serde(rename = "type")]
    ty: String,
    coordinates: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct HourlyResponse {
    #[serde(default)]
    properties: HourlyProperties,
}

#[derive(Debug, Deserialize, Default)]
struct HourlyProperties {
    #[serde(default, rename = "generatedAt")]
    generated_at: Option<String>,
    #[serde(default)]
    periods: Vec<HourlyPeriod>,
}

#[derive(Debug, Deserialize)]
struct HourlyPeriod {
    #[serde(rename = "startTime")]
    start_time: String,
    #[serde(rename = "endTime")]
    end_time: String,
    /// NWS hourly forecast returns this as a JSON number for hourly
    /// products, but as `{value: f, unitCode: "wmoUnit:degF"}` for
    /// some gridded products. We accept both shapes.
    #[serde(default)]
    temperature: Option<TemperatureValue>,
    #[serde(default, rename = "temperatureUnit")]
    temperature_unit: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TemperatureValue {
    Scalar(f64),
    Object {
        #[serde(default)]
        value: Option<f64>,
        #[serde(default, rename = "unitCode")]
        unit_code: Option<String>,
    },
}

fn parse_hourly(body: &[u8]) -> Result<HourlyForecast, Error> {
    let env: HourlyResponse = serde_json::from_slice(body)?;
    let mut periods = Vec::with_capacity(env.properties.periods.len());
    for p in env.properties.periods {
        // Hourly forecast usually carries scalar `temperature` plus
        // `temperatureUnit: "F"`. The gridded product instead nests
        // the value under `value` and reports a WMO unit code.
        let (temp, unit) = match (p.temperature, p.temperature_unit) {
            (Some(TemperatureValue::Scalar(v)), Some(u)) => (v, u),
            (Some(TemperatureValue::Scalar(v)), None) => (v, "F".into()),
            (Some(TemperatureValue::Object { value, unit_code }), explicit_unit) => {
                let v = value.ok_or_else(|| {
                    Error::Invalid("NWS hourly period missing temperature.value".into())
                })?;
                // WMO unit codes look like "wmoUnit:degF" or
                // "wmoUnit:degC". Strip the prefix when present;
                // default to "F" if the explicit field was set.
                let u = explicit_unit
                    .or_else(|| unit_code.as_deref().map(unit_from_wmo).map(str::to_string))
                    .unwrap_or_else(|| "F".into());
                (v, u)
            }
            (None, _) => {
                return Err(Error::Invalid(
                    "NWS hourly period missing temperature".into(),
                ));
            }
        };
        periods.push(HourlyForecastEntry {
            start_time: p.start_time,
            end_time: p.end_time,
            temperature: temp,
            temperature_unit: unit,
        });
    }
    Ok(HourlyForecast {
        generated_at: env.properties.generated_at,
        periods,
    })
}

/// Map a WMO unit code (`"wmoUnit:degF"`) to a single-letter unit
/// label (`"F"`). Falls back to the input on unrecognised codes.
fn unit_from_wmo(code: &str) -> &str {
    match code {
        "wmoUnit:degF" => "F",
        "wmoUnit:degC" => "C",
        _ => code,
    }
}

/// Pure parse helper, exposed for tests and offline replay tools.
pub fn parse_hourly_response(body: &[u8]) -> Result<HourlyForecast, Error> {
    parse_hourly(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_user_agent() {
        assert!(NwsForecastClient::new("").is_err());
        assert!(NwsForecastClient::new("   ").is_err());
    }

    #[test]
    fn parses_scalar_temperature_format() {
        // Shape returned by /forecast/hourly — the human-friendly
        // hourly product. Scalar temperature, explicit Fahrenheit
        // unit string.
        let body = br#"{
            "properties": {
                "generatedAt": "2026-05-05T18:00:00Z",
                "periods": [
                    {
                        "startTime": "2026-05-05T13:00:00-06:00",
                        "endTime":   "2026-05-05T14:00:00-06:00",
                        "temperature": 78,
                        "temperatureUnit": "F"
                    },
                    {
                        "startTime": "2026-05-05T14:00:00-06:00",
                        "endTime":   "2026-05-05T15:00:00-06:00",
                        "temperature": 81,
                        "temperatureUnit": "F"
                    }
                ]
            }
        }"#;
        let f = parse_hourly(body).unwrap();
        assert_eq!(f.generated_at.as_deref(), Some("2026-05-05T18:00:00Z"));
        assert_eq!(f.periods.len(), 2);
        assert!((f.periods[0].temperature - 78.0).abs() < f64::EPSILON);
        assert_eq!(f.periods[0].temperature_unit, "F");
        assert!((f.periods[1].temperature - 81.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_gridded_object_temperature_format() {
        // Shape returned by raw /gridpoints/.../X,Y (no /forecast):
        // `temperature` is `{value, unitCode}` and the explicit
        // `temperatureUnit` is absent.
        let body = br#"{
            "properties": {
                "periods": [
                    {
                        "startTime": "2026-05-05T13:00:00-06:00",
                        "endTime":   "2026-05-05T14:00:00-06:00",
                        "temperature": { "value": 25.5, "unitCode": "wmoUnit:degC" }
                    }
                ]
            }
        }"#;
        let f = parse_hourly(body).unwrap();
        assert_eq!(f.periods.len(), 1);
        assert!((f.periods[0].temperature - 25.5).abs() < f64::EPSILON);
        assert_eq!(f.periods[0].temperature_unit, "C");
    }

    #[test]
    fn rejects_period_without_temperature() {
        let body = br#"{
            "properties": {
                "periods": [
                    {
                        "startTime": "2026-05-05T13:00:00-06:00",
                        "endTime":   "2026-05-05T14:00:00-06:00"
                    }
                ]
            }
        }"#;
        assert!(parse_hourly(body).is_err());
    }

    #[test]
    fn empty_periods_list_is_ok() {
        let body = br#"{ "properties": { "periods": [] } }"#;
        let f = parse_hourly(body).unwrap();
        assert!(f.periods.is_empty());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_hourly(b"{ malformed").is_err());
    }

    #[test]
    fn unit_from_wmo_strips_prefix() {
        assert_eq!(unit_from_wmo("wmoUnit:degF"), "F");
        assert_eq!(unit_from_wmo("wmoUnit:degC"), "C");
        assert_eq!(unit_from_wmo("unknown"), "unknown");
    }
}
