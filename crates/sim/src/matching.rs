//! IOC matching against an `OrderBook`. Pure (modulo the &mut book)
//! and isolated for unit tests.
//!
//! Walks **a single price level only** (the touch). That covers
//! the strategies we have today (`arb-trader` always quotes the
//! touch, so a multi-level walk would be wasted code) and keeps the
//! sim semantically equivalent to a "best-execution" guarantee at
//! the displayed touch. Multi-level walks land when a strategy that
//! sweeps the book arrives.
//!
//! Sells are deliberately not matched — strategies on Kalshi express
//! "exit" as a buy of the opposite side, so a Sell intent in the sim
//! would be a strategy bug and we surface it via `Match::Unsupported`.

use predigy_book::{Delta, OrderBook};
use predigy_core::fees::taker_fee;
use predigy_core::fill::Fill;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId};
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Some or all of the order matched at the touch.
    Filled {
        fill: Fill,
        /// Total filled qty. May be < `order.qty` for a partial.
        cumulative_qty: u32,
        /// True iff the order's full size was consumed.
        terminal: bool,
    },
    /// No liquidity at-or-better-than the limit price.
    NoLiquidity,
    /// Sell intents aren't modelled in this simulator.
    Unsupported(&'static str),
}

/// Match an IOC order against `book`, mutating the book to consume
/// the matched liquidity. Returns the resulting [`Match`].
///
/// `now_ms` is stamped onto the synthesised `Fill`.
pub fn match_ioc(book: &mut OrderBook, order: &Order, now_ms: i64) -> Match {
    match (order.side, order.action) {
        (Side::Yes, Action::Buy) => match_buy_yes(book, order, now_ms),
        (Side::No, Action::Buy) => match_buy_no(book, order, now_ms),
        (_, Action::Sell) => Match::Unsupported(
            "sim does not match sells; strategies should use buy-of-opposite-side instead",
        ),
    }
}

fn match_buy_yes(book: &mut OrderBook, order: &Order, now_ms: i64) -> Match {
    let Some((best_no_bid_price, best_no_bid_qty)) = book.best_no_bid() else {
        return Match::NoLiquidity;
    };
    // YES ask price by complement.
    let Some(yes_ask_cents) = 100u8.checked_sub(best_no_bid_price.cents()) else {
        return Match::NoLiquidity;
    };
    if yes_ask_cents > order.price.cents() {
        return Match::NoLiquidity;
    }
    let Ok(fill_price) = Price::from_cents(yes_ask_cents) else {
        return Match::NoLiquidity;
    };
    let fill_qty = order.qty.get().min(best_no_bid_qty);
    if fill_qty == 0 {
        return Match::NoLiquidity;
    }
    consume(book, order, Side::No, best_no_bid_price, fill_qty);
    Match::Filled {
        fill: synth_fill(order, fill_price, fill_qty, now_ms),
        cumulative_qty: fill_qty,
        terminal: fill_qty == order.qty.get(),
    }
}

fn match_buy_no(book: &mut OrderBook, order: &Order, now_ms: i64) -> Match {
    let Some((best_yes_bid_price, best_yes_bid_qty)) = book.best_yes_bid() else {
        return Match::NoLiquidity;
    };
    let Some(no_ask_cents) = 100u8.checked_sub(best_yes_bid_price.cents()) else {
        return Match::NoLiquidity;
    };
    if no_ask_cents > order.price.cents() {
        return Match::NoLiquidity;
    }
    let Ok(fill_price) = Price::from_cents(no_ask_cents) else {
        return Match::NoLiquidity;
    };
    let fill_qty = order.qty.get().min(best_yes_bid_qty);
    if fill_qty == 0 {
        return Match::NoLiquidity;
    }
    consume(book, order, Side::Yes, best_yes_bid_price, fill_qty);
    Match::Filled {
        fill: synth_fill(order, fill_price, fill_qty, now_ms),
        cumulative_qty: fill_qty,
        terminal: fill_qty == order.qty.get(),
    }
}

/// Apply a synthetic delta that decrements `consumed_qty` from
/// `(consumed_side, consumed_price)`. The book's sequence number is
/// advanced past whatever the recorded stream used so the delta
/// won't trigger a `Gap`.
fn consume(
    book: &mut OrderBook,
    order: &Order,
    consumed_side: Side,
    consumed_price: Price,
    consumed_qty: u32,
) {
    let next_seq = book.last_seq().map_or(0, |s| s + 1);
    let delta = Delta {
        market: order.market.as_str().to_string(),
        seq: next_seq,
        side: consumed_side,
        price: consumed_price,
        qty_delta: -i32::try_from(consumed_qty).unwrap_or(i32::MAX),
    };
    book.apply_delta(&delta);
}

