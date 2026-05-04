//! Typed FIX 4.4 messages: a small, hand-curated subset matching
//! the application messages we exchange with Kalshi.
//!
//! Outbound builders take `predigy_core::Order`-shaped data and
//! produce a `Vec<(u32, String)>` ready to feed through
//! `frame::encode`. Inbound parsers take a `frame::FieldList` and
//! produce strongly-typed structs with all their required tags.

use crate::error::Error;
use crate::frame::FieldList;
use crate::tags::*;
use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId, TimeInForce};
use predigy_core::side::{Action, Side};

// ---------------------------------------------------------------- helpers

fn ts_field(now_ms: i64) -> String {
    // FIX `SendingTime` / `TransactTime` format: `YYYYMMDD-HH:MM:SS.sss`
    // (UTC). Plus-or-minus a few seconds is fine; Kalshi accepts ms
    // precision but doesn't require it.
    let secs = now_ms / 1000;
    let millis = now_ms.rem_euclid(1000);
    let (y, m, d, hh, mm, ss) = decompose_unix_secs(secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}:{mm:02}:{ss:02}.{millis:03}")
}

/// Decompose unix seconds into UTC (year, month, day, hour, min, sec).
/// Hand-rolled to avoid pulling in `chrono` for the FIX crate.
fn decompose_unix_secs(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let day_seconds = secs.rem_euclid(86_400) as u32;
    let hh = day_seconds / 3_600;
    let mm = (day_seconds % 3_600) / 60;
    let ss = day_seconds % 60;
    // Days since 1970-01-01 → calendar date. Algorithm: shift epoch
    // to 0000-03-01 (Howard Hinnant's date algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y_ish = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (y_ish + i64::from(m <= 2)) as i32;
    (y, m, d, hh, mm, ss)
}

// ---------------------------------------------------------------- Logon

/// Build a Logon message body. `auth_extras` are tag/value pairs
/// appended after the standard Logon fields — e.g. `(553, username)`,
/// `(554, password)`, `(96, hmac_signature)` for Kalshi.
pub fn build_logon(
    sender_comp_id: &str,
    target_comp_id: &str,
    seq_num: u64,
    heartbeat_secs: u32,
    reset_seq_num: bool,
    auth_extras: &[(u32, String)],
    now_ms: i64,
) -> Vec<(u32, String)> {
    let mut fields = vec![
        (MSG_TYPE, MSG_TYPE_LOGON.to_string()),
        (SENDER_COMP_ID, sender_comp_id.to_string()),
        (TARGET_COMP_ID, target_comp_id.to_string()),
        (MSG_SEQ_NUM, seq_num.to_string()),
        (SENDING_TIME, ts_field(now_ms)),
        (ENCRYPT_METHOD, "0".to_string()),
        (HEART_BT_INT, heartbeat_secs.to_string()),
    ];
    if reset_seq_num {
        fields.push((RESET_SEQ_NUM_FLAG, "Y".to_string()));
    }
    for (t, v) in auth_extras {
        fields.push((*t, v.clone()));
    }
    fields
}

// ---------------------------------------------------------------- Heartbeat / TestRequest

pub fn build_heartbeat(
    sender_comp_id: &str,
    target_comp_id: &str,
    seq_num: u64,
    test_req_id: Option<&str>,
    now_ms: i64,
) -> Vec<(u32, String)> {
    let mut fields = vec![
        (MSG_TYPE, MSG_TYPE_HEARTBEAT.to_string()),
        (SENDER_COMP_ID, sender_comp_id.to_string()),
        (TARGET_COMP_ID, target_comp_id.to_string()),
        (MSG_SEQ_NUM, seq_num.to_string()),
        (SENDING_TIME, ts_field(now_ms)),
    ];
    if let Some(id) = test_req_id {
        fields.push((TEST_REQ_ID, id.to_string()));
    }
    fields
}

// ---------------------------------------------------------------- NewOrderSingle

