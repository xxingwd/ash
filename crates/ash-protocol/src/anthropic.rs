use ash_core::{ContentBlock, ProtocolError, StopReason};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    base64_image, model_config,
    pending_calls::stop_reason,
    pending_calls::{build_usage, PendingCall, PendingCallAccumulator},
    project_request_messages, sse, MessageGroup, MessageGroupIter, ProviderConfig,
};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

fn anthropic_image_block(media_type: &str, data: &[u8]) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": base64_image(data),
        }
    })
}

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
        self.config.base_url("https://api.anthropic.com")
    }

    fn build_request(req: &ModelRequest) -> Result<Value, ProtocolError> {
        let projected = project_request_messages(req)?;
        let mut messages = Vec::new();
        for group in MessageGroupIter::new(&projected.messages) {
            match group {
                MessageGroup::User(contents) => {
                    messages.push(json!({
                        "role": "user",
                        "content": contents
                            .iter()
                            .map(|content| match content {
                                ash_core::Content::Text(text) => {
                                    json!({"type": "text", "text": text})
                                }
                                ash_core::Content::Image { media_type, data } => {
                                    anthropic_image_block(media_type, data)
                                }
                            })
                            .collect::<Vec<Value>>(),
                    }));
                }
                MessageGroup::Assistant(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Text(text) => json!({"type": "text", "text": text}),
                            ContentBlock::Thought { text, .. } => json!({
                                "type": "thinking",
                                "thinking": text,
                            }),
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => json!({
                                "type": "tool_use",
                                "id": id.as_str(),
                                "name": name,
                                "input": arguments,
                            }),
                        })
                        .collect();
                    if !content.is_empty() {
                        messages.push(json!({"role": "assistant", "content": content}));
                    }
                }
                MessageGroup::ToolResults(group) => {
                    let content: Vec<Value> = group
                        .results
                        .into_iter()
                        .map(|result| {
                            let mut content =
                                vec![json!({"type": "text", "text": result.output.as_ref()})];
                            content.extend(result.attachments.iter().map(|attachment| {
                                match attachment {
                                    ash_core::Content::Text(text) => {
                                        json!({"type": "text", "text": text})
                                    }
                                    ash_core::Content::Image { media_type, data } => {
                                        anthropic_image_block(media_type, data)
                                    }
                                }
                            }));
                            json!({
                                "type": "tool_result",
                                "tool_use_id": result.id.as_str(),
                                "content": content,
                                "is_error": result.is_error,
                            })
                        })
                        .collect();
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }

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
        let body = Self::build_request(&req)?;
        let request = self
            .client
            .post(format!("{}/v1/messages", self.base_url()))
            .header("x-api-key", self.config.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        Ok(sse::stream(request, AnthropicDecoder::default()))
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
            "message_start" => Self::message_start(&event, &mut items),
            "content_block_start" => self.content_block_start(&event)?,
            "content_block_delta" => self.content_block_delta(&event, &mut items)?,
            "content_block_stop" => self.content_block_stop(&event, &mut items)?,
            "message_delta" => self.message_delta(&event, &mut items),
            "message_stop" => return self.message_stop(&mut items),
            "error" => return Err(Self::stream_error_message(&event)),
            _ => {}
        }
        Ok(sse::DecodeResult::continuing(items))
    }
}

impl AnthropicDecoder {
    fn message_start(event: &Value, items: &mut Vec<ModelEvent>) {
        if let Some(usage) = event["message"].get("usage") {
            items.push(ModelEvent::Usage(build_usage(
                usage["input_tokens"].as_u64().unwrap_or(0),
                usage["output_tokens"].as_u64().unwrap_or(0),
            )));
        }
    }

    fn content_block_start(&mut self, event: &Value) -> Result<(), ProtocolError> {
        let block = &event["content_block"];
        if block["type"].as_str() == Some("tool_use") {
            let index = Self::block_index(event)?;
            let id = block["id"].as_str().ok_or_else(|| {
                ProtocolError::InvalidResponse("Anthropic tool-use block is missing id".to_string())
            })?;
            let name = block["name"].as_str().ok_or_else(|| {
                ProtocolError::InvalidResponse(
                    "Anthropic tool-use block is missing name".to_string(),
                )
            })?;
            self.calls.insert(index, PendingCall::new(id, name));
        }
        Ok(())
    }

    fn content_block_delta(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
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
                let index = Self::block_index(event)?;
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
                call.append_arguments(partial);
            }
            _ => {}
        }
        Ok(())
    }

    fn content_block_stop(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
        let index = Self::block_index(event)?;
        if let Some(call) = self.calls.remove(&index) {
            items.push(call.finish("Anthropic")?);
        }
        Ok(())
    }

    fn message_delta(&mut self, event: &Value, items: &mut Vec<ModelEvent>) {
        if let Some(usage) = event.get("usage") {
            items.push(ModelEvent::Usage(build_usage(
                0,
                usage["output_tokens"].as_u64().unwrap_or(0),
            )));
        }
        self.stop = Some(stop_reason(
            event["delta"]["stop_reason"].as_str().unwrap_or(""),
        ));
    }

    fn message_stop(
        &mut self,
        items: &mut Vec<ModelEvent>,
    ) -> Result<sse::DecodeResult, ProtocolError> {
        if !self.calls.is_empty() {
            return Err(ProtocolError::InvalidResponse(
                "Anthropic message stopped with an unfinished tool call".to_string(),
            ));
        }
        items.push(ModelEvent::Stop(
            self.stop.take().unwrap_or(StopReason::EndTurn),
        ));
        Ok(sse::DecodeResult::finished(std::mem::take(items)))
    }

    fn stream_error_message(event: &Value) -> ProtocolError {
        ProtocolError::InvalidResponse(
            event["error"]["message"]
                .as_str()
                .unwrap_or("Anthropic stream error")
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, Protocol};
    use ash_core::{Content, ContentBlock, Message, ModelId, ToolCallId};
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
        let _adapter = AnthropicAdapter::new(ProviderConfig {
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

        let body = AnthropicAdapter::build_request(&request).unwrap();

        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn honors_an_explicit_output_limit() {
        let _adapter = AnthropicAdapter::new(ProviderConfig {
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

        let body = AnthropicAdapter::build_request(&request).unwrap();

        assert_eq!(body["max_tokens"], 1_024);
    }

    #[test]
    fn includes_persisted_thoughts_in_anthropic_history() {
        let _adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message::assistant(vec![
                ContentBlock::Thought {
                    text: "private reasoning".into(),
                    elapsed_seconds: 2,
                },
                ContentBlock::Text("visible answer".into()),
            ])],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = AnthropicAdapter::build_request(&request).unwrap();

        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "private reasoning");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "visible answer");
    }

    #[test]
    fn sends_tool_images_inside_the_anthropic_tool_result() {
        let _adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message::tool_result(
                ToolCallId::from_provider("call"),
                Ok("Read image file [image/png]".into()),
                vec![Content::Image {
                    media_type: "image/png".into(),
                    data: vec![1, 2, 3],
                }],
            )],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = AnthropicAdapter::build_request(&request).unwrap();

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
