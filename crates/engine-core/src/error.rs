//! Engine error type. One enum across the whole engine + every
//! strategy module so the supervisor can pattern-match on the
//! actual cause without `Box<dyn Error>` introspection.
//!
//! Strategies that need their own error types convert into
//! `EngineError::Strategy { ... }` at the trait boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("kalshi rest: {0}")]
    KalshiRest(String),

    #[error("kalshi ws: {0}")]
    KalshiWs(String),

    #[error("kalshi fix: {0}")]
    KalshiFix(String),

    #[error("polymarket: {0}")]
    Polymarket(String),

    #[error("ext-feed {feed}: {reason}")]
    ExternalFeed { feed: &'static str, reason: String },

    #[error("oms: {0}")]
    Oms(String),

    #[error("risk: {0}")]
    Risk(String),

    #[error("strategy {strategy}: {reason}")]
    Strategy { strategy: &'static str, reason: String },

    #[error("config: {0}")]
    Config(String),

    #[error("shutdown")]
    Shutdown,
}

pub type EngineResult<T> = Result<T, EngineError>;

impl EngineError {
    /// Strategy-side errors funnel through this constructor so
    /// the supervisor can route them by `strategy` field.
    pub fn strategy<E: std::error::Error>(strategy: &'static str, e: E) -> Self {
        EngineError::Strategy {
            strategy,
            reason: e.to_string(),
        }
    }

    /// Whether this error means the affected component should
    /// crash + retry vs continue. Used by the supervisor.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            EngineError::Config(_) | EngineError::Database(sqlx::Error::Configuration(_))
        )
    }
}