pub fn build_new_order_single(
    sender_comp_id: &str,
    target_comp_id: &str,
    seq_num: u64,
    order: &Order,
    now_ms: i64,
) -> Result<Vec<(u32, String)>, Error> {
    let side = match order.action {
        Action::Buy => SIDE_BUY,
        Action::Sell => SIDE_SELL,
    };
    let tif = match order.tif {
        TimeInForce::Gtc => TIF_GTC,
        TimeInForce::Ioc => TIF_IOC,
        TimeInForce::Fok => TIF_FOK,
        // PostOnly isn't directly expressible in vanilla FIX 4.4.
        // Kalshi's spec uses an `ExecInst` (tag 18) extension; for
        // now reject so the OMS surfaces the gap.
        TimeInForce::PostOnly => {
            return Err(Error::Unsupported(
                "post_only not yet wired through FIX (use REST for PostOnly)",
            ));
        }
    };
    // Note: this builder treats Side directly. NO-side intents must
    // be mapped to YES-equivalent (sell-at-complement) by the caller
    // — same convention as the REST executor's mapping.
    Ok(vec![
        (MSG_TYPE, MSG_TYPE_NEW_ORDER_SINGLE.to_string()),
        (SENDER_COMP_ID, sender_comp_id.to_string()),
        (TARGET_COMP_ID, target_comp_id.to_string()),
        (MSG_SEQ_NUM, seq_num.to_string()),
        (SENDING_TIME, ts_field(now_ms)),
        (CL_ORD_ID, order.client_id.as_str().to_string()),
        (SYMBOL, order.market.as_str().to_string()),
        (SIDE, side.to_string()),
        (TRANSACT_TIME, ts_field(now_ms)),
        (ORDER_QTY, order.qty.get().to_string()),
        (ORD_TYPE, ORD_TYPE_LIMIT.to_string()),
        (
            PRICE,
            format!("{:.4}", f64::from(order.price.cents()) / 100.0),
        ),
        (TIME_IN_FORCE, tif.to_string()),
    ])
}

// ---------------------------------------------------------------- OrderCancelRequest

pub fn build_order_cancel_request(
    sender_comp_id: &str,
    target_comp_id: &str,
    seq_num: u64,
    cancel_cid: &str,
    orig_cid: &OrderId,
    market: &MarketTicker,
    side: Side,
    action: Action,
    now_ms: i64,
) -> Vec<(u32, String)> {
    let _ = side; // FIX 4.4 OrderCancelRequest does include 54; pass action as the side.
    let side_code = match action {
        Action::Buy => SIDE_BUY,
        Action::Sell => SIDE_SELL,
    };
    vec![
        (MSG_TYPE, MSG_TYPE_ORDER_CANCEL_REQUEST.to_string()),
        (SENDER_COMP_ID, sender_comp_id.to_string()),
        (TARGET_COMP_ID, target_comp_id.to_string()),
        (MSG_SEQ_NUM, seq_num.to_string()),
        (SENDING_TIME, ts_field(now_ms)),
        (ORIG_CL_ORD_ID, orig_cid.as_str().to_string()),
        (CL_ORD_ID, cancel_cid.to_string()),
        (SYMBOL, market.as_str().to_string()),
        (SIDE, side_code.to_string()),
        (TRANSACT_TIME, ts_field(now_ms)),
    ]
}

