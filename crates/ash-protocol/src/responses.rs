use std::collections::{BTreeMap, BTreeSet};

use ash_core::{Item, ProtocolError, Step, StopReason};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::{
    content_value, context_turns, image_data_url, model_config,
    pending_calls::{usage_event, PendingCall},
    sse, text_tool_result, ProviderConfig,
};
use ash_core::{ModelClient, ModelEvent, ModelRequest, ModelStream};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

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

    fn build_request(req: &ModelRequest) -> Result<Value, ProtocolError> {
        let mut input = Vec::new();
        if let Some(summary) = req.context.summary() {
            input.push(json!({"role": "assistant", "content": summary}));
        }
        for (turn_input, steps) in context_turns(&req.context) {
            input.push(json!({
                "role": "user",
                "content": responses_content(&turn_input.content),
            }));
            for step in steps {
                push_step(&mut input, step);
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

fn push_step(input: &mut Vec<Value>, step: &Step) {
    let mut text = String::new();
    let mut extra = Vec::new();
    for item in &step.items {
        match item {
            Item::Text(value) => text.push_str(value),
            Item::Thought { text, .. } => extra.push(json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": text}],
            })),
            Item::ToolCall(call) => extra.push(json!({
                "type": "function_call",
                "call_id": call.id.as_str(),
                "name": call.name,
                "arguments": call.arguments.to_string(),
            })),
        }
    }
    if !text.is_empty() {
        input.push(json!({"role": "assistant", "content": text}));
    }
    input.extend(extra);

    let mut attachments = Vec::new();
    for call in step.items.iter().filter_map(|item| match item {
        Item::ToolCall(call) => Some(call),
        Item::Text(_) | Item::Thought { .. } => None,
    }) {
        let (output, is_error) = match &call.result {
            Ok(output) => {
                attachments.extend(output.attachments.iter());
                (output.text.as_str(), false)
            }
            Err(error) => (error.as_str(), true),
        };
        input.push(json!({
            "type": "function_call_output",
            "call_id": call.id.as_str(),
            "output": text_tool_result(output, is_error),
        }));
    }
    if !attachments.is_empty() {
        input.push(json!({
            "role": "user",
            "content": responses_attachments(&attachments),
        }));
    }
}

fn responses_attachments(contents: &[&ash_core::Content]) -> Value {
    Value::Array(
        contents
            .iter()
            .map(|content| match content {
                ash_core::Content::Text(text) => {
                    json!({"type": "input_text", "text": text})
                }
                ash_core::Content::Image { media_type, data } => json!({
                    "type": "input_image",
                    "image_url": image_data_url(media_type, data),
                }),
            })
            .collect(),
    )
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
        let body = Self::build_request(&req)?;
        let request = self
            .client
            .post(format!(
                "{}/v1/responses",
                self.config.base_url(DEFAULT_BASE_URL)
            ))
            .bearer_auth(self.config.api_key.expose_secret())
            .json(&body);
        Ok(sse::stream(request, ResponsesDecoder::default()))
    }
}

#[derive(Default)]
pub struct ResponsesDecoder {
    calls: BTreeMap<ResponseItemKey, PendingCall>,
    emitted_calls: BTreeSet<ResponseItemKey>,
    output_keys: BTreeMap<u64, ResponseItemKey>,
    streamed_reasoning_summaries: BTreeSet<u64>,
    completed: Option<StopReason>,
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
            key @ ResponseItemKey::Id(_) => Ok(key),
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
            self.calls.entry(key.clone()).or_default().merge(call);
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
            return Ok(sse::DecodeResult::Close(Vec::new()));
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| ProtocolError::InvalidResponse(error.to_string()))?;
        let mut items = Vec::new();
        let event_type = event["type"].as_str().ok_or_else(|| {
            ProtocolError::InvalidResponse("Responses event is missing type".to_string())
        })?;
        match event_type {
            "response.output_text.delta" => Self::output_text_delta(&event, &mut items),
            "response.reasoning_summary_text.delta" => {
                self.reasoning_summary_delta(&event, &mut items)?;
            }
            "response.reasoning_summary_text.done" => {
                self.reasoning_summary_done(&event, &mut items)?;
            }
            "response.output_item.added" => self.output_item_added(&event)?,
            "response.function_call_arguments.delta" => {
                self.function_call_arguments_delta(&event)?;
            }
            "response.function_call_arguments.done" => {
                self.function_call_arguments_done(&event, &mut items)?;
            }
            "response.output_item.done" => self.output_item_done(&event, &mut items)?,
            "response.completed" => {
                self.finish(&event, &mut items, StopReason::EndTurn)?;
                return Ok(sse::DecodeResult::Close(items));
            }
            "response.incomplete" => {
                let reason = event["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .filter(|reason| !reason.trim().is_empty())
                    .ok_or_else(|| {
                        ProtocolError::InvalidResponse(
                            "Responses incomplete event is missing a reason".to_string(),
                        )
                    })?;
                let stop = match reason {
                    "max_output_tokens" => StopReason::MaxTokens,
                    other => StopReason::Other(other.to_string()),
                };
                self.finish(&event, &mut items, stop)?;
                return Ok(sse::DecodeResult::Close(items));
            }
            "error" | "response.failed" => return Err(Self::stream_error(&event)),
            _ => {}
        }
        Ok(sse::DecodeResult::Continue(items))
    }

    fn finalize(self) -> Result<(Vec<ModelEvent>, Option<StopReason>), ProtocolError> {
        Ok((Vec::new(), self.completed))
    }
}

