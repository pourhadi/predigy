//! National Weather Service active-alerts client (Phase 6 free feed).
//!
//! Polls `https://api.weather.gov/alerts/active` on a configurable
//! interval, deduplicates by alert id, and emits one [`NwsAlert`] per
//! newly-seen alert through an `mpsc::Receiver`.
//!
//! ## Why polling, not push
//!
//! NWS doesn't ship a public push API; the active-alerts endpoint is
//! the canonical free feed and is safe to poll at low frequency.
//! `User-Agent` is required by NWS — they ban anonymous pollers.
//!
//! ## What the feed surfaces
//!
//! Only the fields the strategy layer needs to act: `event_type`
//! ("Tornado Warning", "Severe Thunderstorm Watch", …), severity,
//! urgency, area, and the onset/expires timestamps. The full CAP
//! payload is dropped — strategies that need it can extend
//! [`AlertProperties`] and re-parse.

use crate::error::Error;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const DEFAULT_BASE: &str = "https://api.weather.gov/alerts/active";

/// Minimum NWS-accepted poll interval. The agency asks "do not abuse
/// the service"; we keep the default well above what an interactive
/// human refresh would do.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct NwsAlertsConfig {
    /// Two-letter US state/territory codes (`"TX"`, `"CA"`, …).
    /// Empty means "all states" — uses the unfiltered endpoint.
    pub states: Vec<String>,
    /// How often to poll. Subject to [`MIN_POLL_INTERVAL`].
    pub poll_interval: Duration,
    /// `User-Agent` header — NWS requires identifying contact info.
    /// Format suggestion from the docs: `"(myapp.com, x@y.com)"`.
    pub user_agent: String,
    /// Override the base URL (testing / sandbox).
    pub base_url: Option<String>,
}

impl NwsAlertsConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if self.user_agent.trim().is_empty() {
            return Err(Error::Invalid(
                "NWS requires a non-empty User-Agent identifying the caller".into(),
            ));
        }
        if self.poll_interval < MIN_POLL_INTERVAL {
            return Err(Error::Invalid(format!(
                "poll_interval {ms}ms below NWS-recommended minimum {min}ms",
                ms = self.poll_interval.as_millis(),
                min = MIN_POLL_INTERVAL.as_millis(),
            )));
        }
        for s in &self.states {
            if s.len() != 2 || !s.chars().all(|c| c.is_ascii_uppercase()) {
                return Err(Error::Invalid(format!(
                    "state code {s:?} must be 2 ASCII uppercase letters"
                )));
            }
        }
        Ok(())
    }
}

/// One alert as surfaced to the strategy layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NwsAlert {
    /// NWS-assigned id; deduplication key.
    pub id: String,
    /// `"Tornado Warning"`, `"Heat Advisory"`, etc.
    pub event_type: String,
    /// `"Extreme"`, `"Severe"`, `"Moderate"`, `"Minor"`, `"Unknown"`.
    pub severity: String,
    /// `"Immediate"`, `"Expected"`, `"Future"`, `"Past"`, `"Unknown"`.
    pub urgency: String,
    /// Free-text human-readable area (e.g. `"Travis, TX; Hays, TX"`).
    pub area_desc: String,
    /// ISO-8601 timestamp when the alert was issued.
    pub effective: Option<String>,
    /// ISO-8601 timestamp when the event begins (may equal `effective`).
    pub onset: Option<String>,
    /// ISO-8601 timestamp when the alert expires.
    pub expires: Option<String>,
    /// One-line summary, e.g. `"Tornado Warning issued April 3 ..."`.
    pub headline: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlertCollection {
    #[serde(default)]
    features: Vec<AlertFeature>,
}

#[derive(Debug, Deserialize)]
struct AlertFeature {
    #[serde(default)]
    id: String,
    #[serde(default)]
    properties: AlertProperties,
}

#[derive(Debug, Deserialize, Default)]
pub struct AlertProperties {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub urgency: Option<String>,
    #[serde(default, rename = "areaDesc")]
    pub area_desc: Option<String>,
    #[serde(default)]
    pub effective: Option<String>,
    #[serde(default)]
    pub onset: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub headline: Option<String>,
}

impl AlertFeature {
    fn into_alert(self) -> Option<NwsAlert> {
        let id = self.properties.id.clone().unwrap_or(self.id);
        if id.is_empty() {
            return None;
        }
        Some(NwsAlert {
            id,
            event_type: self.properties.event.unwrap_or_else(|| "Unknown".into()),
            severity: self.properties.severity.unwrap_or_else(|| "Unknown".into()),
            urgency: self.properties.urgency.unwrap_or_else(|| "Unknown".into()),
            area_desc: self.properties.area_desc.unwrap_or_default(),
            effective: self.properties.effective,
            onset: self.properties.onset,
            expires: self.properties.expires,
            headline: self.properties.headline,
        })
    }
}

/// Spawn the polling task. Returns the receiver half of an
/// `mpsc::channel`; the task aborts on drop of the returned
/// `JoinHandle`.
pub fn spawn(config: NwsAlertsConfig) -> Result<(mpsc::Receiver<NwsAlert>, JoinHandle<()>), Error> {
    config.validate()?;
    let client = reqwest::Client::builder()
        .user_agent(config.user_agent.clone())
        .timeout(Duration::from_secs(15))
        .build()?;
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(run(client, config, tx));
    Ok((rx, task))
}

