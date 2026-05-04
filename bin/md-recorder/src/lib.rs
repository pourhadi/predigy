//! Library half of the `md-recorder` binary.
//!
//! Splits into:
//! - [`recorded`]: the on-disk schema. One JSON object per line in a
//!   stable, versioned format that round-trips through the recorder.
//! - [`recorder`]: the runtime — consumes events from
//!   [`predigy_kalshi_md::Connection`], writes each to NDJSON, maintains
//!   per-market order books, and on `OrderBook::Gap` re-fetches a REST
//!   snapshot via a [`SnapshotProvider`].
//!
//! Exposed as a library so integration tests can drive the recorder
//! end-to-end against an in-process mock WS + REST server, without
//! shelling out to the binary.

pub mod recorded;
pub mod recorder;

pub use recorded::{RECORDER_SCHEMA_VERSION, RecordedEvent, RecordedKind};
pub use recorder::{Recorder, SnapshotProvider};
