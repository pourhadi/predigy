//! Cross-venue (statistical) arb against Polymarket reference.
//!
//! ## The signal
//!
//! Polymarket and Kalshi quote the same kinds of events, often with
//! offsetting depth profiles and small but persistent price gaps.
//! When Kalshi prices a YES contract noticeably *lower* than
//! Polymarket's mid for the equivalent token, we expect convergence
//! and buy on Kalshi (we never execute on Polymarket — Poly is the
//! reference, not the counter). Symmetric for NO.
//!
//! Edge equation per pair:
//!
//! ```text
//! yes_edge_¢ = poly_yes_mid_¢ − kalshi_yes_ask_¢ − taker_fee
//! no_edge_¢  = poly_no_mid_¢  − kalshi_no_ask_¢  − taker_fee
//! ```
//!
//! `poly_no_mid_¢ = 100 − poly_yes_mid_¢` (binary contracts sum to
//! $1). Either side can fire independently — there's no requirement
//! to lift both legs together since this isn't pure arb.
//!
//! ## What the strategy is and is not
//!
//! It's a **stat-arb** bet: convergence is statistical, not
//! mechanical. The risk module's daily-loss breaker is the
//! must-have backstop. Per the plan: "primary $/risk" engine, but
//! sized small at $5k account capital.

use predigy_book::OrderBook;
use predigy_core::fees::taker_fee;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Debug, Clone)]
pub struct CrossArbConfig {
    /// Minimum edge per contract, in cents, after the taker fee, to
    /// fire a trade. Per-leg (we don't require both legs to be
    /// profitable on the same tick).
    pub min_edge_cents: u32,
    /// Max contracts per trade. The OMS+risk caps may downsize
    /// further.
    pub max_size: u32,
    /// Cooldown between submits on the same Kalshi market.
    pub cooldown: Duration,
}

impl Default for CrossArbConfig {
    fn default() -> Self {
        Self {
            min_edge_cents: 1,
            max_size: 25,
            cooldown: Duration::from_millis(500),
        }
    }
}

/// Snapshot of Polymarket reference quotes for one asset.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolyRef {
    /// Best bid in dollars (e.g. `0.42`). `None` until the first
    /// `book` event or `price_change` lands.
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

