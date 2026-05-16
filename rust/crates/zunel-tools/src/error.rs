use std::path::PathBuf;

/// Tool-layer errors. Converted to user-visible `ToolResult` strings
/// before returning to the runner.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid arguments to {tool}: {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("io error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{tool}: policy violation: {reason}")]
    PolicyViolation { tool: String, reason: String },
    #[error("{tool}: timed out after {after_s}s")]
    Timeout { tool: String, after_s: u64 },
    #[error("{tool}: network error: {source}")]
    Network {
        tool: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{tool}: path not found: {path:?}")]
    NotFound { tool: String, path: PathBuf },
    #[error("{tool}: SSRF blocked for {url}: {reason}")]
    SsrfBlocked {
        tool: String,
        url: String,
        reason: String,
    },
    #[error("{tool}: json {action} failed: {source}")]
    Json {
        tool: String,
        action: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("{what} is not implemented in this build")]
    Unimplemented { what: String },
    #[error("{tool}: {message}")]
    Other { tool: String, message: String },
}

pub type Result<T> = std::result::Result<T, Error>;
