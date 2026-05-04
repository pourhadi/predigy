//! `predigy-core` — shared domain types for the Kalshi trading system.
//!
//! This crate has no I/O, no async, no allocation in hot paths. Every other
//! crate depends on it; nothing here may depend on anything else.

pub mod fees;
pub mod fill;
pub mod intent;
pub mod market;
pub mod order;
pub mod position;
pub mod price;
pub mod side;

pub use fees::{maker_fee, taker_fee};
pub use fill::Fill;
pub use intent::Intent;
pub use market::{Market, MarketTicker};
pub use order::{Order, OrderId, OrderState, OrderType, TimeInForce};
pub use position::Position;
pub use price::{Price, Qty};
pub use side::{Action, Side};
