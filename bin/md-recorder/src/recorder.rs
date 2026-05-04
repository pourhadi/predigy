//! Recorder runtime.
//!
//! Pulls [`predigy_kalshi_md::Event`]s off a `Connection`, writes each as
//! one NDJSON line via [`RecordedEvent`], and maintains per-market
//! [`OrderBook`]s. On `OrderBook::Gap`, asks the configured
//! [`SnapshotProvider`] for a fresh REST snapshot, applies it, and
//! emits a synthetic `RestResync` line so replay tools can reconstruct
//! the same sequence of book states the recorder observed.

use crate::recorded::{RecordedEvent, RecordedKind};
use anyhow::{Context as _, Result};
use predigy_book::{ApplyOutcome, OrderBook, Snapshot};
use predigy_kalshi_md::{Connection, Event};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tracing::{info, warn};

/// Async source of fresh REST snapshots. Generic over implementations so
/// integration tests can plug in a canned in-memory provider without
/// spinning up an HTTP server.
pub trait SnapshotProvider {
    fn fresh_snapshot(
        &self,
        market: &str,
    ) -> impl std::future::Future<Output = Result<Snapshot>> + Send;
}

/// One recorder instance. Owns the output file, the per-market books,
/// and the snapshot provider.
pub struct Recorder<P: SnapshotProvider + Send + Sync> {
    output_path: PathBuf,
    snapshot_provider: P,
    books: HashMap<String, OrderBook>,
}

impl<P: SnapshotProvider + Send + Sync> std::fmt::Debug for Recorder<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("output_path", &self.output_path)
            .field("books", &self.books.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<P: SnapshotProvider + Send + Sync> Recorder<P> {
    pub fn new(output_path: PathBuf, snapshot_provider: P) -> Self {
        Self {
            output_path,
            snapshot_provider,
            books: HashMap::new(),
        }
    }

    /// Drain `conn` until it ends or `stop` resolves. Each event is
    /// written to the NDJSON file before any book mutation, so the file
    /// is the source of truth even on a process crash.
    pub async fn run<F>(&mut self, mut conn: Connection, stop: F) -> Result<()>
    where
        F: std::future::Future<Output = ()>,
    {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.output_path)
            .await
            .with_context(|| format!("open {}", self.output_path.display()))?;
        let mut writer = BufWriter::new(file);
        info!(path = %self.output_path.display(), "recorder writing NDJSON");

        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => {
                    info!("recorder received stop signal");
                    break;
                }
                maybe_ev = conn.next_event() => {
                    let Some(ev) = maybe_ev else { break };
                    self.handle_event(&mut writer, ev).await?;
                }
            }
        }
        writer
            .flush()
            .await
            .with_context(|| format!("flush {}", self.output_path.display()))?;
        Ok(())
    }

    async fn handle_event<W>(&mut self, writer: &mut W, ev: Event) -> Result<()>
    where
        W: AsyncWrite + Unpin + Send,
    {
        // Collect any synthesised events (currently only RestResync) so
        // each iteration of this method writes a deterministic, ordered
        // sequence of NDJSON lines: first the original event, then any
        // recorder-side reactions.
        let mut tail: Vec<RecordedEvent> = Vec::new();

        let kind = match ev {
            Event::Subscribed {
                req_id,
                channel,
                sid,
            } => RecordedKind::Subscribed {
                req_id,
                channel,
                sid,
            },
            Event::Snapshot {
                sid,
                market,
                snapshot,
            } => {
                let book = self
                    .books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                book.apply_snapshot(snapshot.clone());
                RecordedKind::Snapshot {
                    sid,
                    market,
                    snapshot,
                }
            }
            Event::Delta { sid, delta } => {
                let book = self
                    .books
                    .entry(delta.market.clone())
                    .or_insert_with(|| OrderBook::new(delta.market.clone()));
                let outcome = book.apply_delta(&delta);
                if let ApplyOutcome::Gap { expected, got } = outcome {
                    let reason = format!("sequence gap: expected {expected} got {got}");
                    warn!(market = %delta.market, expected, got, "WS sequence gap; resyncing via REST");
                    let market_for_resync = delta.market.clone();
                    let resync_kind = self
                        .resync(&market_for_resync, reason)
                        .await
                        .with_context(|| format!("REST resync for {market_for_resync}"))?;
                    tail.push(wrap(resync_kind));
                }
                RecordedKind::Delta { sid, delta }
            }
            Event::Ticker { sid, body } => RecordedKind::Ticker { sid, body },
            Event::Trade { sid, body } => RecordedKind::Trade { sid, body },
            Event::ServerError { req_id, code, msg } => {
                RecordedKind::ServerError { req_id, code, msg }
            }
            Event::Disconnected { attempt, reason } => {
                RecordedKind::Disconnected { attempt, reason }
            }
            Event::Reconnected => {
                // After a reconnect we have no way to know whether deltas
                // were missed during the gap; force a REST resync of every
                // market we've been tracking. Each one is recorded as its
                // own RestResync line so replay reconstructs identical
                // book state.
                let markets: Vec<String> = self.books.keys().cloned().collect();
                for market in markets {
                    let resync_kind = self
                        .resync(&market, "ws reconnect: forced resync".into())
                        .await
                        .with_context(|| format!("REST resync for {market} on reconnect"))?;
                    tail.push(wrap(resync_kind));
                }
                RecordedKind::Reconnected
            }
            Event::Malformed { raw, error } => RecordedKind::Malformed { raw, error },
        };

        let head = wrap(kind);
        write_line(writer, &head).await?;
        for r in tail {
            write_line(writer, &r).await?;
        }
        // Flush per event so a process crash doesn't lose buffered lines.
        // The recorder's event rate is well below what fdatasync can
        // sustain even on a slow disk.
        writer.flush().await.context("flush after writing event")?;
        Ok(())
    }

    /// Fetch a fresh snapshot, apply it to the in-memory book, and return
    /// the `RecordedKind::RestResync` value the caller should write to
    /// the NDJSON file.
    async fn resync(&mut self, market: &str, reason: String) -> Result<RecordedKind> {
        let snapshot = self
            .snapshot_provider
            .fresh_snapshot(market)
            .await
            .with_context(|| format!("fresh_snapshot({market})"))?;
        let book = self
            .books
            .entry(market.to_string())
            .or_insert_with(|| OrderBook::new(market.to_string()));
        book.apply_snapshot(snapshot.clone());
        Ok(RecordedKind::RestResync {
            market: market.to_string(),
            reason,
            snapshot,
        })
    }

    /// Test-only access to the in-memory books so integration tests can
    /// compare recorder state vs. replayed state without scraping the
    /// NDJSON file.
    #[doc(hidden)]
    pub fn book(&self, market: &str) -> Option<&OrderBook> {
        self.books.get(market)
    }
}

fn wrap(kind: RecordedKind) -> RecordedEvent {
    RecordedEvent::new(now_unix_ms(), "kalshi", kind)
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, ev: &RecordedEvent) -> Result<()> {
    let line = serde_json::to_string(ev).context("serialize RecordedEvent")?;
    writer
        .write_all(line.as_bytes())
        .await
        .context("write event")?;
    writer.write_all(b"\n").await.context("write newline")?;
    Ok(())
}

fn now_unix_ms() -> i64 {
    let raw = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    i64::try_from(raw).unwrap_or(i64::MAX)
}
