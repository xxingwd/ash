pub mod anthropic;
pub mod completions;
mod model_config;
pub mod responses;
mod sse;

use std::sync::Arc;

use ash_core::{ModelClient, Protocol, ProviderConfig};

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
