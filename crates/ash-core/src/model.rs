use std::pin::Pin;

use derive_more::{Display, From, Into};
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::{Message, ProtocolError, StopReason, ToolCallId, ToolDefinition, Usage};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: ModelId,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
}

/// Provider-neutral streaming event. Incremental by nature; only a terminal
/// `Stop` is a reliable boundary across reconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    Usage(Usage),
    Stop(StopReason),
}

pub type ModelStream =
    Pin<Box<dyn Stream<Item = Result<ModelEvent, ProtocolError>> + Send + 'static>>;

/// Provider-neutral streaming model interface used by the agent runtime.
pub trait ModelClient: Send + Sync {
    /// Start a model stream for the given request.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] when the request cannot be started (invalid
    /// request shape, transport failure, or provider rejection).
    fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError>;
}
