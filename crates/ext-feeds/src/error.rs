use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("invalid config: {0}")]
    Invalid(String),
}
