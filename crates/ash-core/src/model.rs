use std::pin::Pin;

use derive_more::{Display, From, Into};
use futures::Stream;
use serde::{Deserialize, Serialize};
use strum::EnumString;

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

#[derive(Debug, Clone, Serialize, Deserialize, Display, EnumString)]
pub enum Protocol {
    #[strum(serialize = "anthropic")]
    AnthropicMessages,
    #[strum(serialize = "openai")]
    OpenaiCompletions,
    #[strum(serialize = "openai-responses")]
    OpenaiResponses,
}

impl Protocol {
    pub const fn as_cli_name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenaiCompletions => "openai",
            Self::OpenaiResponses => "openai-responses",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub api_key: secrecy::SecretString,
    pub base_url: Option<String>,
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
