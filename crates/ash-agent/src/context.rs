use std::collections::HashSet;

use ash_core::{Content, ContentBlock, Message, MessageContent, Role, ToolCallId, ToolDefinition};

use crate::{agent::COMPACTION_TRIGGER_PERCENT, skill::SKILL_TOOL_NAME};

const CHARS_PER_TOKEN: usize = 4;
const TOKENS_PER_MESSAGE_OVERHEAD: usize = 4;
const DEFAULT_TAIL_TURNS: usize = 2;
const MIN_PRESERVE_RECENT_TOKENS: usize = 2_000;
const MAX_PRESERVE_RECENT_TOKENS: usize = 8_000;
const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
const PRUNE_MINIMUM_TOKENS: usize = 20_000;
const PRUNE_PROTECT_TOKENS: usize = 40_000;
const PRUNED_TOOL_OUTPUT: &str = "[Old tool output cleared to reduce context]";
pub(crate) const SUMMARY_MAX_OUTPUT_TOKENS: u32 = 4_096;
const SUMMARY_PREFIX: &str = "<context-summary>\n";
const SUMMARY_SUFFIX: &str = "\n</context-summary>";
const OMITTED_HISTORY_MARKER: &str = "\n\n[older serialized history omitted]\n\n";

const SUMMARY_TEMPLATE: &str = r#"Output exactly this Markdown structure and keep the section order unchanged.

## Objective
- [what the user is trying to accomplish]

## Important Details
- [constraints, decisions, assumptions, exact identifiers, or "(none)"]

## Work State
### Completed
- [finished work and verified facts, or "(none)"]

### Active
- [current work and partial changes, or "(none)"]

### Blocked
- [blockers and unknowns, or "(none)"]

## Next Move
1. [immediate concrete action, or "(none)"]
2. [next action if known, or "(none)"]

## Relevant Files
- [exact path and why it matters, or "(none)"]

Keep every section. Use terse bullets. Preserve exact paths, symbols, commands, errors, URLs, and identifiers. Do not mention compaction or the summary process."#;

#[derive(Clone, Copy)]
enum TextSerialization {
    Full,
    Truncated,
}

#[derive(Clone, Copy)]
enum ThoughtAccounting {
    Exclude,
    Include,
}

#[derive(Debug)]
pub(crate) struct CompactionPlan {
    pub(crate) summary_prompt: String,
    pub(crate) tail: Vec<Message>,
    pub(crate) compacted_messages: usize,
}

#[derive(Default)]
struct CompactionHistory {
    previous_summary: Option<String>,
    messages: Vec<Message>,
}

pub(crate) fn estimate_tokens(input: &str) -> usize {
    estimate_character_count(character_units(input))
}

fn estimate_character_count(characters: usize) -> usize {
    characters.saturating_add(CHARS_PER_TOKEN / 2) / CHARS_PER_TOKEN
}

fn character_units(input: &str) -> usize {
    input.encode_utf16().count()
}

pub(crate) fn count_tokens(messages: &[Message]) -> usize {
    let content = messages
        .iter()
        .map(|message| message_characters(message, ThoughtAccounting::Exclude))
        .fold(0usize, usize::saturating_add);
    estimate_character_count(content)
        .saturating_add(messages.len().saturating_mul(TOKENS_PER_MESSAGE_OVERHEAD))
        .saturating_add(3)
}

pub fn count_output_tokens(message: &Message) -> usize {
    estimate_character_count(message_characters(message, ThoughtAccounting::Include))
}

