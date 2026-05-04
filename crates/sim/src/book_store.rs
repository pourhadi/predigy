//! Shared per-market `OrderBook` state for the simulator.
//!
//! Both [`Replay`](crate::replay::Replay) and
//! [`SimExecutor`](crate::executor::SimExecutor) read and write this
//! store: the replay applies recorded snapshots/deltas, and the
//! executor consumes liquidity (via synthetic deltas) when it
//! matches an IOC order.
//!
//! Backed by `Arc<Mutex<...>>` so the same store can be cloned into
//! both halves. Lock contention is minimal — the simulator runs in a
//! single async task on the hot path and the integration tests are
//! sequential.

use predigy_book::{OrderBook, Snapshot};
use predigy_core::market::MarketTicker;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct BookStore {
    inner: Arc<Mutex<HashMap<MarketTicker, OrderBook>>>,
}

impl BookStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a snapshot to a market, creating the book entry if it
    /// doesn't already exist.
    pub fn apply_snapshot(&self, market: &MarketTicker, snapshot: Snapshot) {
        let mut books = self.inner.lock().unwrap();
        let book = books
            .entry(market.clone())
            .or_insert_with(|| OrderBook::new(market.as_str()));
        book.apply_snapshot(snapshot);
    }

    /// Read-only borrow over the entire store. Use with care — holds
    /// the mutex for the duration of the closure.
    pub fn with_book<R>(
        &self,
        market: &MarketTicker,
        f: impl FnOnce(Option<&OrderBook>) -> R,
    ) -> R {
        let books = self.inner.lock().unwrap();
        f(books.get(market))
    }

    /// Mutable borrow for the executor's match-and-consume path.
    pub fn with_book_mut<R>(
        &self,
        market: &MarketTicker,
        f: impl FnOnce(&mut OrderBook) -> R,
    ) -> Option<R> {
        let mut books = self.inner.lock().unwrap();
        books.get_mut(market).map(f)
    }

    /// Snapshot the entire store as `(market, OrderBook)` pairs. Used
    /// by tests for end-of-run assertions.
    #[must_use]
    pub fn clone_books(&self) -> HashMap<MarketTicker, OrderBook> {
        self.inner.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::price::Price;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn snap(seq: u64, yes: Vec<(u8, u32)>, no: Vec<(u8, u32)>) -> Snapshot {
        Snapshot {
            seq,
            yes_bids: yes.into_iter().map(|(c, q)| (p(c), q)).collect(),
            no_bids: no.into_iter().map(|(c, q)| (p(c), q)).collect(),
        }
    }

    #[test]
    fn apply_snapshot_creates_book_lazily() {
        let store = BookStore::new();
        let m = MarketTicker::new("X");
        store.apply_snapshot(&m, snap(1, vec![(40, 100)], vec![(60, 50)]));
        store.with_book(&m, |b| {
            let b = b.expect("book exists");
            assert_eq!(b.best_yes_bid().unwrap().0.cents(), 40);
            assert_eq!(b.best_no_bid().unwrap().0.cents(), 60);
        });
    }

    #[test]
    fn missing_market_returns_none() {
        let store = BookStore::new();
        let m = MarketTicker::new("nope");
        let seen = store.with_book(&m, |b| b.is_some());
        assert!(!seen);
    }
}
