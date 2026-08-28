use std::collections::BTreeMap;

use ash_core::{ContentBlock, ProtocolError, StopReason};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    content_value, image_data_url, message_groups, model_config,
    pending_calls::{build_usage, PendingCall},
    project_request_messages, sse, text_tool_result, MessageGroup, ProviderConfig,
};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

pub struct CompletionsAdapter {
    config: ProviderConfig,
    client: Client,
}

impl CompletionsAdapter {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    fn build_request(req: &ModelRequest) -> Result<Value, ProtocolError> {
        let projected = project_request_messages(req)?;
        let mut messages = Vec::new();
        if let Some(system) = &projected.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        for group in message_groups(&projected.messages) {
            match group {
                MessageGroup::User(contents) => {
                    messages.push(json!({"role": "user", "content": chat_content(contents)}));
                }
                MessageGroup::Assistant(blocks) => {
                    let mut text = String::new();
                    let mut reasoning = String::new();
                    let mut calls = Vec::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Text(t) => text.push_str(t),
                            ContentBlock::Thought { text: r, .. } => reasoning.push_str(r),
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => calls.push(json!({
                                "id": id.as_str(),
                                "type": "function",
                                "function": {"name": name, "arguments": arguments.to_string()},
                            })),
                        }
                    }
                    let mut value = json!({"role": "assistant", "content": text});
                    if !reasoning.is_empty() {
                        value["reasoning_content"] = json!(reasoning);
                    }
                    if !calls.is_empty() {
                        value["tool_calls"] = json!(calls);
                    }
                    if !text.is_empty() || !reasoning.is_empty() || !calls.is_empty() {
                        messages.push(value);
                    }
                }
                MessageGroup::ToolResults(group) => {
                    for result in &group.results {
                        let output = text_tool_result(result.output, result.is_error);
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": result.id.as_str(),
                            "content": output,
                        }));
                    }
                    let attachments = group.attachments().collect::<Vec<_>>();
                    if !attachments.is_empty() {
                        messages.push(json!({
                            "role": "user",
                            "content": chat_attachments(&attachments),
                        }));
                    }
                }
            }
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters_schema,
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": req.model.as_str(),
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        model_config::apply_from_env(&mut body)?;
        Ok(body)
    }
}

fn chat_attachments(contents: &[&ash_core::Content]) -> Value {
    Value::Array(
        contents
            .iter()
            .map(|content| match content {
                ash_core::Content::Text(text) => json!({"type": "text", "text": text}),
                ash_core::Content::Image { media_type, data } => json!({
                    "type": "image_url",
                    "image_url": {
                        "url": image_data_url(media_type, data),
                    }
                }),
            })
            .collect(),
    )
}

fn chat_content(contents: &[ash_core::Content]) -> Value {
    content_value(contents, "text", |media_type, data| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": image_data_url(media_type, data),
            }
        })
    })
}

impl ModelClient for CompletionsAdapter {
    fn stream(&self, req: ModelRequest) -> Result<ModelStream, ProtocolError> {
        let body = Self::build_request(&req)?;
        let request = self
            .client
            .post(format!(
                "{}/v1/chat/completions",
                self.config.base_url(DEFAULT_BASE_URL)
            ))
            .bearer_auth(self.config.api_key.expose_secret())
            .json(&body);
        Ok(sse::stream(request, CompletionsDecoder::default()))
    }
}

#[derive(Default)]
pub struct CompletionsDecoder {
    calls: BTreeMap<usize, PendingCall>,
    stop: Option<StopReason>,
}

impl CompletionsDecoder {
    fn tool_call_index(call: &Value) -> Result<usize, ProtocolError> {
        let index = call["index"].as_u64().ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Chat Completions tool-call delta is missing index".to_string(),
            )
        })?;
        usize::try_from(index).map_err(|_| {
            ProtocolError::InvalidResponse(
                "Chat Completions tool-call index is too large".to_string(),
            )
        })
    }
}

