use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame: {0}")]
    Frame(String),
    #[error("checksum mismatch: expected {expected:03}, got {got:03}")]
    Checksum { expected: u8, got: u8 },
    #[error("missing required tag {tag} on {msg_type}")]
    MissingTag { tag: u32, msg_type: String },
    #[error("malformed value for tag {tag}: {got:?}")]
    MalformedTag { tag: u32, got: String },
    #[error("session: {0}")]
    Session(&'static str),
    #[error("unsupported intent: {0}")]
    Unsupported(&'static str),
    #[error("session closed")]
    Closed,
}
