use std::collections::BTreeMap;

use ash_core::{ModelEvent, ProtocolError, StopReason, ToolCallId, Usage};
use serde_json::{json, Value};

/// Shared construction of provider-neutral usage records. Stream decoders
/// report token counts under provider-specific field names; once those are
/// read, every protocol builds the same `Usage` shape.
pub const fn build_usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        generation_ms: 0,
        estimated: false,
    }
}

/// Shared mapping from a provider's "ran out of tokens" signal to the
/// provider-neutral `StopReason`. The reason strings cover the three supported
/// providers; anything else is a normal end of turn.
pub fn stop_reason(provider_reason: &str) -> StopReason {
    if matches!(
        provider_reason,
        "length" | "max_tokens" | "max_output_tokens"
    ) {
        StopReason::MaxTokens
    } else {
        StopReason::EndTurn
    }
}

/// A tool call being accumulated from incremental stream deltas.
///
/// Providers deliver the call in fragments: an opening event may or may not
/// carry the id and name, and the arguments arrive as concatenated JSON
/// fragments. This type stores the raw fragments and `finish` closes the
/// call into a provider-neutral `ModelEvent` (empty arguments fall back to
/// `{}`, a missing id falls back to a fresh `ToolCallId`, invalid JSON is a
/// protocol error named after the provider).
#[derive(Default)]
pub struct PendingCall {
    /// Provider tool-call id; empty means the provider never sent one.
    id: String,
    name: String,
    arguments: String,
}

impl PendingCall {
    pub(crate) fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            arguments: String::new(),
        }
    }

    pub(crate) fn set_id(&mut self, id: &str) {
        self.id = id.to_string();
    }

    pub(crate) fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }

    /// Replace the accumulated arguments with a complete payload.
    pub(crate) fn set_arguments(&mut self, arguments: &str) {
        self.arguments = arguments.to_string();
    }

    pub(crate) fn append_arguments(&mut self, arguments: &str) {
        self.arguments.push_str(arguments);
    }

    /// Merge a call re-keyed from an output index into this item-keyed call.
    pub(crate) fn merge(&mut self, other: Self) {
        if self.id.is_empty() {
            self.id = other.id;
        }
        if self.name.is_empty() {
            self.name = other.name;
        }
        if self.arguments.is_empty() {
            self.arguments = other.arguments;
        } else if !other.arguments.is_empty() {
            self.arguments.push_str(&other.arguments);
        }
    }

    /// Apply a `function_call` item payload; `replace_arguments` is true for
    /// atomic `output_item.done` payloads and false for `output_item.added`
    /// placeholders.
    pub(crate) fn apply_item(&mut self, item: &Value, replace_arguments: bool) {
        if let Some(call_id) = item["call_id"]
            .as_str()
            .filter(|call_id| !call_id.is_empty())
        {
            self.set_id(call_id);
        }
        if let Some(name) = item["name"].as_str().filter(|name| !name.is_empty()) {
            self.set_name(name);
        }
        if let Some(arguments) = item["arguments"].as_str() {
            if replace_arguments || self.arguments.is_empty() || !arguments.is_empty() {
                self.set_arguments(arguments);
            }
        }
    }

    /// Close the accumulated call into a provider-neutral event.
    pub(crate) fn finish(self, protocol: &str) -> Result<ModelEvent, ProtocolError> {
        if self.name.trim().is_empty() {
            return Err(ProtocolError::InvalidResponse(format!(
                "{protocol} tool call is missing a name"
            )));
        }
        let arguments = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).map_err(|error| {
                ProtocolError::InvalidResponse(format!(
                    "invalid {protocol} tool arguments: {error}"
                ))
            })?
        };
        let id = if self.id.is_empty() {
            ToolCallId::new()
        } else {
            ToolCallId::from_provider(self.id)
        };
        Ok(ModelEvent::ToolCall {
            id,
            name: self.name,
            arguments,
        })
    }
}

/// Tool calls keyed by the provider's own call identifier (a block index, a
/// delta index, or an item key).
pub struct PendingCallAccumulator<K: Ord> {
    calls: BTreeMap<K, PendingCall>,
}

impl<K: Ord> Default for PendingCallAccumulator<K> {
    fn default() -> Self {
        Self {
            calls: BTreeMap::new(),
        }
    }
}

