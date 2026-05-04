//! On-disk NDJSON schema for the recorder.
//!
//! The format is intentionally a thin wrapper around the typed events
//! produced by [`predigy_kalshi_md::Event`], plus two metadata fields
//! (`received_at_ms`, `schema`) and one synthetic kind, [`RestResync`],
//! that the recorder injects after a successful REST resync on a
//! sequence gap.
//!
//! Bumping `RECORDER_SCHEMA_VERSION` should be paired with a migration
//! note here. Replay tools should refuse files with an unknown version.
//!
//! [`RestResync`]: RecordedKind::RestResync

use predigy_book::{Delta, Snapshot};
use predigy_kalshi_md::messages::{TickerBody, TradeBody};
use serde::{Deserialize, Serialize};

/// On-disk schema version. Increment + add a migration note in
/// `docs/STATUS.md` whenever the layout changes in an incompatible way.
pub const RECORDER_SCHEMA_VERSION: u32 = 1;

/// One line of NDJSON. The `kind` field is internally tagged so consumers
/// can `match` without unwrapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Recorder schema version. Always [`RECORDER_SCHEMA_VERSION`] for
    /// freshly written files.
    pub schema: u32,
    /// Wall-clock epoch milliseconds at which the recorder observed the
    /// event. Useful for replay-rate experiments and for debugging
    /// disconnects against external clocks.
    pub received_at_ms: i64,
    /// Source venue. Phase 1 only writes `"kalshi"`, but the field is
    /// present so a Polymarket sidecar can share the same NDJSON file
    /// without ambiguity.
    pub venue: String,
    /// The actual event payload, tagged on `kind`.
    #[serde(flatten)]
    pub kind: RecordedKind,
}

/// What happened. Mirrors [`predigy_kalshi_md::Event`] one-for-one with
/// one addition: [`RestResync`], emitted by the recorder itself after a
/// REST snapshot is fetched in response to a sequence gap.
///
/// [`RestResync`]: RecordedKind::RestResync
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedKind {
    Subscribed {
        req_id: Option<u64>,
        channel: String,
        sid: u64,
    },
    Snapshot {
        sid: u64,
        market: String,
        snapshot: Snapshot,
    },
    Delta {
        sid: u64,
        delta: Delta,
    },
    Ticker {
        sid: u64,
        body: TickerBody,
    },
    Trade {
        sid: u64,
        body: TradeBody,
    },
    ServerError {
        req_id: Option<u64>,
        code: i64,
        msg: String,
    },
    Disconnected {
        attempt: u32,
        reason: String,
    },
    Reconnected,
    Malformed {
        raw: String,
        error: String,
    },
    /// Synthetic event written by the recorder (not received from the
    /// WS) when a sequence gap forces a REST snapshot fetch. The
    /// snapshot's `seq` is whatever Kalshi REST returned (currently
    /// always 0 — REST has no sequence number — but the field is kept
    /// so a future REST schema change is forward-compatible).
    RestResync {
        market: String,
        /// Reason the resync was needed. For now always `"sequence gap:
        /// expected E got G"`, but the string form keeps room for new
        /// triggers (e.g. WS reconnect → forced resync).
        reason: String,
        snapshot: Snapshot,
    },
}

impl RecordedEvent {
    #[must_use]
    pub fn new(received_at_ms: i64, venue: impl Into<String>, kind: RecordedKind) -> Self {
        Self {
            schema: RECORDER_SCHEMA_VERSION,
            received_at_ms,
            venue: venue.into(),
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use predigy_core::price::Price;
    use predigy_core::side::Side;

    fn p(c: u8) -> Price {
        Price::from_cents(c).unwrap()
    }

    #[test]
    fn snapshot_round_trips() {
        let snap = Snapshot {
            seq: 100,
            yes_bids: vec![(p(40), 100), (p(45), 50)],
            no_bids: vec![(p(55), 25)],
        };
        let ev = RecordedEvent::new(
            1,
            "kalshi",
            RecordedKind::Snapshot {
                sid: 7,
                market: "X".into(),
                snapshot: snap.clone(),
            },
        );
        let line = serde_json::to_string(&ev).unwrap();
        let back: RecordedEvent = serde_json::from_str(&line).unwrap();
        let RecordedKind::Snapshot {
            sid,
            market,
            snapshot,
        } = back.kind
        else {
            panic!("wrong variant");
        };
        assert_eq!(sid, 7);
        assert_eq!(market, "X");
        assert_eq!(snapshot.seq, 100);
        assert_eq!(snapshot.yes_bids, snap.yes_bids);
        assert_eq!(snapshot.no_bids, snap.no_bids);
    }

    #[test]
    fn delta_round_trips() {
        let d = Delta {
            market: "X".into(),
            seq: 101,
            side: Side::Yes,
            price: p(41),
            qty_delta: 25,
        };
        let ev = RecordedEvent::new(
            2,
            "kalshi",
            RecordedKind::Delta {
                sid: 7,
                delta: d.clone(),
            },
        );
        let line = serde_json::to_string(&ev).unwrap();
        let back: RecordedEvent = serde_json::from_str(&line).unwrap();
        let RecordedKind::Delta { delta, .. } = back.kind else {
            panic!("wrong variant");
        };
        assert_eq!(delta.market, d.market);
        assert_eq!(delta.seq, d.seq);
        assert_eq!(delta.side, d.side);
        assert_eq!(delta.price.cents(), d.price.cents());
        assert_eq!(delta.qty_delta, d.qty_delta);
    }

    #[test]
    fn rest_resync_round_trips() {
        let snap = Snapshot {
            seq: 0,
            yes_bids: vec![(p(40), 100)],
            no_bids: vec![(p(60), 50)],
        };
        let ev = RecordedEvent::new(
            3,
            "kalshi",
            RecordedKind::RestResync {
                market: "X".into(),
                reason: "sequence gap: expected 11 got 13".into(),
                snapshot: snap,
            },
        );
        let line = serde_json::to_string(&ev).unwrap();
        let back: RecordedEvent = serde_json::from_str(&line).unwrap();
        let RecordedKind::RestResync { reason, .. } = back.kind else {
            panic!("wrong variant");
        };
        assert!(reason.starts_with("sequence gap"));
    }

    #[test]
    fn schema_version_is_recorded() {
        let ev = RecordedEvent::new(1, "kalshi", RecordedKind::Reconnected);
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains(r#""schema":1"#), "got: {line}");
        assert!(line.contains(r#""kind":"reconnected""#), "got: {line}");
    }
}