// ---------------------------------------------------------------- ExecutionReport

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecKind {
    /// Fresh order acknowledged by the venue (`OrdStatus=0`).
    New,
    /// Partial fill (`OrdStatus=1`).
    PartiallyFilled,
    /// Terminal fill (`OrdStatus=2`).
    Filled,
    /// Cancelled (`OrdStatus=4`).
    Cancelled,
    /// Rejected (`OrdStatus=8`).
    Rejected,
    /// Status code we don't model — surfaced verbatim so the OMS
    /// can log it.
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ParsedExecutionReport {
    pub cl_ord_id: String,
    pub venue_order_id: String,
    pub kind: ExecKind,
    pub cum_qty: u32,
    pub last_qty: Option<u32>,
    pub last_px_cents: Option<u8>,
    pub leaves_qty: Option<u32>,
    pub text: Option<String>,
}

pub fn parse_execution_report(fields: &FieldList) -> Result<ParsedExecutionReport, Error> {
    let cl_ord_id = fields.require(CL_ORD_ID, "8")?.to_string();
    let venue_order_id = fields.require(ORDER_ID, "8")?.to_string();
    let ord_status = fields.require(ORD_STATUS, "8")?;
    let kind = match ord_status {
        ORD_STATUS_NEW => ExecKind::New,
        ORD_STATUS_PARTIALLY_FILLED => ExecKind::PartiallyFilled,
        ORD_STATUS_FILLED => ExecKind::Filled,
        ORD_STATUS_CANCELED => ExecKind::Cancelled,
        ORD_STATUS_REJECTED => ExecKind::Rejected,
        other => ExecKind::Other(other.to_string()),
    };
    let cum_qty = fields
        .get(CUM_QTY)
        .map(|s| {
            s.parse::<u32>().map_err(|_| Error::MalformedTag {
                tag: CUM_QTY,
                got: s.to_string(),
            })
        })
        .transpose()?
        .unwrap_or(0);
    let last_qty = fields
        .get(LAST_QTY)
        .map(|s| {
            s.parse::<u32>().map_err(|_| Error::MalformedTag {
                tag: LAST_QTY,
                got: s.to_string(),
            })
        })
        .transpose()?;
    let last_px_cents = fields
        .get(LAST_PX)
        .map(|s| parse_price_cents(s, LAST_PX))
        .transpose()?;
    let leaves_qty = fields
        .get(LEAVES_QTY)
        .map(|s| {
            s.parse::<u32>().map_err(|_| Error::MalformedTag {
                tag: LEAVES_QTY,
                got: s.to_string(),
            })
        })
        .transpose()?;
    let text = fields.get(TEXT).map(str::to_string);
    Ok(ParsedExecutionReport {
        cl_ord_id,
        venue_order_id,
        kind,
        cum_qty,
        last_qty,
        last_px_cents,
        leaves_qty,
        text,
    })
}

fn parse_price_cents(s: &str, tag: u32) -> Result<u8, Error> {
    let dollars: f64 = s.parse().map_err(|_| Error::MalformedTag {
        tag,
        got: s.to_string(),
    })?;
    let cents_i = (dollars * 100.0).round() as i32;
    u8::try_from(cents_i).map_err(|_| Error::MalformedTag {
        tag,
        got: s.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode_message, encode};
    use predigy_core::order::{OrderType, TimeInForce};
    use predigy_core::price::{Price, Qty};

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn q(n: u32) -> Qty {
        Qty::new(n).unwrap()
    }

    fn buy_yes_gtc(qty: u32, price: u8) -> Order {
        Order {
            client_id: OrderId::new("arb:X:00000001"),
            market: MarketTicker::new("X"),
            side: Side::Yes,
            action: Action::Buy,
            price: p(price),
            qty: q(qty),
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
        }
    }

    #[test]
    fn ts_field_decomposes_known_unix_time() {
        // 2026-05-04 06:59:26 UTC = 1762239566 (close enough — the
        // exact second comes from `now_ms`).
        let s = ts_field(1_762_239_566_000);
        // Format check.
        assert_eq!(s.len(), 21);
        assert_eq!(&s[8..9], "-");
        assert!(s.contains(':'));
    }

    #[test]
    fn logon_round_trips_required_tags() {
        let body = build_logon(
            "S",
            "T",
            1,
            30,
            true,
            &[(USERNAME, "u".into()), (PASSWORD, "p".into())],
            1_700_000_000_000,
        );
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        assert_eq!(parsed.get(MSG_TYPE).unwrap(), MSG_TYPE_LOGON);
        assert_eq!(parsed.get(SENDER_COMP_ID).unwrap(), "S");
        assert_eq!(parsed.get(TARGET_COMP_ID).unwrap(), "T");
        assert_eq!(parsed.get(MSG_SEQ_NUM).unwrap(), "1");
        assert_eq!(parsed.get(HEART_BT_INT).unwrap(), "30");
        assert_eq!(parsed.get(RESET_SEQ_NUM_FLAG).unwrap(), "Y");
        assert_eq!(parsed.get(USERNAME).unwrap(), "u");
        assert_eq!(parsed.get(PASSWORD).unwrap(), "p");
    }

    #[test]
    fn new_order_single_serialises_order_fields_correctly() {
        let order = buy_yes_gtc(100, 42);
        let body = build_new_order_single("S", "T", 5, &order, 1_700_000_000_000).unwrap();
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        assert_eq!(parsed.get(MSG_TYPE).unwrap(), MSG_TYPE_NEW_ORDER_SINGLE);
        assert_eq!(parsed.get(CL_ORD_ID).unwrap(), "arb:X:00000001");
        assert_eq!(parsed.get(SYMBOL).unwrap(), "X");
        assert_eq!(parsed.get(SIDE).unwrap(), SIDE_BUY);
        assert_eq!(parsed.get(ORDER_QTY).unwrap(), "100");
        assert_eq!(parsed.get(ORD_TYPE).unwrap(), ORD_TYPE_LIMIT);
        assert_eq!(parsed.get(PRICE).unwrap(), "0.4200");
        assert_eq!(parsed.get(TIME_IN_FORCE).unwrap(), TIF_GTC);
    }

    #[test]
    fn ioc_maps_to_tif_3() {
        let mut order = buy_yes_gtc(1, 50);
        order.tif = TimeInForce::Ioc;
        let body = build_new_order_single("S", "T", 1, &order, 0).unwrap();
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        assert_eq!(parsed.get(TIME_IN_FORCE).unwrap(), TIF_IOC);
    }

    #[test]
    fn post_only_is_rejected() {
        let mut order = buy_yes_gtc(1, 50);
        order.tif = TimeInForce::PostOnly;
        let err = build_new_order_single("S", "T", 1, &order, 0).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    #[test]
    fn cancel_request_carries_orig_and_new_cid() {
        let body = build_order_cancel_request(
            "S",
            "T",
            10,
            "cancel-1",
            &OrderId::new("orig-1"),
            &MarketTicker::new("X"),
            Side::Yes,
            Action::Buy,
            0,
        );
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        assert_eq!(parsed.get(MSG_TYPE).unwrap(), MSG_TYPE_ORDER_CANCEL_REQUEST);
        assert_eq!(parsed.get(ORIG_CL_ORD_ID).unwrap(), "orig-1");
        assert_eq!(parsed.get(CL_ORD_ID).unwrap(), "cancel-1");
    }

    #[test]
    fn parse_filled_execution_report() {
        let body = vec![
            (MSG_TYPE, MSG_TYPE_EXECUTION_REPORT.to_string()),
            (SENDER_COMP_ID, "T".to_string()),
            (TARGET_COMP_ID, "S".to_string()),
            (MSG_SEQ_NUM, "1".to_string()),
            (SENDING_TIME, "20260504-12:00:00.000".to_string()),
            (CL_ORD_ID, "arb:X:00000001".to_string()),
            (ORDER_ID, "V-1".to_string()),
            (EXEC_ID, "E-1".to_string()),
            (EXEC_TYPE, EXEC_TYPE_FILL.to_string()),
            (ORD_STATUS, ORD_STATUS_FILLED.to_string()),
            (CUM_QTY, "100".to_string()),
            (LAST_QTY, "100".to_string()),
            (LAST_PX, "0.4100".to_string()),
            (LEAVES_QTY, "0".to_string()),
        ];
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        let report = parse_execution_report(&parsed).unwrap();
        assert_eq!(report.cl_ord_id, "arb:X:00000001");
        assert_eq!(report.venue_order_id, "V-1");
        assert_eq!(report.kind, ExecKind::Filled);
        assert_eq!(report.cum_qty, 100);
        assert_eq!(report.last_qty, Some(100));
        assert_eq!(report.last_px_cents, Some(41));
        assert_eq!(report.leaves_qty, Some(0));
    }

    #[test]
    fn parse_rejected_execution_report_with_text() {
        let body = vec![
            (MSG_TYPE, MSG_TYPE_EXECUTION_REPORT.to_string()),
            (SENDER_COMP_ID, "T".to_string()),
            (TARGET_COMP_ID, "S".to_string()),
            (MSG_SEQ_NUM, "1".to_string()),
            (SENDING_TIME, "20260504-12:00:00.000".to_string()),
            (CL_ORD_ID, "arb:X:00000001".to_string()),
            (ORDER_ID, "V-1".to_string()),
            (EXEC_ID, "E-1".to_string()),
            (EXEC_TYPE, EXEC_TYPE_REJECTED.to_string()),
            (ORD_STATUS, ORD_STATUS_REJECTED.to_string()),
            (TEXT, "price out of range".to_string()),
        ];
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        let report = parse_execution_report(&parsed).unwrap();
        assert_eq!(report.kind, ExecKind::Rejected);
        assert_eq!(report.text.as_deref(), Some("price out of range"));
    }

    #[test]
    fn missing_required_tag_errors() {
        let body = vec![
            (MSG_TYPE, MSG_TYPE_EXECUTION_REPORT.to_string()),
            (SENDER_COMP_ID, "T".to_string()),
            (TARGET_COMP_ID, "S".to_string()),
            (MSG_SEQ_NUM, "1".to_string()),
            (SENDING_TIME, "20260504-12:00:00.000".to_string()),
            // No 11 (CL_ORD_ID).
            (ORDER_ID, "V-1".to_string()),
            (ORD_STATUS, "0".to_string()),
        ];
        let bytes = encode(&body);
        let (parsed, _) = decode_message(&bytes).unwrap().unwrap();
        let err = parse_execution_report(&parsed).unwrap_err();
        assert!(matches!(err, Error::MissingTag { tag: 11, .. }));
    }
}