impl ResponsesDecoder {
    fn output_text_delta(event: &Value, items: &mut Vec<ModelEvent>) {
        if let Some(delta) = event["delta"].as_str() {
            items.push(ModelEvent::Text(delta.to_string()));
        }
    }

    fn reasoning_summary_delta(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
        if let Some(delta) = event["delta"].as_str() {
            self.streamed_reasoning_summaries
                .insert(Self::summary_index(event)?);
            items.push(ModelEvent::Reasoning(delta.to_string()));
        }
        Ok(())
    }

    fn reasoning_summary_done(
        &self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
        let summary_index = Self::summary_index(event)?;
        if !self.streamed_reasoning_summaries.contains(&summary_index) {
            if let Some(text) = event["text"].as_str() {
                items.push(ModelEvent::Reasoning(text.to_string()));
            }
        }
        Ok(())
    }

    fn output_item_added(&mut self, event: &Value) -> Result<(), ProtocolError> {
        let item = &event["item"];
        if item["type"].as_str() == Some("reasoning") {
            // A new reasoning item starts a fresh summary sequence and may
            // reuse summary indices from the previous item, so drop the
            // tracking before its deltas arrive.
            self.streamed_reasoning_summaries.clear();
        } else if item["type"].as_str() == Some("function_call") {
            let key = self.item_key(event)?;
            self.calls.entry(key).or_default().apply_item(item, false);
        }
        Ok(())
    }

    fn function_call_arguments_delta(&mut self, event: &Value) -> Result<(), ProtocolError> {
        let key = self.key(event)?;
        if let Some(delta) = event["delta"].as_str() {
            self.calls.entry(key).or_default().append_arguments(delta);
        }
        Ok(())
    }

    fn function_call_arguments_done(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
        let key = self.key(event)?;
        let call = self.calls.entry(key.clone()).or_default();
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
        Ok(())
    }

    fn output_item_done(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
    ) -> Result<(), ProtocolError> {
        let item = &event["item"];
        if item["type"].as_str() == Some("function_call") {
            let key = self.item_key(event)?;
            self.calls
                .entry(key.clone())
                .or_default()
                .apply_item(item, true);
            if let Some(call) = self.emit_call(&key)? {
                items.push(call);
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        event: &Value,
        items: &mut Vec<ModelEvent>,
        stop: StopReason,
    ) -> Result<(), ProtocolError> {
        if !self.calls.is_empty() {
            return Err(ProtocolError::InvalidResponse(
                "Responses API stopped with an unfinished tool call".to_string(),
            ));
        }
        let response = &event["response"];
        let usage = response.get("usage").unwrap_or_else(|| &event["usage"]);
        if !usage.is_null() {
            items.push(usage_event(
                usage["input_tokens"].as_u64().unwrap_or(0),
                usage["output_tokens"].as_u64().unwrap_or(0),
            ));
        }
        self.completed = Some(stop);
        Ok(())
    }

    fn stream_error(event: &Value) -> ProtocolError {
        let error = event["response"].get("error").unwrap_or(event);
        let code = error["code"].as_str();
        let message = error["message"]
            .as_str()
            .unwrap_or("Responses API stream failed");
        match code {
            Some("server_is_overloaded") => ProtocolError::Upstream {
                status: 503,
                message: message.to_string(),
            },
            Some("rate_limit_exceeded" | "rate_limit_error") => ProtocolError::RateLimited,
            _ => ProtocolError::InvalidResponse(
                code.map_or_else(|| message.to_string(), |code| format!("{code}: {message}")),
            ),
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
        let result = decoder
            .decode(
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"Cargo.toml\"}"}"#,
            )
            .unwrap();
        let sse::DecodeResult::Continue(items) = result else {
            panic!("expected the stream to continue");
        };

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
            .unwrap();
        assert!(matches!(duplicate, sse::DecodeResult::Continue(items) if items.is_empty()));
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
        let result = decoder
            .decode(
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"Cargo.toml\"}"}"#,
            )
            .unwrap();
        let sse::DecodeResult::Continue(items) = result else {
            panic!("expected the stream to continue");
        };

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
            .unwrap();
        let done = decoder
            .decode(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"checking"}"#,
            )
            .unwrap();

        assert!(matches!(
            delta,
            sse::DecodeResult::Continue(items)
                if items == vec![ModelEvent::Reasoning("checking".into())]
        ));
        assert!(matches!(done, sse::DecodeResult::Continue(items) if items.is_empty()));
    }

