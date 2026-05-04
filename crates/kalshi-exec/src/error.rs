use thiserror::Error;

/// Errors produced by the REST executor before they're flattened into
/// `oms::ExecutorError` for the OMS.
#[derive(Debug, Error)]
pub enum Error {
    #[error("kalshi rest: {0}")]
    Rest(#[from] predigy_kalshi_rest::Error),

    /// The intent shape isn't expressible on Kalshi V2 (e.g. a market
    /// order type — V2 only takes limit orders, with IOC for take-now
    /// behaviour).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),

    #[error("decode: {0}")]
    Decode(String),
}
