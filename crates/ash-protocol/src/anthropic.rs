use ash_core::{ContentBlock, MessageContent, ProtocolError, StopReason};
use base64::Engine;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    model_config,
    pending_calls::stop_reason,
    pending_calls::{build_usage, PendingCall, PendingCallAccumulator},
    project_request_messages, sse, ProviderConfig,
};
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
        let projected = project_request_messages(req)?;
        let messages: Vec<Value> = projected
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
        if let Some(system) = &projected.system {
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
    calls: PendingCallAccumulator<u64>,
    stop: Option<StopReason>,
}

impl AnthropicDecoder {
    fn block_index(event: &Value) -> Result<u64, ProtocolError> {
        event["index"].as_u64().ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Anthropic content-block event is missing index".to_string(),
            )
        })
    }
}

impl sse::Decoder for AnthropicDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        let event_type = event["type"].as_str().ok_or_else(|| {
            ProtocolError::InvalidResponse("Anthropic event is missing type".to_string())
        })?;
        match event_type {
            "message_start" => {
                if let Some(usage) = event["message"].get("usage") {
                    items.push(ModelEvent::Usage(build_usage(
                        usage["input_tokens"].as_u64().unwrap_or(0),
                        usage["output_tokens"].as_u64().unwrap_or(0),
                    )));
                }
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let index = Self::block_index(&event)?;
                    let id = block["id"].as_str().ok_or_else(|| {
                        ProtocolError::InvalidResponse(
                            "Anthropic tool-use block is missing id".to_string(),
                        )
                    })?;
                    let name = block["name"].as_str().ok_or_else(|| {
                        ProtocolError::InvalidResponse(
                            "Anthropic tool-use block is missing name".to_string(),
                        )
                    })?;
                    self.calls.insert(index, PendingCall::new(id, name));
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
                        let index = Self::block_index(&event)?;
                        let partial = delta["partial_json"].as_str().ok_or_else(|| {
                            ProtocolError::InvalidResponse(
                                "Anthropic tool-input delta is missing partial_json".to_string(),
                            )
                        })?;
                        let call = self.calls.get_mut(&index).ok_or_else(|| {
                            ProtocolError::InvalidResponse(format!(
                                "Anthropic tool-input delta references unknown block {index}"
                            ))
                        })?;
                        call.arguments.push_str(partial);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = Self::block_index(&event)?;
                if let Some(call) = self.calls.remove(&index) {
                    items.push(call.finish("Anthropic")?);
                }
            }
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    items.push(ModelEvent::Usage(build_usage(
                        0,
                        usage["output_tokens"].as_u64().unwrap_or(0),
                    )));
                }
                self.stop = Some(stop_reason(
                    event["delta"]["stop_reason"].as_str() == Some("max_tokens"),
                ));
            }
            "message_stop" => {
                if !self.calls.is_empty() {
                    return Err(ProtocolError::InvalidResponse(
                        "Anthropic message stopped with an unfinished tool call".to_string(),
                    ));
                }
                items.push(ModelEvent::Stop(
                    self.stop.take().unwrap_or(StopReason::EndTurn),
                ));
                return Ok(sse::DecodeResult::finished(items));
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

        Ok(sse::DecodeResult::continuing(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, Protocol};
    use ash_core::{
        Content, ContentBlock, Message, MessageContent, MessageId, ModelId, Role, ToolCallId,
    };
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
    fn rejects_tool_use_without_a_block_index() {
        let mut decoder = AnthropicDecoder::default();

        let result = decoder.decode(
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_123","name":"read"}}"#,
        );

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn rejects_message_stop_with_an_unfinished_tool_call() {
        let mut decoder = AnthropicDecoder::default();
        decoder
            .decode(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"read"}}"#,
            )
            .unwrap();

        let result = decoder.decode(r#"{"type":"message_stop"}"#);

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
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
