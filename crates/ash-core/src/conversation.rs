use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Content, StopReason, ToolCallId, ToolOutput, TurnId};

const MAX_COMPACT_TOOL_OUTPUT_CHARS: usize = 8 * 1024;
const COMPACT_TOOL_OUTPUT_TRUNCATED: &str = "\n... tool output truncated ...";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub content: Vec<Content>,
}

impl Input {
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(text.into())],
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            || self.content.iter().all(|content| match content {
                Content::Text(text) => text.trim().is_empty(),
                Content::Image { data, .. } => data.is_empty(),
            })
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .map(Content::display_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.content.iter().find_map(|content| match content {
            Content::Text(text) if !text.trim().is_empty() => Some(text.as_str()),
            Content::Text(_) | Content::Image { .. } => None,
        })
    }
}

impl From<String> for Input {
    fn from(value: String) -> Self {
        Self::user(value)
    }
}

impl From<&str> for Input {
    fn from(value: &str) -> Self {
        Self::user(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Result<ToolOutput, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    Text(String),
    Thought { text: String, elapsed_seconds: u64 },
    ToolCall(ToolCall),
}

impl Item {
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Thought { .. } | Self::ToolCall(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub items: Vec<Item>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub generation_ms: u64,
}

impl TurnStats {
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            generation_ms: self.generation_ms.saturating_add(other.generation_ms),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnResult {
    Stopped(StopReason),
    Cancelled,
    Truncated,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub input: Input,
    pub steps: Vec<Step>,
    pub result: TurnResult,
    pub stats: TurnStats,
}

impl Turn {
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.steps.iter().flat_map(|step| &step.items)
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.items().filter_map(|item| match item {
            Item::ToolCall(call) => Some(call),
            Item::Text(_) | Item::Thought { .. } => None,
        })
    }

    #[must_use]
    pub fn has_tools(&self) -> bool {
        self.tool_calls().next().is_some()
    }

    #[must_use]
    pub fn visible_text(&self) -> Option<String> {
        let text = self
            .items()
            .filter_map(Item::text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Checkpoint {
    summary: String,
    tail: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    turns: Vec<Arc<Turn>>,
    checkpoint: Option<Checkpoint>,
}

impl Conversation {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            turns: Vec::new(),
            checkpoint: None,
        }
    }

    #[must_use]
    pub fn turns(&self) -> &[Arc<Turn>] {
        &self.turns
    }

    #[must_use]
    pub fn context(&self) -> ModelContext {
        let tail = self
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.tail);
        ModelContext {
            summary: self
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.clone()),
            turns: self.turns[tail..].to_vec(),
            current: None,
        }
    }

    #[must_use]
    pub fn context_with_summary(&self, summary: &str) -> ModelContext {
        ModelContext {
            summary: Some(summary.to_string()),
            turns: Vec::new(),
            current: None,
        }
    }

    pub fn push(&mut self, turn: Arc<Turn>, summary: Option<String>) {
        if let Some(summary) = summary {
            self.checkpoint = Some(Checkpoint {
                summary,
                tail: self.turns.len(),
            });
        }
        self.turns.push(turn);
    }

    pub fn compact(&mut self, summary: String) {
        self.checkpoint = Some(Checkpoint {
            summary,
            tail: self.turns.len(),
        });
    }

    #[must_use]
    pub fn compact_prompt(&self) -> Option<String> {
        let tail = self
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.tail);
        if tail == self.turns.len() {
            return None;
        }

        let mut prompt = String::new();
        if let Some(checkpoint) = &self.checkpoint {
            prompt.push_str("[Previous summary]\n");
            prompt.push_str(checkpoint.summary.trim());
            prompt.push_str("\n\n");
        }
        for turn in &self.turns[tail..] {
            prompt.push_str("[User]\n");
            prompt.push_str(&turn.input.text());
            prompt.push('\n');
            for step in &turn.steps {
                for item in &step.items {
                    match item {
                        Item::Text(text) => {
                            prompt.push_str("[Assistant]\n");
                            prompt.push_str(text);
                            prompt.push('\n');
                        }
                        Item::Thought { text, .. } => {
                            prompt.push_str("[Thought]\n");
                            prompt.push_str(text);
                            prompt.push('\n');
                        }
                        Item::ToolCall(call) => {
                            prompt.push_str("[Tool call: ");
                            prompt.push_str(&call.name);
                            prompt.push_str("]\nArguments: ");
                            prompt.push_str(&call.arguments.to_string());
                            prompt.push_str("\nResult: ");
                            match &call.result {
                                Ok(output) => push_compact_tool_output(&mut prompt, &output.text),
                                Err(error) => push_compact_tool_output(&mut prompt, error),
                            }
                            prompt.push('\n');
                        }
                    }
                }
            }
            prompt.push('\n');
        }
        (!prompt.trim().is_empty()).then_some(prompt)
    }

    #[must_use]
    pub fn before(&self, turn_id: TurnId) -> Option<(Self, Input)> {
        let index = self.turns.iter().position(|turn| turn.id == turn_id)?;
        Some((
            Self {
                turns: self.turns[..index].to_vec(),
                checkpoint: None,
            },
            self.turns[index].input.clone(),
        ))
    }
}

fn push_compact_tool_output(prompt: &mut String, output: &str) {
    let mut chars = output.chars();
    prompt.extend(chars.by_ref().take(MAX_COMPACT_TOOL_OUTPUT_CHARS));
    if chars.next().is_some() {
        prompt.push_str(COMPACT_TOOL_OUTPUT_TRUNCATED);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelContext {
    summary: Option<String>,
    turns: Vec<Arc<Turn>>,
    current: Option<(Input, Vec<Step>)>,
}

impl ModelContext {
    #[must_use]
    pub fn with_current(mut self, input: Input, steps: Vec<Step>) -> Self {
        self.current = Some((input, steps));
        self
    }

    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    #[must_use]
    pub fn turns(&self) -> &[Arc<Turn>] {
        &self.turns
    }

    #[must_use]
    pub fn current(&self) -> Option<(&Input, &[Step])> {
        self.current
            .as_ref()
            .map(|(input, steps)| (input, steps.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: u128, input: &str) -> Arc<Turn> {
        Arc::new(Turn {
            id: TurnId::from_u128(id),
            input: Input::user(input),
            steps: Vec::new(),
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: TurnStats::default(),
        })
    }

    #[test]
    fn before_returns_an_uncheckpointed_prefix_and_selected_input() {
        let first = turn(1, "first");
        let second = turn(2, "second");
        let third = turn(3, "third");
        let mut conversation = Conversation::new();
        conversation.push(Arc::clone(&first), None);
        conversation.push(Arc::clone(&second), None);
        conversation.compact("summary".to_string());
        conversation.push(Arc::clone(&third), None);

        let (prefix, input) = conversation.before(second.id).unwrap();

        assert_eq!(prefix.turns(), &[first]);
        assert_eq!(prefix.context().summary(), None);
        assert_eq!(input, second.input);
    }

    #[test]
    fn context_only_contains_turns_after_the_checkpoint() {
        let first = turn(1, "first");
        let second = turn(2, "second");
        let mut conversation = Conversation::new();
        conversation.push(first, None);
        conversation.compact("summary".to_string());
        conversation.push(Arc::clone(&second), None);

        let context = conversation.context();

        assert_eq!(context.summary(), Some("summary"));
        assert_eq!(context.turns(), &[second]);
    }

    #[test]
    fn compact_prompt_limits_tool_text_and_omits_attachments() {
        let output = "x".repeat(MAX_COMPACT_TOOL_OUTPUT_CHARS + 1);
        let mut conversation = Conversation::new();
        conversation.push(
            Arc::new(Turn {
                id: TurnId::from_u128(1),
                input: Input::user("run"),
                steps: vec![Step {
                    items: vec![Item::ToolCall(ToolCall {
                        id: ToolCallId::from_provider("call"),
                        name: "read".to_string(),
                        arguments: serde_json::json!({}),
                        result: Ok(ToolOutput::with_attachments(
                            output,
                            vec![Content::Image {
                                media_type: "image/png".to_string(),
                                data: vec![1, 2, 3],
                            }],
                        )),
                    })],
                }],
                result: TurnResult::Stopped(StopReason::EndTurn),
                stats: TurnStats::default(),
            }),
            None,
        );

        let prompt = conversation.compact_prompt().unwrap();

        assert!(prompt.contains(COMPACT_TOOL_OUTPUT_TRUNCATED));
        assert!(!prompt.contains("image/png"));
        assert_eq!(
            prompt.chars().filter(|character| *character == 'x').count(),
            MAX_COMPACT_TOOL_OUTPUT_CHARS
        );
    }
}
