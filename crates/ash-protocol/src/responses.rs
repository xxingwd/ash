use std::collections::{BTreeMap, BTreeSet};

use ash_core::{ContentBlock, ProtocolError};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    content_value, image_data_url, model_config,
    pending_calls::stop_reason,
    pending_calls::{build_usage, PendingCallAccumulator},
    project_request_messages, sse, MessageGroup, MessageGroupIter, ProviderConfig,
};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

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
        self.config.base_url("https://api.openai.com")
    }

    fn build_request(&self, req: &ModelRequest) -> Result<Value, ProtocolError> {
        let projected = project_request_messages(req)?;
        let mut input = Vec::new();
        for group in MessageGroupIter::new(&projected.messages) {
            match group {
                MessageGroup::User(contents) => {
                    input.push(json!({"role": "user", "content": responses_content(contents)}));
                }
                MessageGroup::Assistant(blocks) => {
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
                        ContentBlock::Thought { text, .. } => Some(json!({
                            "type": "reasoning",
                            "summary": [{"type": "summary_text", "text": text}],
                        })),
                        ContentBlock::Text(_) | ContentBlock::ToolCall { .. } => None,
                    }));
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
                }
                MessageGroup::ToolResults(group) => {
                    for result in group.results {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": result.id.as_str(),
                            "output": result.output,
                        }));
                    }
                    if !group.attachments.is_empty() {
                        input.push(json!({
                            "role": "user",
                            "content": responses_content(&group.attachments),
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
        if let Some(system) = &projected.system {
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
    content_value(contents, "input_text", |media_type, data| {
        json!({
            "type": "input_image",
            "image_url": image_data_url(media_type, data),
        })
    })
}

impl ModelClient for ResponsesAdapter {
    fn stream(&self, req: ModelRequest) -> Result<ModelStream, ProtocolError> {
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
pub(crate) struct ResponsesDecoder {
    calls: PendingCallAccumulator<ResponseItemKey>,
    emitted_calls: BTreeSet<ResponseItemKey>,
    output_keys: BTreeMap<u64, ResponseItemKey>,
    streamed_reasoning_summaries: BTreeSet<u64>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ResponseItemKey {
    Id(String),
    OutputIndex(u64),
}

impl ResponsesDecoder {
    fn raw_key(event: &Value) -> Result<ResponseItemKey, ProtocolError> {
        if let Some(id) = event["item_id"].as_str() {
            return Ok(ResponseItemKey::Id(id.to_string()));
        }
        event["output_index"]
            .as_u64()
            .map(ResponseItemKey::OutputIndex)
            .ok_or_else(|| {
                ProtocolError::InvalidResponse(
                    "Responses function-call event is missing item_id and output_index".to_string(),
                )
            })
    }

    fn key(&self, event: &Value) -> Result<ResponseItemKey, ProtocolError> {
        match Self::raw_key(event)? {
            ResponseItemKey::OutputIndex(index) => Ok(self
                .output_keys
                .get(&index)
                .cloned()
                .unwrap_or(ResponseItemKey::OutputIndex(index))),
            key => Ok(key),
        }
    }

    fn item_key(&mut self, event: &Value) -> Result<ResponseItemKey, ProtocolError> {
        let key = if let Some(id) = event["item"]["id"].as_str() {
            ResponseItemKey::Id(id.to_string())
        } else {
            Self::raw_key(event)?
        };
        if let Some(index) = event["output_index"].as_u64() {
            self.bind_output_index(index, key.clone());
        }
        Ok(key)
    }

    fn bind_output_index(&mut self, index: u64, key: ResponseItemKey) {
        let old_key = ResponseItemKey::OutputIndex(index);
        self.output_keys.insert(index, key.clone());
        if old_key == key {
            return;
        }
        if let Some(call) = self.calls.remove(&old_key) {
            self.calls.entry(key.clone()).merge(call);
        }
        if self.emitted_calls.remove(&old_key) {
            self.emitted_calls.insert(key);
        }
    }

    fn summary_index(event: &Value) -> Result<u64, ProtocolError> {
        event["summary_index"].as_u64().ok_or_else(|| {
            ProtocolError::InvalidResponse(
                "Responses reasoning event is missing summary_index".to_string(),
            )
        })
    }

    fn emit_call(&mut self, key: &ResponseItemKey) -> Result<Option<ModelEvent>, ProtocolError> {
        if self.emitted_calls.contains(key) {
            self.calls.remove(key);
            return Ok(None);
        }
        let Some(call) = self.calls.remove(key) else {
            return Ok(None);
        };
        let item = call.finish("Responses")?;
        self.emitted_calls.insert(key.clone());
        Ok(Some(item))
    }
}

impl sse::Decoder for ResponsesDecoder {
    fn decode(&mut self, data: &str) -> Result<sse::DecodeResult, ProtocolError> {
        if data == "[DONE]" {
            return Ok(sse::DecodeResult::wire_done(Vec::new()));
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        let event_type = event["type"].as_str().ok_or_else(|| {
            ProtocolError::InvalidResponse("Responses event is missing type".to_string())
        })?;
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    items.push(ModelEvent::Text(delta.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.streamed_reasoning_summaries
                        .insert(Self::summary_index(&event)?);
                    items.push(ModelEvent::Reasoning(delta.to_string()));
                }
            }
            "response.reasoning_summary_text.done" => {
                let summary_index = Self::summary_index(&event)?;
                if !self.streamed_reasoning_summaries.contains(&summary_index) {
                    if let Some(text) = event["text"].as_str() {
                        items.push(ModelEvent::Reasoning(text.to_string()));
                    }
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
                if item["type"].as_str() == Some("reasoning") {
                    // A new reasoning item starts a fresh summary sequence and may
                    // reuse summary indices from the previous item, so drop the
                    // tracking before its deltas arrive.
                    self.streamed_reasoning_summaries.clear();
                } else if item["type"].as_str() == Some("function_call") {
                    let key = self.item_key(&event)?;
                    self.calls.entry(key).apply_item(item, false);
                }
            }
            "response.function_call_arguments.delta" => {
                let key = self.key(&event)?;
                if let Some(delta) = event["delta"].as_str() {
                    self.calls.entry(key).append_arguments(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let key = self.key(&event)?;
                let call = self.calls.entry(key.clone());
                if let Some(arguments) = event["arguments"].as_str() {
                    call.set_arguments(arguments);
                }
                if let Some(name) = event["name"].as_str() {
                    call.set_name(name);
                }
                if let Some(call_id) = event["call_id"].as_str() {
                    call.set_id(call_id);
                }
                if let Some(item) = self.emit_call(&key)? {
                    items.push(item);
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                if item["type"].as_str() == Some("function_call") {
                    let key = self.item_key(&event)?;
                    self.calls.entry(key.clone()).apply_item(item, true);
                    if let Some(call) = self.emit_call(&key)? {
                        items.push(call);
                    }
                }
            }
            "response.completed" => {
                if !self.calls.is_empty() {
                    return Err(ProtocolError::InvalidResponse(
                        "Responses API completed with an unfinished tool call".to_string(),
                    ));
                }
                let response = &event["response"];
                let usage = response.get("usage").unwrap_or(&event["usage"]);
                if !usage.is_null() {
                    items.push(ModelEvent::Usage(build_usage(
                        usage["input_tokens"].as_u64().unwrap_or(0),
                        usage["output_tokens"].as_u64().unwrap_or(0),
                    )));
                }
                items.push(ModelEvent::Stop(stop_reason(
                    response["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or(""),
                )));
                return Ok(sse::DecodeResult::finished(items));
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
            vec![ModelEvent::ToolCall {
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
    fn normalizes_function_call_output_index_to_item_id() {
        let mut decoder = ResponsesDecoder::default();
        decoder
            .decode(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"Cargo.toml\"}"}"#,
            )
            .unwrap();
        let items = decoder
            .decode(
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"Cargo.toml\"}"}"#,
            )
            .unwrap()
            .into_items();

        assert_eq!(
            items,
            vec![ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_1"),
                name: "read".into(),
                arguments: json!({"path": "Cargo.toml"}),
            }]
        );
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

        assert_eq!(delta, vec![ModelEvent::Reasoning("checking".into())]);
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

        assert_eq!(items, vec![ModelEvent::Reasoning("checked".into())]);
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
    fn rejects_completed_response_with_an_unfinished_tool_call() {
        let mut decoder = ResponsesDecoder::default();
        decoder
            .decode(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            )
            .unwrap();

        let result = decoder.decode(r#"{"type":"response.completed","response":{}}"#);

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn rejects_function_call_event_without_a_key() {
        let mut decoder = ResponsesDecoder::default();

        let result =
            decoder.decode(r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#);

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn rejects_completed_function_call_without_a_name() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder.decode(
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{}"}"#,
        );

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn includes_persisted_thoughts_in_responses_history() {
        let adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::Responses,
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

        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(body["input"][0]["content"], "visible answer");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][1]["summary"][0]["text"], "private reasoning");
    }

    #[test]
    fn sends_tool_images_after_all_response_function_outputs() {
        let adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::Responses,
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

        let body = adapter.build_request(&request).unwrap();

        assert_eq!(body["input"][0]["type"], "function_call_output");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(
            body["input"][2]["content"][0]["image_url"],
            "data:image/png;base64,AQID"
        );
    }
}
