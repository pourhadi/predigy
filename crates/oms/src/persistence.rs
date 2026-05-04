//! On-disk OMS state snapshots.
//!
//! The OMS owns three pieces of long-lived state:
//!
//! 1. The cid sequence — already persisted via [`CidBacking`](crate::cid::CidBacking).
//! 2. Account state: positions, daily realised P&L, kill-switch.
//!    The risk engine reads this on every submit; if it resets to
//!    zero on restart, the daily-loss breaker silently re-arms
//!    itself. Bad.
//! 3. The orders map: every in-flight (or recently-terminal) order.
//!    On a crash mid-flight, in-flight orders continue to exist on
//!    the venue. Without this map, the OMS treats their fills as
//!    "untracked" and silently drops them — exactly the orphan
//!    situation we hit during the live shake-down.
//!
//! This module persists (2) and (3). The serialisation format is
//! a single JSON snapshot, written via tmp-file + `rename` so
//! readers see either the old version or the new, never a torn
//! write. Snapshots are taken after every state mutation.
//!
//! ## What's not persisted
//!
//! - Rate-limit sliding window (uses `Instant`; allowed to reset
//!   on restart — worst case is a few extra orders in the first
//!   second).
//! - `submitted_at` / `last_event_at` on individual order records
//!   (also `Instant`; replaced with the load-time on resume).
//!
//! ## Schema versioning
//!
//! `schema_version` is bumped when the on-disk shape changes
//! incompatibly. On load, an unknown version is a hard error so
//! we don't silently mis-deserialise into a half-correct ledger.

use crate::record::OrderRecord;
use predigy_core::order::{Order, OrderId, OrderState};
use predigy_risk::{AccountState, PersistedAccountState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use thiserror::Error;

/// Current snapshot schema version. Bump when an incompatible
/// change lands.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unknown snapshot schema_version {found}; this build supports {supported}")]
    SchemaMismatch { found: u32, supported: u32 },
}

/// Where the OMS writes its state snapshot.
#[derive(Debug, Clone, Default)]
pub enum StateBacking {
    /// No persistence — the orders map and account state live only
    /// in memory. Default for tests and one-shot scripts.
    #[default]
    InMemory,
    /// Atomic-rename JSON snapshot at `path`. The file is rewritten
    /// after every state mutation. Acceptable for non-MM strategies
    /// (rate ≤ ~100 mutations/sec); MM/HFT strategies should keep
    /// `InMemory` and rely on reconciliation against the venue
    /// instead.
    Persistent { path: PathBuf },
}

/// The shape that lands on disk. Top-level wrapper so we can grow
/// it (audit-log digests, last-snapshot-time, etc.) without
/// breaking the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOmsState {
    pub schema_version: u32,
    pub account: PersistedAccountState,
    pub orders: Vec<PersistedOrderRecord>,
}

/// Persistable subset of [`OrderRecord`]. The `Instant` timestamps
/// are intentionally dropped — on resume they're set to the
/// load-time, since their main use is stale-order alerting which
/// only makes sense relative to a continuous run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOrderRecord {
    pub cid: OrderId,
    pub order: Order,
    pub state: OrderState,
    pub cumulative_qty: u32,
    pub avg_fill_price_cents: u16,
    pub cancel_in_flight: bool,
    pub venue_order_id: Option<String>,
}

impl PersistedOrderRecord {
    fn from_record(r: &OrderRecord) -> Self {
        Self {
            cid: r.cid.clone(),
            order: r.order.clone(),
            state: r.state,
            cumulative_qty: r.cumulative_qty,
            avg_fill_price_cents: r.avg_fill_price_cents,
            cancel_in_flight: r.cancel_in_flight,
            venue_order_id: r.venue_order_id.clone(),
        }
    }

    fn into_record(self, now: Instant) -> OrderRecord {
        OrderRecord {
            cid: self.cid,
            order: self.order,
            state: self.state,
            cumulative_qty: self.cumulative_qty,
            avg_fill_price_cents: self.avg_fill_price_cents,
            cancel_in_flight: self.cancel_in_flight,
            venue_order_id: self.venue_order_id,
            submitted_at: now,
            last_event_at: now,
        }
    }
}

/// Snapshot the OMS's state into a serialisable wrapper.
#[allow(clippy::implicit_hasher)] // OMS owns the only HashMap
pub fn snapshot(
    account: &AccountState,
    orders: &HashMap<OrderId, OrderRecord>,
) -> PersistedOmsState {
    let mut orders_vec: Vec<PersistedOrderRecord> = orders
        .values()
        .map(PersistedOrderRecord::from_record)
        .collect();
    // Stable order for byte-deterministic snapshots.
    orders_vec.sort_by(|a, b| a.cid.as_str().cmp(b.cid.as_str()));
    PersistedOmsState {
        schema_version: SCHEMA_VERSION,
        account: account.to_persisted(),
        orders: orders_vec,
    }
}

