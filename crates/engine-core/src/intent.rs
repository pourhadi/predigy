//! `Intent` — the desire-to-trade output of a strategy module,
//! passed to the engine's OMS for idempotency, risk, and venue
//! routing.
//!
//! Strategies don't construct venue-specific orders directly —
//! they emit `Intent`s, the engine maps to FIX or REST.

use predigy_core::market::MarketTicker;
use predigy_core::side::Side;

/// Strip characters Kalshi rejects in `client_order_id`. Confirmed
/// by Kalshi's V2 venue: cids containing `.` (e.g. tickers like
/// `KXBRAZILINF-26APR-T4.30`) get rejected with
/// `400 invalid_parameters`. The legacy `CidAllocator` already
/// strips them; engine strategies must do the same when embedding
/// a ticker in a cid format string.
///
/// Returns a `String` rather than `&str` so callers don't have to
/// worry about borrow lifetimes when feeding into `format!`.
#[must_use]
pub fn cid_safe_ticker(ticker: &str) -> String {
    ticker.replace('.', "")
}

#[derive(Debug, Clone)]
pub struct Intent {
    /// Operator-namespaced client order id; uniqueness is the
    /// engine's idempotency key. Convention: `<strategy>:<ticker>:<8-hex>`.
    pub client_id: String,
    pub strategy: &'static str,
    pub market: MarketTicker,
    pub side: Side,
    pub action: IntentAction,
    /// Limit price in cents; `None` for market orders. Most of
    /// our strategies emit IOC limit orders.
    pub price_cents: Option<i32>,
    pub qty: i32,
    pub order_type: OrderType,
    pub tif: Tif,
    /// Free-form rationale. Persisted to `intents.reason`.
    pub reason: Option<String>,
    /// When true, Kalshi rejects the order if it would cross the
    /// book at submit time. Required for maker-mode quoting where
    /// accidentally taking blows up the economic case (taker fees
    /// vs. 0¢ maker fee on standard markets). Default `false`.
    /// IOC + post_only together is degenerate — the IOC engine
    /// has no resting order to be a maker for; in practice
    /// post_only=true should pair with `Tif::Gtc`.
    pub post_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Ioc,
    Gtc,
    Fok,
}

/// **Audit I7** — atomic multi-leg submit. A `LegGroup` carries a
/// set of intents that share an all-or-none persistence
/// constraint at the OMS layer. The OMS pre-checks every leg AND
/// the combined notional against caps; if any leg or the
/// aggregate fails, the whole group rejects without touching the
/// `intents` table. On success every leg gets the same
/// `group_id`, persisted in one DB transaction.
///
/// Venue-side atomicity is not promised — Kalshi has no native
/// multi-leg orders. The guarantees we DO make:
///
/// 1. All legs persist together or none do (DB transaction).
/// 2. Pre-check fails the group on the first failing leg's
///    reason (no half-checked state).
/// 3. The shared `group_id` lets `apply_execution` cascade a
///    venue-side cancel/reject across siblings — when one leg's
///    venue submit fails, the OMS cancels the others by their
///    group membership.
///
/// Single-leg constructors stay on `Oms::submit`; only S3 / S9
/// style multi-leg arb strategies need `submit_group`. Existing
/// strategies are unaffected.
#[derive(Debug, Clone)]
pub struct LegGroup {
    pub group_id: uuid::Uuid,
    pub intents: Vec<Intent>,
}

impl LegGroup {
    /// Construct a new group with a fresh UUID. Returns `None` if
    /// the input is empty — a zero-leg group is meaningless and
    /// catching it here saves an OMS round trip.
    #[must_use]
    pub fn new(intents: Vec<Intent>) -> Option<Self> {
        if intents.is_empty() {
            return None;
        }
        Some(Self {
            group_id: uuid::Uuid::new_v4(),
            intents,
        })
    }

    /// Reconstruct a group with a known UUID. Used by tests and
    /// by recovery paths that re-attach pre-existing rows to a
    /// known group id.
    #[must_use]
    pub fn with_id(group_id: uuid::Uuid, intents: Vec<Intent>) -> Option<Self> {
        if intents.is_empty() {
            return None;
        }
        Some(Self { group_id, intents })
    }
}
