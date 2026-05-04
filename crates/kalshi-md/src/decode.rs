//! Convert Kalshi WS string-encoded fixed-point values into domain types.
//!
//! Kalshi quotes prices and sizes as decimal strings (`"0.0800"`, `"300.00"`,
//! `"-54.00"`) for forward-compatible precision. We represent prices in
//! whole cents (1..=99) and contract counts as integers, so each conversion
//! is a parse-then-round step that explicitly rejects out-of-range values
//! rather than silently truncating.

use crate::error::Error;
use crate::messages::{OrderbookDeltaBody, OrderbookSnapshotBody};
use predigy_book::{Delta, Snapshot};
use predigy_core::price::Price;
use predigy_core::side::Side;

/// Parse `"0.4200"` → `Price` (cents 1..=99). Rejects 0¢ and 100¢ (those are
/// settled positions, not tradable prices).
pub fn parse_price_dollars(s: &str) -> Result<Price, Error> {
    let dollars: f64 = s
        .parse()
        .map_err(|_| Error::OutOfRange(format!("price {s:?} not a number")))?;
    let cents_i = (dollars * 100.0).round() as i32;
    let cents_u8 =
        u8::try_from(cents_i).map_err(|_| Error::OutOfRange(format!("price {s:?} out of u8")))?;
    Price::from_cents(cents_u8).map_err(|_| Error::OutOfRange(format!("price {s:?} not 1..=99¢")))
}

/// Parse `"300.00"` → `u32`. Rejects negatives and any value that doesn't
/// round to a non-negative integer count.
pub fn parse_qty_fp(s: &str) -> Result<u32, Error> {
    let n: f64 = s
        .parse()
        .map_err(|_| Error::OutOfRange(format!("qty {s:?} not a number")))?;
    let rounded = n.round();
    if rounded < 0.0 {
        return Err(Error::OutOfRange(format!("qty {s:?} is negative")));
    }
    if rounded > f64::from(u32::MAX) {
        return Err(Error::OutOfRange(format!("qty {s:?} exceeds u32::MAX")));
    }
    Ok(rounded as u32)
}

/// Parse `"-54.00"` → `i32`. Used for the `delta_fp` field on
/// `orderbook_delta`. Sign is preserved; `+1.00` adds, `-1.00` lifts.
pub fn parse_delta_fp(s: &str) -> Result<i32, Error> {
    let n: f64 = s
        .parse()
        .map_err(|_| Error::OutOfRange(format!("delta {s:?} not a number")))?;
    let rounded = n.round();
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return Err(Error::OutOfRange(format!("delta {s:?} doesn't fit i32")));
    }
    Ok(rounded as i32)
}

/// Build a `predigy_book::Snapshot` from the wire body.
///
/// The `seq` argument comes from the envelope (not the body — Kalshi puts it
/// at the outer level). Levels with non-tradable prices or zero size are
/// silently dropped, matching `predigy_kalshi_rest::Client::orderbook_snapshot`.
pub fn snapshot_from_wire(body: &OrderbookSnapshotBody, seq: u64) -> Result<Snapshot, Error> {
    let yes = decode_levels(&body.yes_dollars_fp)?;
    let no = decode_levels(&body.no_dollars_fp)?;
    Ok(Snapshot {
        seq,
        yes_bids: yes,
        no_bids: no,
    })
}

fn decode_levels(raw: &[[String; 2]]) -> Result<Vec<(Price, u32)>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    for [px, qty] in raw {
        // Drop levels at 0¢ or 100¢ — those are settlement values, not
        // tradable prices. Treating them as a hard error would make the
        // snapshot reject on edge levels.
        let Ok(price) = parse_price_dollars(px) else {
            continue;
        };
        let q = parse_qty_fp(qty)?;
        if q == 0 {
            continue;
        }
        out.push((price, q));
    }
    Ok(out)
}

