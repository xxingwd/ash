mod anthropic;
mod completions;
mod model_config;
mod pending_calls;
mod responses;
mod sse;

use std::{borrow::Cow, sync::Arc};

use ash_core::{Content, Input, ModelContext, Step};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use strum::{AsRefStr, Display, EnumString};

/// Provider protocols supported by the adapters.
///
/// Variant names match the adapter modules; the serialized forms (`anthropic`,
/// `openai`, `openai-responses`) are the stable CLI and configuration values.
/// Both serde and `strum::EnumString` accept exactly those serialized forms, so
/// `Protocol` round-trips through JSON, `ASH_PROTOCOL`, and the `--protocol` flag
/// with the same vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AsRefStr, Display, EnumString)]
pub enum Protocol {
    /// Anthropic Messages API.
    #[serde(rename = "anthropic")]
    #[strum(serialize = "anthropic")]
    AnthropicMessages,
    /// `OpenAI` Chat Completions API.
    #[serde(rename = "openai")]
    #[strum(serialize = "openai")]
    Completions,
    /// `OpenAI` Responses API.
    #[serde(rename = "openai-responses")]
    #[strum(serialize = "openai-responses")]
    Responses,
}

/// Default model for protocols that have a sensible built-in.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-20250514";

impl Protocol {
    /// Stable configuration name used by the CLI and `ASH_PROTOCOL`.
    #[must_use]
    pub fn as_cli_name(&self) -> &str {
        self.as_ref()
    }

    /// Built-in default model for this protocol, if one exists. Protocols
    /// without a default require an explicit `--model` or `ASH_MODEL`.
    #[must_use]
    pub const fn default_model(&self) -> Option<&'static str> {
        match self {
            Self::AnthropicMessages => Some(DEFAULT_ANTHROPIC_MODEL),
            Self::Completions | Self::Responses => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub api_key: secrecy::SecretString,
    pub base_url: Option<String>,
    pub model_config: ModelConfig,
}

pub use model_config::ModelConfig;

impl ProviderConfig {
    pub(crate) fn base_url<'a>(&'a self, default: &'a str) -> &'a str {
        self.base_url
            .as_deref()
            .unwrap_or(default)
            .trim_end_matches('/')
    }
}

pub(crate) fn base64_image(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub(crate) fn image_data_url(media_type: &str, data: &[u8]) -> String {
    format!("data:{media_type};base64,{}", base64_image(data))
}

pub(crate) fn join_text_contents(contents: &[Content]) -> String {
    contents
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a provider content value for a user-shaped message body. `text_type`
/// is the provider's plain-text item name and `image_block` renders each image
/// item, because the three adapters differ in both names and shape.
pub(crate) fn content_value(
    contents: &[Content],
    text_type: &str,
    image_block: impl Fn(&str, &[u8]) -> Value,
) -> Value {
    if !contents
        .iter()
        .any(|content| matches!(content, Content::Image { .. }))
    {
        return json!(join_text_contents(contents));
    }

    json!(contents
        .iter()
        .map(|content| match content {
            Content::Text(text) => json!({"type": text_type, "text": text}),
            Content::Image { media_type, data } => image_block(media_type, data),
        })
        .collect::<Vec<_>>())
}

pub(crate) fn text_tool_result(output: &str, is_error: bool) -> Cow<'_, str> {
    if is_error {
        format!("Error: {output}").into()
    } else {
        output.into()
    }
}

/// Provider-neutral model client and stream vocabulary, re-exported for adapter code.
pub use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

#[must_use]
pub fn create_adapter(cfg: ProviderConfig) -> Arc<dyn ModelClient> {
    match cfg.protocol {
        Protocol::AnthropicMessages => Arc::new(anthropic::AnthropicAdapter::new(cfg)),
        Protocol::Completions => Arc::new(completions::CompletionsAdapter::new(cfg)),
        Protocol::Responses => Arc::new(responses::ResponsesAdapter::new(cfg)),
    }
}

pub(crate) fn context_turns(
    context: &ModelContext,
) -> impl Iterator<Item = (&Input, &[Arc<Step>])> {
    context
        .turns()
        .iter()
        .map(|turn| (&turn.input, turn.steps.as_slice()))
        .chain(context.current())
}

#[cfg(test)]
pub(crate) mod test_support {
    use ash_core::{
        Content, Input, Item, ModelContext, ModelId, ModelRequest, Step, ToolCall, ToolCallId,
        ToolOutput,
    };

    pub fn request(items: Vec<Item>) -> ModelRequest {
        ModelRequest {
            model: ModelId::new("test"),
            system: None,
            context: ModelContext::default()
                .with_current(Input::user("question"), vec![Step { items }.into()]),
            tools: Vec::new(),
            max_tokens: None,
        }
    }

    pub fn tool_result(id: &str, result: Result<ToolOutput, String>) -> Item {
        Item::ToolCall(ToolCall {
            id: ToolCallId::from_provider(id),
            name: "tool".to_string(),
            arguments: serde_json::json!({}),
            result,
        })
    }

    pub fn output(text: &str, attachments: Vec<Content>) -> ToolOutput {
        ToolOutput {
            text: text.to_string(),
            attachments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::Content;

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
    fn default_model_exists_only_for_anthropic() {
        assert_eq!(
            Protocol::AnthropicMessages.default_model(),
            Some(DEFAULT_ANTHROPIC_MODEL)
        );
        assert_eq!(Protocol::Completions.default_model(), None);
        assert_eq!(Protocol::Responses.default_model(), None);
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

    #[test]
    fn provider_base_url_uses_default_and_trims_trailing_slashes() {
        let default = ProviderConfig {
            protocol: Protocol::Completions,
            api_key: secrecy::SecretString::new("key".into()),
            base_url: None,
            model_config: ModelConfig::default(),
        };
        assert_eq!(
            default.base_url("https://api.openai.com"),
            "https://api.openai.com"
        );

        let configured = ProviderConfig {
            protocol: Protocol::Completions,
            api_key: secrecy::SecretString::new("key".into()),
            base_url: Some("https://example.com/v1/".to_string()),
            model_config: ModelConfig::default(),
        };
        assert_eq!(
            configured.base_url("https://api.openai.com"),
            "https://example.com/v1"
        );
    }

    #[test]
    fn image_helpers_share_one_base64_encoding() {
        assert_eq!(
            image_data_url("image/png", b"\x00\x01\x02"),
            "data:image/png;base64,AAEC"
        );
        assert_eq!(join_text_contents(&[Content::Text("one".into())]), "one");
        assert_eq!(
            join_text_contents(&[
                Content::Text("one".into()),
                Content::Image {
                    media_type: "image/png".into(),
                    data: vec![1],
                },
                Content::Text("two".into()),
            ]),
            "one\ntwo"
        );
    }
}
