//! End-to-end test against a local in-process WebSocket server.
//!
//! Spins up `tokio_tungstenite::accept_async` on `127.0.0.1:0`, has the
//! server reply to a `subscribe` command with a fixture sequence
//! (`subscribed` → `orderbook_snapshot` → `orderbook_delta`), and asserts
//! the client surfaces the corresponding decoded events. Validates the
//! whole stack: command serialisation, frame sending, frame receiving,
//! envelope parsing, and the wire→domain conversion in `decode`.

use futures_util::{SinkExt as _, StreamExt as _};
use predigy_book::ApplyOutcome;
use predigy_book::OrderBook;
use predigy_core::side::Side;
use predigy_kalshi_md::{Channel, Client, Connection, Event};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

/// Each recv should arrive in milliseconds; anything longer means a
/// deadlock or routing bug.
async fn next_event_timeout(conn: &mut Connection) -> Event {
    tokio::time::timeout(Duration::from_secs(5), conn.next_event())
        .await
        .expect("event arrived in time")
        .expect("stream not closed")
}

/// Drive the test server: accept one connection, expect one subscribe,
/// emit the fixture sequence, then idle until the client closes.
async fn run_mock_server(listener: TcpListener) {
    let (sock, _addr) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_async(sock)
        .await
        .expect("upgrade");

    // Wait for the client's subscribe command.
    let first = ws
        .next()
        .await
        .expect("client sent something")
        .expect("ok msg");
    let Message::Text(sub_text) = first else {
        panic!("expected text frame, got {first:?}");
    };
    let sub: serde_json::Value = serde_json::from_str(&sub_text).expect("subscribe is JSON");
    assert_eq!(sub["cmd"], "subscribe");
    let req_id = sub["id"].as_u64().expect("id is number");

    // Reply: subscribed, snapshot, delta, ticker, trade.
    let subscribed = serde_json::json!({
        "id": req_id,
        "type": "subscribed",
        "msg": { "channel": "orderbook_delta", "sid": 7 }
    });
    let snapshot = serde_json::json!({
        "type": "orderbook_snapshot",
        "sid": 7,
        "seq": 100,
        "msg": {
            "market_ticker": "TEST-MKT",
            "market_id": "uuid",
            "yes_dollars_fp": [["0.40", "100.00"], ["0.45", "50.00"]],
            "no_dollars_fp":  [["0.50", "75.00"],  ["0.55", "25.00"]]
        }
    });
    let delta = serde_json::json!({
        "type": "orderbook_delta",
        "sid": 7,
        "seq": 101,
        "msg": {
            "market_ticker": "TEST-MKT",
            "price_dollars": "0.41",
            "delta_fp": "25.00",
            "side": "yes",
            "ts_ms": 1
        }
    });
    let ticker = serde_json::json!({
        "type": "ticker",
        "sid": 7,
        "msg": { "market_ticker": "TEST-MKT", "price_dollars": "0.42" }
    });
    let trade = serde_json::json!({
        "type": "trade",
        "sid": 7,
        "msg": {
            "trade_id": "t-1",
            "market_ticker": "TEST-MKT",
            "yes_price_dollars": "0.42",
            "no_price_dollars": "0.58",
            "count_fp": "10.00",
            "taker_side": "yes",
            "ts_ms": 1
        }
    });

    for v in [subscribed, snapshot, delta, ticker, trade] {
        ws.send(Message::Text(v.to_string().into()))
            .await
            .expect("send");
    }

    // Drain anything else (close, etc.) until the client disconnects.
    while let Some(msg) = ws.next().await {
        if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
            break;
        }
    }
}

#[tokio::test]
async fn end_to_end_subscribe_snapshot_delta_ticker_trade() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(run_mock_server(listener));

    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url, None);
    let mut conn = client.connect();

    let req_id = conn
        .subscribe(&[Channel::OrderbookDelta], &["TEST-MKT".into()])
        .await
        .expect("subscribe queued");

    // 1. Subscribed
    match next_event_timeout(&mut conn).await {
        Event::Subscribed {
            req_id: id,
            channel,
            sid,
        } => {
            assert_eq!(id, Some(req_id));
            assert_eq!(channel, "orderbook_delta");
            assert_eq!(sid, 7);
        }
        other => panic!("expected Subscribed, got {other:?}"),
    }

    // 2. Snapshot — and feed it into a real OrderBook to confirm the
    //    domain conversion is consistent end-to-end.
    let mut book = OrderBook::new("TEST-MKT");
    match next_event_timeout(&mut conn).await {
        Event::Snapshot {
            sid,
            market,
            snapshot,
        } => {
            assert_eq!(sid, 7);
            assert_eq!(market, "TEST-MKT");
            assert_eq!(snapshot.seq, 100);
            book.apply_snapshot(snapshot);
            assert_eq!(book.best_yes_bid().unwrap().0.cents(), 45);
            assert_eq!(book.best_no_bid().unwrap().0.cents(), 55);
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }

    // 3. Delta — applies cleanly to the book (seq 101 follows 100).
    match next_event_timeout(&mut conn).await {
        Event::Delta { sid, delta } => {
            assert_eq!(sid, 7);
            assert_eq!(delta.market, "TEST-MKT");
            assert_eq!(delta.seq, 101);
            assert_eq!(delta.side, Side::Yes);
            assert_eq!(delta.price.cents(), 41);
            assert_eq!(delta.qty_delta, 25);
            assert_eq!(book.apply_delta(&delta), ApplyOutcome::Ok);
        }
        other => panic!("expected Delta, got {other:?}"),
    }

    // 4. Ticker
    match next_event_timeout(&mut conn).await {
        Event::Ticker { sid, body } => {
            assert_eq!(sid, 7);
            assert_eq!(body.market_ticker, "TEST-MKT");
            assert_eq!(body.price_dollars.as_deref(), Some("0.42"));
        }
        other => panic!("expected Ticker, got {other:?}"),
    }

    // 5. Trade
    match next_event_timeout(&mut conn).await {
        Event::Trade { sid, body } => {
            assert_eq!(sid, 7);
            assert_eq!(body.trade_id, "t-1");
            assert_eq!(body.taker_side, Side::Yes);
        }
        other => panic!("expected Trade, got {other:?}"),
    }

    conn.close().await;
    let _ = server.await;
}