impl PolyRef {
    /// Midpoint of bid and ask, or whichever side is populated.
    /// `None` if both are absent.
    #[must_use]
    pub fn mid(self) -> Option<f64> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) => Some(f64::midpoint(a, b)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

#[derive(Debug)]
pub struct CrossArbStrategy {
    config: CrossArbConfig,
    /// Kalshi ticker → Polymarket asset_id. Configured at startup;
    /// stable for the strategy's lifetime.
    market_map: HashMap<MarketTicker, String>,
    /// Latest Polymarket reference per asset_id.
    poly_ref: HashMap<String, PolyRef>,
    /// Per-Kalshi-market submit cooldown.
    last_submit_at: HashMap<MarketTicker, Instant>,
}

impl CrossArbStrategy {
    #[must_use]
    pub fn new(config: CrossArbConfig, market_map: HashMap<MarketTicker, String>) -> Self {
        Self {
            config,
            market_map,
            poly_ref: HashMap::new(),
            last_submit_at: HashMap::new(),
        }
    }

    pub fn config(&self) -> &CrossArbConfig {
        &self.config
    }

    /// Update the reference for one Polymarket asset. Call this from
    /// the Polymarket WS event handler whenever a `book` or
    /// `price_change` fires.
    pub fn update_poly(&mut self, asset_id: &str, best_bid: Option<f64>, best_ask: Option<f64>) {
        let entry = self.poly_ref.entry(asset_id.to_string()).or_default();
        if best_bid.is_some() {
            entry.best_bid = best_bid;
        }
        if best_ask.is_some() {
            entry.best_ask = best_ask;
        }
    }

    /// All Kalshi markets the strategy is configured for.
    pub fn kalshi_markets(&self) -> impl Iterator<Item = &MarketTicker> {
        self.market_map.keys()
    }

    /// All Polymarket asset_ids the strategy needs.
    pub fn poly_assets(&self) -> impl Iterator<Item = &str> {
        self.market_map.values().map(String::as_str)
    }

    /// Evaluate the Kalshi book against the latest Polymarket
    /// reference. Returns 0, 1, or 2 intents (one per side that
    /// crosses the edge threshold).
    pub fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Vec<Intent> {
        let Some(asset_id) = self.market_map.get(market) else {
            return Vec::new();
        };
        let Some(poly_ref) = self.poly_ref.get(asset_id) else {
            return Vec::new();
        };
        let Some(poly_yes_mid) = poly_ref.mid() else {
            return Vec::new();
        };
        if !(0.01..=0.99).contains(&poly_yes_mid) {
            return Vec::new();
        }

        if let Some(&last) = self.last_submit_at.get(market)
            && now.duration_since(last) < self.config.cooldown
        {
            return Vec::new();
        }

        let poly_yes_mid_cents = (poly_yes_mid * 100.0).round().clamp(1.0, 99.0) as u8;
        let poly_no_mid_cents = 100 - poly_yes_mid_cents;

        let mut intents = Vec::new();
        // Kalshi YES ask vs Polymarket YES mid.
        if let Some((no_bid_px, no_bid_qty)) = book.best_no_bid() {
            let yes_ask_cents = 100 - no_bid_px.cents();
            if let Some(intent) = build_intent(
                market,
                Side::Yes,
                yes_ask_cents,
                no_bid_qty.min(self.config.max_size),
                poly_yes_mid_cents,
                self.config.min_edge_cents,
            ) {
                intents.push(intent);
            }
        }
        // Kalshi NO ask vs (1 − Polymarket YES mid).
        if let Some((yes_bid_px, yes_bid_qty)) = book.best_yes_bid() {
            let no_ask_cents = 100 - yes_bid_px.cents();
            if let Some(intent) = build_intent(
                market,
                Side::No,
                no_ask_cents,
                yes_bid_qty.min(self.config.max_size),
                poly_no_mid_cents,
                self.config.min_edge_cents,
            ) {
                intents.push(intent);
            }
        }

        if !intents.is_empty() {
            self.last_submit_at.insert(market.clone(), now);
        }
        intents
    }
}

fn build_intent(
    market: &MarketTicker,
    side: Side,
    kalshi_ask_cents: u8,
    available_qty: u32,
    poly_mid_cents: u8,
    min_edge_cents: u32,
) -> Option<Intent> {
    if available_qty == 0 {
        return None;
    }
    if poly_mid_cents <= kalshi_ask_cents {
        // Poly's reference says the contract is worth less than what
        // Kalshi is asking — no edge.
        return None;
    }
    let raw_edge = u32::from(poly_mid_cents) - u32::from(kalshi_ask_cents);
    let kalshi_price = Price::from_cents(kalshi_ask_cents).ok()?;
    let qty = Qty::new(available_qty).ok()?;
    let fee_cents = taker_fee(kalshi_price, qty);
    // Per-contract fee (rounded up). For a single-contract decision
    // we use the per-fill fee.
    let fee_per_contract = fee_cents.div_ceil(available_qty.max(1));
    if raw_edge <= fee_per_contract {
        debug!(
            market = %market,
            side = ?side,
            raw_edge,
            fee_per_contract,
            "edge below per-contract fee; skipping"
        );
        return None;
    }
    let net_edge = raw_edge - fee_per_contract;
    if net_edge < min_edge_cents {
        return None;
    }
    Some(
        Intent::limit(market.clone(), side, Action::Buy, kalshi_price, qty)
            .with_tif(TimeInForce::Ioc),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn book(yes_bids: &[(u8, u32)], no_bids: &[(u8, u32)]) -> OrderBook {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(Snapshot {
            seq: 1,
            yes_bids: yes_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
            no_bids: no_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
        });
        b
    }

    fn market_map() -> HashMap<MarketTicker, String> {
        let mut m = HashMap::new();
        m.insert(MarketTicker::new("X"), "0xabc".into());
        m
    }

    #[test]
    fn no_intent_until_poly_reference_arrives() {
        let mut s = CrossArbStrategy::new(CrossArbConfig::default(), market_map());
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(60, 100)], &[(50, 100)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn buys_kalshi_yes_when_kalshi_underprices_vs_poly() {
        // Kalshi YES ask = 100 − best NO bid = 100 − 30 = 70¢ → cheap.
        // Poly YES mid = 80¢ → above Kalshi ask + fees.
        let mut s = CrossArbStrategy::new(
            CrossArbConfig {
                min_edge_cents: 1,
                max_size: 10,
                cooldown: Duration::from_millis(1),
            },
            market_map(),
        );
        s.update_poly("0xabc", Some(0.78), Some(0.82));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(20, 5)], &[(30, 50)]),
            Instant::now(),
        );
        // YES side should fire: Kalshi YES ask = 70¢, Poly mid = 80¢,
        // raw edge = 10¢, fee_per_contract = ceil(0.07 * 0.7 * 0.3)
        //           ≈ ceil(1.47) = 2¢ → net 8¢ ≥ 1.
        // NO side: Kalshi NO ask = 100 − 20 = 80¢, Poly NO mid = 20¢,
        //          raw edge = -60 < 0 → no fire.
        assert_eq!(intents.len(), 1);
        let yes = &intents[0];
        assert_eq!(yes.side, Side::Yes);
        assert_eq!(yes.action, Action::Buy);
        assert_eq!(yes.price.cents(), 70);
        assert_eq!(yes.tif, TimeInForce::Ioc);
    }

    #[test]
    fn buys_kalshi_no_when_kalshi_no_side_underprices() {
        // Kalshi NO ask = 100 − best YES bid = 100 − 40 = 60¢. Poly
        // NO mid = 100 − 30 = 70¢. Raw edge = 10¢; same fee math
        // → fires.
        let mut s = CrossArbStrategy::new(
            CrossArbConfig {
                min_edge_cents: 1,
                max_size: 10,
                cooldown: Duration::from_millis(1),
            },
            market_map(),
        );
        s.update_poly("0xabc", Some(0.28), Some(0.32));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(40, 50)], &[(60, 5)]),
            Instant::now(),
        );
        // YES side: Kalshi ask = 100 − 60 = 40¢, Poly YES = 30¢ → no edge.
        // NO side: ask 60¢, Poly NO = 70¢ → fires.
        assert!(intents.iter().any(|i| i.side == Side::No));
    }

    #[test]
    fn no_intent_when_kalshi_overprices() {
        let mut s = CrossArbStrategy::new(CrossArbConfig::default(), market_map());
        // Kalshi YES ask = 70¢, Poly mid = 50¢ → no edge.
        s.update_poly("0xabc", Some(0.49), Some(0.51));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(20, 5)], &[(30, 50)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn cooldown_throttles_after_submit() {
        let mut s = CrossArbStrategy::new(
            CrossArbConfig {
                min_edge_cents: 1,
                max_size: 10,
                cooldown: Duration::from_secs(1),
            },
            market_map(),
        );
        s.update_poly("0xabc", Some(0.78), Some(0.82));
        let now = Instant::now();
        assert!(
            !s.evaluate(&MarketTicker::new("X"), &book(&[(20, 5)], &[(30, 50)]), now)
                .is_empty()
        );
        // Within cooldown.
        assert!(
            s.evaluate(
                &MarketTicker::new("X"),
                &book(&[(20, 5)], &[(30, 50)]),
                now + Duration::from_millis(50),
            )
            .is_empty()
        );
        // After cooldown.
        assert!(
            !s.evaluate(
                &MarketTicker::new("X"),
                &book(&[(20, 5)], &[(30, 50)]),
                now + Duration::from_secs(2),
            )
            .is_empty()
        );
    }

    #[test]
    fn unknown_market_is_ignored() {
        let mut s = CrossArbStrategy::new(CrossArbConfig::default(), market_map());
        s.update_poly("0xabc", Some(0.5), Some(0.5));
        let intents = s.evaluate(
            &MarketTicker::new("UNKNOWN"),
            &book(&[(60, 100)], &[(40, 100)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }
}
