use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("provider error: {0}")]
    Provider(#[from] zunel_providers::Error),

    #[error("session io error at {path}: {source}")]
    Session {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("approval denied for tool {tool}")]
    ApprovalDenied { tool: String },

    #[error("approval timed out after {after_s}s for tool {tool}")]
    ApprovalTimeout { tool: String, after_s: u64 },

    #[error("failed to assemble streamed tool call: {0}")]
    ToolCallAssembly(String),

    #[error("bus error: {0}")]
    Bus(#[from] zunel_bus::BusError),

    #[error("agent loop cancelled mid-turn")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