async fn run(client: reqwest::Client, config: NwsAlertsConfig, tx: mpsc::Sender<NwsAlert>) {
    let url = build_url(&config);
    let mut seen: HashSet<String> = HashSet::new();
    info!(url = %url, ?config.poll_interval, "nws-alerts: polling");
    let mut tick = tokio::time::interval(config.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let url = Arc::new(url);
    loop {
        tick.tick().await;
        if tx.is_closed() {
            debug!("nws-alerts: receiver dropped; exiting");
            return;
        }
        match fetch_alerts(&client, &url).await {
            Ok(alerts) => {
                for alert in alerts {
                    if seen.insert(alert.id.clone()) && tx.send(alert).await.is_err() {
                        debug!("nws-alerts: receiver dropped mid-batch; exiting");
                        return;
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "nws-alerts: poll failed; will retry next tick");
            }
        }
    }
}

fn build_url(config: &NwsAlertsConfig) -> String {
    let base = config.base_url.as_deref().unwrap_or(DEFAULT_BASE);
    if config.states.is_empty() {
        base.to_string()
    } else {
        // NWS accepts repeated `area=XX` query params for multiple states.
        let params: Vec<String> = config.states.iter().map(|s| format!("area={s}")).collect();
        format!("{base}?{}", params.join("&"))
    }
}

async fn fetch_alerts(client: &reqwest::Client, url: &str) -> Result<Vec<NwsAlert>, Error> {
    let resp = client
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
    let bytes = resp.bytes().await?;
    let collection: AlertCollection = serde_json::from_slice(&bytes)?;
    let alerts: Vec<NwsAlert> = collection
        .features
        .into_iter()
        .filter_map(AlertFeature::into_alert)
        .collect();
    Ok(alerts)
}

/// Pure parse helper, exposed for tests and offline replay tools.
pub fn parse_collection(body: &[u8]) -> Result<Vec<NwsAlert>, Error> {
    let collection: AlertCollection = serde_json::from_slice(body)?;
    Ok(collection
        .features
        .into_iter()
        .filter_map(AlertFeature::into_alert)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> NwsAlertsConfig {
        NwsAlertsConfig {
            states: vec!["TX".into()],
            poll_interval: Duration::from_mins(1),
            user_agent: "test/1.0 (test@example.com)".into(),
            base_url: None,
        }
    }

    #[test]
    fn validate_rejects_empty_user_agent() {
        let mut c = cfg();
        c.user_agent = String::new();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_short_poll() {
        let mut c = cfg();
        c.poll_interval = Duration::from_secs(1);
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_lowercase_state() {
        let mut c = cfg();
        c.states = vec!["tx".into()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_three_letter_state() {
        let mut c = cfg();
        c.states = vec!["TEX".into()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_accepts_no_states() {
        let mut c = cfg();
        c.states.clear();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn build_url_no_states_uses_base() {
        let mut c = cfg();
        c.states.clear();
        assert_eq!(build_url(&c), DEFAULT_BASE);
    }

    #[test]
    fn build_url_repeats_area_per_state() {
        let mut c = cfg();
        c.states = vec!["TX".into(), "CA".into()];
        assert_eq!(build_url(&c), format!("{DEFAULT_BASE}?area=TX&area=CA"));
    }

    #[test]
    fn parse_collection_extracts_known_fields() {
        let body = br#"{
            "features": [{
                "id": "urn:oid:2.49.0.1.840.0.1",
                "properties": {
                    "id": "urn:oid:2.49.0.1.840.0.1",
                    "event": "Tornado Warning",
                    "severity": "Extreme",
                    "urgency": "Immediate",
                    "areaDesc": "Travis, TX; Hays, TX",
                    "effective": "2026-05-04T18:00:00Z",
                    "onset":     "2026-05-04T18:05:00Z",
                    "expires":   "2026-05-04T19:00:00Z",
                    "headline":  "Tornado Warning issued..."
                }
            }]
        }"#;
        let alerts = parse_collection(body).unwrap();
        assert_eq!(alerts.len(), 1);
        let a = &alerts[0];
        assert_eq!(a.event_type, "Tornado Warning");
        assert_eq!(a.severity, "Extreme");
        assert_eq!(a.urgency, "Immediate");
        assert_eq!(a.area_desc, "Travis, TX; Hays, TX");
        assert_eq!(a.expires.as_deref(), Some("2026-05-04T19:00:00Z"));
    }

    #[test]
    fn parse_collection_drops_id_less_features() {
        let body = br#"{ "features": [{ "id": "", "properties": {} }] }"#;
        let alerts = parse_collection(body).unwrap();
        assert!(alerts.is_empty());
    }

    #[test]
    fn parse_collection_handles_missing_optional_fields() {
        let body = br#"{
            "features": [{
                "id": "urn:oid:1",
                "properties": { "event": "Heat Advisory" }
            }]
        }"#;
        let alerts = parse_collection(body).unwrap();
        assert_eq!(alerts.len(), 1);
        let a = &alerts[0];
        assert_eq!(a.event_type, "Heat Advisory");
        assert_eq!(a.severity, "Unknown");
        assert!(a.expires.is_none());
    }

    #[test]
    fn parse_collection_empty_features_list_is_ok() {
        let alerts = parse_collection(b"{\"features\": []}").unwrap();
        assert!(alerts.is_empty());
    }

    #[test]
    fn parse_collection_rejects_bad_json() {
        assert!(parse_collection(b"{ malformed").is_err());
    }
}
