//! Kalshi WebSocket market-data client.
//!
//! The crate connects to `wss://api.elections.kalshi.com/trade-api/ws/v2`,
//! signs the upgrade with the same RSA-PSS scheme as the REST API
//! (re-using [`predigy_kalshi_rest::Signer`]), and surfaces a high-level
//! event stream that decodes the public market-data channels into the
//! domain types from [`predigy_book`] and [`predigy_core`].
//!
//! ## Quick start
//!
//! ```no_run
//! use predigy_kalshi_md::{Channel, Client, Event};
//! use predigy_kalshi_rest::Signer;
//! # async fn run(pem: &str) -> Result<(), Box<dyn std::error::Error>> {
//! let signer = Signer::from_pem("KEY-ID", pem)?;
//! let client = Client::new(signer)?;
//! let mut conn = client.connect();
//!
//! conn.subscribe(&[Channel::OrderbookDelta], &["FED-23DEC-T3.00".into()]).await?;
//!
//! while let Some(ev) = conn.next_event().await {
//!     match ev {
//!         Event::Snapshot { market, snapshot, .. } => println!("snap {market} seq={}", snapshot.seq),
//!         Event::Delta { delta, .. } => println!("delta seq={} {:?}", delta.seq, delta.qty_delta),
//!         Event::Disconnected { attempt, reason } => eprintln!("retry {attempt}: {reason}"),
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! ## What the crate does and does not do
//!
//! Does:
//! - Auth on upgrade, reconnect with exponential-backoff full-jitter,
//!   replay of saved subscriptions across reconnects.
//! - Decode `orderbook_snapshot`, `orderbook_delta`, `ticker`, `trade`.
//! - Surface server-side errors and unparseable frames as events rather
//!   than swallowing them.
//!
//! Does not (deferred):
//! - Authenticated channels (`fill`, `user_orders`, `market_positions`).
//! - REST resync on sequence gap — that is integration glue between this
//!   crate and `predigy-kalshi-rest`, lives in the `md-recorder` binary.
//! - In-session command/event stats (counters, timing). Add when needed.

pub mod backoff;
pub mod client;
pub mod decode;
pub mod error;
pub mod messages;

pub use backoff::Backoff;
pub use client::{Client, Connection, DEFAULT_ENDPOINT, Event};
pub use error::Error;
pub use messages::{Channel, FillBody, MarketPositionBody, TickerBody, TradeBody};
