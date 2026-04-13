use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("auth: {0}")]
    Auth(String),

    #[error("validation: {code}")]
    Validation { code: String, message: String },

    #[error("rate limited (retry in {retry_after:?})")]
    RateLimited { retry_after: Duration },

    #[error("server: {0}")]
    Server(String),

    #[error("network: {0}")]
    Network(#[from] reqwest::Error),

    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
}