/// Build a `predigy_book::Delta` from the wire body.
///
/// `seq` and `market_ticker` come from the envelope and body respectively;
/// `Side` is the bid book side the delta affects, identical to the wire
/// field (no complement applied — the book stores YES/NO bids verbatim).
pub fn delta_from_wire(body: &OrderbookDeltaBody, seq: u64) -> Result<Delta, Error> {
    Ok(Delta {
        market: body.market_ticker.clone(),
        seq,
        side: match body.side {
            Side::Yes => Side::Yes,
            Side::No => Side::No,
        },
        price: parse_price_dollars(&body.price_dollars)?,
        qty_delta: parse_delta_fp(&body.delta_fp)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_round_trips_typical_levels() {
        assert_eq!(parse_price_dollars("0.0800").unwrap().cents(), 8);
        assert_eq!(parse_price_dollars("0.5400").unwrap().cents(), 54);
        assert_eq!(parse_price_dollars("0.99").unwrap().cents(), 99);
        assert_eq!(parse_price_dollars("0.01").unwrap().cents(), 1);
    }

    #[test]
    fn price_rejects_settlement_and_garbage() {
        assert!(parse_price_dollars("0.00").is_err());
        assert!(parse_price_dollars("1.00").is_err());
        assert!(parse_price_dollars("not-a-number").is_err());
        assert!(parse_price_dollars("-0.50").is_err());
    }

    #[test]
    fn qty_parses_typical_values() {
        assert_eq!(parse_qty_fp("0").unwrap(), 0);
        assert_eq!(parse_qty_fp("300.00").unwrap(), 300);
        assert_eq!(parse_qty_fp("1").unwrap(), 1);
    }

    #[test]
    fn qty_rejects_negative() {
        assert!(parse_qty_fp("-1").is_err());
        assert!(parse_qty_fp("xyz").is_err());
    }

    #[test]
    fn delta_preserves_sign() {
        assert_eq!(parse_delta_fp("100.00").unwrap(), 100);
        assert_eq!(parse_delta_fp("-54.00").unwrap(), -54);
        assert_eq!(parse_delta_fp("0").unwrap(), 0);
    }

    #[test]
    fn snapshot_decodes_documented_example() {
        let body = OrderbookSnapshotBody {
            market_ticker: "FED-23DEC-T3.00".into(),
            market_id: None,
            yes_dollars_fp: vec![
                ["0.0800".into(), "300.00".into()],
                ["0.2200".into(), "333.00".into()],
            ],
            no_dollars_fp: vec![
                ["0.5400".into(), "20.00".into()],
                ["0.5600".into(), "146.00".into()],
            ],
        };
        let snap = snapshot_from_wire(&body, 42).unwrap();
        assert_eq!(snap.seq, 42);
        assert_eq!(snap.yes_bids.len(), 2);
        assert_eq!(snap.no_bids.len(), 2);
        assert_eq!(snap.yes_bids[0].0.cents(), 8);
        assert_eq!(snap.yes_bids[0].1, 300);
        assert_eq!(snap.no_bids[1].0.cents(), 56);
        assert_eq!(snap.no_bids[1].1, 146);
    }

    #[test]
    fn snapshot_drops_settlement_levels_and_zero_size() {
        // 0¢ and 100¢ aren't tradable; a 0-size level is functionally absent.
        let body = OrderbookSnapshotBody {
            market_ticker: "X".into(),
            market_id: None,
            yes_dollars_fp: vec![
                ["0.00".into(), "10.00".into()],
                ["0.50".into(), "0".into()],
                ["0.42".into(), "100.00".into()],
            ],
            no_dollars_fp: vec![],
        };
        let snap = snapshot_from_wire(&body, 1).unwrap();
        assert_eq!(snap.yes_bids.len(), 1);
        assert_eq!(snap.yes_bids[0].0.cents(), 42);
        assert_eq!(snap.yes_bids[0].1, 100);
    }

    #[test]
    fn delta_decodes_documented_example() {
        let body = OrderbookDeltaBody {
            market_ticker: "FED-23DEC-T3.00".into(),
            market_id: None,
            price_dollars: "0.96".into(),
            delta_fp: "-54.00".into(),
            side: Side::Yes,
            ts_ms: Some(1_669_149_841_000),
        };
        let d = delta_from_wire(&body, 3).unwrap();
        assert_eq!(d.market, "FED-23DEC-T3.00");
        assert_eq!(d.seq, 3);
        assert_eq!(d.side, Side::Yes);
        assert_eq!(d.price.cents(), 96);
        assert_eq!(d.qty_delta, -54);
    }
}