    #[test]
    fn emits_atomic_reasoning_summaries_when_no_deltas_arrive() {
        let mut decoder = ResponsesDecoder::default();
        let result = decoder
            .decode(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"checked"}"#,
            )
            .unwrap();
        let sse::DecodeResult::Continue(items) = result else {
            panic!("expected the stream to continue");
        };

        assert_eq!(items, vec![ModelEvent::Reasoning("checked".into())]);
    }

    #[test]
    fn completed_response_finishes_the_decoder() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder
            .decode(r#"{"type":"response.completed","response":{}}"#)
            .unwrap();

        assert!(matches!(result, sse::DecodeResult::Close(items) if items.is_empty()));
        assert_eq!(decoder.finalize().unwrap().1, Some(StopReason::EndTurn));
    }

    #[test]
    fn incomplete_response_preserves_known_and_unknown_reasons() {
        let mut max_tokens = ResponsesDecoder::default();
        max_tokens
            .decode(
                r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            )
            .unwrap();
        assert_eq!(
            max_tokens.finalize().unwrap().1,
            Some(StopReason::MaxTokens)
        );

        let mut unknown = ResponsesDecoder::default();
        unknown
            .decode(
                r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
            )
            .unwrap();
        assert_eq!(
            unknown.finalize().unwrap().1,
            Some(StopReason::Other("content_filter".to_string()))
        );
    }

    #[test]
    fn rejects_incomplete_response_without_a_reason() {
        let result =
            ResponsesDecoder::default().decode(r#"{"type":"response.incomplete","response":{}}"#);

        assert!(matches!(result, Err(ProtocolError::InvalidResponse(_))));
    }

    #[test]
    fn maps_server_overload_to_a_retryable_upstream_error() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder.decode(
            r#"{"type":"error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}"#,
        );

        assert!(matches!(
            result,
            Err(ProtocolError::Upstream { status: 503, message })
                if message == "Our servers are currently overloaded. Please try again later."
        ));
    }

    #[test]
    fn maps_rate_limit_stream_errors_to_the_retryable_category() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder
            .decode(r#"{"type":"error","code":"rate_limit_exceeded","message":"Slow down"}"#);

        assert!(matches!(result, Err(ProtocolError::RateLimited)));
    }

    #[test]
    fn preserves_unknown_stream_error_details_without_retrying() {
        let mut decoder = ResponsesDecoder::default();

        let result = decoder
            .decode(r#"{"type":"error","code":"unexpected_error","message":"something failed"}"#);

        assert!(matches!(
            result,
            Err(ProtocolError::InvalidResponse(message))
                if message == "unexpected_error: something failed"
        ));
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
        let _adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::Responses,
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

        let body = ResponsesAdapter::build_request(&request).unwrap();

        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"], "visible answer");
        assert_eq!(body["input"][2]["type"], "reasoning");
        assert_eq!(body["input"][2]["summary"][0]["text"], "private reasoning");
    }

    #[test]
    fn preserves_text_only_tool_error_wire_format() {
        let request = test_support::request(vec![test_support::tool_result(
            "call",
            Err("permission denied".into()),
        )]);

        let body = ResponsesAdapter::build_request(&request).unwrap();

        assert_eq!(body["input"][2]["output"], "Error: permission denied");
    }

    #[test]
    fn sends_tool_images_after_all_response_function_outputs() {
        let _adapter = ResponsesAdapter::new(ProviderConfig {
            protocol: Protocol::Responses,
            api_key: SecretString::from("test"),
            base_url: None,
        });
        let request = test_support::request(vec![
            test_support::tool_result("first", Ok(test_support::output("first", Vec::new()))),
            test_support::tool_result(
                "second",
                Ok(test_support::output(
                    "second",
                    vec![Content::Image {
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    }],
                )),
            ),
        ]);

        let body = ResponsesAdapter::build_request(&request).unwrap();

        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["input"][4]["type"], "function_call_output");
        assert_eq!(body["input"][5]["role"], "user");
        assert_eq!(
            body["input"][5]["content"][0]["image_url"],
            "data:image/png;base64,AQID"
        );
    }
}
