use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("kalshi returned {status}: {body}")]
    Api { status: u16, body: String },

    #[error("auth: {0}")]
    Auth(String),

    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("invalid base url: {0}")]
    Url(#[from] url::ParseError),
}
