use std::pin::Pin;

use derive_more::{Display, From, Into};
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::{Message, ProtocolError, StopReason, ToolCallId, ToolDefinition};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, From, Into, Display)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

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

#[derive(Debug, Clone, PartialEq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Stop(StopReason),
}

pub type ModelStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ProtocolError>> + Send + 'static>>;

/// Provider-neutral streaming model interface used by the agent runtime.
pub trait ModelClient: Send + Sync {
    fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError>;
}
