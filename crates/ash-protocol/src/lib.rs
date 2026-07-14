pub mod anthropic;
pub mod completions;
pub mod responses;
mod sse;

use std::{pin::Pin, sync::Arc};

use ash_core::{
    Message, ModelId, Protocol, ProtocolError, ProviderConfig, StopReason, ToolCallId,
    ToolDefinition,
};
use futures::Stream;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
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

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: ModelId,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
}

pub type ProtocolStream =
    Pin<Box<dyn Stream<Item = Result<StreamItem, ProtocolError>> + Send + 'static>>;

pub trait ProtocolAdapter: Send + Sync {
    fn stream(&self, req: LlmRequest) -> Result<ProtocolStream, ProtocolError>;
}

pub fn create_adapter(cfg: ProviderConfig) -> Arc<dyn ProtocolAdapter> {
    match cfg.protocol {
        Protocol::AnthropicMessages => Arc::new(anthropic::AnthropicAdapter::new(cfg)),
        Protocol::OpenaiCompletions => Arc::new(completions::CompletionsAdapter::new(cfg)),
        Protocol::OpenaiResponses => Arc::new(responses::ResponsesAdapter::new(cfg)),
    }
}
