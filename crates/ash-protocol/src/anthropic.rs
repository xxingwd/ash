use std::collections::BTreeMap;

use ash_core::{Item, ProtocolError, Step, StopReason};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    base64_image, context_turns, model_config,
    pending_calls::{usage_event, PendingCall},
    sse, ProviderConfig,
};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
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

    fn build_request(req: &ModelRequest) -> Result<Value, ProtocolError> {
        let mut messages = Vec::new();
        if let Some(summary) = req.context.summary() {
            messages.push(json!({
                "role": "assistant",
                "content": [{"type": "text", "text": summary}],
            }));
        }
        for (input, steps) in context_turns(&req.context) {
            messages.push(json!({
                "role": "user",
                "content": input.content.iter().map(|content| match content {
                    ash_core::Content::Text(text) => json!({"type": "text", "text": text}),
                    ash_core::Content::Image { media_type, data } => {
                        anthropic_image_block(media_type, data)
                    }
                }).collect::<Vec<Value>>(),
            }));
            for step in steps {
                push_step(&mut messages, step);
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

fn push_step(messages: &mut Vec<Value>, step: &Step) {
    let assistant = step
        .items
        .iter()
        .map(|item| match item {
            Item::Text(text) => json!({"type": "text", "text": text}),
            Item::Thought { text, .. } => json!({"type": "thinking", "thinking": text}),
            Item::ToolCall(call) => json!({
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": call.name,
                "input": call.arguments,
            }),
        })
        .collect::<Vec<_>>();
    if !assistant.is_empty() {
        messages.push(json!({"role": "assistant", "content": assistant}));
    }

    let results = step
        .items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall(call) => Some(call),
            Item::Text(_) | Item::Thought { .. } => None,
        })
        .map(|call| {
            let (text, attachments, is_error) = match &call.result {
                Ok(output) => (output.text.as_str(), output.attachments.as_slice(), false),
                Err(error) => (error.as_str(), &[][..], true),
            };
            let mut content = vec![json!({"type": "text", "text": text})];
            content.extend(attachments.iter().map(|attachment| match attachment {
                ash_core::Content::Text(text) => json!({"type": "text", "text": text}),
                ash_core::Content::Image { media_type, data } => {
                    anthropic_image_block(media_type, data)
                }
            }));
            json!({
                "type": "tool_result",
                "tool_use_id": call.id.as_str(),
                "content": content,
                "is_error": is_error,
            })
        })
        .collect::<Vec<_>>();
    if !results.is_empty() {
        messages.push(json!({"role": "user", "content": results}));
    }
}

impl ModelClient for AnthropicAdapter {
    fn stream(&self, req: ModelRequest) -> Result<ModelStream, ProtocolError> {
        let body = Self::build_request(&req)?;
        let request = self
            .client
            .post(format!(
                "{}/v1/messages",
                self.config.base_url(DEFAULT_BASE_URL)
            ))
            .header("x-api-key", self.config.api_key.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        Ok(sse::stream(request, AnthropicDecoder::default()))
    }
}

#[derive(Default)]
pub(crate) struct AnthropicDecoder {
    calls: BTreeMap<u64, PendingCall>,
    /// Stop reason announced by `message_delta`; not yet a terminal state.
    announced_stop: Option<StopReason>,
    /// Stop reason confirmed by `message_stop`; drives `finalize`.
    completed_stop: Option<StopReason>,
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
            "message_stop" => {
                self.message_stop()?;
                return Ok(sse::DecodeResult::Close(items));
            }
            "error" => return Err(Self::stream_error(&event)),
            _ => {}
        }
        Ok(sse::DecodeResult::Continue(items))
    }

    fn finalize(self) -> Result<(Vec<ModelEvent>, Option<StopReason>), ProtocolError> {
        Ok((Vec::new(), self.completed_stop))
    }
}

impl AnthropicDecoder {
    fn message_start(event: &Value, items: &mut Vec<ModelEvent>) {
        if let Some(usage) = event["message"].get("usage") {
            items.push(usage_event(
                usage["input_tokens"].as_u64().unwrap_or(0),
                usage["output_tokens"].as_u64().unwrap_or(0),
            ));
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
            items.push(usage_event(0, usage["output_tokens"].as_u64().unwrap_or(0)));
        }
        if let Some(reason) = event["delta"]["stop_reason"]
            .as_str()
            .filter(|reason| !reason.trim().is_empty())
        {
            self.announced_stop = Some(match reason {
                "end_turn" | "tool_use" | "stop_sequence" => StopReason::EndTurn,
                "max_tokens" => StopReason::MaxTokens,
                other => StopReason::Other(other.to_string()),
            });
        }
    }

