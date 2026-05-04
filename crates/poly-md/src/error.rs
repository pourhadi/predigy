use thiserror::Error;

/// Errors produced by the Polymarket WebSocket client.
#[derive(Debug, Error)]
pub enum Error {
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("upgrade request: {0}")]
    Upgrade(String),

    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("invalid argument: {0}")]
    Invalid(String),

    #[error("connection closed")]
    Closed,

    #[error("invalid base url: {0}")]
    Url(#[from] url::ParseError),
}
