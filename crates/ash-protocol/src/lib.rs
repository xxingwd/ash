pub mod anthropic;
pub mod completions;
mod model_config;
pub mod responses;
mod sse;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use strum::EnumString;

/// Provider protocols supported by the adapters.
///
/// Variant names match the adapter modules; the serialized forms (`anthropic`,
/// `openai`, `openai-responses`) are the stable CLI and configuration values.
/// Both serde and `strum::EnumString` accept exactly those serialized forms, so
/// `Protocol` round-trips through JSON, `ASH_PROTOCOL`, and the `--protocol` flag
/// with the same vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, EnumString)]
pub enum Protocol {
    /// Anthropic Messages API.
    #[serde(rename = "anthropic")]
    #[strum(serialize = "anthropic")]
    AnthropicMessages,
    /// OpenAI Chat Completions API.
    #[serde(rename = "openai")]
    #[strum(serialize = "openai")]
    Completions,
    /// OpenAI Responses API.
    #[serde(rename = "openai-responses")]
    #[strum(serialize = "openai-responses")]
    Responses,
}

impl std::fmt::Display for Protocol {
    /// Same vocabulary as serde, `EnumString`, and `as_cli_name`: the stable
    /// configuration values (`anthropic`, `openai`, `openai-responses`).
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_cli_name())
    }
}

impl Protocol {
    /// Stable configuration name used by the CLI and `ASH_PROTOCOL`.
    pub const fn as_cli_name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::Completions => "openai",
            Self::Responses => "openai-responses",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub api_key: secrecy::SecretString,
    pub base_url: Option<String>,
}

/// Provider-neutral model client and stream vocabulary, re-exported for adapter code.
pub use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

pub fn create_adapter(cfg: ProviderConfig) -> Arc<dyn ModelClient> {
    match cfg.protocol {
        Protocol::AnthropicMessages => Arc::new(anthropic::AnthropicAdapter::new(cfg)),
        Protocol::Completions => Arc::new(completions::CompletionsAdapter::new(cfg)),
        Protocol::Responses => Arc::new(responses::ResponsesAdapter::new(cfg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_display_uses_the_stable_configuration_names() {
        assert_eq!(Protocol::AnthropicMessages.to_string(), "anthropic");
        assert_eq!(Protocol::Completions.to_string(), "openai");
        assert_eq!(Protocol::Responses.to_string(), "openai-responses");
        // Display and `as_cli_name` must stay in lockstep.
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::Completions,
            Protocol::Responses,
        ] {
            assert_eq!(protocol.to_string(), protocol.as_cli_name());
        }
    }

    #[test]
    fn protocol_serde_and_cli_names_share_one_vocabulary() {
        // serde, `EnumString`, and `as_cli_name` must all accept the same
        // configuration values so JSON round-trips and CLI parsing never diverge.
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::Completions,
            Protocol::Responses,
        ] {
            let json = serde_json::to_value(&protocol).unwrap();
            let round_tripped: Protocol = serde_json::from_value(json).unwrap();
            assert_eq!(round_tripped, protocol);
            let parsed: Protocol = protocol.as_cli_name().parse().unwrap();
            assert_eq!(parsed, protocol);
        }
    }
}
