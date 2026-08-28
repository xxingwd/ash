use std::time::Duration;

use crate::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum AshError {
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("tool: {0}")]
    Tool(#[from] ToolError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("config: {0}")]
    Config(String),
    #[error("{0}")]
    Session(#[from] SessionError),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("session is already open: {path}")]
    AlreadyOpen { path: String },
    #[error("session identity mismatch: expected {expected}, found {actual}")]
    IdentityMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("corrupt session: {message}")]
    Corrupt {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Session lifecycle failures that callers already branch on.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("session is busy; cancel and wait for the active turn before retrying")]
    Busy,
    #[error("session turn queue is full")]
    QueueFull,
    #[error("child sessions cannot be resumed, forked, or undone")]
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
