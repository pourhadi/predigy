//! Persistent state for the incremental curator.
//!
//! Loaded at the start of each tick and written at the end. Tracks
//! the live pair set + every Polymarket id ever sent to Claude so
//! repeat runs don't re-pay for the same candidates. Atomic-rename
//! pattern (`tmp` + `rename`) — same as wx-curator and the OMS.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CuratorState {
    #[serde(default = "schema_v1")]
    pub schema_version: u32,
    /// Active pairs that should be in the output file.
    #[serde(default)]
    pub pairs: Vec<StoredPair>,
    /// Polymarket market ids ever sent to Claude. Skipping these
    /// on subsequent ticks is the whole point of the incremental
    /// model — a candidate that Claude rejected once shouldn't be
    /// re-evaluated every tick.
    #[serde(default)]
    pub seen_poly_ids: Vec<String>,
}

const fn schema_v1() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPair {
    pub kalshi_ticker: String,
    pub poly_token_id: String,
    /// Polymarket market id (NOT the token id). Lets us check
    /// whether the market is still listed without a separate index.
    pub poly_market_id: String,
    pub reasoning: String,
    pub added_unix: i64,
}

impl CuratorState {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_string_pretty(self).expect("serialize curator state");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn seen_set(&self) -> HashSet<&str> {
        self.seen_poly_ids.iter().map(String::as_str).collect()
    }

    pub fn paired_kalshi(&self) -> HashSet<&str> {
        self.pairs
            .iter()
            .map(|p| p.kalshi_ticker.as_str())
            .collect()
    }

    pub fn paired_poly(&self) -> HashSet<&str> {
        self.pairs
            .iter()
            .map(|p| p.poly_market_id.as_str())
            .collect()
    }

    pub fn add_pair(&mut self, pair: StoredPair) {
        self.pairs.push(pair);
    }

    /// Drop pairs whose Kalshi ticker is no longer in `open_set`.
    /// Returns the dropped tickers (for logging).
    pub fn retain_open(&mut self, open_set: &HashSet<&str>) -> Vec<String> {
        let mut dropped = Vec::new();
        self.pairs.retain(|p| {
            if open_set.contains(p.kalshi_ticker.as_str()) {
                true
            } else {
                dropped.push(p.kalshi_ticker.clone());
                false
            }
        });
        dropped
    }

    pub fn record_seen<I: IntoIterator<Item = String>>(&mut self, ids: I) {
        let mut set: HashSet<String> = self.seen_poly_ids.drain(..).collect();
        for id in ids {
            set.insert(id);
        }
        self.seen_poly_ids = set.into_iter().collect();
        self.seen_poly_ids.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pair(k: &str, pm: &str) -> StoredPair {
        StoredPair {
            kalshi_ticker: k.into(),
            poly_token_id: "1".repeat(40),
            poly_market_id: pm.into(),
            reasoning: "test".into(),
            added_unix: 1_700_000_000,
        }
    }

    #[test]
    fn load_returns_default_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let s = CuratorState::load(&path);
        assert!(s.pairs.is_empty());
        assert!(s.seen_poly_ids.is_empty());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = CuratorState::default();
        s.add_pair(pair("KX-A", "P1"));
        s.record_seen(["P1".into(), "P2".into()]);
        s.save(&path).unwrap();
        let loaded = CuratorState::load(&path);
        assert_eq!(loaded.pairs.len(), 1);
        assert_eq!(loaded.pairs[0].kalshi_ticker, "KX-A");
        assert_eq!(loaded.seen_poly_ids.len(), 2);
    }

    #[test]
    fn retain_open_drops_settled() {
        let mut s = CuratorState::default();
        s.add_pair(pair("KX-A", "P1"));
        s.add_pair(pair("KX-B", "P2"));
        let mut open: HashSet<&str> = HashSet::new();
        open.insert("KX-A");
        let dropped = s.retain_open(&open);
        assert_eq!(dropped, vec!["KX-B"]);
        assert_eq!(s.pairs.len(), 1);
    }

    #[test]
    fn record_seen_dedups() {
        let mut s = CuratorState::default();
        s.record_seen(["A".into(), "B".into()]);
        s.record_seen(["B".into(), "C".into()]);
        assert_eq!(s.seen_poly_ids, vec!["A", "B", "C"]);
    }
}
