// Vendor / product names appear throughout doc comments.
#![allow(clippy::doc_markdown)]

//! Engine-core: the runtime contracts the consolidated
//! `predigy-engine` binary builds against, plus the `Strategy`
//! trait every strategy module implements.
//!
//! Architectural overview lives in `docs/ARCHITECTURE.md`. This
//! crate is small and pure-types-and-traits — strategy
//! implementations and the engine itself live in their own
//! crates that depend on this one.
//!
//! ## Why a separate crate
//!
//! Strategy modules need to talk to the engine through a tight
//! contract (events in, intents out, shared DB handle, kill
//! switch). Putting the trait in `predigy-engine` directly would
//! make every strategy depend on the whole engine. A small
//! shared crate keeps the dependency graph clean: each strategy
//! depends on `predigy-engine-core`, the engine depends on
//! every strategy.

pub mod cross_strategy;
pub mod db;
pub mod discovery;
pub mod error;
pub mod events;
pub mod intent;
pub mod metrics;
pub mod oms;
pub mod state;
pub mod strategy;

pub use cross_strategy::{CrossStrategyDelivery, CrossStrategyEvent, topic};
pub use db::{DailyPnl, Db, LatestModelP, OpenPosition, RuleRow};
pub use discovery::{DiscoveredMarket, DiscoverySubscription};
pub use error::{EngineError, EngineResult};
pub use events::{Event, ExternalEvent, KalshiPolyPair};
pub use intent::{Intent, IntentAction, LegGroup, OrderType, Tif, cid_safe_ticker};
pub use metrics::{InMemoryMetrics, Metrics, NullMetrics, Tags};
pub use oms::{
    ExecutionStatus, ExecutionUpdate, KillSwitchView, Oms, ReconciliationDiff, RejectionReason,
    RiskCaps, SubmitGroupOutcome, SubmitOutcome, VenueChoice,
};
pub use state::{SelfSubscribeRequest, StrategyState};
pub use strategy::{Strategy, StrategyId};