/// Estimate the token count of a full request: system prompt + tools + messages.
///
/// This is the single estimation entry point used both at runtime (when the
/// API does not report usage) and when recomputing the current context size
/// after restore, rollback, or fork.
pub(crate) fn estimate_request_tokens(
    system_prompt: Option<&str>,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> usize {
    let system_tokens = system_prompt.map_or(0, estimate_tokens);
    let tool_tokens = serde_json::to_string(tools)
        .ok()
        .map_or(0, |tools| estimate_tokens(&tools));
    count_tokens(messages)
        .saturating_add(system_tokens)
        .saturating_add(tool_tokens)
}

pub(crate) fn compaction_threshold(max_context_tokens: usize) -> usize {
    max_context_tokens.saturating_mul(COMPACTION_TRIGGER_PERCENT) / 100
}

pub(crate) fn summary_output_tokens(max_context_tokens: usize) -> u32 {
    let input_fraction = u32::try_from(max_context_tokens / 5).unwrap_or(u32::MAX);
    SUMMARY_MAX_OUTPUT_TOKENS.min(input_fraction.max(1))
}

pub(crate) fn needs_compaction(estimated_tokens: usize, max_context_tokens: usize) -> bool {
    estimated_tokens >= compaction_threshold(max_context_tokens)
}

pub(crate) fn prune_tool_outputs(messages: &[Message]) -> Option<Vec<Message>> {
    let protected = protected_tool_calls(messages);
    let mut turns = 0usize;
    let mut retained_tokens = 0usize;
    let mut pruned_tokens = 0usize;
    let mut candidates = Vec::new();

    for (index, message) in messages.iter().enumerate().rev() {
        if extract_summary(message).is_some() {
            break;
        }
        if message.is_user_turn() {
            turns = turns.saturating_add(1);
        }
        if turns < DEFAULT_TAIL_TURNS {
            continue;
        }
        let MessageContent::ToolResult { id, result, .. } = &message.content else {
            continue;
        };
        if protected.contains(id) || result.is_err() {
            continue;
        }
        let tokens = count_tokens(std::slice::from_ref(message));
        retained_tokens = retained_tokens.saturating_add(tokens);
        if retained_tokens <= PRUNE_PROTECT_TOKENS {
            continue;
        }
        pruned_tokens = pruned_tokens.saturating_add(tokens);
        candidates.push(index);
    }

    if pruned_tokens <= PRUNE_MINIMUM_TOKENS {
        return None;
    }
    let mut pruned = messages.to_vec();
    for index in candidates {
        let MessageContent::ToolResult {
            result,
            attachments,
            ..
        } = &mut pruned[index].content
        else {
            continue;
        };
        *result = Ok(PRUNED_TOOL_OUTPUT.to_string());
        attachments.clear();
    }
    Some(pruned)
}

fn protected_tool_calls(messages: &[Message]) -> HashSet<ToolCallId> {
    messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::Assistant(blocks) => Some(blocks),
            MessageContent::User(_) | MessageContent::ToolResult { .. } => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, .. } if name == SKILL_TOOL_NAME => Some(id.clone()),
            ContentBlock::Text(_)
            | ContentBlock::Thought { .. }
            | ContentBlock::ToolCall { .. } => None,
        })
        .collect()
}

pub(crate) fn plan_compaction(
    messages: &[Message],
    max_context_tokens: usize,
) -> Option<CompactionPlan> {
    let CompactionHistory {
        previous_summary,
        messages: history,
    } = compaction_history(messages);
    let split = recent_tail_start(&history, max_context_tokens);
    let head = &history[..split];
    if head.is_empty() {
        return None;
    }
    let serialized = serialize_messages(head);
    if serialized.trim().is_empty() && previous_summary.is_none() {
        return None;
    }
    let summary_prompt =
        fit_summary_prompt(previous_summary.as_deref(), &serialized, max_context_tokens);

    Some(CompactionPlan {
        summary_prompt,
        tail: history[split..].to_vec(),
        compacted_messages: head.len(),
    })
}

fn compaction_history(messages: &[Message]) -> CompactionHistory {
    messages
        .iter()
        .fold(CompactionHistory::default(), |mut history, message| {
            if let Some(summary) = extract_summary(message) {
                history.previous_summary = Some(summary.to_string());
            } else {
                history.messages.push(message.clone());
            }
            history
        })
}

fn recent_tail_start(messages: &[Message], max_context_tokens: usize) -> usize {
    let recent_budget = preserve_recent_budget(max_context_tokens);
    let mut recent_turns = user_turns(messages).into_iter().rev();
    let Some((latest_start, latest_end)) = recent_turns.next() else {
        return messages.len();
    };
    let mut used = count_tokens(&messages[latest_start..latest_end]);
    let mut tail_start = latest_start;
    for (start, end) in recent_turns.take(DEFAULT_TAIL_TURNS.saturating_sub(1)) {
        let turn_tokens = count_tokens(&messages[start..end]);
        if used.saturating_add(turn_tokens) > recent_budget {
            break;
        }
        used = used.saturating_add(turn_tokens);
        tail_start = start;
    }
    tail_start
}

