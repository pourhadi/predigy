//! Pair-file service — watches the cross-arb-curator's output
//! file for changes, diffs, and emits [`Event::PairUpdate`] into
//! the cross-arb supervisor's queue.
//!
//! Side effects per tick where the file changed:
//!
//! 1. **Router subscribe** for added Kalshi tickers via
//!    `RouterCommand::AddTickers`.
//! 2. **Polymarket subscribe** for added asset_ids via
//!    `PolyFeedCommand::AddAssets`.
//! 3. **PolyFeedCommand::PruneAssets** for removed asset_ids so the
//!    next reconnect doesn't re-subscribe to dropped pairs (Poly
//!    has no in-band unsubscribe).
//! 4. **Event::PairUpdate** dispatched to the cross-arb
//!    supervisor's event channel.
//!
//! Watch mechanism: mtime poll (default 30s). The legacy daemon
//! used the same mechanism — inotify/kqueue would be more
//! efficient but the file changes max once every 10 minutes
//! (cross-arb-curator's StartInterval) so polling is fine.
//!
//! Pair-file format: same as the legacy daemon's
//! `bin/cross-arb-trader/src/pair_file.rs`. One pair per line:
//! `KALSHI_TICKER=POLY_ASSET_ID`. Blank lines + `#` comments
//! tolerated. Whitespace trimmed.

use anyhow::{Context as _, Result};
use predigy_core::market::MarketTicker;
use predigy_engine_core::events::{Event, KalshiPolyPair};
use predigy_engine_core::strategy::StrategyId;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::external_feeds::{PolyCommandTx, PolyFeedCommand};
use crate::market_data::RouterCommand;

#[derive(Debug, Clone)]
pub struct PairFileConfig {
    pub path: PathBuf,
    pub poll_interval: Duration,
    pub strategy: StrategyId,
}

pub struct PairFileService {
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for PairFileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairFileService").finish_non_exhaustive()
    }
}

impl PairFileService {
    pub fn start(
        config: PairFileConfig,
        router_tx: mpsc::Sender<RouterCommand>,
        poly_tx: PolyCommandTx,
        event_tx: mpsc::Sender<Event>,
    ) -> Self {
        let task = tokio::spawn(run(config, router_tx, poly_tx, event_tx));
        Self { task }
    }

    pub async fn shutdown(self, grace: Duration) {
        self.task.abort();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

async fn run(
    config: PairFileConfig,
    router_tx: mpsc::Sender<RouterCommand>,
    poly_tx: PolyCommandTx,
    event_tx: mpsc::Sender<Event>,
) {
    let mut tick = tokio::time::interval(config.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_mtime: Option<SystemTime> = None;
    let mut current: HashMap<String, String> = HashMap::new();

    loop {
        tick.tick().await;
        let mtime = match std::fs::metadata(&config.path).and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %config.path.display(),
                    "pair-file: not present yet; skipping tick"
                );
                continue;
            }
            Err(e) => {
                warn!(
                    path = %config.path.display(),
                    error = %e,
                    "pair-file: stat failed"
                );
                continue;
            }
        };
        if Some(mtime) == last_mtime {
            continue;
        }

        let next = match read_pairs(&config.path) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    path = %config.path.display(),
                    error = %e,
                    "pair-file: parse failed"
                );
                last_mtime = Some(mtime);
                continue;
            }
        };
        let (added, removed) = diff(&current, &next);
        if added.is_empty() && removed.is_empty() {
            last_mtime = Some(mtime);
            continue;
        }
        info!(
            n_added = added.len(),
            n_removed = removed.len(),
            n_total = next.len(),
            "pair-file: change detected"
        );

        // 1. Tell the router about added Kalshi tickers FIRST so
        //    book updates start flowing before the strategy sees
        //    the PairUpdate.
        if !added.is_empty() {
            let tickers: Vec<String> = added.iter().map(|p| p.kalshi_ticker.as_str().to_string()).collect();
            let cmd = RouterCommand::AddTickers {
                strategy: config.strategy,
                markets: tickers,
                tx: event_tx.clone(),
            };
            if router_tx.send(cmd).await.is_err() {
                warn!("pair-file: router cmd channel closed; exiting");
                return;
            }
        }

        // 2. Tell the poly dispatcher about added asset_ids.
        if !added.is_empty() {
            let assets: Vec<String> = added.iter().map(|p| p.poly_asset_id.clone()).collect();
            if poly_tx
                .send(PolyFeedCommand::AddAssets(assets))
                .await
                .is_err()
            {
                warn!("pair-file: poly cmd channel closed; exiting");
                return;
            }
        }

        // 3. Prune removed assets from the poly saved-sub set
        //    (no in-band unsubscribe; this just stops the next
        //    reconnect from re-subscribing to dropped pairs).
        if !removed.is_empty() {
            // Look up the asset_ids that go with the removed
            // tickers. They're still in `current` at this point.
            let removed_assets: Vec<String> = removed
                .iter()
                .filter_map(|t| current.get(t.as_str()).cloned())
                .collect();
            if !removed_assets.is_empty() {
                let _ = poly_tx
                    .send(PolyFeedCommand::PruneAssets(removed_assets))
                    .await;
            }
        }

        // 4. Push the delta to the strategy.
        let pair_update = Event::PairUpdate {
            added: added.clone(),
            removed: removed.clone(),
        };
        if event_tx.send(pair_update).await.is_err() {
            warn!("pair-file: strategy event channel closed; exiting");
            return;
        }

        // Update bookkeeping.
        for p in &added {
            current.insert(p.kalshi_ticker.as_str().to_string(), p.poly_asset_id.clone());
        }
        for t in &removed {
            current.remove(t.as_str());
        }
        last_mtime = Some(mtime);
    }
}

