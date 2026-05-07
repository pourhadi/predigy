// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! `predigy-strategy-cross-arb` — cross-venue stat-arb between
//! Kalshi and Polymarket. Implements
//! [`predigy_engine_core::Strategy`].
//!
//! ## Signal
//!
//! Polymarket and Kalshi quote the same kinds of events, often
//! with offsetting depth profiles and small but persistent price
//! gaps. When Kalshi prices a YES contract noticeably *lower*
//! than Polymarket's mid for the equivalent token, we expect
//! convergence and buy on Kalshi. We never execute on Polymarket
//! — Poly is the reference, not the counter.
//!
//! Edge equation per pair:
//!
//! ```text
//! yes_edge_¢ = poly_yes_mid_¢ − kalshi_yes_ask_¢ − taker_fee
//! no_edge_¢  = poly_no_mid_¢  − kalshi_no_ask_¢  − taker_fee
//! ```
//!
//! `poly_no_mid_¢ = 100 − poly_yes_mid_¢` (binary contracts sum
//! to $1). Either side can fire independently — there's no
//! requirement to lift both legs together since this isn't pure
//! arb.
//!
//! ## What this is and is not
//!
//! - **Stat-arb, not pure arb.** Convergence is statistical, not
//!   mechanical. The OMS's daily-loss + per-side caps are the
//!   load-bearing backstop.
//! - **Per-pair, not portfolio.** Each Kalshi ↔ Polymarket pair
//!   is evaluated independently.
//! - **Reference-only on Poly.** We never submit a Polymarket
//!   order — only its book is read.
//!
//! ## Engine wiring
//!
//! - `external_subscriptions() -> ["polymarket"]` — the engine's
//!   external-feed dispatcher routes
//!   `ExternalEvent::PolymarketBook` for every asset_id any
//!   cross-arb pair references into the strategy's queue.
//! - `Event::PairUpdate { added, removed }` — emitted by the
//!   pair-file dispatcher when the curator-managed pair file
//!   changes. The engine has already registered the new Kalshi
//!   tickers with the router and subscribed the new Poly assets
//!   on the dispatcher; the strategy just updates its internal
//!   `market_map`.
//! - `Event::BookUpdate { market, book }` — Kalshi book deltas
//!   for paired tickers. Each delta triggers an `evaluate(...)`
//!   pass that returns 0/1/2 intents.

use async_trait::async_trait;
use predigy_book::OrderBook;
use predigy_core::market::MarketTicker;
use predigy_core::side::Side;
use predigy_engine_core::events::{Event, ExternalEvent, KalshiPolyPair};
use predigy_engine_core::intent::{Intent, IntentAction, OrderType, Tif};
use predigy_engine_core::state::StrategyState;
use predigy_engine_core::strategy::{Strategy, StrategyId};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info};

pub const STRATEGY_ID: StrategyId = StrategyId("cross-arb");

#[derive(Debug, Clone)]
pub struct CrossArbConfig {
    /// Minimum edge per contract, in cents, after the taker fee,
    /// to fire a trade. Per-leg.
    pub min_edge_cents: u32,
    /// Max contracts per fire. The OMS+risk caps may downsize
    /// further.
    pub max_size: u32,
    /// Cooldown between submits on the same Kalshi market.
    pub cooldown: Duration,
    /// **Phase 6.2 active exits**:
    /// take-profit threshold in cents per contract.
    /// Convergence-aware: when the Kalshi mark moves favorably
    /// toward Polymarket's reference, we lock in profit.
    /// `0` disables.
    pub take_profit_cents: i32,
    /// Stop-loss threshold in cents per contract. Triggered when
    /// Kalshi moves adversely (poly didn't follow through, or
    /// reverted). `0` disables.
    pub stop_loss_cents: i32,
    /// How often to refresh the open-position cache from Postgres.
    pub position_refresh_interval: Duration,
}

