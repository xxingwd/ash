mod anthropic;
mod completions;
mod model_config;
mod pending_calls;
mod responses;
mod sse;

use std::{borrow::Cow, sync::Arc};

use ash_core::{Content, ContentBlock, Message, MessageContent, ProtocolError, Role, ToolCallId};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    /// `OpenAI` Chat Completions API.
    #[serde(rename = "openai")]
    #[strum(serialize = "openai")]
    Completions,
    /// `OpenAI` Responses API.
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

/// Default model for protocols that have a sensible built-in.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-20250514";

impl Protocol {
    /// Stable configuration name used by the CLI and `ASH_PROTOCOL`.
    #[must_use]
    pub const fn as_cli_name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::Completions => "openai",
            Self::Responses => "openai-responses",
        }
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
}

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

struct ProjectedMessages<'a> {
    system: Option<String>,
    messages: Vec<&'a Message>,
}

/// Validate the provider-neutral role/content pair and lift system messages to
/// the provider's privileged instruction field. `MessageContent` predates a
/// dedicated system variant, so persisted system messages intentionally carry
/// user-shaped text content.
fn project_request_messages(
    request: &ModelRequest,
) -> Result<ProjectedMessages<'_>, ProtocolError> {
    let mut system_parts = request.system.iter().cloned().collect::<Vec<_>>();
    let mut messages = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        match (&message.role, &message.content) {
            (Role::System, MessageContent::User(contents)) => {
                let text = contents
                    .iter()
                    .map(|content| match content {
                        Content::Text(value) => Ok(value.as_str()),
                        Content::Image { .. } => Err(ProtocolError::InvalidRequest(
                            "system messages cannot contain images".to_string(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("\n");
                system_parts.push(text);
            }
            (Role::User, MessageContent::User(_) | MessageContent::ToolResult { .. })
            | (Role::Assistant, MessageContent::Assistant(_)) => messages.push(message),
            (role, content) => {
                return Err(ProtocolError::InvalidRequest(format!(
                    "message role {role:?} does not match {} content",
                    content_kind(content)
                )));
            }
        }
    }

    Ok(ProjectedMessages {
        system: (!system_parts.is_empty()).then(|| system_parts.join("\n\n")),
        messages,
    })
}

const fn content_kind(content: &MessageContent) -> &'static str {
    match content {
        MessageContent::User(_) => "user",
        MessageContent::Assistant(_) => "assistant",
        MessageContent::ToolResult { .. } => "tool-result",
    }
}

/// A maximal run of consecutive tool-result messages grouped for one adapter
/// request, so providers that key results to calls can emit them as a block.
pub(crate) struct ToolResultGroup<'a> {
    pub(crate) results: Vec<ToolResultRef<'a>>,
    pub(crate) attachments: Vec<Content>,
}

pub(crate) struct ToolResultRef<'a> {
    pub(crate) id: &'a ToolCallId,
    pub(crate) output: Cow<'a, str>,
}

fn consecutive_tool_results<'a>(messages: &[&'a Message], start: usize) -> ToolResultGroup<'a> {
    let mut results = Vec::new();
    let mut attachments = Vec::new();
    let mut index = start;
    while let Some(message) = messages.get(index) {
        let MessageContent::ToolResult {
            id,
            result,
            attachments: result_attachments,
        } = &message.content
        else {
            break;
        };
        let output = match result {
            Ok(output) => Cow::Borrowed(output.as_str()),
            Err(error) => Cow::Owned(format!("Error: {error}")),
        };
        results.push(ToolResultRef { id, output });
        attachments.extend(result_attachments.iter().cloned());
        index += 1;
    }
    ToolResultGroup {
        results,
        attachments,
    }
}

/// One request-building step over the projected history: a user turn, an
/// assistant turn, or a maximal run of tool results.
pub(crate) enum MessageGroup<'a> {
    User(&'a [Content]),
    Assistant(&'a [ContentBlock]),
    ToolResults(ToolResultGroup<'a>),
}

/// Iterates `MessageGroup`s over the projected messages, merging consecutive
/// tool-result messages into a single group. Both request-building adapters
/// share this walk so their per-message grouping never diverges.
pub(crate) struct MessageGroupIter<'a> {
    messages: &'a [&'a Message],
    index: usize,
}

impl<'a> MessageGroupIter<'a> {
    pub(crate) const fn new(messages: &'a [&'a Message]) -> Self {
        Self { messages, index: 0 }
    }
}

impl<'a> Iterator for MessageGroupIter<'a> {
    type Item = MessageGroup<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let message = *self.messages.get(self.index)?;
        let group = match &message.content {
            MessageContent::User(contents) => MessageGroup::User(contents.as_slice()),
            MessageContent::Assistant(blocks) => MessageGroup::Assistant(blocks.as_slice()),
            MessageContent::ToolResult { .. } => {
                let group = consecutive_tool_results(self.messages, self.index);
                self.index += group.results.len();
                MessageGroup::ToolResults(group)
            }
        };
        if !matches!(group, MessageGroup::ToolResults(_)) {
            self.index += 1;
        }
        Some(group)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use ash_core::{Content, Message, MessageContent, MessageId, Role, ToolCallId};

    /// A provider-neutral tool-result message shared by the adapter tests.
    pub fn tool_result(text: &str, attachments: Vec<Content>) -> Message {
        Message {
            id: MessageId::new(),
            role: Role::User,
            content: MessageContent::ToolResult {
                id: ToolCallId::new(),
                result: Ok(text.into()),
                attachments,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{ContentBlock, MessageId, ModelId};

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
        };
        assert_eq!(
            default.base_url("https://api.openai.com"),
            "https://api.openai.com"
        );

        let configured = ProviderConfig {
            protocol: Protocol::Completions,
            api_key: secrecy::SecretString::new("key".into()),
            base_url: Some("https://example.com/v1/".to_string()),
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

    #[test]
    fn request_projection_lifts_system_messages_without_reordering_history() {
        let user = Message::user("question");
        let assistant = Message::assistant_text("answer");
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: Some("base rules".to_string()),
            messages: vec![
                user.clone(),
                Message::system("extension rules"),
                assistant.clone(),
            ],
            tools: Vec::new(),
            max_tokens: None,
        };

        let projected = project_request_messages(&request).unwrap();

        assert_eq!(
            projected.system.as_deref(),
            Some("base rules\n\nextension rules")
        );
        assert_eq!(projected.messages, vec![&user, &assistant]);
    }

    #[test]
    fn request_projection_rejects_role_content_mismatches() {
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::Assistant(vec![ContentBlock::Text("answer".into())]),
            }],
            tools: Vec::new(),
            max_tokens: None,
        };

        assert!(matches!(
            project_request_messages(&request),
            Err(ProtocolError::InvalidRequest(_))
        ));
    }
}
