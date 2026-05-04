//! Drive a [`BookStore`] from an `md-recorder` NDJSON file.
//!
//! [`Replay::drive`] streams events line-by-line, applies snapshot /
//! delta / `RestResync` events to the store, and invokes a
//! caller-supplied callback after each book mutation. The callback
//! is where the strategy under test runs and (typically) submits via
//! the OMS.
//!
//! The replay is **synchronous-time** — it walks the file as fast as
//! tokio will schedule it. Realistic-cadence replay (sleep until
//! `received_at_ms`) is a small follow-up; for backtests we mostly
//! want speed-of-disk replays so a year of data finishes in minutes.

use crate::book_store::BookStore;
use md_recorder::{RecordedEvent, RecordedKind};
use predigy_book::ApplyOutcome;
use predigy_core::market::MarketTicker;
use std::path::Path;
use std::pin::Pin;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tracing::{debug, warn};

/// Outcome of one applied event from the recorded stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayUpdate {
    /// Book mutated for `market`. The callback should evaluate any
    /// strategy that's keyed on this market.
    BookUpdated(MarketTicker),
    /// Event consumed but didn't move the book (ticker, trade, etc.).
    /// Surfaced for callers that want full visibility.
    NonBook,
    /// A delta arrived with a sequence gap. The book has been left
    /// unchanged; sim callers should keep walking the stream — a
    /// later `RestResync` or `Snapshot` event will recover.
    Gap {
        market: MarketTicker,
        expected: u64,
        got: u64,
    },
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ndjson decode at line {line}: {error}")]
    Decode { line: usize, error: String },
    #[error("unsupported recorder schema version {0}")]
    UnsupportedSchema(u32),
}

#[derive(Debug)]
pub struct Replay {
    store: BookStore,
}

