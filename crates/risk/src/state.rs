//! Account state observable by the risk engine.
//!
//! `AccountState` is the single in-memory mirror of "what the OMS
//! believes about us" — positions, today's realised P&L, the
//! kill-switch flag, and a sliding window of recent order timestamps
//! for rate limiting. The risk engine reads it; the OMS writes it.
//!
//! Time inputs are [`Instant`] (monotonic) — wall-clock skew, NTP
//! jumps, and DST cannot break the rate limiter. Daily P&L is reset
//! externally via [`AccountState::reset_for_new_day`]; this crate
//! does not depend on a calendar.

use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// The OMS's mirror of account state, exposed to the risk engine.
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    /// Positions per `(market, side)`. Absent → 0 contracts.
    positions: HashMap<(MarketTicker, Side), u32>,
    /// VWAP entry price in cents per `(market, side)`, alongside the
    /// `positions` entry. Always read with the matching position to
    /// compute notional.
    avg_entry_cents: HashMap<(MarketTicker, Side), u16>,
    /// Realised P&L for the current trading day, in cents (signed).
    daily_realized_pnl_cents: i64,
    /// Sliding window of recent order-submit timestamps. Pruned lazily
    /// during reads.
    recent_orders: VecDeque<Instant>,
    /// When `true`, every risk check rejects with `KillSwitchActive`.
    kill_switch: bool,
}

/// Serialisable snapshot of the persistable subset of `AccountState`.
/// Excludes `recent_orders` (whose `Instant`s aren't meaningful across
/// restarts — the rate-limit window is allowed to reset on resume).
/// The OMS calls [`AccountState::to_persisted`] before writing to disk
/// and [`AccountState::from_persisted`] on restart.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedAccountState {
    /// Schema version of this snapshot. Bump when the on-disk shape
    /// changes incompatibly.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub positions: Vec<PersistedPositionEntry>,
    #[serde(default)]
    pub daily_realized_pnl_cents: i64,
    #[serde(default)]
    pub kill_switch: bool,
}

fn default_schema_version() -> u32 {
    1
}

/// One row of the persisted positions table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPositionEntry {
    pub market: MarketTicker,
    pub side: Side,
    pub qty: u32,
    pub avg_entry_cents: u16,
}

impl AccountState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn position(&self, market: &MarketTicker, side: Side) -> u32 {
        self.positions
            .get(&(market.clone(), side))
            .copied()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn avg_entry_cents(&self, market: &MarketTicker, side: Side) -> u16 {
        self.avg_entry_cents
            .get(&(market.clone(), side))
            .copied()
            .unwrap_or(0)
    }

    /// Notional held on `(market, side)` in cents — `position × avg_entry`.
    #[must_use]
    pub fn notional_cents(&self, market: &MarketTicker, side: Side) -> u64 {
        u64::from(self.position(market, side)) * u64::from(self.avg_entry_cents(market, side))
    }

    /// Sum of `position × avg_entry` across every `(market, side)`
    /// the OMS has booked. `O(positions)`; called by the risk engine
    /// once per check.
    #[must_use]
    pub fn gross_notional_cents(&self) -> u64 {
        self.positions
            .iter()
            .map(|(key, &qty)| {
                let avg = self.avg_entry_cents.get(key).copied().unwrap_or(0);
                u64::from(qty) * u64::from(avg)
            })
            .sum()
    }

    #[must_use]
    pub fn daily_realized_pnl_cents(&self) -> i64 {
        self.daily_realized_pnl_cents
    }

    /// Realised loss today, in absolute cents. `0` if today is up.
    #[must_use]
    pub fn daily_realized_loss_cents(&self) -> u64 {
        if self.daily_realized_pnl_cents >= 0 {
            0
        } else {
            u64::try_from(-self.daily_realized_pnl_cents).unwrap_or(u64::MAX)
        }
    }

    #[must_use]
    pub fn kill_switch_active(&self) -> bool {
        self.kill_switch
    }

    /// Count of orders submitted in the last `window` ending at `now`.
    /// Prunes anything older than `now - window` from the deque as a
    /// side effect — amortises the O(n) cost across all callers.
    pub fn orders_in_window(&mut self, now: Instant, window: Duration) -> u32 {
        let cutoff = now.checked_sub(window).unwrap_or(now);
        while let Some(&front) = self.recent_orders.front() {
            if front < cutoff {
                self.recent_orders.pop_front();
            } else {
                break;
            }
        }
        u32::try_from(self.recent_orders.len()).unwrap_or(u32::MAX)
    }

    // ---------- mutation ----------

    /// Replace the position on `(market, side)` outright. Use after the
    /// OMS has reconciled an `ExecutionReport` and computed the new VWAP.
    pub fn set_position(
        &mut self,
        market: MarketTicker,
        side: Side,
        qty: u32,
        avg_entry_cents: u16,
    ) {
        let key = (market, side);
        if qty == 0 {
            self.positions.remove(&key);
            self.avg_entry_cents.remove(&key);
        } else {
            self.positions.insert(key.clone(), qty);
            self.avg_entry_cents.insert(key, avg_entry_cents);
        }
    }

    /// Add `delta_cents` (signed) to today's realised P&L. Called after
    /// a fill closes a position or after a fee debit settles.
    pub fn add_realized_pnl(&mut self, delta_cents: i64) {
        self.daily_realized_pnl_cents = self.daily_realized_pnl_cents.saturating_add(delta_cents);
    }

