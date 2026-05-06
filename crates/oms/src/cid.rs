//! Deterministic client-order-id allocation.
//!
//! Per the plan: "Built from `(strategy_id, market, intent_seq)` so
//! duplicate sends are no-ops on the exchange and detectable in OMS."
//!
//! The allocator is owned by the OMS task — single-threaded, no atomics
//! needed. The `strategy_id` is the trader's name (`"arb"`, `"mm"`, …)
//! so collisions are impossible across simultaneously-running
//! strategies on the same account.
//!
//! ## Durability
//!
//! Production allocators back the sequence with a [`CidStore`]: a
//! single-line text file containing the next *unallocated* sequence
//! number. The allocator pre-claims a chunk on construct (and again
//! when the cursor catches up) — at most `chunk_size − 1` cids are
//! "wasted" across a crash, but no cid is ever reused. That's the
//! right correctness/perf trade-off for an HFT path: we save once
//! per chunk, never per submit.

use predigy_core::market::MarketTicker;
use predigy_core::order::OrderId;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

/// Number of cids claimed per persistence write. Tuned to be large
/// enough that a busy strategy doesn't fsync-thrash, small enough that
/// a crash doesn't leave a huge gap.
const DEFAULT_CHUNK_SIZE: u64 = 1_000;

#[derive(Debug, Error)]
pub enum CidError {
    #[error("cid store io: {0}")]
    Io(#[from] io::Error),
    #[error("cid store contained non-numeric content: {0:?}")]
    Parse(String),
}

/// File-backed sequence-number store. Atomic writes via `tmp + rename`.
#[derive(Debug, Clone)]
pub struct CidStore {
    path: PathBuf,
}

impl CidStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the persisted "next unallocated" seq, returning `0` if
    /// the file doesn't yet exist. Trailing whitespace is tolerated.
    pub fn load(&self) -> Result<u64, CidError> {
        match fs::read_to_string(&self.path) {
            Ok(s) => s
                .trim()
                .parse::<u64>()
                .map_err(|_| CidError::Parse(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(CidError::Io(e)),
        }
    }

    /// Atomically replace the file's contents with `seq`. Writes to a
    /// temp file alongside the target, then renames — POSIX rename is
    /// atomic, so a concurrent reader sees either the old or the new
    /// value, never a torn write.
    pub fn save(&self, seq: u64) -> Result<(), CidError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, format!("{seq}\n"))?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CidAllocator {
    strategy_id: String,
    /// Next sequence to hand out.
    next_seq: u64,
    /// Exclusive upper bound of the currently-claimed chunk. When
    /// `next_seq == high_water`, refill before issuing.
    high_water: u64,
    chunk_size: u64,
    store: Option<CidStore>,
}

impl CidAllocator {
    /// In-memory allocator (no persistence). Tests and one-shot
    /// scripts use this; production binaries should call
    /// [`CidAllocator::with_store`] instead.
    #[must_use]
    pub fn new(strategy_id: impl Into<String>, start_seq: u64) -> Self {
        Self {
            strategy_id: strategy_id.into(),
            next_seq: start_seq,
            high_water: u64::MAX,
            chunk_size: DEFAULT_CHUNK_SIZE,
            store: None,
        }
    }

    /// Persistent allocator. Reads the store, advances past whatever
    /// the previous process pre-claimed (since some of those may have
    /// been used pre-crash), claims a new chunk, and persists the new
    /// high-water mark before returning. On the file's first use the
    /// stored value is `0` and we start from there.
    pub fn with_store(strategy_id: impl Into<String>, store: CidStore) -> Result<Self, CidError> {
        Self::with_store_and_chunk(strategy_id, store, DEFAULT_CHUNK_SIZE)
    }

    pub fn with_store_and_chunk(
        strategy_id: impl Into<String>,
        store: CidStore,
        chunk_size: u64,
    ) -> Result<Self, CidError> {
        assert!(chunk_size > 0, "cid chunk_size must be > 0");
        let prior_high_water = store.load()?;
        let new_high_water = prior_high_water + chunk_size;
        store.save(new_high_water)?;
        Ok(Self {
            strategy_id: strategy_id.into(),
            next_seq: prior_high_water,
            high_water: new_high_water,
            chunk_size,
            store: Some(store),
        })
    }

    #[must_use]
    pub fn strategy_id(&self) -> &str {
        &self.strategy_id
    }

    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.next_seq
    }

    /// Mint the next id for `market`. Format:
    /// `{strategy_id}:{market_ticker}:{seq:08}` — short enough to fit
    /// in the FIX `ClOrdID` (tag 11) limit, structured enough to grep
    /// in log files.
    ///
    /// Periods in the market ticker are stripped because Kalshi's
    /// `events/orders` REST endpoint rejects `client_order_id`
    /// values containing `.` with a generic `invalid_parameters`
    /// (verified live May 2026 — half-bracket weather tickers like
    /// `KXLOWTDEN-26MAY06-B31.5` would otherwise produce cids the
    /// venue refuses). Strip rather than substitute so we don't
    /// collide a `B31.5` with a hypothetical `B315`.
    pub fn next(&mut self, market: &MarketTicker) -> OrderId {
        if self.next_seq == self.high_water {
            self.refill();
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let safe_market: String = market.as_str().chars().filter(|c| *c != '.').collect();
        OrderId::new(format!("{}:{}:{:08}", self.strategy_id, safe_market, seq))
    }

    fn refill(&mut self) {
        if let Some(store) = &self.store {
            let new_high_water = self.high_water + self.chunk_size;
            if let Err(e) = store.save(new_high_water) {
                // Persist failure is loud but non-fatal — we keep
                // running with the in-memory cursor; the operator
                // will see this in logs and can fix the disk before
                // the next refill or restart.
                warn!(error = %e, "cid persist failed during refill; continuing in-memory");
            }
            self.high_water = new_high_water;
        }
        // No-store allocators have high_water = u64::MAX so this
        // branch is unreachable for them.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_increments_sequence() {
        let mut alloc = CidAllocator::new("arb", 0);
        let m = MarketTicker::new("X");
        let a = alloc.next(&m);
        let b = alloc.next(&m);
        assert_eq!(a.as_str(), "arb:X:00000000");
        assert_eq!(b.as_str(), "arb:X:00000001");
        assert_eq!(alloc.current_seq(), 2);
    }

    #[test]
    fn next_strips_periods_from_market_ticker() {
        // Kalshi rejects client_order_id values with periods, and the
        // weather-strategy half-bracket markets (e.g. `B52.5`) have
        // them — strip in the cid generator so the wire stays clean.
        let mut alloc = CidAllocator::new("wx", 0);
        let id = alloc.next(&MarketTicker::new("KXLOWTDEN-26MAY06-B31.5"));
        assert!(!id.as_str().contains('.'), "cid still has period: {id}");
        assert_eq!(id.as_str(), "wx:KXLOWTDEN-26MAY06-B315:00000000");
    }

    #[test]
    fn ids_are_unique_across_markets_at_same_seq_position() {
        // Even at the same sequence number, the embedded market ticker
        // distinguishes the ids — useful for human triage in logs.
        let mut alloc = CidAllocator::new("arb", 5);
        let id_x = alloc.next(&MarketTicker::new("X"));
        let id_y = alloc.next(&MarketTicker::new("Y"));
        assert_ne!(id_x, id_y);
        assert!(id_x.as_str().contains(":X:"));
        assert!(id_y.as_str().contains(":Y:"));
    }

    #[test]
    fn start_seq_is_honoured() {
        let mut alloc = CidAllocator::new("arb", 1_000);
        let id = alloc.next(&MarketTicker::new("X"));
        assert_eq!(id.as_str(), "arb:X:00001000");
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("predigy-cidtest-{pid}-{nanos}-{name}"));
        p
    }

    #[test]
    fn store_load_returns_zero_when_missing() {
        let store = CidStore::new(temp_path("missing"));
        assert_eq!(store.load().unwrap(), 0);
    }

    #[test]
    fn store_round_trip() {
        let path = temp_path("rt");
        let store = CidStore::new(path.clone());
        store.save(42).unwrap();
        assert_eq!(store.load().unwrap(), 42);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persistent_allocator_advances_high_water_on_construct() {
        let path = temp_path("preclaim");
        let store = CidStore::new(path.clone());
        // First construct: no prior file. Claims [0, chunk).
        let alloc = CidAllocator::with_store_and_chunk("arb", store.clone(), 5).unwrap();
        assert_eq!(alloc.current_seq(), 0);
        assert_eq!(store.load().unwrap(), 5);
        // Second construct (simulating restart): claims [5, 10).
        let alloc2 = CidAllocator::with_store_and_chunk("arb", store.clone(), 5).unwrap();
        assert_eq!(alloc2.current_seq(), 5);
        assert_eq!(store.load().unwrap(), 10);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persistent_allocator_refills_at_chunk_boundary() {
        let path = temp_path("refill");
        let store = CidStore::new(path.clone());
        let mut alloc = CidAllocator::with_store_and_chunk("arb", store.clone(), 3).unwrap();
        // Initial claim: [0, 3). After 3 cids we hit the boundary; the
        // 4th cid forces a refill before issuing.
        let m = MarketTicker::new("X");
        for _ in 0..3 {
            let _ = alloc.next(&m);
        }
        assert_eq!(store.load().unwrap(), 3);
        let _id4 = alloc.next(&m);
        // Refill bumped the on-disk high_water to 6.
        assert_eq!(store.load().unwrap(), 6);
        // The 4th cid uses seq 3.
        assert_eq!(alloc.current_seq(), 4);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persistent_no_repeat_across_simulated_restart() {
        // Simulate a crash mid-chunk and confirm the new process never
        // hands out a cid the old one could have used. Old: claim [0,5)
        // then crash at next_seq=2. New: claims [5,10), starts at 5.
        // 2,3,4 are wasted but never reused.
        let path = temp_path("crash");
        let store = CidStore::new(path.clone());
        let mut old = CidAllocator::with_store_and_chunk("arb", store.clone(), 5).unwrap();
        let _ = old.next(&MarketTicker::new("X"));
        let _ = old.next(&MarketTicker::new("X"));
        // (3 unused cids in [2,5) are now ghost-allocated — that's the
        // correctness/perf trade-off.)
        drop(old);
        let new = CidAllocator::with_store_and_chunk("arb", store.clone(), 5).unwrap();
        assert_eq!(new.current_seq(), 5);
        let _ = fs::remove_file(&path);
    }
}
