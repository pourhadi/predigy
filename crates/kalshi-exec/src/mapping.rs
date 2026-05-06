//! Translate between `predigy-core` domain types and Kalshi V2 REST
//! wire shapes.
//!
//! ## V2 createorder shape (May 2026 production)
//!
//! Kalshi V2 takes:
//!
//! - `side`: `"yes" | "no"` — the contract leg
//! - `action`: `"buy" | "sell"` — separate field, required
//! - `count`: integer contracts
//! - `yes_price` (cents 1..=99) iff side=yes, else `no_price`
//!
//! The (Side, Action) pair maps 1:1 to (side, action) on the wire —
//! no complement-price flipping. The price always rides on the leg
//! that matches `side`.

use crate::error::Error;
use predigy_core::fill::Fill as DomainFill;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId, OrderType, TimeInForce};
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_kalshi_rest::types::{
    CreateOrderRequest, FillRecord, OrderAction, OrderSideV2, SelfTradePreventionV2, TimeInForceV2,
};

/// Map a `predigy_core::Order` to a Kalshi V2 `CreateOrderRequest`.
///
/// `self_trade_prevention_type` is hard-coded to `TakerAtCross` —
/// strategies that rest a quote and may cross their own resting order
/// should be coded to keep their book consistent rather than rely on
/// the venue's STP. We can revisit if we run into a strategy that
/// needs `Maker`-side cancellation.
pub fn order_to_create_request(order: &Order) -> Result<CreateOrderRequest, Error> {
    if order.order_type != OrderType::Limit {
        return Err(Error::Unsupported(
            "Kalshi V2 only accepts limit orders; map market intents to IOC at the worst price",
        ));
    }
    let (wire_side, wire_action) = map_side_action(order.side, order.action);
    // For NO-side intents, Kalshi takes the YES-equivalent limit
    // price (complement). The `side` already encodes which book
    // side; `price` is always the YES-side dollar limit.
    let yes_equiv_cents = match order.side {
        Side::Yes => order.price.cents(),
        Side::No => 100u8.saturating_sub(order.price.cents()),
    };
    let (tif, post_only) = map_tif(order.tif);
    Ok(CreateOrderRequest {
        ticker: order.market.as_str().to_string(),
        client_order_id: order.client_id.as_str().to_string(),
        side: wire_side,
        action: wire_action,
        count: format!("{}.00", order.qty.get()),
        price: format_cents_to_dollars(yes_equiv_cents),
        time_in_force: tif,
        self_trade_prevention_type: SelfTradePreventionV2::TakerAtCross,
        post_only,
        reduce_only: None,
    })
}

/// `42` → `"0.4200"`. Kalshi expects 4-decimal precision; trailing
/// zeros are fine.
fn format_cents_to_dollars(cents: u8) -> String {
    let dollars = u32::from(cents) / 100;
    let frac = u32::from(cents) % 100;
    format!("{dollars}.{frac:02}00")
}

/// Map (domain Side, domain Action) → (wire side on the YES book,
/// wire action). The trader's economic intent is preserved on the
/// `action` field and on which `*_price` leg carries the limit; the
/// `side` field tells Kalshi which YES-book side the order sits on.
fn map_side_action(side: Side, action: Action) -> (OrderSideV2, OrderAction) {
    // The wire `side` is the YES-book side the order rests on. NO-
    // intent buys post on the YES ask side; NO-intent sells post on
    // the YES bid side (because buy-NO ≡ sell-YES at complement).
    let wire_side = match (side, action) {
        (Side::Yes, Action::Buy) | (Side::No, Action::Sell) => OrderSideV2::Bid,
        (Side::Yes, Action::Sell) | (Side::No, Action::Buy) => OrderSideV2::Ask,
    };
    let wire_action = match action {
        Action::Buy => OrderAction::Buy,
        Action::Sell => OrderAction::Sell,
    };
    (wire_side, wire_action)
}

/// `predigy_core::TimeInForce` → Kalshi V2 `time_in_force` + the
/// `post_only` boolean. `PostOnly` is GTC + `post_only=true` on the wire.
fn map_tif(tif: TimeInForce) -> (TimeInForceV2, Option<bool>) {
    match tif {
        TimeInForce::Gtc => (TimeInForceV2::GoodTillCanceled, None),
        TimeInForce::Ioc => (TimeInForceV2::ImmediateOrCancel, None),
        TimeInForce::Fok => (TimeInForceV2::FillOrKill, None),
        TimeInForce::PostOnly => (TimeInForceV2::GoodTillCanceled, Some(true)),
    }
}

