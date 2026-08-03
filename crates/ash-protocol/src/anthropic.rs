use std::collections::HashMap;

use ash_core::{ContentBlock, MessageContent, ProtocolError, StopReason, ToolCallId, Usage};
use base64::Engine;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{model_config, sse, ProviderConfig};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

pub struct AnthropicAdapter {
    config: ProviderConfig,
    client: Client,
}

impl AnthropicAdapter {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    fn base_url(&self) -> &str {
        self.config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/')
    }

    fn build_request(&self, req: &ModelRequest) -> Result<Value, ProtocolError> {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .filter_map(|msg| match &msg.content {
                MessageContent::User(contents) => {
                    let content: Vec<Value> = contents
                        .iter()
                        .map(|content| match content {
                            ash_core::Content::Text(text) => {
                                json!({"type": "text", "text": text})
                            }
                            ash_core::Content::Image { media_type, data } => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                                }
                            }),
                        })
                        .collect();
                    Some(json!({"role": "user", "content": content}))
                }
                MessageContent::Assistant(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(json!({"type": "text", "text": text})),
                            ContentBlock::Thought { .. } => None,
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => Some(json!({
                                "type": "tool_use",
                                "id": id.as_str(),
                                "name": name,
                                "input": arguments,
                            })),
                        })
                        .collect();
                    (!content.is_empty()).then(|| json!({"role": "assistant", "content": content}))
                }
                MessageContent::ToolResult {
                    id,
                    result,
                    attachments,
                } => {
                    let (text, is_error) = match result {
                        Ok(output) => (output, false),
                        Err(error) => (error, true),
                    };
                    let mut content = vec![json!({"type": "text", "text": text})];
                    content.extend(attachments.iter().map(|attachment| match attachment {
                        ash_core::Content::Text(text) => json!({"type": "text", "text": text}),
                        ash_core::Content::Image { media_type, data } => json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": base64::engine::general_purpose::STANDARD.encode(data),
                            }
                        }),
                    }));
                    Some(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": id.as_str(),
                            "content": content,
                            "is_error": is_error,
                        }]
                    }))
                }
            })
            .collect();

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": req.model.as_str(),
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        });
        if let Some(system) = &req.system {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        model_config::apply_from_env(&mut body)?;
        Ok(body)
    }
}

impl ModelClient for AnthropicAdapter {
    fn stream(&self, req: ModelRequest) -> Result<ModelStream, ProtocolError> {
        let body = self.build_request(&req)?;
        let request = self
            .client
            .post(format!("{}/v1/messages", self.base_url()))
            .header("x-api-key", self.config.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        sse::stream(request, AnthropicDecoder::default())
    }
}

#[derive(Default)]
struct AnthropicDecoder {
    calls: HashMap<u64, PendingCall>,
    stop: Option<StopReason>,
}

struct PendingCall {
    id: ToolCallId,
    name: String,
    arguments: String,
}

impl sse::Decoder for AnthropicDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        let mut finished = false;

        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                if let Some(usage) = event["message"].get("usage") {
                    items.push(ModelEvent::Usage(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        generation_ms: 0,
                        estimated: false,
                    }));
                }
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let index = event["index"].as_u64().unwrap_or(0);
                    let id = block["id"]
                        .as_str()
                        .map(ToolCallId::from_provider)
                        .unwrap_or_default();
                    self.calls.insert(
                        index,
                        PendingCall {
                            id,
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            arguments: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            items.push(ModelEvent::Text(text.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str() {
                            items.push(ModelEvent::Reasoning(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        if let (Some(call), Some(partial)) =
                            (self.calls.get_mut(&index), delta["partial_json"].as_str())
                        {
                            call.arguments.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().unwrap_or(0);
                if let Some(call) = self.calls.remove(&index) {
                    let arguments = if call.arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&call.arguments).map_err(|error| {
                            ProtocolError::InvalidResponse(format!(
                                "invalid Anthropic tool arguments: {error}"
                            ))
                        })?
                    };
                    items.push(ModelEvent::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments,
                    });
                }
            }
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    items.push(ModelEvent::Usage(Usage {
                        input_tokens: 0,
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        generation_ms: 0,
                        estimated: false,
                    }));
                }
                self.stop = Some(match event["delta"]["stop_reason"].as_str() {
                    Some("max_tokens") => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                });
            }
            "message_stop" => {
                items.push(ModelEvent::Stop(
                    self.stop.take().unwrap_or(StopReason::EndTurn),
                ));
                finished = true;
            }
            "error" => {
                return Err(ProtocolError::InvalidResponse(
                    event["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic stream error")
                        .to_string(),
                ));
            }
            _ => {}
        }

        Ok(if finished {
            sse::DecodeResult::finished(items)
        } else {
            sse::DecodeResult::continuing(items)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, Protocol};
    use ash_core::{Content, ContentBlock, Message, MessageContent, MessageId, ModelId, Role};
    use secrecy::SecretString;

    #[test]
    fn aggregates_tool_arguments_and_preserves_provider_id() {
        let mut decoder = AnthropicDecoder::default();
        decoder
            .decode(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"read"}}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"README.md\"}"}}"#,
            )
            .unwrap();
        let items = decoder
            .decode(r#"{"type":"content_block_stop","index":1}"#)
            .unwrap()
            .into_items();

        assert_eq!(
            items,
            vec![ModelEvent::ToolCall {
                id: ToolCallId::from_provider("toolu_123"),
                name: "read".into(),
                arguments: json!({"path": "README.md"}),
            }]
        );
    }

    #[test]
    fn message_stop_finishes_the_decoder() {
        let mut decoder = AnthropicDecoder::default();

        let result = decoder.decode(r#"{"type":"message_stop"}"#).unwrap();

        assert!(matches!(result, sse::DecodeResult::Finished(_)));
    }

    #[test]
    fn uses_the_default_output_limit() {
        let adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn honors_an_explicit_output_limit() {
        let adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: Some(1_024),
        };

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(body["max_tokens"], 1_024);
    }

    #[test]
    fn omits_persisted_thoughts_from_anthropic_history() {
        let adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message {
                id: MessageId::new(),
                role: Role::Assistant,
                content: MessageContent::Assistant(vec![
                    ContentBlock::Thought {
                        text: "private reasoning".into(),
                        elapsed_seconds: 2,
                    },
                    ContentBlock::Text("visible answer".into()),
                ]),
            }],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(body["messages"][0]["content"][0]["text"], "visible answer");
        assert!(!body.to_string().contains("private reasoning"));
    }

    #[test]
    fn sends_tool_images_inside_the_anthropic_tool_result() {
        let adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message {
                id: MessageId::new(),
                role: Role::User,
                content: MessageContent::ToolResult {
                    id: ToolCallId::from_provider("call"),
                    result: Ok("Read image file [image/png]".into()),
                    attachments: vec![Content::Image {
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    }],
                },
            }],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["type"],
            "image"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["source"]["data"],
            "AQID"
        );
    }
}
