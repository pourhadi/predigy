// FIX 4.4 has a fixed vocabulary of protocol terms (ExecType, OrdStatus,
// ClOrdID, MsgSeqNum, ResendRequest, …) that read clearly without
// backticks. Suppress doc_markdown for the whole crate rather than
// peppering every doc comment with backticks. Also: FIX message
// builders genuinely take many arguments (sender, target, seq, body…)
// and the session task is naturally long.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::wildcard_imports
)]

//! Kalshi FIX 4.4 executor for the OMS.
//!
//! Hand-rolled framing + a curated subset of messages (Logon,
//! Heartbeat, NewOrderSingle, OrderCancelRequest, ExecutionReport).
//! Reuses `predigy-oms`'s `Executor` trait so strategies that
//! compile against the OMS can be retargeted to FIX without code
//! changes.
//!
//! ## Why hand-rolled
//!
//! - We use a small subset of FIX 4.4 (a dozen tags, three message
//!   types). Pulling in `quickfix-rs` would add a C++ build
//!   dependency for ~200 lines of message machinery.
//! - The wire format is small, well-specified, and security-relevant
//!   (a corrupt order could double-fire). A focused
//!   implementation we own is easier to audit than a wrapper around
//!   a generic engine.
//! - `fefix` (pure Rust) was the alternative. It's a fine library
//!   but has a heavier API surface than we need for the four FIX
//!   message types this crate exchanges.
//!
//! ## What's in here today
//!
//! - [`frame`]: SOH framing, checksum verification, partial-frame
//!   detection.
//! - [`messages`]: typed builders for Logon/Heartbeat/
//!   NewOrderSingle/OrderCancelRequest, parser for ExecutionReport.
//! - [`session`]: sequence-number tracking and the session-level
//!   state machine (logon → connected → logout).
//! - [`executor::FixExecutor`]: implements `predigy_oms::Executor`
//!   over a TCP+TLS connection running the above session.
//!
//! ## What's deliberately deferred
//!
//! - **ResendRequest gap fill** (35=2 / 35=4). On a sequence gap we
//!   currently disconnect and let the operator restart with the
//!   right starting seq via `--reset-seq`. Live-shake-down feedback
//!   will tell us whether this is too brittle.
//! - **OrderMassCancelRequest** (35=q). The REST batch-cancel path
//!   on `predigy-kalshi-rest` covers the kill-switch case today.
//! - **OrderCancelReplaceRequest** (35=G) — order amend.
//! - **`PostOnly` TIF** in `NewOrderSingle`. Kalshi exposes this via
//!   `ExecInst` (tag 18) but the exact code list isn't documented
//!   on the public docs site; will fill in once we have FIX
//!   sandbox access.
//! - **Live Kalshi auth handshake** in [`session::Session::logon`].
//!   The skeleton accepts a `Vec<(u32, String)>` of auth tags so a
//!   binary can pass the right Kalshi-specific values once we have
//!   the spec.

pub mod error;
pub mod executor;
pub mod frame;
pub mod messages;
pub mod session;
pub mod tags;

pub use error::Error;
pub use executor::{FixConfig, FixExecutor};
pub use frame::{FieldList, body_with_msg_type, decode_message, encode, pretty};
pub use messages::{
    ExecKind, ParsedExecutionReport, build_heartbeat, build_logon, build_new_order_single,
    build_order_cancel_request, parse_execution_report,
};
pub use session::Session;
