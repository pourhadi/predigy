//! Market identifiers.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Kalshi market ticker (e.g. `"KXNFLGAME-25NOV02DETMIN-DET"`).
///
/// Kept as an owned `String`. Cheap-clone usage on hot paths should switch to
/// `Arc<str>` if profiling shows it; for now correctness > micro-optimization.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketTicker(String);

impl MarketTicker {
    #[inline]
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MarketTicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for MarketTicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Static-ish market metadata pulled from Kalshi REST. The order book lives
/// elsewhere; this struct is the bits a strategy reads but doesn't update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Market {
    pub ticker: MarketTicker,
    pub event_ticker: String,
    pub title: String,
    pub status: MarketStatus,
    pub close_time_unix: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStatus {
    Initialized,
    Active,
    Closed,
    Settled,
    Determined,
}
