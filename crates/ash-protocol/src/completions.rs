use std::collections::BTreeMap;

use ash_core::{ContentBlock, MessageContent, ProtocolError, StopReason, ToolCallId, Usage};
use base64::Engine;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{model_config, sse, ProviderConfig};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

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

    fn base_url(&self) -> &str {
        self.config
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com")
            .trim_end_matches('/')
    }

    fn build_request(&self, req: &ModelRequest) -> Result<Value, ProtocolError> {
        let mut messages = Vec::new();
        if let Some(system) = &req.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        let mut index = 0;
        while index < req.messages.len() {
            match &req.messages[index].content {
                MessageContent::User(contents) => {
                    messages.push(json!({"role": "user", "content": chat_content(contents)}));
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
                    let calls: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => Some(json!({
                                "id": id.as_str(),
                                "type": "function",
                                "function": {"name": name, "arguments": arguments.to_string()},
                            })),
                            ContentBlock::Text(_) | ContentBlock::Thought { .. } => None,
                        })
                        .collect();
                    let mut value = json!({"role": "assistant", "content": text});
                    if !calls.is_empty() {
                        value["tool_calls"] = json!(calls);
                    }
                    if !text.is_empty() || !calls.is_empty() {
                        messages.push(value);
                    }
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
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": id.as_str(),
                            "content": result.as_ref().map_or_else(
                                |error| format!("Error: {error}"),
                                Clone::clone,
                            ),
                        }));
                        attachments.extend(result_attachments.iter().cloned());
                        index += 1;
                    }
                    if !attachments.is_empty() {
                        messages.push(json!({
                            "role": "user",
                            "content": chat_content(&attachments),
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

fn chat_content(contents: &[ash_core::Content]) -> Value {
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
            ash_core::Content::Text(text) => json!({"type": "text", "text": text}),
            ash_core::Content::Image { media_type, data } => json!({
                "type": "image_url",
                "image_url": {
                    "url": format!(
                        "data:{media_type};base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(data)
                    )
                }
            }),
        })
        .collect::<Vec<_>>())
}

impl ModelClient for CompletionsAdapter {
    fn stream(&self, req: ModelRequest) -> Result<ModelStream, ProtocolError> {
        let body = self.build_request(&req)?;
        let request = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url()))
            .bearer_auth(self.config.api_key.expose_secret())
            .json(&body);
        sse::stream(request, CompletionsDecoder::default())
    }
}

#[derive(Default)]
struct CompletionsDecoder {
    calls: BTreeMap<usize, PendingCall>,
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

impl CompletionsDecoder {
    fn finish_items(&mut self, reason: StopReason) -> Result<Vec<ModelEvent>, ProtocolError> {
        let mut items = Vec::new();
        for (_, call) in std::mem::take(&mut self.calls) {
            let arguments = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).map_err(|error| {
                    ProtocolError::InvalidResponse(format!(
                        "invalid Chat Completions tool arguments: {error}"
                    ))
                })?
            };
            items.push(ModelEvent::ToolCall {
                id: if call.id.is_empty() {
                    ToolCallId::new()
                } else {
                    ToolCallId::from_provider(call.id)
                },
                name: call.name,
                arguments,
            });
        }
        items.push(ModelEvent::Stop(reason));
        Ok(items)
    }
}

impl sse::Decoder for CompletionsDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        if data == "[DONE]" {
            return Ok(sse::DecodeResult::finished(
                self.finish_items(StopReason::EndTurn)?,
            ));
        }

        let chunk: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            items.push(ModelEvent::Usage(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                generation_ms: 0,
                estimated: false,
            }));
        }

        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return Ok(sse::DecodeResult::continuing(items));
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
                let entry = self
                    .calls
                    .entry(call["index"].as_u64().unwrap_or(0) as usize)
                    .or_default();
                if let Some(id) = call["id"].as_str() {
                    entry.id = id.to_string();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.name = name.to_string();
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    entry.arguments.push_str(arguments);
                }
            }
        }

        if let Some(reason) = choice["finish_reason"].as_str() {
            let reason = if reason == "length" {
                StopReason::MaxTokens
            } else {
                StopReason::EndTurn
            };
            items.extend(self.finish_items(reason)?);
            return Ok(sse::DecodeResult::finished(items));
        }
        Ok(sse::DecodeResult::continuing(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sse::Decoder, Protocol};
    use ash_core::{Content, ContentBlock, Message, MessageContent, MessageId, ModelId, Role};
    use secrecy::SecretString;

    #[test]
    fn aggregates_fragmented_tool_call() {
        let mut decoder = CompletionsDecoder::default();
        decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_7","function":{"name":"bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        let result = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();
        assert!(matches!(&result, sse::DecodeResult::Finished(_)));
        let items = result.into_items();

        assert_eq!(
            items,
            vec![
                ModelEvent::ToolCall {
                    id: ToolCallId::from_provider("call_7"),
                    name: "bash".into(),
                    arguments: json!({"command": "pwd"}),
                },
                ModelEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn emits_compatible_reasoning_before_answer_text() {
        let mut decoder = CompletionsDecoder::default();
        let items = decoder
            .decode(
                r#"{"choices":[{"delta":{"reasoning_content":"inspect first","content":"done"},"finish_reason":null}]}"#,
            )
            .unwrap()
            .into_items();

        assert_eq!(
            items,
            vec![
                ModelEvent::Reasoning("inspect first".into()),
                ModelEvent::Text("done".into()),
            ]
        );
    }

    #[test]
    fn omits_persisted_thoughts_from_chat_completion_history() {
        let adapter = CompletionsAdapter::new(ProviderConfig {
            protocol: Protocol::Completions,
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

        assert_eq!(body["messages"][0]["content"], "visible answer");
        assert!(!body.to_string().contains("private reasoning"));
    }

    #[test]
    fn sends_tool_images_after_all_chat_completion_tool_results() {
        let adapter = CompletionsAdapter::new(ProviderConfig {
            protocol: Protocol::Completions,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = ModelRequest {
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

        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(
            body["messages"][2]["content"][0]["image_url"]["url"],
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
