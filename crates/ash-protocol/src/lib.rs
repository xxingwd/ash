mod anthropic;
mod completions;
mod model_config;
mod pending_calls;
mod responses;
mod sse;

use std::{borrow::Cow, sync::Arc};

use ash_core::{Content, Message, MessageContent, ProtocolError, Role, ToolCallId};
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

/// Default model for protocols that have a sensible built-in.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-20250514";

impl Protocol {
    /// Stable configuration name used by the CLI and `ASH_PROTOCOL`.
    pub const fn as_cli_name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::Completions => "openai",
            Self::Responses => "openai-responses",
        }
    }

    /// Built-in default model for this protocol, if one exists. Protocols
    /// without a default require an explicit `--model` or `ASH_MODEL`.
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

/// Provider-neutral model client and stream vocabulary, re-exported for adapter code.
pub use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

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
                let mut text = Vec::with_capacity(contents.len());
                for content in contents {
                    match content {
                        Content::Text(value) => text.push(value.as_str()),
                        Content::Image { .. } => {
                            return Err(ProtocolError::InvalidRequest(
                                "system messages cannot contain images".to_string(),
                            ));
                        }
                    }
                }
                system_parts.push(text.join("\n"));
            }
            (Role::User, MessageContent::User(_))
            | (Role::Assistant, MessageContent::Assistant(_))
            | (Role::User, MessageContent::ToolResult { .. }) => messages.push(message),
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

fn content_kind(content: &MessageContent) -> &'static str {
    match content {
        MessageContent::User(_) => "user",
        MessageContent::Assistant(_) => "assistant",
        MessageContent::ToolResult { .. } => "tool-result",
    }
}

struct ToolResultGroup<'a> {
    results: Vec<ToolResultRef<'a>>,
    attachments: Vec<Content>,
    next_index: usize,
}

struct ToolResultRef<'a> {
    id: &'a ToolCallId,
    output: Cow<'a, str>,
}

fn consecutive_tool_results<'a>(messages: &[&'a Message], start: usize) -> ToolResultGroup<'a> {
    let mut results = Vec::new();
    let mut attachments = Vec::new();
    let mut next_index = start;
    while let Some(message) = messages.get(next_index) {
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
        next_index += 1;
    }
    ToolResultGroup {
        results,
        attachments,
        next_index,
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
