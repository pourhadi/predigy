use thiserror::Error;

/// Errors produced by the Kalshi WebSocket client.
#[derive(Debug, Error)]
pub enum Error {
    /// Transport-layer failure: TLS, DNS, TCP, or WebSocket protocol.
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    /// HTTP upgrade-request construction failed (bad URL or bad header value).
    #[error("upgrade request: {0}")]
    Upgrade(String),

    /// Server returned an `error`-typed envelope on the wire.
    #[error("server error (code {code}): {msg}")]
    Server { code: i64, msg: String },

    /// Could not decode an incoming JSON frame into the expected schema.
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),

    /// A price or quantity field on the wire was out of the supported range
    /// (price must be 1..=99 cents; quantity fits in u32).
    #[error("decode: out-of-range fixed-point value: {0}")]
    OutOfRange(String),

    /// The background connection task has exited and the channel is closed.
    #[error("connection closed")]
    Closed,

    /// A configuration value was rejected (e.g. empty market ticker list).
    #[error("invalid argument: {0}")]
    Invalid(String),

    /// URL parse failure for a custom base override.
    #[error("invalid base url: {0}")]
    Url(#[from] url::ParseError),
}