/// Convert a Kalshi `FillRecord` into a `predigy_core::Fill`, using
/// caller-supplied `side` and `action` from the originating order's
/// tracking entry.
///
/// Rationale: Kalshi V2's fill records have an empty `action` field
/// and the wire `side` is the venue's book side (always YES
/// post-mapping for any (No, *) order), not the trader's intended
/// side. The executor knows the originating order's intended side
/// and action; it passes them in and we use those as authoritative.
///
/// The fill's price is taken from whichever wire leg matches the
/// resolved domain `side`: a YES-side fill uses `yes_price_dollars`,
/// NO-side fills use `no_price_dollars`. Out-of-range prices (Kalshi
/// sometimes reports `"0.00"` or `"1.00"` on settlement-priced
/// fills) are rejected — those should never reach a live trader, so
/// a hard error is the right signal.
pub fn fill_to_domain(
    record: &FillRecord,
    side: Side,
    action: Action,
) -> Result<DomainFill, Error> {
    let qty = parse_count_fp(&record.count_fp)?;
    let price_str = match side {
        Side::Yes => &record.yes_price_dollars,
        Side::No => &record.no_price_dollars,
    };
    let price = parse_price_dollars(price_str)?;
    let fee_cents = record
        .fee_cost
        .as_deref()
        .map(parse_dollars_to_cents)
        .transpose()?
        .unwrap_or(0);
    let ts_ms = record
        .ts_ms
        .unwrap_or_else(|| record.ts.unwrap_or(0) * 1_000);
    let ts_ms_u = u64::try_from(ts_ms).unwrap_or(0);
    let market_str = record.ticker_str();
    Ok(DomainFill {
        order_id: OrderId::new(record.order_id.clone()),
        market: MarketTicker::new(market_str),
        side,
        action,
        price,
        qty,
        is_maker: !record.is_taker.unwrap_or(false),
        fee_cents,
        ts_ms: ts_ms_u,
    })
}

fn parse_count_fp(s: &str) -> Result<Qty, Error> {
    let n: f64 = s
        .parse()
        .map_err(|_| Error::Decode(format!("count {s:?} not a number")))?;
    let rounded = n.round();
    if rounded < 1.0 || rounded > f64::from(u32::MAX) {
        return Err(Error::Decode(format!("count {s:?} out of range")));
    }
    Qty::new(rounded as u32).map_err(|_| Error::Decode(format!("count {s:?} is zero")))
}

fn parse_price_dollars(s: &str) -> Result<Price, Error> {
    let dollars: f64 = s
        .parse()
        .map_err(|_| Error::Decode(format!("price {s:?} not a number")))?;
    let cents_i = (dollars * 100.0).round() as i32;
    let cents_u8 =
        u8::try_from(cents_i).map_err(|_| Error::Decode(format!("price {s:?} out of u8")))?;
    Price::from_cents(cents_u8).map_err(|_| Error::Decode(format!("price {s:?} not 1..=99¢")))
}