impl Replay {
    #[must_use]
    pub fn new(store: BookStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &BookStore {
        &self.store
    }

    /// Stream the file at `path` through the store, calling
    /// `on_update` after each event with the [`ReplayUpdate`]. The
    /// callback is `async` so it can submit to the OMS and await
    /// fills inline.
    pub async fn drive_file<F>(
        &self,
        path: impl AsRef<Path>,
        on_update: F,
    ) -> Result<(), ReplayError>
    where
        F: FnMut(ReplayUpdate) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
    {
        let file = File::open(path.as_ref()).await?;
        let reader = BufReader::new(file);
        self.drive_lines(reader, on_update).await
    }

    /// Same as [`drive_file`] but reads from any [`AsyncBufRead`] —
    /// used by tests that drive a `Vec<u8>` of in-memory NDJSON.
    pub async fn drive_lines<R, F>(&self, reader: R, mut on_update: F) -> Result<(), ReplayError>
    where
        R: tokio::io::AsyncBufRead + Unpin + Send,
        F: FnMut(ReplayUpdate) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
    {
        let mut lines = reader.lines();
        let mut line_no: usize = 0;
        while let Some(line) = lines.next_line().await? {
            line_no += 1;
            if line.trim().is_empty() {
                continue;
            }
            let event: RecordedEvent =
                serde_json::from_str(&line).map_err(|e| ReplayError::Decode {
                    line: line_no,
                    error: e.to_string(),
                })?;
            let update = self.apply(event)?;
            on_update(update).await;
        }
        Ok(())
    }

    fn apply(&self, event: RecordedEvent) -> Result<ReplayUpdate, ReplayError> {
        if event.schema != md_recorder::RECORDER_SCHEMA_VERSION {
            return Err(ReplayError::UnsupportedSchema(event.schema));
        }
        match event.kind {
            RecordedKind::Snapshot {
                market, snapshot, ..
            } => {
                let ticker = MarketTicker::new(&market);
                self.store.apply_snapshot(&ticker, snapshot);
                debug!(market = %ticker, "replay: snapshot applied");
                Ok(ReplayUpdate::BookUpdated(ticker))
            }
            RecordedKind::Delta { delta, .. } => {
                let ticker = MarketTicker::new(&delta.market);
                let outcome = self
                    .store
                    .with_book_mut(&ticker, |book| book.apply_delta(&delta))
                    .unwrap_or(ApplyOutcome::WrongMarket);
                match outcome {
                    ApplyOutcome::Ok => Ok(ReplayUpdate::BookUpdated(ticker)),
                    ApplyOutcome::Gap { expected, got } => {
                        warn!(
                            market = %ticker,
                            expected,
                            got,
                            "replay: sequence gap; awaiting snapshot/resync"
                        );
                        Ok(ReplayUpdate::Gap {
                            market: ticker,
                            expected,
                            got,
                        })
                    }
                    ApplyOutcome::WrongMarket => Ok(ReplayUpdate::NonBook),
                }
            }
            RecordedKind::RestResync {
                market, snapshot, ..
            } => {
                let ticker = MarketTicker::new(&market);
                self.store.apply_rest_snapshot(&ticker, snapshot);
                debug!(market = %ticker, "replay: rest_resync applied");
                Ok(ReplayUpdate::BookUpdated(ticker))
            }
            RecordedKind::Subscribed { .. }
            | RecordedKind::Ticker { .. }
            | RecordedKind::Trade { .. }
            | RecordedKind::ServerError { .. }
            | RecordedKind::Disconnected { .. }
            | RecordedKind::Reconnected
            | RecordedKind::Malformed { .. } => Ok(ReplayUpdate::NonBook),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md_recorder::RECORDER_SCHEMA_VERSION;
    use predigy_book::{Delta, Snapshot};
    use predigy_core::price::Price;
    use predigy_core::side::Side;
    use std::io::Cursor;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    fn line(event: &RecordedEvent) -> String {
        let mut s = serde_json::to_string(event).unwrap();
        s.push('\n');
        s
    }

    #[tokio::test]
    async fn applies_snapshot_then_delta_in_order() {
        let store = BookStore::new();
        let replay = Replay::new(store.clone());

        let mut payload = String::new();
        payload.push_str(&line(&RecordedEvent::new(
            1,
            "kalshi",
            RecordedKind::Snapshot {
                sid: 7,
                market: "X".into(),
                snapshot: Snapshot {
                    seq: 100,
                    yes_bids: vec![(p(40), 50)],
                    no_bids: vec![(p(60), 50)],
                },
            },
        )));
        payload.push_str(&line(&RecordedEvent::new(
            2,
            "kalshi",
            RecordedKind::Delta {
                sid: 7,
                delta: Delta {
                    market: "X".into(),
                    seq: 101,
                    side: Side::Yes,
                    price: p(41),
                    qty_delta: 25,
                },
            },
        )));

        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates_w = updates.clone();
        replay
            .drive_lines(
                BufReader::new(Cursor::new(payload.into_bytes())),
                move |u| {
                    let updates_w = updates_w.clone();
                    Box::pin(async move {
                        updates_w.lock().unwrap().push(u);
                    })
                },
            )
            .await
            .unwrap();

        let updates = updates.lock().unwrap().clone();
        assert_eq!(updates.len(), 2);
        for u in &updates {
            assert!(matches!(u, ReplayUpdate::BookUpdated(_)));
        }
        store.with_book(&MarketTicker::new("X"), |b| {
            let b = b.unwrap();
            assert_eq!(b.best_yes_bid().unwrap().0.cents(), 41);
            assert_eq!(b.best_yes_bid().unwrap().1, 25);
        });
    }

    #[tokio::test]
    async fn delta_with_gap_is_surfaced_and_book_unchanged() {
        let store = BookStore::new();
        let replay = Replay::new(store.clone());
        let mut payload = String::new();
        payload.push_str(&line(&RecordedEvent::new(
            1,
            "kalshi",
            RecordedKind::Snapshot {
                sid: 7,
                market: "X".into(),
                snapshot: Snapshot {
                    seq: 100,
                    yes_bids: vec![(p(40), 50)],
                    no_bids: vec![(p(60), 50)],
                },
            },
        )));
        // Skips seq 101.
        payload.push_str(&line(&RecordedEvent::new(
            2,
            "kalshi",
            RecordedKind::Delta {
                sid: 7,
                delta: Delta {
                    market: "X".into(),
                    seq: 105,
                    side: Side::Yes,
                    price: p(41),
                    qty_delta: 25,
                },
            },
        )));

        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates_w = updates.clone();
        replay
            .drive_lines(
                BufReader::new(Cursor::new(payload.into_bytes())),
                move |u| {
                    let updates_w = updates_w.clone();
                    Box::pin(async move {
                        updates_w.lock().unwrap().push(u);
                    })
                },
            )
            .await
            .unwrap();

        let updates = updates.lock().unwrap().clone();
        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[0], ReplayUpdate::BookUpdated(_)));
        match &updates[1] {
            ReplayUpdate::Gap { expected, got, .. } => {
                assert_eq!(*expected, 101);
                assert_eq!(*got, 105);
            }
            other => panic!("expected Gap, got {other:?}"),
        }
        // Snapshot's state is preserved (last_seq is 100, level untouched).
        store.with_book(&MarketTicker::new("X"), |b| {
            let b = b.unwrap();
            assert_eq!(b.best_yes_bid().unwrap().1, 50);
            assert_eq!(b.last_seq(), Some(100));
        });
    }

    #[tokio::test]
    async fn rejects_unsupported_schema() {
        let store = BookStore::new();
        let replay = Replay::new(store);
        // Hand-rolled JSON with schema=999.
        let raw = r#"{"schema":999,"received_at_ms":1,"venue":"kalshi","kind":"reconnected"}
"#;
        let err = replay
            .drive_lines(BufReader::new(Cursor::new(raw.as_bytes().to_vec())), |_u| {
                Box::pin(async {})
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ReplayError::UnsupportedSchema(999)));
    }

    #[tokio::test]
    async fn schema_version_constant_matches_recorder() {
        // Sanity: if we ever bump the schema in md-recorder we'll
        // notice here before users hit the runtime error.
        assert_eq!(RECORDER_SCHEMA_VERSION, 1);
    }
}