pub(crate) fn apply_summary(summary: &str, tail: Vec<Message>) -> Vec<Message> {
    let mut messages = Vec::with_capacity(tail.len() + 1);
    messages.push(Message::assistant_text(&format!(
        "{SUMMARY_PREFIX}{}{SUMMARY_SUFFIX}",
        summary.trim()
    )));
    messages.extend(tail);
    messages
}

fn extract_summary(message: &Message) -> Option<&str> {
    let MessageContent::Assistant(blocks) = &message.content else {
        return None;
    };
    let [ContentBlock::Text(text)] = blocks.as_slice() else {
        return None;
    };
    text.strip_prefix(SUMMARY_PREFIX)?
        .strip_suffix(SUMMARY_SUFFIX)
}

fn preserve_recent_budget(max_context_tokens: usize) -> usize {
    (compaction_threshold(max_context_tokens) / 4)
        .clamp(MIN_PRESERVE_RECENT_TOKENS, MAX_PRESERVE_RECENT_TOKENS)
}

fn user_turns(messages: &[Message]) -> Vec<(usize, usize)> {
    let mut turns = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| message.is_user_turn().then_some((index, messages.len())))
        .collect::<Vec<_>>();
    for index in 0..turns.len().saturating_sub(1) {
        turns[index].1 = turns[index + 1].0;
    }
    turns
}

fn fit_summary_prompt(
    previous_summary: Option<&str>,
    serialized_history: &str,
    max_context_tokens: usize,
) -> String {
    let prompt = build_summary_prompt(previous_summary, serialized_history);
    let output_reserve =
        usize::try_from(summary_output_tokens(max_context_tokens)).unwrap_or(usize::MAX);
    let prompt_budget = max_context_tokens.saturating_sub(output_reserve);
    if estimate_tokens(&prompt) <= prompt_budget {
        return prompt;
    }

    let fixed = build_summary_prompt(previous_summary, "");
    let fixed_tokens = estimate_tokens(&fixed);
    let history_char_budget = prompt_budget
        .saturating_sub(fixed_tokens)
        .saturating_mul(CHARS_PER_TOKEN);
    build_summary_prompt(
        previous_summary,
        &truncate_middle(serialized_history, history_char_budget),
    )
}

fn build_summary_prompt(previous_summary: Option<&str>, serialized_history: &str) -> String {
    let instruction = previous_summary.map_or_else(
        || "Create a new anchored summary from the conversation history.".to_string(),
        |summary| {
            format!(
                "Update the anchored summary below. Preserve still-true details, remove stale details, and merge new facts.\n\n<previous-summary>\n{summary}\n</previous-summary>"
            )
        },
    );
    format!(
        "{instruction}\n\n{SUMMARY_TEMPLATE}\n\n<conversation-history>\n{serialized_history}\n</conversation-history>"
    )
}