fn parse_dollars_to_cents(s: &str) -> Result<u32, Error> {
    let dollars: f64 = s
        .parse()
        .map_err(|_| Error::Decode(format!("fee {s:?} not a number")))?;
    let cents_i = (dollars * 100.0).round() as i32;
    u32::try_from(cents_i).map_err(|_| Error::Decode(format!("fee {s:?} negative")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::order::{Order, OrderId};
    use predigy_core::price::{Price, Qty};

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn order(side: Side, action: Action, price: u8, qty: u32, tif: TimeInForce) -> Order {
        Order {
            client_id: OrderId::new("c"),
            market: MarketTicker::new("X"),
            side,
            action,
            price: p(price),
            qty: q(qty),
            order_type: OrderType::Limit,
            tif,
        }
    }

    #[test]
    fn maps_buy_yes_to_bid_with_yes_price_and_buy_action() {
        let req =
            order_to_create_request(&order(Side::Yes, Action::Buy, 42, 100, TimeInForce::Gtc))
                .unwrap();
        assert_eq!(req.side, OrderSideV2::Bid);
        assert_eq!(req.action, OrderAction::Buy);
        assert_eq!(req.price, "0.4200");
        assert_eq!(req.count, "100.00");
        assert!(matches!(req.time_in_force, TimeInForceV2::GoodTillCanceled));
        assert_eq!(req.post_only, None);
    }

    #[test]
    fn maps_sell_yes_to_ask_with_yes_price_and_sell_action() {
        let req =
            order_to_create_request(&order(Side::Yes, Action::Sell, 60, 10, TimeInForce::Ioc))
                .unwrap();
        assert_eq!(req.side, OrderSideV2::Ask);
        assert_eq!(req.action, OrderAction::Sell);
        assert_eq!(req.price, "0.6000");
    }

    #[test]
    fn maps_buy_no_to_ask_with_no_price_and_buy_action() {
        // Buy NO @ 30¢ → posts on YES ask side (sell-yes-equivalent),
        // but action stays `buy` and price rides `no_price` at face
        // value. Kalshi accepts (side=ask, action=buy, no_price=30).
        let req = order_to_create_request(&order(Side::No, Action::Buy, 30, 5, TimeInForce::Gtc))
            .unwrap();
        assert_eq!(req.side, OrderSideV2::Ask);
        assert_eq!(req.action, OrderAction::Buy);
        // Buy NO @ 30¢ ≡ buy YES at the complement (70¢) — Kalshi's
        // `price` is the YES-equivalent dollar limit.
        assert_eq!(req.price, "0.7000");
    }

    #[test]
    fn maps_sell_no_to_bid_with_no_price_and_sell_action() {
        let req = order_to_create_request(&order(Side::No, Action::Sell, 30, 5, TimeInForce::Gtc))
            .unwrap();
        assert_eq!(req.side, OrderSideV2::Bid);
        assert_eq!(req.action, OrderAction::Sell);
        assert_eq!(req.price, "0.7000");
    }

    #[test]
    fn post_only_becomes_gtc_plus_post_only_flag() {
        let req =
            order_to_create_request(&order(Side::Yes, Action::Buy, 42, 1, TimeInForce::PostOnly))
                .unwrap();
        assert!(matches!(req.time_in_force, TimeInForceV2::GoodTillCanceled));
        assert_eq!(req.post_only, Some(true));
    }

    #[test]
    fn rejects_market_order_type() {
        let mut o = order(Side::Yes, Action::Buy, 42, 1, TimeInForce::Ioc);
        o.order_type = OrderType::Market;
        let err = order_to_create_request(&o).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    fn fill_record(side: &str, action: &str, yes: &str, no: &str, count: &str) -> FillRecord {
        FillRecord {
            fill_id: "f-1".into(),
            trade_id: None,
            order_id: "o-1".into(),
            market_ticker: Some("X".into()),
            ticker: None,
            side: side.into(),
            action: action.into(),
            count_fp: count.into(),
            yes_price_dollars: yes.into(),
            no_price_dollars: no.into(),
            is_taker: Some(true),
            fee_cost: Some("0.07".into()),
            ts: Some(1_700_000_000),
            ts_ms: None,
        }
    }

    #[test]
    fn fill_to_domain_uses_yes_price_for_yes_side() {
        let f = fill_to_domain(
            &fill_record("yes", "buy", "0.4200", "0.5800", "10.00"),
            Side::Yes,
            Action::Buy,
        )
        .unwrap();
        assert_eq!(f.side, Side::Yes);
        assert_eq!(f.action, Action::Buy);
        assert_eq!(f.price.cents(), 42);
        assert_eq!(f.qty.get(), 10);
        assert_eq!(f.fee_cents, 7);
        assert_eq!(f.ts_ms, 1_700_000_000_000);
        assert!(!f.is_maker);
    }

    #[test]
    fn fill_to_domain_uses_no_price_for_no_side() {
        let f = fill_to_domain(
            &fill_record("no", "sell", "0.4200", "0.5800", "5.00"),
            Side::No,
            Action::Sell,
        )
        .unwrap();
        assert_eq!(f.side, Side::No);
        assert_eq!(f.price.cents(), 58);
    }

    /// Real Kalshi V2 fill records arrive with `action: ""` —
    /// the wire field is unused. The decoder must treat the
    /// caller-supplied (Side, Action) as authoritative and ignore
    /// `record.action` entirely.
    #[test]
    fn fill_to_domain_ignores_empty_wire_action_in_favor_of_caller_supplied() {
        // Real wire shape captured during the live shake-down on
        // KXNBASERIES-26LALOKCR2-LAL: action="", side="yes",
        // yes_price="0.0800". The originating order was (Yes, Buy).
        let f = fill_to_domain(
            &fill_record("yes", "", "0.0800", "0.9200", "1.00"),
            Side::Yes,
            Action::Buy,
        )
        .unwrap();
        assert_eq!(f.side, Side::Yes);
        assert_eq!(f.action, Action::Buy);
        assert_eq!(f.price.cents(), 8);
        assert_eq!(f.qty.get(), 1);
    }

    /// (No, *) orders are submitted as wire-YES at the complement
    /// price. Their fills come back with wire `side: "yes"`. The
    /// decoder must trust the caller-supplied `Side::No` and pull
    /// the price from `no_price_dollars` (the correct side from the
    /// trader's perspective).
    #[test]
    fn fill_to_domain_uses_caller_side_when_wire_side_is_post_mapping() {
        let f = fill_to_domain(
            &fill_record("yes", "", "0.0800", "0.9200", "3.00"),
            Side::No,
            Action::Buy,
        )
        .unwrap();
        assert_eq!(f.side, Side::No);
        assert_eq!(f.action, Action::Buy);
        assert_eq!(f.price.cents(), 92);
        assert_eq!(f.qty.get(), 3);
    }
}
