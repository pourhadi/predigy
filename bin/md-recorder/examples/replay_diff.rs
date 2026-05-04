//! Replay an NDJSON capture through the same logic the recorder used,
//! then print the per-market top-of-book and summary stats.
//!
//! Useful for live-shake-down validation: the structure is the same as
//! the integration test (`replay_vs_recorder.rs`) but free-form so the
//! operator can eyeball top-of-book against a fresh REST snapshot.
//!
//!     cargo run -p md-recorder --example replay_diff -- /path/to/capture.ndjson

use anyhow::{Context, Result, anyhow};
use md_recorder::{RecordedEvent, RecordedKind};
use predigy_book::OrderBook;
use std::collections::HashMap;
use std::path::PathBuf;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: replay_diff <ndjson_path>"))?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;

    let mut books: HashMap<String, OrderBook> = HashMap::new();
    let mut snapshots = 0u64;
    let mut deltas = 0u64;
    let mut deltas_after_gap = 0u64;
    let mut resyncs = 0u64;
    let mut gaps_during_replay = 0u64;
    let mut other = 0u64;
    let mut last_was_resync: HashMap<String, bool> = HashMap::new();

    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let ev: RecordedEvent =
            serde_json::from_str(line).with_context(|| format!("line {}: parse", i + 1))?;
        match ev.kind {
            RecordedKind::Snapshot {
                market, snapshot, ..
            } => {
                snapshots += 1;
                let book = books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                book.apply_snapshot(snapshot);
                last_was_resync.insert(market, false);
            }
            RecordedKind::Delta { delta, .. } => {
                deltas += 1;
                let market = delta.market.clone();
                let book = books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                let was_resync = *last_was_resync.get(&market).unwrap_or(&false);
                let outcome = book.apply_delta(&delta);
                if matches!(outcome, predigy_book::ApplyOutcome::Gap { .. }) {
                    gaps_during_replay += 1;
                    eprintln!(
                        "replay: GAP at line {} market={} outcome={:?}",
                        i + 1,
                        market,
                        outcome
                    );
                }
                if was_resync {
                    deltas_after_gap += 1;
                }
                last_was_resync.insert(market, false);
            }
            RecordedKind::RestResync {
                market, snapshot, ..
            } => {
                resyncs += 1;
                let book = books
                    .entry(market.clone())
                    .or_insert_with(|| OrderBook::new(market.clone()));
                book.apply_rest_snapshot(snapshot);
                last_was_resync.insert(market, true);
            }
            _ => other += 1,
        }
    }

    println!(
        "replay summary: snapshots={snapshots} deltas={deltas} resyncs={resyncs} \
         gaps_during_replay={gaps_during_replay} deltas_directly_after_resync={deltas_after_gap} \
         other_events={other}"
    );
    println!("per-market state at end of capture:");
    let mut markets: Vec<&String> = books.keys().collect();
    markets.sort();
    for m in markets {
        let b = &books[m];
        println!(
            "  {}: last_seq={:?} best_yes_bid={:?} best_no_bid={:?} best_yes_ask={:?}",
            m,
            b.last_seq(),
            b.best_yes_bid().map(|(p, q)| (p.cents(), q)),
            b.best_no_bid().map(|(p, q)| (p.cents(), q)),
            b.best_yes_ask().map(|(p, q)| (p.cents(), q)),
        );
    }
    // A gap during replay is *expected* whenever the live recorder
    // hit one, because the recorder writes the gappy delta line
    // before the synthesised rest_resync line. So `gap_count ==
    // resync_count` is the healthy invariant: every gap paired with
    // a resync. `gap_count > resync_count` means some gap was left
    // unhandled (recorder bug or torn write).
    if gaps_during_replay > resyncs {
        eprintln!(
            "{} gaps left unresolved (resyncs={resyncs}). Recorded NDJSON is missing \
             expected resyncs.",
            gaps_during_replay - resyncs
        );
        std::process::exit(1);
    }
    println!("ok: {gaps_during_replay} gap(s) all paired with resync; final state replays cleanly");
    Ok(())
}
