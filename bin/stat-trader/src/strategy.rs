//! Statistical alpha strategy: per-market model probability vs the
//! Kalshi book's touch, sized by Kelly fraction.
//!
//! Strategy fires when the YES (or NO) ask is enough below the
//! model's probability that the after-fee per-contract edge clears
//! `min_edge_cents`. Sizing comes from
//! [`predigy_signals::kelly::contracts_to_buy`] using a configured
//! bankroll snapshot (we don't dynamically rebalance against
//! position P&L within a session — the operator restarts with a
//! fresh bankroll between trading windows).
//!
//! ## What "model probability" means here
//!
//! For v1 it's a static value per market — the operator's belief
//! about the market's true probability of YES, encoded at
//! configuration time. Future revisions will plug
//! [`predigy_signals::Posterior`] in so the probability updates as
//! evidence streams in.

use predigy_book::OrderBook;
use predigy_core::fees::taker_fee;
use predigy_core::intent::Intent;
use predigy_core::market::MarketTicker;
use predigy_core::order::TimeInForce;
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_signals::kelly;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatRule {
    pub kalshi_market: MarketTicker,
    /// Operator's model probability that YES will resolve true.
    /// `0 < model_p < 1`.
    pub model_p: f64,
    /// Side to bet when the implied edge clears the threshold.
    /// `Side::Yes` triggers on cheap YES asks; `Side::No` triggers
    /// on cheap NO asks. The strategy only takes the configured
    /// side for a given market.
    pub side: Side,
    /// Min after-fee per-contract edge to fire (cents).
    pub min_edge_cents: u32,
    /// Local settlement date (`YYYY-MM-DD`) for horizon gating.
    #[serde(default)]
    pub settlement_date: Option<String>,
    /// Curator generation timestamp (RFC3339 UTC) for stale-rule gating.
    #[serde(default)]
    pub generated_at_utc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StatConfig {
    /// Total bankroll in cents — input to Kelly sizing.
    pub bankroll_cents: u64,
    /// Fractional Kelly modifier ∈ `(0, 1]`. 0.25 is a common
    /// "quarter-Kelly" choice that's robust to model error.
    pub kelly_factor: f64,
    /// Hard cap on contracts per fire (top of the Kelly result).
    pub max_size: u32,
    /// Cooldown between fires per market.
    pub cooldown: Duration,
}

#[derive(Debug)]
pub struct StatStrategy {
    config: StatConfig,
    rules: HashMap<MarketTicker, StatRule>,
    last_fire_at: HashMap<MarketTicker, Instant>,
}

impl StatStrategy {
    pub fn new(config: StatConfig, rules: Vec<StatRule>) -> Self {
        let rules = rules
            .into_iter()
            .map(|r| (r.kalshi_market.clone(), r))
            .collect();
        Self {
            config,
            rules,
            last_fire_at: HashMap::new(),
        }
    }

    pub fn markets(&self) -> impl Iterator<Item = &MarketTicker> {
        self.rules.keys()
    }

    /// Evaluate the rule for `market`. Returns an `Intent` when the
    /// strategy should fire, `None` otherwise.
    pub fn evaluate(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Option<Intent> {
        let rule = self.rules.get(market)?;
        if let Some(&last) = self.last_fire_at.get(market)
            && now.duration_since(last) < self.config.cooldown
        {
            return None;
        }
        let (ask_cents, available_qty) = derive_ask(book, rule.side)?;
        let intent = build_intent(rule, &self.config, ask_cents, available_qty)?;
        self.last_fire_at.insert(market.clone(), now);
        Some(intent)
    }
}

/// `(ask_cents, qty_at_touch)` for the requested side, using the
/// complement-of-opposite-bid trick.
fn derive_ask(book: &OrderBook, side: Side) -> Option<(u8, u32)> {
    let (px, qty) = match side {
        // YES ask = 100 − best NO bid.
        Side::Yes => book.best_no_bid()?,
        // NO ask = 100 − best YES bid.
        Side::No => book.best_yes_bid()?,
    };
    let ask = 100u8.checked_sub(px.cents())?;
    Some((ask, qty))
}

fn build_intent(
    rule: &StatRule,
    config: &StatConfig,
    ask_cents: u8,
    available_qty: u32,
) -> Option<Intent> {
    if !(0.01..=0.99).contains(&rule.model_p) {
        return None;
    }
    if ask_cents == 0 || ask_cents >= 100 {
        return None;
    }
    let ask_dollars = f64::from(ask_cents) / 100.0;
    // For a NO bet, the relevant "probability" the strategy is betting
    // on is 1 − model_p (the model's belief that YES does NOT resolve).
    let bet_p = match rule.side {
        Side::Yes => rule.model_p,
        Side::No => 1.0 - rule.model_p,
    };
    let kelly_f = kelly::fraction_with_factor(bet_p, ask_dollars, config.kelly_factor).ok()?;
    if kelly_f <= 0.0 {
        return None;
    }
    // After-fee per-contract edge check. Kelly naturally weighs this
    // in via the (p − a) numerator, but the operator's "min_edge_cents"
    // is a hard cents-per-contract floor.
    let raw_edge_cents = (bet_p - ask_dollars) * 100.0;
    let kalshi_price = Price::from_cents(ask_cents).ok()?;
    let probe_qty = Qty::new(1).ok()?;
    let fee_per_contract = taker_fee(kalshi_price, probe_qty);
    if (raw_edge_cents - f64::from(fee_per_contract)) < f64::from(rule.min_edge_cents) {
        debug!(
            market = %rule.kalshi_market,
            raw_edge = raw_edge_cents,
            fee_per_contract,
            min_edge = rule.min_edge_cents,
            "stat: edge below threshold"
        );
        return None;
    }
    let target =
        kelly::contracts_to_buy(config.bankroll_cents, ask_cents, kelly_f, config.max_size);
    if target == 0 {
        return None;
    }
    let size = target.min(available_qty);
    if size == 0 {
        return None;
    }
    let qty = Qty::new(size).ok()?;
    Some(
        Intent::limit(
            rule.kalshi_market.clone(),
            rule.side,
            Action::Buy,
            kalshi_price,
            qty,
        )
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

    fn cfg() -> StatConfig {
        StatConfig {
            bankroll_cents: 50_000,
            kelly_factor: 0.5,
            max_size: 100,
            cooldown: Duration::from_millis(1),
        }
    }

    fn yes_rule(model_p: f64, edge: u32) -> StatRule {
        StatRule {
            kalshi_market: MarketTicker::new("X"),
            model_p,
            side: Side::Yes,
            min_edge_cents: edge,
            settlement_date: None,
            generated_at_utc: None,
        }
    }

    #[test]
    fn fires_when_kalshi_underprices_vs_model() {
        // YES ask = 100 - 30 = 70¢. Model p = 0.85 → bet_p = 0.85,
        // raw edge = 15¢, fee≈ ceil(0.07*0.7*0.3)=2¢, net 13 ≥ 2.
        let mut s = StatStrategy::new(cfg(), vec![yes_rule(0.85, 2)]);
        let intent = s
            .evaluate(
                &MarketTicker::new("X"),
                &book(&[], &[(30, 100)]),
                Instant::now(),
            )
            .expect("should fire");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, Action::Buy);
        assert_eq!(intent.price.cents(), 70);
        assert_eq!(intent.tif, TimeInForce::Ioc);
        assert!(intent.qty.get() > 0);
    }

    #[test]
    fn no_fire_when_market_already_priced_above_model() {
        let mut s = StatStrategy::new(cfg(), vec![yes_rule(0.4, 1)]);
        let result = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[], &[(30, 100)]),
            Instant::now(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn no_fire_below_min_edge_threshold() {
        // Tiny edge: model 71, ask 70 → 1¢ raw, fees swamp it.
        let mut s = StatStrategy::new(cfg(), vec![yes_rule(0.71, 5)]);
        let result = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[], &[(30, 100)]),
            Instant::now(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let mut s = StatStrategy::new(
            StatConfig {
                cooldown: Duration::from_secs(10),
                ..cfg()
            },
            vec![yes_rule(0.85, 2)],
        );
        let now = Instant::now();
        assert!(
            s.evaluate(&MarketTicker::new("X"), &book(&[], &[(30, 100)]), now)
                .is_some()
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("X"),
                &book(&[], &[(30, 100)]),
                now + Duration::from_millis(100),
            )
            .is_none()
        );
    }

    #[test]
    fn no_rule_for_unknown_market() {
        let mut s = StatStrategy::new(cfg(), vec![yes_rule(0.85, 2)]);
        assert!(
            s.evaluate(
                &MarketTicker::new("OTHER"),
                &book(&[], &[(30, 100)]),
                Instant::now(),
            )
            .is_none()
        );
    }

    #[test]
    fn invalid_model_p_skips_silently() {
        let mut s = StatStrategy::new(
            cfg(),
            vec![StatRule {
                kalshi_market: MarketTicker::new("X"),
                model_p: 1.0, // boundary — degenerate
                side: Side::Yes,
                min_edge_cents: 1,
                settlement_date: None,
                generated_at_utc: None,
            }],
        );
        assert!(
            s.evaluate(
                &MarketTicker::new("X"),
                &book(&[], &[(30, 100)]),
                Instant::now(),
            )
            .is_none()
        );
    }

    #[test]
    fn no_side_rule_uses_complement_probability() {
        // Model says YES is 0.30 → NO model_p = 0.70.
        // Best NO ask = 100 - best YES bid = 100 - 40 = 60.
        // bet_p = 0.70, ask_dollars = 0.60 → 10¢ edge − fee ≥ threshold.
        let no_rule = StatRule {
            kalshi_market: MarketTicker::new("X"),
            model_p: 0.30,
            side: Side::No,
            min_edge_cents: 2,
            settlement_date: None,
            generated_at_utc: None,
        };
        let mut s = StatStrategy::new(cfg(), vec![no_rule]);
        let intent = s
            .evaluate(
                &MarketTicker::new("X"),
                &book(&[(40, 100)], &[]),
                Instant::now(),
            )
            .expect("NO side should fire");
        assert_eq!(intent.side, Side::No);
        assert_eq!(intent.price.cents(), 60);
    }

    #[test]
    fn size_capped_by_thinnest_book() {
        let mut s = StatStrategy::new(
            StatConfig {
                bankroll_cents: 1_000_000,
                kelly_factor: 1.0,
                max_size: 100_000,
                cooldown: Duration::from_millis(1),
            },
            vec![yes_rule(0.99, 1)],
        );
        // Book only has 3 contracts at the touch.
        let intent = s
            .evaluate(
                &MarketTicker::new("X"),
                &book(&[], &[(30, 3)]),
                Instant::now(),
            )
            .expect("should fire");
        assert_eq!(intent.qty.get(), 3, "size capped by available qty");
    }
}