/// Atomic write to `path`: serialise → tmp file → `rename`.
pub fn save(path: &Path, state: &PersistedOmsState) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a snapshot. Returns `Ok(None)` if the file doesn't exist
/// (first-run case); `Err` on partial / corrupt files so the
/// operator notices rather than the OMS silently starting fresh.
pub fn load(path: &Path) -> Result<Option<PersistedOmsState>, StateError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StateError::Io(e)),
    };
    let state: PersistedOmsState = serde_json::from_slice(&bytes)?;
    if state.schema_version != SCHEMA_VERSION {
        return Err(StateError::SchemaMismatch {
            found: state.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(Some(state))
}

/// Rehydrate an `AccountState` and orders map from a snapshot.
/// `now` is the resume-time used for `submitted_at` / `last_event_at`
/// on each order.
#[must_use]
pub fn rehydrate(
    state: PersistedOmsState,
    now: Instant,
) -> (AccountState, HashMap<OrderId, OrderRecord>) {
    let account = AccountState::from_persisted(&state.account);
    let mut orders = HashMap::with_capacity(state.orders.len());
    for p in state.orders {
        let cid = p.cid.clone();
        orders.insert(cid, p.into_record(now));
    }
    (account, orders)
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::market::MarketTicker;
    use predigy_core::order::{Order, OrderId, OrderState, OrderType, TimeInForce};
    use predigy_core::price::{Price, Qty};
    use predigy_core::side::{Action, Side};
    use std::time::Instant;
    use tempfile::TempDir;

    fn make_order(price: u8) -> Order {
        Order {
            client_id: OrderId::new("strat:MKT:00000001"),
            market: MarketTicker::new("KX-TEST"),
            side: Side::Yes,
            action: Action::Buy,
            price: Price::from_cents(price).unwrap(),
            qty: Qty::new(5).unwrap(),
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
        }
    }

    #[test]
    fn roundtrip_empty_state_snapshot() {
        let acc = AccountState::new();
        let orders: HashMap<OrderId, OrderRecord> = HashMap::new();
        let snap = snapshot(&acc, &orders);
        let bytes = serde_json::to_vec_pretty(&snap).unwrap();
        let back: PersistedOmsState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert!(back.orders.is_empty());
        assert_eq!(back.account.daily_realized_pnl_cents, 0);
    }

    #[test]
    fn roundtrip_state_with_position_and_in_flight_order() {
        let mut acc = AccountState::new();
        acc.set_position(MarketTicker::new("KX-TEST"), Side::Yes, 3, 42);
        acc.add_realized_pnl(-50);

        let mut orders = HashMap::new();
        let cid = OrderId::new("strat:MKT:00000001");
        let mut rec = OrderRecord::new(make_order(42), Instant::now());
        rec.state = OrderState::Acked;
        rec.cumulative_qty = 1;
        rec.venue_order_id = Some("v-abc".into());
        orders.insert(cid.clone(), rec);

        let snap = snapshot(&acc, &orders);
        let bytes = serde_json::to_vec_pretty(&snap).unwrap();
        let back: PersistedOmsState = serde_json::from_slice(&bytes).unwrap();
        let (acc2, orders2) = rehydrate(back, Instant::now());

        assert_eq!(acc2.position(&MarketTicker::new("KX-TEST"), Side::Yes), 3);
        assert_eq!(acc2.daily_realized_pnl_cents(), -50);
        let r2 = orders2.get(&cid).expect("order rehydrated");
        assert_eq!(r2.state, OrderState::Acked);
        assert_eq!(r2.cumulative_qty, 1);
        assert_eq!(r2.venue_order_id.as_deref(), Some("v-abc"));
    }

    #[test]
    fn save_and_load_atomic_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oms-state.json");
        let mut acc = AccountState::new();
        acc.set_position(MarketTicker::new("KX-TEST"), Side::No, 7, 30);
        let snap = snapshot(&acc, &HashMap::new());
        save(&path, &snap).unwrap();
        let loaded = load(&path).unwrap().expect("file exists");
        assert_eq!(loaded.account.positions.len(), 1);
        assert_eq!(loaded.account.positions[0].qty, 7);
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn load_rejects_unknown_schema_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("future.json");
        let acc = AccountState::new();
        let mut snap = snapshot(&acc, &HashMap::new());
        snap.schema_version = SCHEMA_VERSION + 99;
        save(&path, &snap).unwrap();
        let err = load(&path).expect_err("should reject");
        assert!(matches!(err, StateError::SchemaMismatch { .. }));
    }
}
