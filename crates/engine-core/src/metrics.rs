//! Lightweight metrics surface — strategies and OMS push
//! counters / gauges via this trait, the engine binary picks an
//! implementation (Prometheus exporter, in-memory for tests, or
//! null for performance benchmarking).
//!
//! No metric noise budget: keep counter + gauge names short and
//! prefix everything with `predigy_<subsystem>_`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Metric label set. Order-stable string→string map.
pub type Labels = HashMap<&'static str, String>;

#[derive(Debug, Clone, Default)]
pub struct Tags {
    inner: Labels,
}

impl Tags {
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.inner.insert(key, value.into());
        self
    }
    pub fn as_map(&self) -> &Labels {
        &self.inner
    }
}

/// What metric implementations expose. Concrete sinks live in
/// the engine binary.
pub trait Metrics: Send + Sync {
    fn counter_inc(&self, name: &'static str, by: u64, tags: &Tags);
    fn gauge_set(&self, name: &'static str, value: f64, tags: &Tags);
    fn observe(&self, name: &'static str, value: f64, tags: &Tags);
}

/// Test-only implementation that records every event.
#[derive(Debug, Default, Clone)]
pub struct InMemoryMetrics {
    inner: Arc<Mutex<MetricsInner>>,
}

#[derive(Debug, Default)]
struct MetricsInner {
    counters: Vec<(&'static str, u64, Labels)>,
    gauges: Vec<(&'static str, f64, Labels)>,
    observations: Vec<(&'static str, f64, Labels)>,
}

impl InMemoryMetrics {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot_counters(&self) -> Vec<(&'static str, u64, Labels)> {
        self.inner.lock().unwrap().counters.clone()
    }
    pub fn snapshot_gauges(&self) -> Vec<(&'static str, f64, Labels)> {
        self.inner.lock().unwrap().gauges.clone()
    }
    pub fn snapshot_observations(&self) -> Vec<(&'static str, f64, Labels)> {
        self.inner.lock().unwrap().observations.clone()
    }
}

impl Metrics for InMemoryMetrics {
    fn counter_inc(&self, name: &'static str, by: u64, tags: &Tags) {
        self.inner
            .lock()
            .unwrap()
            .counters
            .push((name, by, tags.as_map().clone()));
    }
    fn gauge_set(&self, name: &'static str, value: f64, tags: &Tags) {
        self.inner
            .lock()
            .unwrap()
            .gauges
            .push((name, value, tags.as_map().clone()));
    }
    fn observe(&self, name: &'static str, value: f64, tags: &Tags) {
        self.inner
            .lock()
            .unwrap()
            .observations
            .push((name, value, tags.as_map().clone()));
    }
}

/// Null sink — no-op.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullMetrics;

impl Metrics for NullMetrics {
    fn counter_inc(&self, _: &'static str, _: u64, _: &Tags) {}
    fn gauge_set(&self, _: &'static str, _: f64, _: &Tags) {}
    fn observe(&self, _: &'static str, _: f64, _: &Tags) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_records_each_event() {
        let m = InMemoryMetrics::new();
        let tags = Tags::new().with("strategy", "stat");
        m.counter_inc("predigy_oms_intent_submitted", 1, &tags);
        m.gauge_set("predigy_oms_in_flight", 7.0, &tags);
        m.observe("predigy_oms_submit_latency_ms", 12.4, &tags);
        assert_eq!(m.snapshot_counters().len(), 1);
        assert_eq!(m.snapshot_gauges().len(), 1);
        assert_eq!(m.snapshot_observations().len(), 1);
    }

    #[test]
    fn null_sink_is_zero_op() {
        let m = NullMetrics;
        let tags = Tags::new();
        m.counter_inc("anything", 1, &tags);
        m.gauge_set("anything", 1.0, &tags);
        m.observe("anything", 1.0, &tags);
    }
}