/// Parse the pair file. One pair per line, `KALSHI=POLY`. Blank
/// lines and `#` comments tolerated. Returns a map for diff
/// purposes.
fn read_pairs(path: &Path) -> Result<HashMap<String, String>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read pair file {}", path.display()))?;
    let mut out = HashMap::new();
    for (line_no, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, p)) = trimmed.split_once('=') else {
            warn!(
                line_no = line_no + 1,
                content = trimmed,
                "pair-file: line missing '=' separator; skipping"
            );
            continue;
        };
        let k = k.trim().to_string();
        let p = p.trim().to_string();
        if k.is_empty() || p.is_empty() {
            warn!(
                line_no = line_no + 1,
                "pair-file: empty kalshi or poly side; skipping"
            );
            continue;
        }
        out.insert(k, p);
    }
    Ok(out)
}

fn diff(
    prev: &HashMap<String, String>,
    next: &HashMap<String, String>,
) -> (Vec<KalshiPolyPair>, Vec<MarketTicker>) {
    let prev_keys: HashSet<&String> = prev.keys().collect();
    let next_keys: HashSet<&String> = next.keys().collect();
    let added: Vec<KalshiPolyPair> = next_keys
        .difference(&prev_keys)
        .map(|k| KalshiPolyPair {
            kalshi_ticker: MarketTicker::new((*k).clone()),
            poly_asset_id: next[*k].clone(),
        })
        .collect();
    let removed: Vec<MarketTicker> = prev_keys
        .difference(&next_keys)
        .map(|k| MarketTicker::new((*k).clone()))
        .collect();
    (added, removed)
}

/// Pull the pair-file path from env var. Returns `None` if not
/// set; the engine then skips registering cross-arb.
pub fn pair_file_from_env() -> Option<PathBuf> {
    std::env::var("PREDIGY_CROSS_ARB_PAIR_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pairs_parses_basic_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.txt");
        std::fs::write(
            &path,
            "# header comment\n\
             KX-A=0xabc\n\
             \n\
             KX-B=0xdef  \n\
             # mid comment\n\
             KX-C  =  0x111\n",
        )
        .unwrap();
        let m = read_pairs(&path).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("KX-A"), Some(&"0xabc".to_string()));
        assert_eq!(m.get("KX-C"), Some(&"0x111".to_string()));
    }

    #[test]
    fn read_pairs_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.txt");
        std::fs::write(
            &path,
            "KX-A=0xabc\n\
             malformed-no-equals\n\
             =0xempty-kalshi\n\
             KX-B=\n\
             KX-C=0xfine\n",
        )
        .unwrap();
        let m = read_pairs(&path).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("KX-A"));
        assert!(m.contains_key("KX-C"));
    }

    #[test]
    fn diff_emits_only_changes() {
        let mut prev = HashMap::new();
        prev.insert("A".to_string(), "0xa".to_string());
        prev.insert("B".to_string(), "0xb".to_string());
        let mut next = HashMap::new();
        next.insert("B".to_string(), "0xb".to_string());
        next.insert("C".to_string(), "0xc".to_string());
        let (added, removed) = diff(&prev, &next);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kalshi_ticker.as_str(), "C");
        assert_eq!(added[0].poly_asset_id, "0xc");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].as_str(), "A");
    }
}