    fn message_stop(&mut self) -> Result<(), ProtocolError> {
        if !self.calls.is_empty() {
            return Err(ProtocolError::InvalidResponse(
                "Anthropic message stopped with an unfinished tool call".to_string(),
            ));
        }
        let stop = self.announced_stop.take().ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Anthropic message stopped without a stop reason".to_string(),
            )
        })?;
        self.completed_stop = Some(stop);
        Ok(())
    }

    fn stream_error(event: &Value) -> ProtocolError {
        let error = &event["error"];
        let error_type = error["type"].as_str();
        let message = error["message"]
            .as_str()
            .unwrap_or("Anthropic stream error");
        match error_type {
            Some("authentication_error" | "permission_error") => ProtocolError::Auth {
                message: message.to_string(),
            },
            Some("rate_limit_error") => ProtocolError::RateLimited {
                message: message.to_string(),
            },
            Some("api_error") => ProtocolError::Upstream {
                status: 500,
                message: message.to_string(),
            },
            Some("overloaded_error") => ProtocolError::Upstream {
                status: 529,
                message: message.to_string(),
            },
            Some(
                error_type @ ("invalid_request_error"
                | "request_too_large"
                | "not_found_error"
                | "billing_error"),
            ) => ProtocolError::InvalidRequest(format!("{error_type}: {message}")),
            Some(error_type) => ProtocolError::InvalidResponse(format!("{error_type}: {message}")),
            None => ProtocolError::InvalidResponse(message.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, test_support, Protocol};
    use ash_core::{Content, Item, ToolCallId};
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
        let result = decoder
            .decode(r#"{"type":"content_block_stop","index":1}"#)
            .unwrap();
        let sse::DecodeResult::Continue(items) = result else {
            panic!("expected the stream to continue");
        };

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
    fn rejects_message_stop_without_a_semantic_stop_reason() {
        let mut decoder = AnthropicDecoder::default();

        let result = decoder.decode(r#"{"type":"message_stop"}"#);

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn message_stop_finishes_after_a_semantic_stop_reason() {
        let mut decoder = AnthropicDecoder::default();
        decoder
            .decode(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            )
            .unwrap();

        let result = decoder.decode(r#"{"type":"message_stop"}"#).unwrap();

        assert!(matches!(result, sse::DecodeResult::Close(items) if items.is_empty()));
        assert_eq!(decoder.finalize().unwrap().1, Some(StopReason::EndTurn));
    }

    #[test]
    fn preserves_unknown_stop_reason() {
        let mut decoder = AnthropicDecoder::default();
        decoder
            .decode(
                r#"{"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"}}"#,
            )
            .unwrap();
        decoder.decode(r#"{"type":"message_stop"}"#).unwrap();

        assert_eq!(
            decoder.finalize().unwrap().1,
            Some(StopReason::Other(
                "model_context_window_exceeded".to_string()
            ))
        );
    }

    #[test]
    fn maps_max_tokens_stop_reason() {
        let mut decoder = AnthropicDecoder::default();
        decoder
            .decode(r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#)
            .unwrap();
        decoder.decode(r#"{"type":"message_stop"}"#).unwrap();

        assert_eq!(decoder.finalize().unwrap().1, Some(StopReason::MaxTokens));
    }

    #[test]
    fn maps_overload_and_rate_limit_stream_errors_to_retryable_categories() {
        let overloaded = AnthropicDecoder::default().decode(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        let rate_limited = AnthropicDecoder::default().decode(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down"}}"#,
        );

        assert!(matches!(
            overloaded,
            Err(ProtocolError::Upstream { status: 529, message }) if message == "Overloaded"
        ));
        assert!(matches!(
            rate_limited,
            Err(ProtocolError::RateLimited { message }) if message == "Slow down"
        ));
    }

    #[test]
    fn maps_non_retryable_stream_errors_without_losing_details() {
        let invalid = AnthropicDecoder::default().decode(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Bad request"}}"#,
        );
        let unknown = AnthropicDecoder::default()
            .decode(r#"{"type":"error","error":{"type":"new_error","message":"New failure"}}"#);

        assert!(matches!(
            invalid,
            Err(ProtocolError::InvalidRequest(message))
                if message == "invalid_request_error: Bad request"
        ));
        assert!(matches!(
            unknown,
            Err(ProtocolError::InvalidResponse(message)) if message == "new_error: New failure"
        ));
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
        let request = test_support::request(Vec::new());

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
        let mut request = test_support::request(Vec::new());
        request.max_tokens = Some(1_024);

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
        let request = test_support::request(vec![
            Item::Thought {
                text: "private reasoning".into(),
                elapsed_seconds: 2,
            },
            Item::Text("visible answer".into()),
        ]);

        let body = AnthropicAdapter::build_request(&request).unwrap();

        let content = &body["messages"][1]["content"];
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "private reasoning");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "visible answer");
    }

    #[test]
    fn groups_consecutive_tool_results_into_one_user_message() {
        let request = test_support::request(vec![
            test_support::tool_result("call_1", Ok(test_support::output("first", Vec::new()))),
            test_support::tool_result("call_2", Ok(test_support::output("second", Vec::new()))),
        ]);

        let body = AnthropicAdapter::build_request(&request).unwrap();

        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(body["messages"][2]["content"][1]["tool_use_id"], "call_2");
    }

    #[test]
    fn sends_raw_tool_error_with_is_error() {
        let request = test_support::request(vec![test_support::tool_result(
            "call",
            Err("permission denied".into()),
        )]);

        let body = AnthropicAdapter::build_request(&request).unwrap();
        let result = &body["messages"][2]["content"][0];

        assert_eq!(result["content"][0]["text"], "permission denied");
        assert_eq!(result["is_error"], true);
    }

    #[test]
    fn sends_tool_images_inside_the_anthropic_tool_result() {
        let _adapter = AnthropicAdapter::new(ProviderConfig {
            protocol: Protocol::AnthropicMessages,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = test_support::request(vec![test_support::tool_result(
            "call",
            Ok(test_support::output(
                "Read image file [image/png]",
                vec![Content::Image {
                    media_type: "image/png".into(),
                    data: vec![1, 2, 3],
                }],
            )),
        )]);

        let body = AnthropicAdapter::build_request(&request).unwrap();

        assert_eq!(
            body["messages"][2]["content"][0]["content"][1]["type"],
            "image"
        );
        assert_eq!(
            body["messages"][2]["content"][0]["content"][1]["source"]["data"],
            "AQID"
        );
    }
}
