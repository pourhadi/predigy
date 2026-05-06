//! End-to-end test against an in-process Polymarket-shaped WS server.
//!
//! Validates: subscribe payload format on the wire, decoding of each
//! documented event type, and that batched (JSON-array) frames produce
//! one event per element. Polymarket sometimes batches events; we handle
//! both `{ ... }` and `[ { ... }, { ... } ]` framing.

use futures_util::{SinkExt as _, StreamExt as _};
use predigy_poly_md::{Client, Connection, Event};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

async fn next_event_timeout(conn: &mut Connection) -> Event {
    tokio::time::timeout(Duration::from_secs(5), conn.next_event())
        .await
        .expect("event arrived in time")
        .expect("stream not closed")
}

async fn run_mock_server(listener: TcpListener) {
    let (sock, _addr) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_async(sock)
        .await
        .expect("upgrade");

    // Expect the client's subscribe payload.
    let first = ws.next().await.expect("got something").expect("no err");
    let Message::Text(sub_text) = first else {
        panic!("expected text, got {first:?}");
    };
    let sub: serde_json::Value = serde_json::from_str(&sub_text).expect("subscribe is JSON");
    assert_eq!(sub["type"], "market");
    assert!(sub["assets_ids"].is_array(), "got: {sub_text}");

    // Reply with a book event, a price_change event (single-element array
    // form), a last_trade_price, and a tick_size_change.
    let book = serde_json::json!({
        "event_type":"book",
        "asset_id":"0xabc",
        "market":"0x123",
        "bids":[{"price":"0.42","size":"100"}],
        "asks":[{"price":"0.45","size":"75"}],
        "timestamp":"1700",
        "hash":"deadbeef"
    });
    // Polymarket sometimes ships a JSON array with multiple events in one
    // frame — exercise that path explicitly.
    let batch = serde_json::json!([
        {
            "event_type":"price_change",
            "market":"0x123",
            "price_changes":[{
                "asset_id":"0xabc","price":"0.43","size":"60","side":"buy",
                "best_bid":"0.43","best_ask":"0.45"
            }],
            "timestamp":"1701"
        },
        {
            "event_type":"last_trade_price",
            "asset_id":"0xabc","market":"0x123","fee_rate_bps":"50",
            "price":"0.43","side":"buy","size":"5","timestamp":"1702"
        }
    ]);
    let tick = serde_json::json!({
        "event_type":"tick_size_change",
        "asset_id":"0xabc","market":"0x123",
        "old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1703"
    });

    ws.send(Message::Text(book.to_string().into()))
        .await
        .unwrap();
    ws.send(Message::Text(batch.to_string().into()))
        .await
        .unwrap();
    ws.send(Message::Text(tick.to_string().into()))
        .await
        .unwrap();

    while let Some(msg) = ws.next().await {
        if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
            break;
        }
    }
}

#[tokio::test]
async fn end_to_end_book_pricechange_lasttrade_ticksize() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(run_mock_server(listener));

    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url);
    let mut conn = client.connect();
    conn.subscribe(&["0xabc".into()]).await.unwrap();

    // 1. Book
    match next_event_timeout(&mut conn).await {
        Event::Book(b) => {
            assert_eq!(b.asset_id, "0xabc");
            assert_eq!(b.bids[0].price, "0.42");
            assert_eq!(b.asks[0].price, "0.45");
        }
        other => panic!("expected Book, got {other:?}"),
    }

    // 2. PriceChange (from the batch)
    match next_event_timeout(&mut conn).await {
        Event::PriceChange(p) => {
            assert_eq!(p.price_changes.len(), 1);
            let pc = &p.price_changes[0];
            assert_eq!(pc.asset_id, "0xabc");
            assert_eq!(pc.best_bid.as_deref(), Some("0.43"));
        }
        other => panic!("expected PriceChange, got {other:?}"),
    }

    // 3. LastTradePrice (from the same batch)
    match next_event_timeout(&mut conn).await {
        Event::LastTradePrice(t) => {
            assert_eq!(t.asset_id, "0xabc");
            assert_eq!(t.price, "0.43");
            assert_eq!(t.fee_rate_bps.as_deref(), Some("50"));
        }
        other => panic!("expected LastTradePrice, got {other:?}"),
    }

    // 4. TickSizeChange
    match next_event_timeout(&mut conn).await {
        Event::TickSizeChange(t) => {
            assert_eq!(t.old_tick_size, "0.01");
            assert_eq!(t.new_tick_size, "0.001");
        }
        other => panic!("expected TickSizeChange, got {other:?}"),
    }

    conn.close().await;
    let _ = server.await;
}

/// Mock server for the keepalive test: accepts the subscribe, then
/// records every subsequent text frame.  Returns a `Vec<String>` of
/// raw frame contents seen after the subscribe.
async fn run_ping_recorder(listener: TcpListener, frame_count: usize) -> Vec<String> {
    let (sock, _addr) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_async(sock)
        .await
        .expect("upgrade");
    // First frame is the subscribe; consume + ignore.
    let _ = ws.next().await;

    let mut frames = Vec::new();
    while frames.len() < frame_count {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => frames.push(t.to_string()),
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            Some(_) => {}
        }
    }
    frames
}

#[tokio::test]
async fn text_ping_keepalive_sends_ping_on_interval() {
    // Regression for the Polymarket WS reconnect-every-2-min issue:
    // when text_ping_interval is set, the client should push a
    // `"PING"` text frame on the configured cadence.  This test
    // doesn't try to validate Polymarket's response — only that
    // OUR side of the wire emits the right frame at the right rate.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Wait for two PING frames so we exercise the ticker, not just
    // the first-tick edge case.
    let server = tokio::spawn(run_ping_recorder(listener, 2));

    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url).with_text_ping_interval(Duration::from_millis(150));
    let mut conn = client.connect();
    conn.subscribe(&["0xabc".into()]).await.unwrap();

    let frames = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("recorder finished in time")
        .expect("recorder task did not panic");
    assert!(
        frames.len() >= 2,
        "expected at least 2 PING frames, got {}",
        frames.len()
    );
    for f in &frames {
        assert_eq!(f, "PING", "every recorded frame should be PING, got {f:?}");
    }
    conn.close().await;
}

#[tokio::test]
async fn pong_response_is_silently_dropped() {
    // Mock server immediately replies "PONG" and sends a real Book
    // event after.  The test verifies that:
    //   1. The client does not surface "PONG" as a Malformed event.
    //   2. The Book that follows is decoded normally.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
        // Consume the subscribe.
        let _ = ws.next().await;
        ws.send(Message::Text("PONG".into())).await.unwrap();
        let book = serde_json::json!({
            "event_type":"book",
            "asset_id":"0xabc",
            "market":"0x123",
            "bids":[{"price":"0.42","size":"100"}],
            "asks":[{"price":"0.45","size":"75"}],
            "timestamp":"1700",
            "hash":"deadbeef"
        });
        ws.send(Message::Text(book.to_string().into()))
            .await
            .unwrap();
        // Drain until close.
        while let Some(msg) = ws.next().await {
            if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    });

    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url);
    let mut conn = client.connect();
    conn.subscribe(&["0xabc".into()]).await.unwrap();

    match next_event_timeout(&mut conn).await {
        Event::Book(b) => assert_eq!(b.asset_id, "0xabc"),
        Event::Malformed { raw, .. } => {
            panic!("PONG should be silently dropped, not surfaced as Malformed (raw={raw:?})");
        }
        other => panic!("expected Book after PONG, got {other:?}"),
    }
    conn.close().await;
    let _ = server.await;
}
