use ash_core::{ContentBlock, MessageContent, ProtocolError, StopReason};
use base64::Engine;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    consecutive_tool_results, model_config,
    pending_calls::stop_reason,
    pending_calls::{build_usage, PendingCallAccumulator},
    project_request_messages, sse, ProviderConfig,
};
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
        let projected = project_request_messages(req)?;
        let mut messages = Vec::new();
        if let Some(system) = &projected.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        let mut index = 0;
        while index < projected.messages.len() {
            match &projected.messages[index].content {
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
                    let group = consecutive_tool_results(&projected.messages, index);
                    for result in group.results {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": result.id.as_str(),
                            "content": result.output,
                        }));
                    }
                    index = group.next_index;
                    if !group.attachments.is_empty() {
                        messages.push(json!({
                            "role": "user",
                            "content": chat_content(&group.attachments),
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
pub(crate) struct CompletionsDecoder {
    calls: PendingCallAccumulator<usize>,
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

    fn finish_calls(&mut self) -> Result<Vec<ModelEvent>, ProtocolError> {
        let mut items = Vec::new();
        for (_, call) in self.calls.drain() {
            items.push(call.finish("Chat Completions")?);
        }
        Ok(items)
    }

    fn record_stop(&mut self, reason: StopReason) -> Result<(), ProtocolError> {
        if self.stop.replace(reason).is_some() {
            return Err(ProtocolError::InvalidResponse(
                "Chat Completions emitted more than one finish_reason".to_string(),
            ));
        }
        Ok(())
    }
}

impl sse::Decoder for CompletionsDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        if data == "[DONE]" {
            return Ok(if self.stop.is_some() {
                sse::DecodeResult::finished(Vec::new())
            } else {
                // `[DONE]` is only a wire delimiter. Without a preceding
                // finish_reason the response has no semantic terminal state.
                sse::DecodeResult::wire_done(Vec::new())
            });
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
            return Ok(sse::DecodeResult::continuing(items));
        };
        if self.stop.is_some() {
            return Err(ProtocolError::InvalidResponse(
                "Chat Completions emitted a choice after finish_reason".to_string(),
            ));
        }
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
                let entry = self.calls.entry(Self::tool_call_index(call)?);
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
            items.extend(self.finish_calls()?);
            self.record_stop(stop_reason(reason == "length"))?;
            return Ok(sse::DecodeResult::terminal(items));
        }
        Ok(sse::DecodeResult::continuing(items))
    }

    fn finish(&mut self) -> Result<Vec<ModelEvent>, ProtocolError> {
        Ok(self.stop.take().map(ModelEvent::Stop).into_iter().collect())
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
        assert!(matches!(&result, sse::DecodeResult::Terminal(_)));
        let items = result.into_items();

        assert_eq!(
            items,
            vec![ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_7"),
                name: "bash".into(),
                arguments: json!({"command": "pwd"}),
            }]
        );
        assert_eq!(
            decoder.finish().unwrap(),
            vec![ModelEvent::Stop(StopReason::EndTurn)]
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

        let result = decoder.decode(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_7","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        );

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
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
