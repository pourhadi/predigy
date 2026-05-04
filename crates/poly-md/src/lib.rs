//! Polymarket WebSocket market-data client (read-only reference feed).
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/market`,
//! subscribes by ERC-1155 token `asset_id`, and surfaces a high-level
//! event stream over the public market channel: `book`, `price_change`,
//! `last_trade_price`, `tick_size_change`. No auth.
//!
//! ## Scope
//!
//! Polymarket prices are used as a **reference signal** in our
//! cross-venue strategies — we never execute on Polymarket. Accordingly
//! this crate intentionally does not maintain an L2 book. The
//! `price_change` event already carries `best_bid` and `best_ask`
//! fields, which is what reference-price consumers need; full L2
//! reconstruction would be wasted CPU.
//!
//! ## Quick start
//!
//! ```no_run
//! use predigy_poly_md::{Client, Event};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::new()?;
//! let mut conn = client.connect();
//! conn.subscribe(&[
//!     "0xabc...".into(),
//!     "0xdef...".into(),
//! ]).await?;
//!
//! while let Some(ev) = conn.next_event().await {
//!     match ev {
//!         Event::Book(b) => println!("book {} bids={}", b.asset_id, b.bids.len()),
//!         Event::PriceChange(p) => println!("price_change {} changes", p.price_changes.len()),
//!         Event::Disconnected { attempt, reason } => eprintln!("retry {attempt}: {reason}"),
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```

pub mod backoff;
pub mod client;
pub mod decode;
pub mod error;
pub mod messages;

pub use backoff::Backoff;
pub use client::{Client, Connection, DEFAULT_ENDPOINT, Event};
pub use error::Error;
pub use messages::{
    BookEvent, Incoming, LastTradePriceEvent, Level, PriceChange, PriceChangeEvent, Subscribe,
    TickSizeChangeEvent,
};
