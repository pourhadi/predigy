//! Market discovery contracts.
//!
//! Some strategies — settlement-trader being the canonical case —
//! need a live, periodically-refreshed list of markets matching
//! some criterion (sports games approaching their per-event
//! settlement, weather markets covering the next forecast cycle,
//! etc.). The set is dynamic: games come and go every few hours,
//! so a static `--market` list at startup wastes operator time
//! and misses entries published after boot.
//!
//! The engine takes care of the polling + subscribing; strategies
//! declare what they want via [`Strategy::discovery_subscriptions`].
//! When the discovery service finds a new (or no-longer-eligible)
//! market it emits an [`Event::DiscoveryDelta`] to the supervisor
//! AND auto-registers the new tickers with the market-data
//! router (no per-strategy boilerplate to wire up book
//! subscriptions).
//!
//! ## Lifecycle
//!
//! 1. Strategy declares `discovery_subscriptions()` at startup.
//! 2. Engine spawns one [`DiscoveryServiceLoop`] per declared
//!    subscription (deduplicated by config equality).
//! 3. Each loop polls the configured series at `interval`,
//!    diffs against the previous tick, and:
//!    - For each new ticker: registers with the router for book
//!      updates, then emits `DiscoveryDelta { added: [..] }` to
//!      the strategy.
//!    - For each ticker that fell out of the window: emits
//!      `DiscoveryDelta { removed: [..] }`. (Unsubscribing from
//!      the venue WS is best-effort; settled markets stop
//!      emitting deltas anyway.)
//!
//! Strategies are responsible for their own per-ticker bookkeeping
//! (close_times, last-fired cooldowns) — discovery just tells
//! them which markets to care about.

use std::time::Duration;

/// Declarative subscription a strategy emits at registration.
/// One [`DiscoveryServiceLoop`] is spawned per unique config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiscoverySubscription {
    /// Kalshi event/series tickers (e.g. `KXMLBGAME`, `KXNHLGAME`).
    pub series: Vec<String>,
    /// How often to re-poll. Settlement is fine at 60s; sports
    /// schedules don't move faster than that.
    pub interval_secs: u64,
    /// Drop markets whose expected settlement is more than this
    /// far in the future. 30 min default for settlement-trader
    /// (its strategy fires only in the last 10 min anyway).
    pub max_secs_to_settle: i64,
    /// Skip markets whose `yes_ask` is missing — saves the
    /// strategy from evaluating empty books.
    pub require_quote: bool,
}

impl DiscoverySubscription {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

/// Per-market summary the engine emits to strategies on each
/// discovery tick. Vendor-agnostic shape — the engine maps from
/// `predigy_kalshi_rest::types::MarketSummary` (or a future
/// Polymarket equivalent) into this view at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredMarket {
    pub ticker: String,
    /// Per-event expected settlement, unix seconds. For sports
    /// markets this is the actual game-end timestamp from
    /// `expected_expiration_time`; for daily markets it falls
    /// back to `close_time`.
    pub settle_unix: i64,
}