fn serialize_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(serialize_message)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn serialize_message(message: &Message) -> String {
    match &message.content {
        MessageContent::User(contents) => {
            let label = if message.role == Role::System {
                "System update"
            } else {
                "User"
            };
            format!(
                "[{label}]: {}",
                serialize_contents(contents, TextSerialization::Full)
            )
        }
        MessageContent::Assistant(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(format!("[Assistant]: {text}")),
                ContentBlock::Thought { text, .. } if !text.is_empty() => {
                    Some(format!("[Assistant reasoning]: {text}"))
                }
                ContentBlock::Thought { .. } => None,
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => Some(format!("[Assistant tool call]: {name}({arguments})")),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::ToolResult {
            result,
            attachments,
            ..
        } => {
            let (label, output) = match result {
                Ok(output) => ("Tool result", output),
                Err(error) => ("Tool error", error),
            };
            let output = truncate_chars(output, TOOL_OUTPUT_MAX_CHARS);
            let attachments = serialize_contents(attachments, TextSerialization::Truncated);
            if attachments.is_empty() {
                format!("[{label}]: {output}")
            } else {
                format!("[{label}]: {output}\n{attachments}")
            }
        }
    }
}

fn serialize_contents(contents: &[Content], mode: TextSerialization) -> String {
    contents
        .iter()
        .map(|content| match content {
            Content::Text(text) => match mode {
                TextSerialization::Full => text.clone(),
                TextSerialization::Truncated => truncate_chars(text, TOOL_OUTPUT_MAX_CHARS),
            },
            Content::Image { media_type, data } => {
                format!("[Attached {media_type}: {} bytes omitted]", data.len())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut result = value.chars().take(max_chars).collect::<String>();
    result.push_str("\n[truncated]");
    result
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= OMITTED_HISTORY_MARKER.chars().count() {
        return value.chars().take(max_chars).collect();
    }
    let content = max_chars - OMITTED_HISTORY_MARKER.chars().count();
    let head = content / 4;
    let tail = content - head;
    let prefix = value.chars().take(head).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}{OMITTED_HISTORY_MARKER}{suffix}")
}

fn message_characters(message: &Message, thoughts: ThoughtAccounting) -> usize {
    match &message.content {
        MessageContent::User(contents) => content_characters(contents),
        MessageContent::Assistant(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => character_units(text),
                ContentBlock::Thought { text, .. }
                    if matches!(thoughts, ThoughtAccounting::Include) =>
                {
                    character_units(text)
                }
                ContentBlock::Thought { .. } => 0,
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => character_units(name).saturating_add(character_units(&arguments.to_string())),
            })
            .fold(0usize, usize::saturating_add),
        MessageContent::ToolResult {
            result,
            attachments,
            ..
        } => {
            // Both outcomes are character-counted the same way; take the
            // payload first so the conversion is written once.
            let payload = match result.as_ref() {
                Ok(output) => output,
                Err(error) => error,
            };
            character_units(payload).saturating_add(content_characters(attachments))
        }
    }
}

fn content_characters(contents: &[Content]) -> usize {
    contents
        .iter()
        .map(|content| match content {
            Content::Text(text) => character_units(text),
            Content::Image { media_type, data } => character_units(media_type)
                .saturating_add(data.len().saturating_mul(4).saturating_add(2) / 3),
        })
        .fold(0usize, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use ash_core::{MessageId, ToolCallId};

    use super::*;

    fn tool_call(id: &ToolCallId) -> Message {
        Message {
            id: MessageId::new(),
            role: Role::Assistant,
            content: MessageContent::Assistant(vec![ContentBlock::ToolCall {
                id: id.clone(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "Cargo.toml"}),
            }]),
        }
    }

    fn tool_result(id: ToolCallId, output: String, attachments: Vec<Content>) -> Message {
        Message {
            id: MessageId::new(),
            role: Role::User,
            content: MessageContent::ToolResult {
                id,
                result: Ok(output),
                attachments,
            },
        }
    }

    #[test]
    fn estimates_one_token_per_four_characters() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("12345"), 1);
        assert_eq!(estimate_tokens("123456"), 2);
        assert_eq!(estimate_tokens("你好世界"), 1);
        assert_eq!(estimate_tokens("😀"), 1);
    }

    #[test]
    fn compaction_starts_at_eighty_percent() {
        assert_eq!(compaction_threshold(200_000), 160_000);
        assert!(!needs_compaction(159_999, 200_000));
        assert!(needs_compaction(160_000, 200_000));
    }

    #[test]
    fn compaction_plan_keeps_two_recent_complete_turns() {
        let first_call = ToolCallId::from_provider("first");
        let second_call = ToolCallId::from_provider("second");
        let messages = vec![
            Message::user("old request"),
            Message::assistant_text("old answer"),
            Message::user("middle request"),
            tool_call(&first_call),
            tool_result(first_call, "middle output".into(), Vec::new()),
            Message::assistant_text("middle answer"),
            Message::user("new request"),
            tool_call(&second_call),
            tool_result(second_call, "new output".into(), Vec::new()),
            Message::assistant_text("new answer"),
        ];

        let plan = plan_compaction(&messages, 200_000).unwrap();

        assert_eq!(plan.compacted_messages, 2);
        assert_eq!(plan.tail.len(), 8);
        assert!(matches!(plan.tail[0].content, MessageContent::User(_)));
        assert!(matches!(
            plan.tail[2].content,
            MessageContent::ToolResult { .. }
        ));
        assert!(matches!(plan.tail[4].content, MessageContent::User(_)));
        assert!(matches!(
            plan.tail[6].content,
            MessageContent::ToolResult { .. }
        ));
    }

    #[test]
    fn compaction_plan_always_keeps_the_latest_user_turn() {
        let messages = vec![
            Message::user("old request"),
            Message::assistant_text("old answer"),
            Message::user(&"x".repeat(20_000)),
        ];

        let plan = plan_compaction(&messages, 1_000).unwrap();

        assert_eq!(plan.compacted_messages, 2);
        assert_eq!(plan.tail.len(), 1);
        assert!(matches!(&plan.tail[0].content, MessageContent::User(_)));
    }

    #[test]
    fn summary_prompt_truncates_tool_output_and_strips_images() {
        let call = ToolCallId::from_provider("call");
        let marker = "SECRET_END";
        let messages = vec![
            Message::user("old"),
            tool_result(
                call,
                format!("{}{marker}", "x".repeat(3_000)),
                vec![Content::Image {
                    media_type: "image/png".into(),
                    data: vec![7; 4096],
                }],
            ),
            Message::user("recent one"),
            Message::assistant_text("answer one"),
            Message::user("recent two"),
            Message::assistant_text("answer two"),
        ];

        let plan = plan_compaction(&messages, 200_000).unwrap();

        assert!(!plan.summary_prompt.contains(marker));
        assert!(!plan.summary_prompt.contains("7,7,7"));
        assert!(plan
            .summary_prompt
            .contains("[Attached image/png: 4096 bytes omitted]"));
        assert!(plan.summary_prompt.contains("[truncated]"));
    }

    #[test]
    fn pruning_protects_recent_tool_output_and_clears_large_older_results() {
        let mut messages = Vec::new();
        for turn in 0..7 {
            let call = ToolCallId::from_provider(format!("call-{turn}"));
            messages.push(Message::user(&format!("request {turn}")));
            messages.push(tool_call(&call));
            messages.push(tool_result(call, "x".repeat(64_000), Vec::new()));
            messages.push(Message::assistant_text(&format!("answer {turn}")));
        }

        let pruned = prune_tool_outputs(&messages).unwrap();
        let outputs = pruned
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::ToolResult {
                    result: Ok(output), ..
                } => Some(output.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(outputs.len(), 7);
        assert_eq!(
            outputs
                .iter()
                .filter(|output| **output == PRUNED_TOOL_OUTPUT)
                .count(),
            3
        );
        assert_eq!(outputs[5].len(), 64_000);
        assert_eq!(outputs[6].len(), 64_000);
    }

    #[test]
    fn repeated_compaction_updates_the_previous_summary() {
        let messages = vec![
            apply_summary("previous facts", Vec::new()).remove(0),
            Message::user("older turn"),
            Message::assistant_text("older answer"),
            Message::user("recent one"),
            Message::assistant_text("one"),
            Message::user("recent two"),
            Message::assistant_text("two"),
        ];

        let plan = plan_compaction(&messages, 200_000).unwrap();

        assert!(plan.summary_prompt.contains("<previous-summary>"));
        assert!(plan.summary_prompt.contains("previous facts"));
        assert_eq!(plan.tail.len(), 4);
    }

    #[test]
    fn output_estimate_includes_visible_reasoning() {
        let message = Message {
            id: MessageId::new(),
            role: Role::Assistant,
            content: MessageContent::Assistant(vec![
                ContentBlock::Thought {
                    text: "reasoning".to_string(),
                    elapsed_seconds: 0,
                },
                ContentBlock::Text("answer".to_string()),
            ]),
        };

        assert!(count_output_tokens(&message) >= 3);
    }
}