impl<K: Ord> PendingCallAccumulator<K> {
    pub(crate) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub(crate) fn entry(&mut self, key: K) -> &mut PendingCall {
        self.calls.entry(key).or_default()
    }

    pub(crate) fn insert(&mut self, key: K, call: PendingCall) -> Option<PendingCall> {
        self.calls.insert(key, call)
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut PendingCall> {
        self.calls.get_mut(key)
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<PendingCall> {
        self.calls.remove(key)
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (K, PendingCall)> + '_ {
        std::mem::take(&mut self.calls).into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finish_falls_back_to_empty_object_and_fresh_id() {
        let event = PendingCall::new("", "read").finish("Anthropic").unwrap();

        match event {
            ModelEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(name, "read");
                assert_eq!(arguments, json!({}));
                assert!(!id.as_str().is_empty());
            }
            _ => panic!("expected a tool call event"),
        }
    }

    #[test]
    fn finish_preserves_provider_id_and_parses_arguments() {
        let mut call = PendingCall::new("toolu_123", "bash");
        call.append_arguments("{\"command\":");
        call.append_arguments("\"pwd\"}");

        let event = call.finish("Anthropic").unwrap();

        match event {
            ModelEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, ToolCallId::from_provider("toolu_123"));
                assert_eq!(name, "bash");
                assert_eq!(arguments, json!({"command": "pwd"}));
            }
            _ => panic!("expected a tool call event"),
        }
    }

    #[test]
    fn finish_names_the_protocol_in_errors() {
        let mut invalid = PendingCall::new("id", "bash");
        invalid.append_arguments("{\"command\":");
        let invalid = invalid.finish("Anthropic");

        match invalid {
            Err(ProtocolError::InvalidResponse(message)) => {
                assert!(message.contains("Anthropic"));
                assert!(message.contains("invalid Anthropic tool arguments"));
            }
            _ => panic!("expected an invalid response error"),
        }

        let unnamed = PendingCall::new("id", "").finish("Chat Completions");

        match unnamed {
            Err(ProtocolError::InvalidResponse(message)) => {
                assert!(message.contains("Chat Completions"));
            }
            _ => panic!("expected an invalid response error"),
        }
    }

    #[test]
    fn item_placeholders_do_not_erase_accumulated_arguments() {
        let mut call = PendingCall::new("call_1", "read");
        call.set_arguments(r#"{"path":"Cargo.toml"}"#);

        call.apply_item(&json!({"arguments": ""}), false);

        assert_eq!(
            call.finish("Responses").unwrap(),
            ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_1"),
                name: "read".to_string(),
                arguments: json!({"path": "Cargo.toml"}),
            }
        );
    }

    #[test]
    fn complete_items_replace_accumulated_arguments() {
        let mut call = PendingCall::new("call_1", "read");
        call.set_arguments(r#"{"path":"old"}"#);

        call.apply_item(
            &json!({
                "call_id": "call_2",
                "name": "write",
                "arguments": ""
            }),
            true,
        );

        assert_eq!(
            call.finish("Responses").unwrap(),
            ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_2"),
                name: "write".to_string(),
                arguments: json!({}),
            }
        );
    }

    #[test]
    fn nonempty_item_arguments_replace_accumulated_fragments() {
        let mut call = PendingCall::new("call_1", "read");
        call.append_arguments(r#"{"path":"old"}"#);

        call.apply_item(&json!({"arguments": r#"{"path":"new"}"#}), false);

        assert_eq!(
            call.finish("Responses").unwrap(),
            ModelEvent::ToolCall {
                id: ToolCallId::from_provider("call_1"),
                name: "read".to_string(),
                arguments: json!({"path": "new"}),
            }
        );
    }

    #[test]
    fn accumulator_drains_calls_in_key_order() {
        let mut accumulator = PendingCallAccumulator::default();
        accumulator.entry(1).set_name("second");
        accumulator.entry(0).set_name("first");

        let names = accumulator
            .drain()
            .map(|(_, call)| call.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn usage_and_stop_reason_helpers_are_provider_neutral() {
        let usage = build_usage(120, 25);
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.generation_ms, 0);
        assert!(!usage.estimated);

        assert_eq!(stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(stop_reason("max_output_tokens"), StopReason::MaxTokens);
        assert_eq!(stop_reason("stop"), StopReason::EndTurn);
    }
}