impl sse::Decoder for CompletionsDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        if data == "[DONE]" {
            return Ok(sse::DecodeResult::Close(Vec::new()));
        }

        let chunk: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            items.push(ModelEvent::Usage(build_usage(
                usage["prompt_tokens"].as_u64().unwrap_or(0),
                usage["completion_tokens"].as_u64().unwrap_or(0),
            )));
        }

        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return Ok(sse::DecodeResult::Continue(items));
        };
        let delta = &choice["delta"];
        if let Some(reasoning) = ["reasoning_content", "reasoning", "thinking"]
            .into_iter()
            .find_map(|field| delta[field].as_str())
            .filter(|reasoning| !reasoning.is_empty())
        {
            items.push(ModelEvent::Reasoning(reasoning.to_string()));
        }
        if let Some(content) = delta["content"]
            .as_str()
            .filter(|content| !content.is_empty())
        {
            items.push(ModelEvent::Text(content.to_string()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let entry = self.calls.entry(Self::tool_call_index(call)?).or_default();
                if let Some(id) = call["id"].as_str() {
                    entry.set_id(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.set_name(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    entry.append_arguments(arguments);
                }
            }
        }

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop = Some(match reason {
                "stop" | "tool_calls" | "function_call" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                other => StopReason::Other(other.to_string()),
            });
        }
        Ok(sse::DecodeResult::Continue(items))
    }

    fn finalize(self) -> Result<(Vec<ModelEvent>, StopReason), ProtocolError> {
        let Self { calls, stop } = self;
        let Some(stop) = stop else {
            return Ok((Vec::new(), StopReason::Truncated));
        };
        let items = calls
            .into_values()
            .map(|call| call.finish("Chat Completions"))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((items, stop))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, Protocol};
    use ash_core::{Content, ContentBlock, Message, ModelId, ToolCallId};
    use secrecy::SecretString;

    #[test]
    fn aggregates_fragmented_tool_call() {
        let mut decoder = CompletionsDecoder::default();
        decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_7","function":{"name":"bash","arguments":"{\"command\":"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();
        let result = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();
        assert!(matches!(result, sse::DecodeResult::Continue(items) if items.is_empty()));
        let (items, stop) = decoder.finalize().unwrap();
        assert_eq!(
            items,
            vec![ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_7"),
                name: "bash".into(),
                arguments: json!({"command": "pwd"}),
            }]
        );
        assert_eq!(stop, StopReason::EndTurn);
    }

    #[test]
    fn preserves_unknown_finish_reason() {
        let mut decoder = CompletionsDecoder::default();
        decoder
            .decode(r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#)
            .unwrap();

        assert_eq!(
            decoder.finalize().unwrap().1,
            StopReason::Other("content_filter".to_string())
        );
    }

    #[test]
    fn emits_compatible_reasoning_before_answer_text() {
        let mut decoder = CompletionsDecoder::default();
        let result = decoder
            .decode(
                r#"{"choices":[{"delta":{"reasoning_content":"inspect first","content":"done"},"finish_reason":null}]}"#,
            )
            .unwrap();
        let sse::DecodeResult::Continue(items) = result else {
            panic!("expected the stream to continue");
        };

        assert_eq!(
            items,
            vec![
                ModelEvent::Reasoning("inspect first".into()),
                ModelEvent::Text("done".into()),
            ]
        );
    }

    #[test]
    fn rejects_tool_call_delta_without_index() {
        let mut decoder = CompletionsDecoder::default();

        let result = decoder.decode(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_7","function":{"name":"bash"}}]},"finish_reason":null}]}"#,
        );

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn rejects_completed_tool_call_without_a_name() {
        let mut decoder = CompletionsDecoder::default();

        decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_7","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();
        let result = decoder.finalize();

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn includes_persisted_thoughts_in_chat_completion_history() {
        let _adapter = CompletionsAdapter::new(ProviderConfig {
            protocol: Protocol::Completions,
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

        let body = CompletionsAdapter::build_request(&request).unwrap();

        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"], "visible answer");
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            "private reasoning"
        );
    }

    #[test]
    fn includes_persisted_thoughts_alongside_tool_calls_in_chat_completion_history() {
        // DeepSeek requires the `reasoning_content` of a tool-calling turn to be
        // echoed back verbatim in the next request; the assistant message then
        // carries both the thought and the tool calls together.
        let _adapter = CompletionsAdapter::new(ProviderConfig {
            protocol: Protocol::Completions,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message::assistant(vec![
                ContentBlock::Thought {
                    text: "step reasoning".into(),
                    elapsed_seconds: 2,
                },
                ContentBlock::ToolCall {
                    id: ToolCallId::from_provider("call_1"),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "pwd"}),
                },
            ])],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = CompletionsAdapter::build_request(&request).unwrap();

        let message = &body["messages"][0];
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], "");
        assert_eq!(message["reasoning_content"], "step reasoning");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn preserves_text_only_tool_error_wire_format() {
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![Message::tool_result(
                ToolCallId::from_provider("call"),
                Err("permission denied".into()),
                Vec::new(),
            )],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = CompletionsAdapter::build_request(&request).unwrap();

        assert_eq!(body["messages"][0]["content"], "Error: permission denied");
    }

    #[test]
    fn sends_tool_images_after_all_chat_completion_tool_results() {
        let _adapter = CompletionsAdapter::new(ProviderConfig {
            protocol: Protocol::Completions,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![
                crate::test_support::tool_result("first", Vec::new()),
                crate::test_support::tool_result(
                    "second",
                    vec![Content::Image {
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    }],
                ),
            ],
            tools: Vec::new(),
            max_tokens: None,
        };

        let body = CompletionsAdapter::build_request(&request).unwrap();

        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(
            body["messages"][2]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
    }
}
