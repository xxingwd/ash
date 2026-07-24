use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AshError {
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("tool: {0}")]
    Tool(#[from] ToolError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("auth failed")]
    Auth,
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Option<Duration> },
    #[error("context too long: {tokens}/{limit}")]
    ContextTooLong { tokens: u64, limit: u64 },
    #[error("upstream {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("request failed: {0}")]
    Request(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("invalid upstream response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Execution(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
}
