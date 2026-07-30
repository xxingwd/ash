pub mod anthropic;
pub mod completions;
mod model_config;
pub mod responses;
mod sse;

use std::sync::Arc;

use ash_core::ModelClient;
use serde::{Deserialize, Serialize};
use strum::EnumString;

#[derive(Debug, Clone, Serialize, Deserialize, EnumString)]
pub enum Protocol {
    #[strum(serialize = "anthropic")]
    AnthropicMessages,
    #[strum(serialize = "openai")]
    OpenaiCompletions,
    #[strum(serialize = "openai-responses")]
    OpenaiResponses,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AnthropicMessages => "AnthropicMessages",
            Self::OpenaiCompletions => "OpenaiCompletions",
            Self::OpenaiResponses => "OpenaiResponses",
        })
    }
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

pub use ash_core::ModelClient as ProtocolAdapter;
pub use ash_core::{
    ModelRequest as LlmRequest, ModelStream as ProtocolStream, ModelStreamEvent as StreamItem,
};

pub fn create_adapter(cfg: ProviderConfig) -> Arc<dyn ModelClient> {
    match cfg.protocol {
        Protocol::AnthropicMessages => Arc::new(anthropic::AnthropicAdapter::new(cfg)),
        Protocol::OpenaiCompletions => Arc::new(completions::CompletionsAdapter::new(cfg)),
        Protocol::OpenaiResponses => Arc::new(responses::ResponsesAdapter::new(cfg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_display_is_stable() {
        assert_eq!(Protocol::AnthropicMessages.to_string(), "AnthropicMessages");
        assert_eq!(Protocol::OpenaiCompletions.to_string(), "OpenaiCompletions");
        assert_eq!(Protocol::OpenaiResponses.to_string(), "OpenaiResponses");
    }
}
