use std::collections::{BTreeMap, BTreeSet};

use ash_core::{ContentBlock, MessageContent, ProtocolError, StopReason, ToolCallId};
use base64::Engine;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    model_config, sse, LlmRequest, ProtocolAdapter, ProtocolStream, ProviderConfig, StreamItem,
};

pub struct ResponsesAdapter {
    config: ProviderConfig,
    client: Client,
}

impl ResponsesAdapter {
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
            .unwrap_or("https://api.openai.com")
            .trim_end_matches('/')
    }

    fn build_request(&self, req: &LlmRequest) -> Result<Value, ProtocolError> {
        let mut input = Vec::new();
        let mut index = 0;
        while index < req.messages.len() {
            match &req.messages[index].content {
                MessageContent::User(contents) => {
                    input.push(json!({"role": "user", "content": responses_content(contents)}));
                    index += 1;
                }
                MessageContent::Assistant(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.as_str()),
                            ContentBlock::Thought { .. } | ContentBlock::ToolCall { .. } => None,
                        })
                        .collect::<String>();
                    if !text.is_empty() {
                        input.push(json!({"role": "assistant", "content": text}));
                    }
                    input.extend(blocks.iter().filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "type": "function_call",
                            "call_id": id.as_str(),
                            "name": name,
                            "arguments": arguments.to_string(),
                        })),
                        ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
                    }));
                    index += 1;
                }
                MessageContent::ToolResult { .. } => {
                    let mut attachments = Vec::new();
                    while index < req.messages.len() {
                        let MessageContent::ToolResult {
                            id,
                            result,
                            attachments: result_attachments,
                        } = &req.messages[index].content
                        else {
                            break;
                        };
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": id.as_str(),
                            "output": result.as_ref().map_or_else(
                                |error| format!("Error: {error}"),
                                Clone::clone,
                            ),
                        }));
                        attachments.extend(result_attachments.iter().cloned());
                        index += 1;
                    }
                    if !attachments.is_empty() {
                        input.push(json!({
                            "role": "user",
                            "content": responses_content(&attachments),
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
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_schema,
                })
            })
            .collect();
        let mut body = json!({
            "model": req.model.as_str(),
            "input": input,
            "stream": true,
        });
        if let Some(system) = &req.system {
            body["instructions"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(max_tokens) = req.max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }
        model_config::apply_from_env(&mut body)?;
        Ok(body)
    }
}

fn responses_content(contents: &[ash_core::Content]) -> Value {
    if !contents
        .iter()
        .any(|content| matches!(content, ash_core::Content::Image { .. }))
    {
        return json!(contents
            .iter()
            .filter_map(|content| match content {
                ash_core::Content::Text(text) => Some(text.as_str()),
                ash_core::Content::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }

    json!(contents
        .iter()
        .map(|content| match content {
            ash_core::Content::Text(text) => json!({"type": "input_text", "text": text}),
            ash_core::Content::Image { media_type, data } => json!({
                "type": "input_image",
                "image_url": format!(
                    "data:{media_type};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(data)
                ),
            }),
        })
        .collect::<Vec<_>>())
}

impl ProtocolAdapter for ResponsesAdapter {
    fn stream(&self, req: LlmRequest) -> Result<ProtocolStream, ProtocolError> {
        let body = self.build_request(&req)?;
        let request = self
            .client
            .post(format!("{}/v1/responses", self.base_url()))
            .bearer_auth(self.config.api_key.expose_secret())
            .json(&body);
        sse::stream(request, ResponsesDecoder::default())
    }
}

#[derive(Default)]
struct ResponsesDecoder {
    calls: BTreeMap<String, PendingCall>,
    emitted_calls: BTreeSet<String>,
    streamed_reasoning_summaries: BTreeSet<u64>,
}

#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl ResponsesDecoder {
    fn key(event: &Value) -> String {
        event["item_id"]
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("output: {}", event["output_index"].as_u64().unwrap_or(0)))
    }

    fn emit_call(&mut self, key: &str) -> Result<Option<StreamItem>, ProtocolError> {
        if self.emitted_calls.contains(key) {
            return Ok(None);
        }
        let Some(call) = self.calls.remove(key) else {
            return Ok(None);
        };
        let arguments = if call.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&call.arguments).map_err(|error| {
                ProtocolError::InvalidResponse(format!("invalid Responses tool arguments: {error}"))
            })?
        };
        let item = StreamItem::ToolCall {
            id: if call.call_id.is_empty() {
                ToolCallId::new()
            } else {
                ToolCallId::from_provider(call.call_id)
            },
            name: call.name,
            arguments,
        };
        self.emitted_calls.insert(key.to_string());
        Ok(Some(item))
    }
}