    /// Note that an order was submitted at `now`. Bookkeeping for the
    /// rate limiter — call once per attempted submit, regardless of
    /// whether the venue accepted it.
    pub fn record_order_sent(&mut self, now: Instant) {
        self.recent_orders.push_back(now);
    }

    pub fn arm_kill_switch(&mut self) {
        self.kill_switch = true;
    }

    pub fn disarm_kill_switch(&mut self) {
        self.kill_switch = false;
    }

    /// Reset daily-P&L bookkeeping at the start of a new trading day.
    /// Positions and the kill switch are left untouched — those persist
    /// across day boundaries.
    pub fn reset_for_new_day(&mut self) {
        self.daily_realized_pnl_cents = 0;
        self.recent_orders.clear();
    }

    /// Snapshot the persistable subset to a [`PersistedAccountState`].
    /// Does not include the rate-limit sliding window — those `Instant`s
    /// are meaningless across process restarts and the window is allowed
    /// to reset on resume (worst case: a few extra orders fire in the
    /// first second after restart).
    #[must_use]
    pub fn to_persisted(&self) -> PersistedAccountState {
        let mut entries: Vec<PersistedPositionEntry> = self
            .positions
            .iter()
            .map(|((market, side), &qty)| PersistedPositionEntry {
                market: market.clone(),
                side: *side,
                qty,
                avg_entry_cents: self
                    .avg_entry_cents
                    .get(&(market.clone(), *side))
                    .copied()
                    .unwrap_or(0),
            })
            .collect();
        // Stable order so successive snapshots that differ in nothing
        // produce byte-identical files (helps if you `diff` or hash
        // them).
        entries.sort_by(|a, b| {
            a.market
                .as_str()
                .cmp(b.market.as_str())
                .then_with(|| a.side.cmp(&b.side))
        });
        PersistedAccountState {
            schema_version: 1,
            positions: entries,
            daily_realized_pnl_cents: self.daily_realized_pnl_cents,
            kill_switch: self.kill_switch,
        }
    }

    /// Rehydrate an `AccountState` from a snapshot produced by
    /// [`AccountState::to_persisted`]. The rate-limit window is left
    /// empty.
    #[must_use]
    pub fn from_persisted(p: &PersistedAccountState) -> Self {
        let mut s = Self::new();
        for e in &p.positions {
            s.set_position(e.market.clone(), e.side, e.qty, e.avg_entry_cents);
        }
        s.daily_realized_pnl_cents = p.daily_realized_pnl_cents;
        s.kill_switch = p.kill_switch;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> MarketTicker {
        MarketTicker::new("X")
    }

    #[test]
    fn position_default_zero() {
        let s = AccountState::new();
        assert_eq!(s.position(&m(), Side::Yes), 0);
        assert_eq!(s.notional_cents(&m(), Side::Yes), 0);
    }

    #[test]
    fn set_position_round_trips() {
        let mut s = AccountState::new();
        s.set_position(m(), Side::Yes, 100, 42);
        assert_eq!(s.position(&m(), Side::Yes), 100);
        assert_eq!(s.notional_cents(&m(), Side::Yes), 4200);
    }

    #[test]
    fn set_position_to_zero_clears() {
        let mut s = AccountState::new();
        s.set_position(m(), Side::Yes, 100, 42);
        s.set_position(m(), Side::Yes, 0, 0);
        assert_eq!(s.position(&m(), Side::Yes), 0);
        assert_eq!(s.gross_notional_cents(), 0);
    }

    #[test]
    fn gross_notional_sums_across_markets_and_sides() {
        let mut s = AccountState::new();
        s.set_position(MarketTicker::new("A"), Side::Yes, 50, 40); // 2000
        s.set_position(MarketTicker::new("B"), Side::No, 25, 60); // 1500
        assert_eq!(s.gross_notional_cents(), 3500);
    }

    #[test]
    fn daily_loss_only_reports_negative_pnl() {
        let mut s = AccountState::new();
        s.add_realized_pnl(500);
        assert_eq!(s.daily_realized_loss_cents(), 0);
        s.add_realized_pnl(-1500); // net -1000
        assert_eq!(s.daily_realized_loss_cents(), 1000);
    }

    #[test]
    fn order_window_counts_recent_only() {
        let mut s = AccountState::new();
        let t0 = Instant::now();
        s.record_order_sent(t0);
        s.record_order_sent(t0 + Duration::from_millis(100));
        s.record_order_sent(t0 + Duration::from_millis(900));

        // Window = 1s, asking at t0 + 1.5s → only the last (t0+0.9s) is inside [0.5s, 1.5s].
        let now = t0 + Duration::from_millis(1500);
        assert_eq!(s.orders_in_window(now, Duration::from_secs(1)), 1);
        // The pruning is observable: subsequent ask should match.
        assert_eq!(s.orders_in_window(now, Duration::from_secs(1)), 1);
    }

    #[test]
    fn reset_for_new_day_clears_pnl_and_rate_window() {
        let mut s = AccountState::new();
        s.add_realized_pnl(-500);
        s.record_order_sent(Instant::now());
        s.arm_kill_switch();

        s.reset_for_new_day();
        assert_eq!(s.daily_realized_pnl_cents(), 0);
        assert_eq!(
            s.orders_in_window(Instant::now(), Duration::from_mins(1)),
            0
        );
        // Kill switch persists across the day boundary.
        assert!(s.kill_switch_active());
    }
}