impl Default for CrossArbConfig {
    fn default() -> Self {
        Self {
            min_edge_cents: 1,
            max_size: 25,
            cooldown: Duration::from_millis(500),
            // Phase 6.2 defaults: take 5¢ profit, cap 4¢ loss.
            // Cross-arb's edges per fire are smaller than stat's
            // (it scalps small convergences), so the exit
            // thresholds are correspondingly tighter.
            take_profit_cents: 5,
            stop_loss_cents: 4,
            position_refresh_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PolyRef {
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

impl PolyRef {
    pub fn mid(self) -> Option<f64> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) => Some(f64::midpoint(a, b)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

/// Phase 6.2 — in-memory position snapshot. Refreshed on Tick
/// from Postgres; stale up to `position_refresh_interval` (60s
/// default). For exit logic this is acceptable; new positions
/// become exit-eligible within one cadence of the fill.
#[derive(Debug, Clone)]
struct CachedPosition {
    side: Side,
    /// Signed: positive = long.
    signed_qty: i32,
    avg_entry_cents: i32,
}

#[derive(Debug)]
pub struct CrossArbStrategy {
    config: CrossArbConfig,
    /// Kalshi ticker → Polymarket asset_id. Populated from
    /// Event::PairUpdate.
    market_map: HashMap<MarketTicker, String>,
    /// Latest Polymarket reference per asset_id.
    poly_ref: HashMap<String, PolyRef>,
    /// Per-Kalshi-market submit cooldown.
    last_submit_at: HashMap<MarketTicker, Instant>,
    /// Phase 6.2 — open positions per (ticker, side). Key is
    /// `"{ticker}:{side_tag}"`.
    positions: HashMap<String, CachedPosition>,
    /// Phase 6.2 — per-position exit cooldown.
    last_exit_at: HashMap<String, Instant>,
    last_position_refresh: Option<Instant>,
}

impl CrossArbStrategy {
    pub fn new(config: CrossArbConfig) -> Self {
        Self {
            config,
            market_map: HashMap::new(),
            poly_ref: HashMap::new(),
            last_submit_at: HashMap::new(),
            positions: HashMap::new(),
            last_exit_at: HashMap::new(),
            last_position_refresh: None,
        }
    }

    pub fn config(&self) -> &CrossArbConfig {
        &self.config
    }

    pub fn pair_count(&self) -> usize {
        self.market_map.len()
    }

    fn add_pair(&mut self, kalshi: MarketTicker, poly_asset: String) {
        self.market_map.insert(kalshi, poly_asset);
    }

    fn remove_pair(&mut self, kalshi: &MarketTicker) -> Option<String> {
        self.last_submit_at.remove(kalshi);
        self.market_map.remove(kalshi)
    }

    fn apply_pair_update(&mut self, added: &[KalshiPolyPair], removed: &[MarketTicker]) {
        for p in added {
            self.add_pair(p.kalshi_ticker.clone(), p.poly_asset_id.clone());
        }
        for t in removed {
            // Drop the poly_ref tied to the removed pair too. If
            // multiple kalshi markets shared one poly asset we'd
            // want to refcount, but the curator emits 1:1 pairs.
            if let Some(asset_id) = self.remove_pair(t) {
                self.poly_ref.remove(&asset_id);
            }
        }
        info!(
            n_pairs = self.market_map.len(),
            n_added = added.len(),
            n_removed = removed.len(),
            "cross-arb: pair map updated"
        );
    }

    fn update_poly(&mut self, asset_id: &str, best_bid: Option<f64>, best_ask: Option<f64>) {
        let entry = self.poly_ref.entry(asset_id.to_string()).or_default();
        if best_bid.is_some() {
            entry.best_bid = best_bid;
        }
        if best_ask.is_some() {
            entry.best_ask = best_ask;
        }
    }

    fn evaluate(&mut self, market: &MarketTicker, book: &OrderBook, now: Instant) -> Vec<Intent> {
        let Some(asset_id) = self.market_map.get(market).cloned() else {
            return Vec::new();
        };
        let Some(poly_ref) = self.poly_ref.get(&asset_id) else {
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
        let poly_no_mid_cents = 100u8.saturating_sub(poly_yes_mid_cents);
        let mut intents = Vec::new();

        if let Some((no_bid_px, no_bid_qty)) = book.best_no_bid() {
            let yes_ask_cents = 100u8.saturating_sub(no_bid_px.cents());
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
        if let Some((yes_bid_px, yes_bid_qty)) = book.best_yes_bid() {
            let no_ask_cents = 100u8.saturating_sub(yes_bid_px.cents());
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

    /// Phase 6.2 — refresh the in-memory open-position cache from
    /// Postgres.
    async fn refresh_positions(
        &mut self,
        state: &mut StrategyState,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let rows = state.db.open_positions(Some(STRATEGY_ID.0)).await?;
        let n = rows.len();
        let mut next: HashMap<String, CachedPosition> = HashMap::with_capacity(n);
        for r in rows {
            let side = match r.side.as_str() {
                "yes" => Side::Yes,
                "no" => Side::No,
                _ => continue,
            };
            let key = position_key(&r.ticker, side);
            next.insert(
                key,
                CachedPosition {
                    side,
                    signed_qty: r.current_qty,
                    avg_entry_cents: r.avg_entry_cents,
                },
            );
        }
        self.positions = next;
        self.last_position_refresh = Some(Instant::now());
        debug!(n_positions = n, "cross-arb: position cache refreshed");
        Ok(())
    }

    /// Phase 6.2 — evaluate open positions on this market for
    /// take-profit / stop-loss exits. Returns 0/1 closing intent.
    fn evaluate_exit(
        &mut self,
        market: &MarketTicker,
        book: &OrderBook,
        now: Instant,
    ) -> Option<Intent> {
        for side in [Side::Yes, Side::No] {
            let key = position_key(market.as_str(), side);
            let pos = match self.positions.get(&key) {
                Some(p) => p.clone(),
                None => continue,
            };
            if pos.signed_qty == 0 {
                continue;
            }
            if let Some(&last) = self.last_exit_at.get(&key)
                && now.duration_since(last) < self.config.cooldown
            {
                continue;
            }

            // Mark = price we'd realize unwinding. Cross-arb only
            // ever buys (it's a stat-arb against poly reference);
            // signed_qty should be positive for normal flows. We
            // handle short fallback for safety.
            let mark_cents = match (pos.side, pos.signed_qty.is_positive()) {
                (Side::Yes, true) => book.best_yes_bid()?.0.cents() as i32,
                (Side::No, true) => book.best_no_bid()?.0.cents() as i32,
                (Side::Yes, false) => 100i32 - book.best_no_bid()?.0.cents() as i32,
                (Side::No, false) => 100i32 - book.best_yes_bid()?.0.cents() as i32,
            };

            let pnl_per = if pos.signed_qty > 0 {
                mark_cents - pos.avg_entry_cents
            } else {
                pos.avg_entry_cents - mark_cents
            };

            let take =
                self.config.take_profit_cents > 0 && pnl_per >= self.config.take_profit_cents;
            let stop = self.config.stop_loss_cents > 0 && pnl_per <= -self.config.stop_loss_cents;
            if !(take || stop) {
                continue;
            }

            let action = if pos.signed_qty > 0 {
                IntentAction::Sell
            } else {
                IntentAction::Buy
            };
            let abs_qty = pos.signed_qty.unsigned_abs() as i32;
            let limit_cents = mark_cents.clamp(1, 99);
            let side_tag = match pos.side {
                Side::Yes => "Y",
                Side::No => "N",
            };
            let reason_tag = if take { "tp" } else { "sl" };
            // No minute-bucket on the exit cid (cross-arb fires
            // sub-second); use the Kalshi limit + qty as the
            // discriminator instead so repeated triggers at the
            // same price collapse via OMS idempotency.
            let client_id = format!(
                "cross-arb-exit:{ticker}:{side_tag}:{tag}:{lim:02}:{qty:04}",
                ticker = market.as_str(),
                tag = reason_tag,
                lim = limit_cents,
                qty = abs_qty,
            );
            let intent = Intent {
                client_id,
                strategy: STRATEGY_ID.0,
                market: market.clone(),
                side: pos.side,
                action,
                price_cents: Some(limit_cents),
                qty: abs_qty,
                order_type: OrderType::Limit,
                tif: Tif::Ioc,
                reason: Some(format!(
                    "cross-arb-exit: {reason_tag} entry={}¢ mark={}¢ pnl={}¢/contract",
                    pos.avg_entry_cents, mark_cents, pnl_per
                )),
            };
            info!(
                market = %market.as_str(),
                side = ?pos.side,
                signed_qty = pos.signed_qty,
                avg_entry = pos.avg_entry_cents,
                mark = mark_cents,
                pnl_per,
                trigger = reason_tag,
                "cross-arb: emitting exit"
            );
            self.last_exit_at.insert(key, now);
            return Some(intent);
        }
        None
    }
}

fn position_key(ticker: &str, side: Side) -> String {
    let tag = match side {
        Side::Yes => 'y',
        Side::No => 'n',
    };
    format!("{ticker}:{tag}")
}

#[async_trait]
impl Strategy for CrossArbStrategy {
    fn id(&self) -> StrategyId {
        STRATEGY_ID
    }

    async fn subscribed_markets(
        &self,
        _state: &StrategyState,
    ) -> Result<Vec<MarketTicker>, Box<dyn std::error::Error + Send + Sync>> {
        // Pure pair-file driven. The pair-file dispatcher
        // auto-registers added tickers with the router on
        // PairUpdate. Returning a static list here would force
        // operators to seed pairs at engine boot, defeating the
        // curator's incremental output.
        Ok(Vec::new())
    }

    fn external_subscriptions(&self) -> Vec<&'static str> {
        vec!["polymarket"]
    }

    async fn on_event(
        &mut self,
        ev: &Event,
        state: &mut StrategyState,
    ) -> Result<Vec<Intent>, Box<dyn std::error::Error + Send + Sync>> {
        // Phase 6.2 — refresh the open-position cache on the
        // configured cadence + on first call. Stale up to one
        // position_refresh_interval; new fills become exit-
        // eligible within that window.
        let needs_refresh = self
            .last_position_refresh
            .is_none_or(|t| t.elapsed() >= self.config.position_refresh_interval);
        if needs_refresh {
            self.refresh_positions(state).await?;
        }

        match ev {
            Event::BookUpdate { market, book } => {
                let now = Instant::now();
                let mut intents = self.evaluate(market, book, now);
                if let Some(exit) = self.evaluate_exit(market, book, now) {
                    intents.push(exit);
                }
                Ok(intents)
            }
            Event::External(ExternalEvent::PolymarketBook {
                asset_id,
                best_bid,
                best_ask,
            }) => {
                self.update_poly(asset_id, *best_bid, *best_ask);
                Ok(Vec::new())
            }
            Event::PairUpdate { added, removed } => {
                self.apply_pair_update(added, removed);
                Ok(Vec::new())
            }
            Event::External(_) | Event::Tick | Event::DiscoveryDelta { .. } => Ok(Vec::new()),
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        // Periodic ticks drive the position-cache refresh in the
        // absence of book updates (e.g. quiet markets). Shares
        // the same cadence as the cache freshness window.
        Some(self.config.position_refresh_interval)
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
        return None;
    }
    if kalshi_ask_cents == 0 || kalshi_ask_cents >= 100 {
        return None;
    }
    let raw_edge = u32::from(poly_mid_cents) - u32::from(kalshi_ask_cents);
    // Per-contract fee. Kalshi taker fee formula approximated
    // per-contract: floor(0.07 × price × (1 − price) × 100), with
    // a 1¢ minimum on the per-contract fee. We round up to be
    // conservative on edge accounting.
    let p = f64::from(kalshi_ask_cents) / 100.0;
    let fee_per_contract_cents = ((0.07 * p * (1.0 - p)) * 100.0).ceil().max(1.0) as u32;
    if raw_edge <= fee_per_contract_cents {
        debug!(
            market = %market,
            ?side,
            raw_edge,
            fee_per_contract_cents,
            "cross-arb: edge below per-contract fee"
        );
        return None;
    }
    let net_edge = raw_edge - fee_per_contract_cents;
    if net_edge < min_edge_cents {
        return None;
    }
    let qty = i32::try_from(available_qty).ok()?;
    if qty <= 0 {
        return None;
    }
    // Idempotency: include kalshi_ask in the cid so the same
    // touch at a different price gets a fresh row, but the same
    // touch at the same price within cooldown collapses (the
    // cooldown is the primary dedup; cid is a secondary safety
    // net).
    let side_tag = match side {
        Side::Yes => "Y",
        Side::No => "N",
    };
    let client_id = format!(
        "cross-arb:{ticker}:{side_tag}:{ask:02}:{qty:04}",
        ticker = market.as_str(),
        ask = kalshi_ask_cents,
    );
    Some(Intent {
        client_id,
        strategy: STRATEGY_ID.0,
        market: market.clone(),
        side,
        action: IntentAction::Buy,
        price_cents: Some(i32::from(kalshi_ask_cents)),
        qty,
        order_type: OrderType::Limit,
        tif: Tif::Ioc,
        reason: Some(format!(
            "cross-arb: poly_mid={poly_mid_cents}¢ k_ask={kalshi_ask_cents}¢ edge={net_edge}¢"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;
    use predigy_core::price::Price;

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

    fn cfg() -> CrossArbConfig {
        CrossArbConfig {
            min_edge_cents: 1,
            max_size: 10,
            cooldown: Duration::from_millis(1),
            // Tight thresholds let unit tests drive exits
            // deterministically.
            take_profit_cents: 5,
            stop_loss_cents: 4,
            position_refresh_interval: Duration::from_secs(60),
        }
    }

    fn cached_position(side: Side, signed_qty: i32, avg_entry_cents: i32) -> CachedPosition {
        CachedPosition {
            side,
            signed_qty,
            avg_entry_cents,
        }
    }

    #[test]
    fn no_intent_until_pair_added() {
        let mut s = CrossArbStrategy::new(cfg());
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(60, 100)], &[(50, 100)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn no_intent_until_poly_reference_arrives() {
        let mut s = CrossArbStrategy::new(cfg());
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(60, 100)], &[(50, 100)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn buys_kalshi_yes_when_kalshi_underprices_vs_poly() {
        // Kalshi YES ask = 100 − no_bid(30) = 70¢. Poly mid = 80¢.
        // raw edge = 10¢. Fee at 70¢ ≈ 1.47 → 2¢. Net = 8¢ ≥ 1¢ min.
        let mut s = CrossArbStrategy::new(cfg());
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        s.update_poly("0xabc", Some(0.78), Some(0.82));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(20, 5)], &[(30, 50)]),
            Instant::now(),
        );
        assert_eq!(intents.len(), 1);
        let i = &intents[0];
        assert_eq!(i.market.as_str(), "X");
        assert_eq!(i.side, Side::Yes);
        assert_eq!(i.action, IntentAction::Buy);
        assert_eq!(i.price_cents, Some(70));
        assert_eq!(i.tif, Tif::Ioc);
        assert!(i.client_id.starts_with("cross-arb:X:Y:70:"));
    }

    #[test]
    fn buys_kalshi_no_when_kalshi_underprices_vs_poly_no() {
        // Kalshi NO ask = 100 − yes_bid(20) = 80¢. Poly NO mid =
        // 100 − 30 = 70¢ → 70 < 80, no edge. Try the other
        // direction: yes_bid 5 → no_ask 95, poly_no_mid = 90 →
        // poly < ask, no edge. Use yes_bid 5, poly = 0.10 (low)
        // → poly_yes=10, poly_no=90, no_ask = 95 — still no edge.
        // To make NO fire: kalshi yes_bid LOW, poly_yes LOW,
        // so no_ask LOW and poly_no HIGH.
        // yes_bid 25 → no_ask=75, poly_yes=0.10 → poly_no=90 → edge=15.
        let mut s = CrossArbStrategy::new(cfg());
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        s.update_poly("0xabc", Some(0.08), Some(0.12));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(25, 50)], &[(80, 5)]),
            Instant::now(),
        );
        let no = intents
            .iter()
            .find(|i| i.side == Side::No)
            .expect("NO leg fires");
        assert_eq!(no.price_cents, Some(75));
    }

    #[test]
    fn no_fire_when_below_min_edge() {
        let mut s = CrossArbStrategy::new(CrossArbConfig {
            min_edge_cents: 50,
            max_size: 10,
            cooldown: Duration::from_millis(1),
            take_profit_cents: 5,
            stop_loss_cents: 4,
            position_refresh_interval: Duration::from_secs(60),
        });
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        s.update_poly("0xabc", Some(0.78), Some(0.82));
        let intents = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(20, 5)], &[(30, 50)]),
            Instant::now(),
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn cooldown_blocks_repeat() {
        let mut s = CrossArbStrategy::new(CrossArbConfig {
            min_edge_cents: 1,
            max_size: 10,
            cooldown: Duration::from_secs(60),
            take_profit_cents: 5,
            stop_loss_cents: 4,
            position_refresh_interval: Duration::from_secs(60),
        });
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        s.update_poly("0xabc", Some(0.78), Some(0.82));
        let now = Instant::now();
        let first = s.evaluate(&MarketTicker::new("X"), &book(&[(20, 5)], &[(30, 50)]), now);
        assert!(!first.is_empty());
        let second = s.evaluate(
            &MarketTicker::new("X"),
            &book(&[(20, 5)], &[(30, 50)]),
            now + Duration::from_millis(100),
        );
        assert!(second.is_empty());
    }

    #[test]
    fn pair_update_added_populates_map() {
        let mut s = CrossArbStrategy::new(cfg());
        let added = vec![
            KalshiPolyPair {
                kalshi_ticker: MarketTicker::new("KX-A"),
                poly_asset_id: "0xa".into(),
            },
            KalshiPolyPair {
                kalshi_ticker: MarketTicker::new("KX-B"),
                poly_asset_id: "0xb".into(),
            },
        ];
        s.apply_pair_update(&added, &[]);
        assert_eq!(s.pair_count(), 2);
    }

    #[test]
    fn pair_update_removed_drops_map_and_poly_ref() {
        let mut s = CrossArbStrategy::new(cfg());
        s.add_pair(MarketTicker::new("X"), "0xabc".into());
        s.update_poly("0xabc", Some(0.5), Some(0.5));
        s.last_submit_at
            .insert(MarketTicker::new("X"), Instant::now());
        s.apply_pair_update(&[], &[MarketTicker::new("X")]);
        assert_eq!(s.pair_count(), 0);
        assert!(!s.poly_ref.contains_key("0xabc"));
        assert!(!s.last_submit_at.contains_key(&MarketTicker::new("X")));
    }

    #[test]
    fn declares_polymarket_external_subscription() {
        let s = CrossArbStrategy::new(cfg());
        assert_eq!(s.external_subscriptions(), vec!["polymarket"]);
    }

    // ─── Phase 6.2 active-exit tests ─────────────────────────

    #[test]
    fn exit_take_profit_long_yes() {
        // Long YES at 50¢. Mark = 56¢. PnL = +6¢ ≥ tp(5¢).
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-A");
        s.positions.insert(
            position_key("KX-EXIT-A", Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book(&[(56, 100)], &[(40, 100)]);
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("take-profit fires");
        assert_eq!(intent.side, Side::Yes);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 4);
        assert_eq!(intent.price_cents, Some(56));
        assert_eq!(intent.tif, Tif::Ioc);
        assert!(
            intent
                .client_id
                .starts_with("cross-arb-exit:KX-EXIT-A:Y:tp:")
        );
    }

    #[test]
    fn exit_stop_loss_long_yes() {
        // Long YES at 50¢. Mark = 45¢. PnL = -5¢ ≤ -sl(4¢).
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-B");
        s.positions.insert(
            position_key("KX-EXIT-B", Side::Yes),
            cached_position(Side::Yes, 6, 50),
        );
        let book = book(&[(45, 100)], &[(50, 100)]);
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("stop-loss fires");
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.qty, 6);
        assert_eq!(intent.price_cents, Some(45));
        assert!(intent.client_id.contains(":sl:"));
    }

    #[test]
    fn no_exit_inside_band() {
        // Long YES at 50¢. Mark = 53¢. PnL = +3¢, inside [-4, +5).
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-C");
        s.positions.insert(
            position_key("KX-EXIT-C", Side::Yes),
            cached_position(Side::Yes, 3, 50),
        );
        let book = book(&[(53, 100)], &[(40, 100)]);
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }

    #[test]
    fn exit_take_profit_long_no() {
        // Long NO at 30¢. Mark (best NO bid) = 36¢. PnL = +6¢ ≥
        // tp(5¢). Sell-NO IOC at the touch.
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-D");
        s.positions.insert(
            position_key("KX-EXIT-D", Side::No),
            cached_position(Side::No, 5, 30),
        );
        let book = book(&[(60, 100)], &[(36, 100)]);
        let intent = s
            .evaluate_exit(&market, &book, Instant::now())
            .expect("NO-side take-profit fires");
        assert_eq!(intent.side, Side::No);
        assert_eq!(intent.action, IntentAction::Sell);
        assert_eq!(intent.price_cents, Some(36));
    }

    #[test]
    fn exit_cooldown_blocks_repeat() {
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-E");
        s.positions.insert(
            position_key("KX-EXIT-E", Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book(&[(56, 100)], &[(40, 100)]);
        let now = Instant::now();
        assert!(s.evaluate_exit(&market, &book, now).is_some());
        assert!(s.evaluate_exit(&market, &book, now).is_none());
    }

    #[test]
    fn exit_disabled_when_thresholds_zero() {
        let mut cfg_off = cfg();
        cfg_off.take_profit_cents = 0;
        cfg_off.stop_loss_cents = 0;
        let mut s = CrossArbStrategy::new(cfg_off);
        let market = MarketTicker::new("KX-EXIT-F");
        s.positions.insert(
            position_key("KX-EXIT-F", Side::Yes),
            cached_position(Side::Yes, 4, 50),
        );
        let book = book(&[(95, 100)], &[(2, 100)]);
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }

    #[test]
    fn exit_only_for_known_position() {
        let mut s = CrossArbStrategy::new(cfg());
        let market = MarketTicker::new("KX-EXIT-G");
        let book = book(&[(56, 100)], &[(40, 100)]);
        assert!(s.evaluate_exit(&market, &book, Instant::now()).is_none());
    }
}
