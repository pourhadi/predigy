//! End-to-end FIX session against a hand-rolled loopback server.
//!
//! The mock server on `127.0.0.1:0` accepts the Logon, replies with
//! a Logon ack, then watches for `NewOrderSingle` and replies with a
//! pair of `ExecutionReport`s (Acked + Filled). Asserts that the
//! `FixExecutor` plumbs both through to its `report_rx`.

use predigy_core::market::MarketTicker;
use predigy_core::order::{Order, OrderId, OrderType, TimeInForce};
use predigy_core::price::{Price, Qty};
use predigy_core::side::{Action, Side};
use predigy_kalshi_fix::{
    ExecKind, FixConfig, FixExecutor, body_with_msg_type, decode_message, encode,
    parse_execution_report,
};
use predigy_oms::{ExecutionReportKind, Executor};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TAG_MSG_TYPE: u32 = 35;
const TAG_SENDER: u32 = 49;
const TAG_TARGET: u32 = 56;
const TAG_SEQ: u32 = 34;
const TAG_TIME: u32 = 52;
const TAG_CL_ORD_ID: u32 = 11;
const TAG_ORDER_ID: u32 = 37;
const TAG_EXEC_ID: u32 = 17;
const TAG_EXEC_TYPE: u32 = 150;
const TAG_ORD_STATUS: u32 = 39;
const TAG_CUM_QTY: u32 = 14;
const TAG_LAST_QTY: u32 = 32;
const TAG_LAST_PX: u32 = 31;

fn p(c: u8) -> Price {
    Price::from_cents(c).unwrap()
}
fn q(n: u32) -> Qty {
    Qty::new(n).unwrap()
}

fn buy_yes(qty: u32, price: u8) -> Order {
    Order {
        client_id: OrderId::new("arb:X:00000001"),
        market: MarketTicker::new("X"),
        side: Side::Yes,
        action: Action::Buy,
        price: p(price),
        qty: q(qty),
        order_type: OrderType::Limit,
        tif: TimeInForce::Ioc,
    }
}

#[tokio::test]
async fn fix_session_logon_submit_fill() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut inbound = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];

        // Wait for Logon (35=A).
        let logon = read_one_message(&mut sock, &mut inbound, &mut buf).await;
        assert_eq!(logon.get(TAG_MSG_TYPE).unwrap(), "A");
        let cli_sender = logon.get(TAG_SENDER).unwrap().to_string();
        let cli_target = logon.get(TAG_TARGET).unwrap().to_string();

        // Send Logon ack.
        let body = body_with_msg_type(
            "A",
            &mut vec![
                (TAG_SENDER, cli_target.clone()),
                (TAG_TARGET, cli_sender.clone()),
                (TAG_SEQ, "1".into()),
                (TAG_TIME, "20260504-12:00:00.000".into()),
                (98, "0".into()),
                (108, "30".into()),
            ],
        );
        sock.write_all(&encode(&body)).await.unwrap();

        // Wait for NewOrderSingle (35=D).
        let nos = read_one_message(&mut sock, &mut inbound, &mut buf).await;
        assert_eq!(nos.get(TAG_MSG_TYPE).unwrap(), "D");
        let cl_ord_id = nos.get(TAG_CL_ORD_ID).unwrap().to_string();

        // Send Acked ExecutionReport.
        let body = body_with_msg_type(
            "8",
            &mut vec![
                (TAG_SENDER, cli_target.clone()),
                (TAG_TARGET, cli_sender.clone()),
                (TAG_SEQ, "2".into()),
                (TAG_TIME, "20260504-12:00:00.001".into()),
                (TAG_CL_ORD_ID, cl_ord_id.clone()),
                (TAG_ORDER_ID, "V-1".into()),
                (TAG_EXEC_ID, "E-1".into()),
                (TAG_EXEC_TYPE, "0".into()),
                (TAG_ORD_STATUS, "0".into()),
            ],
        );
        sock.write_all(&encode(&body)).await.unwrap();

        // Send Filled ExecutionReport.
        let body = body_with_msg_type(
            "8",
            &mut vec![
                (TAG_SENDER, cli_target),
                (TAG_TARGET, cli_sender),
                (TAG_SEQ, "3".into()),
                (TAG_TIME, "20260504-12:00:00.002".into()),
                (TAG_CL_ORD_ID, cl_ord_id),
                (TAG_ORDER_ID, "V-1".into()),
                (TAG_EXEC_ID, "E-2".into()),
                (TAG_EXEC_TYPE, "F".into()),
                (TAG_ORD_STATUS, "2".into()),
                (TAG_CUM_QTY, "100".into()),
                (TAG_LAST_QTY, "100".into()),
                (TAG_LAST_PX, "0.4100".into()),
            ],
        );
        sock.write_all(&encode(&body)).await.unwrap();
    });

    let (executor, mut reports) = FixExecutor::spawn(FixConfig {
        addr: addr.to_string(),
        sender_comp_id: "CLIENT".into(),
        target_comp_id: "KALSHI".into(),
        heartbeat_secs: 60,
        auth_tags: vec![],
        reset_seq_num: true,
    })
    .await
    .expect("connect+logon");

    executor.submit(&buy_yes(100, 42)).await.expect("submit ok");

    // Acked
    let r1 = tokio::time::timeout(Duration::from_secs(5), reports.recv())
        .await
        .expect("acked in time")
        .expect("stream open");
    match r1.kind {
        ExecutionReportKind::Acked { venue_order_id } => assert_eq!(venue_order_id, "V-1"),
        other => panic!("expected Acked, got {other:?}"),
    }
    // Filled
    let r2 = tokio::time::timeout(Duration::from_secs(5), reports.recv())
        .await
        .expect("filled in time")
        .expect("stream open");
    match r2.kind {
        ExecutionReportKind::Filled {
            cumulative_qty,
            fill,
        } => {
            assert_eq!(cumulative_qty, 100);
            assert_eq!(fill.price.cents(), 41);
            assert_eq!(fill.qty.get(), 100);
        }
        other => panic!("expected Filled, got {other:?}"),
    }

    let _ = server.await;
}

async fn read_one_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    inbound: &mut Vec<u8>,
    buf: &mut [u8],
) -> predigy_kalshi_fix::FieldList {
    loop {
        let n = reader.read(buf).await.unwrap();
        assert!(n > 0, "peer closed before message arrived");
        inbound.extend_from_slice(&buf[..n]);
        if let Some((fields, consumed)) = decode_message(inbound).unwrap() {
            inbound.drain(..consumed);
            return fields;
        }
    }
}

/// Sanity: `parse_execution_report` and the `ExecKind` re-export are
/// reachable from the integration test (catches future API breakage).
#[test]
fn re_exports_compile() {
    let mut f = predigy_kalshi_fix::FieldList::new();
    f.push(TAG_CL_ORD_ID, "x");
    f.push(TAG_ORDER_ID, "y");
    f.push(TAG_ORD_STATUS, "0");
    let parsed = parse_execution_report(&f).unwrap();
    assert!(matches!(parsed.kind, ExecKind::New));
}
