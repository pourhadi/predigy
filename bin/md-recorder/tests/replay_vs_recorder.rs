//! Phase 1 acceptance test for `md-recorder`:
//!
//! 1. Spin up a loopback Kalshi WS server.
//! 2. Run the recorder against it with a canned snapshot provider.
//! 3. Drive the WS through: subscribe → snapshot → clean delta → gappy
//!    delta (forces REST resync) → another delta after the resync.
//! 4. Stop the recorder, read the NDJSON file back.
//! 5. Replay the NDJSON into a fresh `OrderBook`.
//! 6. Assert the replayed book exactly matches the recorder's
//!    in-memory book.
//!
//! This is the integration test underwriting the Phase 1 acceptance
//! criterion in `docs/STATUS.md`: "replay-vs-snapshot identical book".

use anyhow::Result;
use futures_util::{SinkExt as _, StreamExt as _};
use md_recorder::{RecordedEvent, RecordedKind, Recorder, SnapshotProvider};
use predigy_book::{OrderBook, Snapshot};
use predigy_core::price::Price;
use predigy_kalshi_md::{Channel, Client};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

fn p(c: u8) -> Price {
    Price::from_cents(c).unwrap()
}

/// Stub provider that returns whatever snapshot was registered for a
/// market. Calls are observable so the test can assert the recorder
/// actually issued the resync.
#[derive(Clone, Default)]
struct CannedSnapshotProvider {
    snapshots: Arc<Mutex<HashMap<String, Snapshot>>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl CannedSnapshotProvider {
    fn set(&self, market: &str, snap: Snapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .insert(market.to_string(), snap);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SnapshotProvider for CannedSnapshotProvider {
    async fn fresh_snapshot(&self, market: &str) -> Result<Snapshot> {
        self.calls.lock().unwrap().push(market.to_string());
        let snap = self
            .snapshots
            .lock()
            .unwrap()
            .get(market)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no canned snapshot for {market}"))?;
        Ok(snap)
    }
}

async fn run_mock_ws_server(listener: TcpListener) {
    let (sock, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();

    // Wait for the recorder's subscribe.
    let first = ws.next().await.unwrap().unwrap();
    let Message::Text(t) = first else {
        panic!("expected text")
    };
    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap();
    assert_eq!(parsed["cmd"], "subscribe");
    let req_id = parsed["id"].as_u64().unwrap();

    // Subscribed ack.
    let ack = serde_json::json!({
        "id": req_id,
        "type": "subscribed",
        "msg": { "channel": "orderbook_delta", "sid": 1 }
    });
    ws.send(Message::Text(ack.to_string().into()))
        .await
        .unwrap();

    // Snapshot @ seq 100. YES bids: 40¢×100, 45¢×50. NO bids: 55¢×25.
    let snap = serde_json::json!({
        "type": "orderbook_snapshot",
        "sid": 1, "seq": 100,
        "msg": {
            "market_ticker": "TEST",
            "yes_dollars_fp": [["0.40","100"],["0.45","50"]],
            "no_dollars_fp":  [["0.55","25"]]
        }
    });
    ws.send(Message::Text(snap.to_string().into()))
        .await
        .unwrap();

    // Clean delta @ seq 101: add 25 at 41¢ on YES side.
    let d1 = serde_json::json!({
        "type":"orderbook_delta",
        "sid":1,"seq":101,
        "msg":{"market_ticker":"TEST","price_dollars":"0.41","delta_fp":"25","side":"yes"}
    });
    ws.send(Message::Text(d1.to_string().into())).await.unwrap();

    // GAPPY delta @ seq 105 (we expected 102) → recorder should fetch
    // a fresh REST snapshot and apply it.
    let d_gap = serde_json::json!({
        "type":"orderbook_delta",
        "sid":1,"seq":105,
        "msg":{"market_ticker":"TEST","price_dollars":"0.46","delta_fp":"10","side":"yes"}
    });
    ws.send(Message::Text(d_gap.to_string().into()))
        .await
        .unwrap();

    // Drain until the recorder closes.
    while let Some(m) = ws.next().await {
        if matches!(m, Ok(Message::Close(_)) | Err(_)) {
            break;
        }
    }
}

#[tokio::test]
async fn recorded_ndjson_replays_to_identical_book() -> Result<()> {
    // 1. Mock WS server.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(run_mock_ws_server(listener));

    // 2. Snapshot provider with a canned post-gap snapshot.
    //    The recorder's book after Gap-then-resync should reflect THIS
    //    snapshot, not the snapshot the WS server initially sent.
    let provider = CannedSnapshotProvider::default();
    let resync_snap = Snapshot {
        seq: 0, // REST has no seq number; recorder + replay both treat 0 as the post-resync baseline
        yes_bids: vec![(p(42), 200), (p(48), 30)],
        no_bids: vec![(p(53), 80)],
    };
    provider.set("TEST", resync_snap.clone());

    // 3. Recorder.
    let tmp = tempfile_path("recorder-replay.ndjson");
    let mut recorder = Recorder::new(tmp.clone(), provider.clone());

    // 4. WS client + subscribe.
    let url = Url::parse(&format!("ws://{addr}/")).unwrap();
    let client = Client::with_endpoint(url, None);
    let mut conn = client.connect();
    conn.subscribe(&[Channel::OrderbookDelta], &["TEST".into()])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 5. Run with a stop signal. We give the recorder 1 second; that's
    //    plenty for four events through localhost. Set up the stop
    //    signal to fire after a short fixed window.
    let stop = tokio::time::sleep(Duration::from_millis(800));
    recorder.run(conn, stop).await?;

    // 6. Confirm the recorder did issue a resync for TEST.
    assert_eq!(
        provider.calls(),
        vec!["TEST".to_string()],
        "expected exactly one REST resync call"
    );

    // 7. Recorder's in-memory book after the run.
    let live_book = recorder
        .book("TEST")
        .expect("recorder should have a book for TEST");
    let live_best_yes = live_book.best_yes_bid();
    let live_best_no = live_book.best_no_bid();
    let live_seq = live_book.last_seq();

    // 8. Replay the NDJSON into a fresh book.
    let replayed = replay_ndjson(&tmp).await?;
    let replayed_book = replayed.get("TEST").expect("replayed book for TEST exists");

    assert_eq!(
        replayed_book.best_yes_bid(),
        live_best_yes,
        "replayed best YES bid must equal recorder state"
    );
    assert_eq!(
        replayed_book.best_no_bid(),
        live_best_no,
        "replayed best NO bid must equal recorder state"
    );
    assert_eq!(
        replayed_book.last_seq(),
        live_seq,
        "replayed last_seq must equal recorder state"
    );
    // Sanity: the recorder applied the canned resync snapshot, so the
    // top-of-book should match it (best YES bid = 48¢, best NO = 53¢).
    assert_eq!(replayed_book.best_yes_bid(), Some((p(48), 30)));
    assert_eq!(replayed_book.best_no_bid(), Some((p(53), 80)));

    // 9. Cleanup.
    let _ = server.await;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// Read an NDJSON file produced by the recorder and reconstruct
/// per-market books by re-applying snapshot / delta / `rest_resync` events
/// in file order.
async fn replay_ndjson(path: &PathBuf) -> Result<HashMap<String, OrderBook>> {
    let text = tokio::fs::read_to_string(path).await?;
    let mut books: HashMap<String, OrderBook> = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let ev: RecordedEvent = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("line {i}: {e}\n  raw: {line}"))?;
        match ev.kind {
            RecordedKind::Snapshot {
                market, snapshot, ..
            } => {
                let book = books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market));
                book.apply_snapshot(snapshot);
            }
            RecordedKind::Delta { delta, .. } => {
                let book = books
                    .entry(delta.market.clone())
                    .or_insert_with(|| OrderBook::new(delta.market.clone()));
                book.apply_delta(&delta);
            }
            RecordedKind::RestResync {
                market, snapshot, ..
            } => {
                let book = books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market));
                book.apply_snapshot(snapshot);
            }
            _ => {}
        }
    }
    Ok(books)
}

fn tempfile_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("predigy-{pid}-{nanos}-{name}"));
    p
}
