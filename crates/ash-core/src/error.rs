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
    #[error("{0}")]
    Session(#[from] SessionError),
    #[error("cancelled")]
    Cancelled,
}

/// Session lifecycle failures that callers already branch on.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("session is busy; cancel and wait for the active turn before retrying")]
    Busy,
    #[error("session turn queue is full")]
    QueueFull,
    #[error("a child session cannot be resumed through the root path")]
    ChildSession,
    #[error("the target turn is not active")]
    InactiveTurn,
    #[error("session runtime has stopped")]
    Closed,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProtocolError {
    #[error("auth failed")]
    Auth,
    #[error("rate limited")]
    RateLimited,
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
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("cancelled")]
    Cancelled,
}