fn synth_fill(order: &Order, fill_price: Price, fill_qty: u32, now_ms: i64) -> Fill {
    let qty = Qty::new(fill_qty).expect("fill_qty > 0 enforced by caller");
    let fee_cents = taker_fee(fill_price, qty);
    Fill {
        order_id: OrderId::new(order.client_id.as_str().to_string()),
        market: MarketTicker::new(order.market.as_str()),
        side: order.side,
        action: order.action,
        price: fill_price,
        qty,
        is_maker: false,
        fee_cents,
        ts_ms: u64::try_from(now_ms).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_book::Snapshot;
    use predigy_core::order::{OrderType, TimeInForce};

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn book_with(yes_bids: &[(u8, u32)], no_bids: &[(u8, u32)]) -> OrderBook {
        let mut b = OrderBook::new("X");
        b.apply_snapshot(Snapshot {
            seq: 1,
            yes_bids: yes_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
            no_bids: no_bids.iter().map(|(c, q)| (p(*c), *q)).collect(),
        });
        b
    }

    fn buy(side: Side, price: u8, qty: u32) -> Order {
        Order {
            client_id: OrderId::new("c"),
            market: MarketTicker::new("X"),
            side,
            action: Action::Buy,
            price: p(price),
            qty: q(qty),
            order_type: OrderType::Limit,
            tif: TimeInForce::Ioc,
        }
    }

    #[test]
    fn buy_yes_at_touch_fills_full_size_and_consumes_liquidity() {
        // YES ask = 100 − best NO bid = 100 − 60 = 40. Buy YES @ 40, qty 30.
        let mut b = book_with(&[], &[(60, 100)]);
        let m = match_ioc(&mut b, &buy(Side::Yes, 40, 30), 1_700_000);
        match m {
            Match::Filled {
                fill,
                cumulative_qty,
                terminal,
            } => {
                assert_eq!(fill.price.cents(), 40);
                assert_eq!(fill.qty.get(), 30);
                assert!(terminal);
                assert_eq!(cumulative_qty, 30);
                assert!(!fill.is_maker);
                assert_eq!(fill.fee_cents, taker_fee(p(40), q(30)));
            }
            other => panic!("expected Filled, got {other:?}"),
        }
        // Liquidity consumed: 100 − 30 = 70 left at the 60¢ NO bid.
        assert_eq!(b.best_no_bid().unwrap().1, 70);
    }

    #[test]
    fn buy_yes_above_touch_does_not_match() {
        // YES ask 40, but limit is only 30 → no match.
        let mut b = book_with(&[], &[(60, 100)]);
        let m = match_ioc(&mut b, &buy(Side::Yes, 30, 30), 0);
        assert_eq!(m, Match::NoLiquidity);
        assert_eq!(b.best_no_bid().unwrap().1, 100);
    }

    #[test]
    fn buy_yes_partial_fills_when_touch_thinner_than_order() {
        // 5 contracts at the YES ask; ask qty = NO bid qty = 5.
        let mut b = book_with(&[], &[(60, 5)]);
        let m = match_ioc(&mut b, &buy(Side::Yes, 40, 30), 0);
        match m {
            Match::Filled {
                fill,
                cumulative_qty,
                terminal,
            } => {
                assert_eq!(fill.qty.get(), 5);
                assert_eq!(cumulative_qty, 5);
                assert!(!terminal);
            }
            other => panic!("expected partial Filled, got {other:?}"),
        }
        assert!(b.best_no_bid().is_none(), "level fully consumed");
    }

    #[test]
    fn buy_no_at_touch_fills() {
        // NO ask = 100 − best YES bid = 100 − 70 = 30. Buy NO @ 30, qty 10.
        let mut b = book_with(&[(70, 50)], &[]);
        let m = match_ioc(&mut b, &buy(Side::No, 30, 10), 0);
        match m {
            Match::Filled { fill, .. } => {
                assert_eq!(fill.side, Side::No);
                assert_eq!(fill.price.cents(), 30);
                assert_eq!(fill.qty.get(), 10);
            }
            other => panic!("expected Filled, got {other:?}"),
        }
        // YES bid level decremented from 50 → 40.
        assert_eq!(b.best_yes_bid().unwrap().1, 40);
    }

    #[test]
    fn empty_book_side_no_liquidity() {
        let mut b = book_with(&[], &[]);
        let m = match_ioc(&mut b, &buy(Side::Yes, 50, 1), 0);
        assert_eq!(m, Match::NoLiquidity);
    }

    #[test]
    fn sell_intent_unsupported() {
        let mut b = book_with(&[(40, 50)], &[(60, 50)]);
        let mut order = buy(Side::Yes, 40, 10);
        order.action = Action::Sell;
        let m = match_ioc(&mut b, &order, 0);
        assert!(matches!(m, Match::Unsupported(_)));
    }

    #[test]
    fn second_match_sees_first_consumption() {
        // Two takers in a row: first should consume some of the
        // touch, second should match the remainder.
        let mut b = book_with(&[], &[(60, 30)]);
        let m1 = match_ioc(&mut b, &buy(Side::Yes, 40, 20), 0);
        assert!(matches!(m1, Match::Filled { terminal: true, .. }));
        let m2 = match_ioc(&mut b, &buy(Side::Yes, 40, 20), 1);
        // Only 10 left at the touch.
        match m2 {
            Match::Filled {
                cumulative_qty,
                terminal,
                ..
            } => {
                assert_eq!(cumulative_qty, 10);
                assert!(!terminal);
            }
            other => panic!("expected partial Filled, got {other:?}"),
        }
        assert!(b.best_no_bid().is_none());
    }
}
