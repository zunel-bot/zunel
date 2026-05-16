use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    #[error("provider returned status {status}: {body}")]
    ProviderReturned { status: u16, body: String },

    #[error("failed to parse provider response: {0}")]
    Parse(String),

    #[error("provider misconfigured: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
