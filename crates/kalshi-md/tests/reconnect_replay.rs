//! Verify the reconnect path: server drops the first connection, client
//! reconnects, and on reconnect the saved subscription is replayed with
//! the original `id`. Confirms idempotent state survives a transport drop.

use futures_util::{SinkExt as _, StreamExt as _};
use predigy_kalshi_md::{Backoff, Channel, Client, Connection, Event};
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

#[tokio::test]
async fn replays_saved_subscription_after_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Channel for the server task to publish each subscribe it sees.
    let (subs_tx, mut subs_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();

    let server = tokio::spawn(async move {
        // First connection: receive subscribe, ack with `subscribed`, then
        // close the socket to simulate a drop.
        let (sock1, _) = listener.accept().await.expect("accept 1");
        let mut ws1 = tokio_tungstenite::accept_async(sock1)
            .await
            .expect("upgrade 1");
        let first = ws1.next().await.expect("first frame").expect("no err");
        let Message::Text(t) = first else {
            panic!("expected text")
        };
        let parsed: serde_json::Value = serde_json::from_str(&t).unwrap();
        subs_tx.send(parsed.clone()).unwrap();
        let req_id = parsed["id"].as_u64().unwrap();
        let ack = serde_json::json!({
            "id": req_id,
            "type": "subscribed",
            "msg": { "channel": "orderbook_delta", "sid": 1 }
        });
        ws1.send(Message::Text(ack.to_string().into()))
            .await
            .unwrap();
        // Drop the connection — this exercises the reconnect path.
        drop(ws1);

        // Second connection: client reconnects and replays the saved sub.
        let (sock2, _) = listener.accept().await.expect("accept 2");
        let mut ws2 = tokio_tungstenite::accept_async(sock2)
            .await
            .expect("upgrade 2");
        let replay = ws2.next().await.expect("replay frame").expect("no err");
        let Message::Text(t) = replay else {
            panic!("expected text")
        };
        let parsed: serde_json::Value = serde_json::from_str(&t).unwrap();
        subs_tx.send(parsed.clone()).unwrap();
        let replay_req_id = parsed["id"].as_u64().unwrap();
        let ack2 = serde_json::json!({
            "id": replay_req_id,
            "type": "subscribed",
            "msg": { "channel": "orderbook_delta", "sid": 2 }
        });
        ws2.send(Message::Text(ack2.to_string().into()))
            .await
            .unwrap();

        // Idle until close.
        while let Some(msg) = ws2.next().await {
            if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    });

    // Use a tiny backoff so the test reconnects quickly.
    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url, None).with_backoff(Backoff {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(20),
        max_doublings: 1,
    });
    let mut conn = client.connect();

    let original_req_id = conn
        .subscribe(&[Channel::OrderbookDelta], &["X".into()])
        .await
        .unwrap();

    // 1st connection: Subscribed{ sid: 1 }
    match next_event_timeout(&mut conn).await {
        Event::Subscribed { req_id, sid, .. } => {
            assert_eq!(req_id, Some(original_req_id));
            assert_eq!(sid, 1);
        }
        other => panic!("expected first Subscribed, got {other:?}"),
    }

    // The server dropped — we should see Disconnected, then Reconnected,
    // then a fresh Subscribed{ sid: 2 } as the replay arrives. The exact
    // count of intermediate events is not load-bearing; loop until we see
    // a second Subscribed.
    let mut saw_disconnected = false;
    let mut saw_reconnected = false;
    let mut second_subscribed = None;
    for _ in 0..10 {
        match next_event_timeout(&mut conn).await {
            Event::Disconnected { .. } => saw_disconnected = true,
            Event::Reconnected => saw_reconnected = true,
            Event::Subscribed { req_id, sid, .. } => {
                second_subscribed = Some((req_id, sid));
                break;
            }
            _ => {}
        }
    }
    assert!(saw_disconnected, "expected a Disconnected event");
    assert!(saw_reconnected, "expected a Reconnected event");
    let (replay_req_id, replay_sid) = second_subscribed.expect("expected second Subscribed");
    assert_eq!(
        replay_req_id,
        Some(original_req_id),
        "replay should reuse the original client req_id so callers can correlate"
    );
    assert_eq!(replay_sid, 2);

    // The server must have seen two subscribes — the original and the
    // replay — both carrying the same `id` field.
    let first = subs_rx.try_recv().expect("server saw first sub");
    let second = subs_rx.try_recv().expect("server saw replay sub");
    assert_eq!(first["id"], second["id"]);
    assert_eq!(first["cmd"], "subscribe");
    assert_eq!(second["cmd"], "subscribe");

    conn.close().await;
    let _ = server.await;
}
