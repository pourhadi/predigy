//! Kalshi REST API client (read-only for Phase 1).
//!
//! Default base URL: `https://api.elections.kalshi.com/trade-api/v2`. The
//! client is auth-optional — public endpoints (markets, orderbook) work
//! unauthenticated; portfolio endpoints require an [`auth::Signer`].

pub mod auth;
pub mod client;
pub mod error;
pub mod types;

pub use auth::Signer;
pub use client::Client;
pub use error::Error;