impl sse::Decoder for ResponsesDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        if data == "[DONE]" {
            return Ok(sse::DecodeResult::finished(Vec::new()));
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        let mut finished = false;

        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    items.push(StreamItem::TextDelta(delta.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.streamed_reasoning_summaries
                        .insert(event["summary_index"].as_u64().unwrap_or(0));
                    items.push(StreamItem::ThinkingDelta(delta.to_string()));
                }
            }
            "response.reasoning_summary_text.done" => {
                let summary_index = event["summary_index"].as_u64().unwrap_or(0);
                if !self.streamed_reasoning_summaries.contains(&summary_index) {
                    if let Some(text) = event["text"].as_str() {
                        items.push(StreamItem::ThinkingDelta(text.to_string()));
                    }
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
                if item["type"].as_str() == Some("reasoning") {
                    self.streamed_reasoning_summaries.clear();
                } else if item["type"].as_str() == Some("function_call") {
                    self.calls.insert(
                        item["id"]
                            .as_str()
                            .map(ToOwned::to_owned)
                            .unwrap_or_else(|| Self::key(&event)),
                        PendingCall {
                            call_id: item["call_id"].as_str().unwrap_or_default().to_string(),
                            name: item["name"].as_str().unwrap_or_default().to_string(),
                            arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let key = Self::key(&event);
                let call = self.calls.entry(key).or_default();
                if let Some(delta) = event["delta"].as_str() {
                    call.arguments.push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let key = Self::key(&event);
                let call = self.calls.entry(key.clone()).or_default();
                if let Some(arguments) = event["arguments"].as_str() {
                    call.arguments = arguments.to_string();
                }
                if let Some(name) = event["name"].as_str() {
                    call.name = name.to_string();
                }
                if let Some(call_id) = event["call_id"].as_str() {
                    call.call_id = call_id.to_string();
                }
                if let Some(item) = self.emit_call(&key)? {
                    items.push(item);
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                if item["type"].as_str() == Some("function_call") {
                    let key = item["id"]
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| Self::key(&event));
                    self.calls
                        .entry(key.clone())
                        .or_insert_with(|| PendingCall {
                            call_id: item["call_id"].as_str().unwrap_or_default().to_string(),
                            name: item["name"].as_str().unwrap_or_default().to_string(),
                            arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                        });
                    if let Some(call) = self.emit_call(&key)? {
                        items.push(call);
                    }
                }
            }
            "response.completed" => {
                let response = &event["response"];
                let usage = response.get("usage").unwrap_or(&event["usage"]);
                if !usage.is_null() {
                    items.push(StreamItem::Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    });
                }
                let reason = if response["incomplete_details"]["reason"].as_str()
                    == Some("max_output_tokens")
                {
                    StopReason::MaxTokens
                } else {
                    StopReason::EndTurn
                };
                items.push(StreamItem::Stop(reason));
                finished = true;
            }
            "response.failed" => {
                return Err(ProtocolError::InvalidResponse(
                    event["response"]["error"]["message"]
                        .as_str()
                        .unwrap_or("Responses API stream failed")
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
    fn emits_one_call_for_many_argument_deltas() {
        let mut decoder = ResponsesDecoder::default();
        decoder
            .decode(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"Cargo.toml\"}"}"#,
            )
            .unwrap();
        let items = decoder
            .decode(
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"Cargo.toml\"}"}"#,
            )
            .unwrap()
            .into_items();

        assert_eq!(
            items,
            vec![StreamItem::ToolCall {
                id: ToolCallId::from_provider("call_1"),
                name: "read".into(),
                arguments: json!({"path": "Cargo.toml"}),
            }]
        );

        let duplicate = decoder
            .decode(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}}"#,
            )
            .unwrap()
            .into_items();
        assert!(duplicate.is_empty());
    }

    #[test]
    fn emits_reasoning_summary_deltas_without_repeating_done_text() {
        let mut decoder = ResponsesDecoder::default();
        let delta = decoder
            .decode(
                r#"{"type":"response.reasoning_summary_text.delta","summary_index":0,"delta":"checking"}"#,
            )
            .unwrap()
            .into_items();
        let done = decoder
            .decode(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"checking"}"#,
            )
            .unwrap()
            .into_items();

        assert_eq!(delta, vec![StreamItem::ThinkingDelta("checking".into())]);
        assert!(done.is_empty());
    }

    #[test]
    fn emits_atomic_reasoning_summaries_when_no_deltas_arrive() {
        let mut decoder = ResponsesDecoder::default();
        let items = decoder
            .decode(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"checked"}"#,
            )
            .unwrap()
            .into_items();

        assert_eq!(items, vec![StreamItem::ThinkingDelta("checked".into())]);
    }

    #[test]
    fn completed_response_finishes_the_decoder() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder
            .decode(r#"{"type":"response.completed","response":{}}"#)
            .unwrap();

        assert!(matches!(result, sse::DecodeResult::Finished(_)));
    }

    #[test]
    fn omits_persisted_thoughts_from_responses_history() {
        let adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::OpenaiResponses,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = LlmRequest {
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

        assert_eq!(body["input"][0]["content"], "visible answer");
        assert!(!body.to_string().contains("private reasoning"));
    }

    #[test]
    fn sends_tool_images_after_all_response_function_outputs() {
        let adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::OpenaiResponses,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = LlmRequest {
            model: ModelId::new("test"),
            system: None,
            messages: vec![
                tool_result("first", Vec::new()),
                tool_result(
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

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(body["input"][0]["type"], "function_call_output");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(
            body["input"][2]["content"][0]["image_url"],
            "data:image/png;base64,AQID"
        );
    }

    fn tool_result(text: &str, attachments: Vec<Content>) -> Message {
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
