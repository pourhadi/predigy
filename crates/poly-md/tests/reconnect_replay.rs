//! Reconnect path: server drops the first connection; client must
//! reconnect and re-subscribe with the consolidated saved asset list.

use futures_util::{SinkExt as _, StreamExt as _};
use predigy_poly_md::{Backoff, Client, Connection, Event};
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
async fn replays_saved_subscription_after_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (subs_tx, mut subs_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();

    let server = tokio::spawn(async move {
        // First connection: take subscribe, send a book, then drop.
        let (sock1, _) = listener.accept().await.unwrap();
        let mut ws1 = tokio_tungstenite::accept_async(sock1).await.unwrap();
        let first = ws1.next().await.unwrap().unwrap();
        let Message::Text(t) = first else {
            panic!("expected text")
        };
        let sub1: serde_json::Value = serde_json::from_str(&t).unwrap();
        subs_tx.send(sub1).unwrap();
        let book = serde_json::json!({
            "event_type":"book","asset_id":"0xabc","market":"0x123",
            "bids":[],"asks":[],"timestamp":"1"
        });
        ws1.send(Message::Text(book.to_string().into()))
            .await
            .unwrap();
        drop(ws1);

        // Second connection: must receive a subscribe carrying both saved
        // asset_ids (the original "0xabc" plus "0xdef" added during the
        // backoff window).
        let (sock2, _) = listener.accept().await.unwrap();
        let mut ws2 = tokio_tungstenite::accept_async(sock2).await.unwrap();
        let replay = ws2.next().await.unwrap().unwrap();
        let Message::Text(t) = replay else {
            panic!("expected text")
        };
        let sub2: serde_json::Value = serde_json::from_str(&t).unwrap();
        subs_tx.send(sub2).unwrap();
        let book2 = serde_json::json!({
            "event_type":"book","asset_id":"0xabc","market":"0x123",
            "bids":[],"asks":[],"timestamp":"2"
        });
        ws2.send(Message::Text(book2.to_string().into()))
            .await
            .unwrap();
        while let Some(msg) = ws2.next().await {
            if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    });

    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url).with_backoff(Backoff {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(20),
        max_doublings: 1,
    });
    let mut conn = client.connect();
    conn.subscribe(&["0xabc".into()]).await.unwrap();

    // First book event over the original connection.
    match next_event_timeout(&mut conn).await {
        Event::Book(b) => assert_eq!(b.asset_id, "0xabc"),
        other => panic!("expected first Book, got {other:?}"),
    }

    // Server drops; queue a second asset during backoff so the consolidated
    // re-subscribe must include both.
    let _ = conn.subscribe(&["0xdef".into()]).await;

    let mut saw_disconnected = false;
    let mut saw_reconnected = false;
    let mut second_book = None;
    for _ in 0..10 {
        match next_event_timeout(&mut conn).await {
            Event::Disconnected { .. } => saw_disconnected = true,
            Event::Reconnected => saw_reconnected = true,
            Event::Book(b) => {
                second_book = Some(b);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_disconnected, "expected Disconnected");
    assert!(saw_reconnected, "expected Reconnected");
    assert!(second_book.is_some(), "expected second Book event");

    // First subscribe: ["0xabc"]. Second subscribe (after reconnect):
    // sorted union — ["0xabc", "0xdef"].
    let first = subs_rx.try_recv().expect("server saw first sub");
    let second = subs_rx.try_recv().expect("server saw replay sub");
    let first_ids: Vec<String> = serde_json::from_value(first["assets_ids"].clone()).unwrap();
    let second_ids: Vec<String> = serde_json::from_value(second["assets_ids"].clone()).unwrap();
    assert_eq!(first_ids, vec!["0xabc"]);
    let mut second_sorted = second_ids;
    second_sorted.sort();
    assert_eq!(second_sorted, vec!["0xabc", "0xdef"]);

    conn.close().await;
    let _ = server.await;
}
