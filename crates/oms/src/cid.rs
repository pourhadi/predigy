//! Deterministic client-order-id allocation.
//!
//! Per the plan: "Built from `(strategy_id, market, intent_seq)` so
//! duplicate sends are no-ops on the exchange and detectable in OMS."
//!
//! The allocator is owned by the OMS task — single-threaded, no atomics
//! needed. The `strategy_id` is the trader's name (`"arb"`, `"mm"`, …)
//! so collisions are impossible across simultaneously-running
//! strategies on the same account.

use predigy_core::market::MarketTicker;
use predigy_core::order::OrderId;

#[derive(Debug, Clone)]
pub struct CidAllocator {
    strategy_id: String,
    next_seq: u64,
}

impl CidAllocator {
    /// Construct with a strategy id and a starting sequence number. In
    /// production the starting sequence is read from durable storage so
    /// ids never repeat across process restarts; for tests `0` is fine.
    #[must_use]
    pub fn new(strategy_id: impl Into<String>, start_seq: u64) -> Self {
        Self {
            strategy_id: strategy_id.into(),
            next_seq: start_seq,
        }
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
    /// `{strategy_id}:{market_ticker}:{seq:08}` — short enough to fit in
    /// the FIX `ClOrdID` (tag 11) limit, structured enough to grep in
    /// log files.
    pub fn next(&mut self, market: &MarketTicker) -> OrderId {
        let seq = self.next_seq;
        self.next_seq += 1;
        OrderId::new(format!("{}:{}:{:08}", self.strategy_id, market, seq))
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
}
