use ash_core::{Content, ContentBlock, Message, MessageContent, Role};
use tiktoken_rs::CoreBPE;
use tracing::debug;

const TOKENS_PER_MESSAGE_OVERHEAD: usize = 4;

pub fn count_tokens(messages: &[Message], bpe: &CoreBPE) -> usize {
    let mut total = 0;
    for msg in messages {
        total += TOKENS_PER_MESSAGE_OVERHEAD;
        total += count_message_tokens(msg, bpe);
    }
    total += 3;
    total
}

fn count_message_tokens(msg: &Message, bpe: &CoreBPE) -> usize {
    match &msg.content {
        MessageContent::User(contents) => contents
            .iter()
            .map(|c| match c {
                Content::Text(t) => bpe.encode_ordinary(t).len(),
                Content::Image { .. } => 0,
            })
            .sum(),
        MessageContent::Assistant(blocks) => blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text(t) => bpe.encode_ordinary(t).len(),
                ContentBlock::Thought { .. } => 0,
                ContentBlock::ToolCall {
                    arguments, name, ..
                } => {
                    bpe.encode_ordinary(name).len()
                        + bpe.encode_ordinary(&arguments.to_string()).len()
                }
            })
            .sum(),
        MessageContent::ToolResult { result, .. } => {
            let text = match result {
                Ok(s) => s.as_str(),
                Err(s) => s.as_str(),
            };
            bpe.encode_ordinary(text).len()
        }
    }
}

pub fn compress_if_needed(
    messages: Vec<Message>,
    bpe: &CoreBPE,
    max_tokens: usize,
) -> Vec<Message> {
    let total = count_tokens(&messages, bpe);
    if total <= max_tokens {
        return messages;
    }

    debug!(total_tokens = total, max_tokens, "compressing context");

    let system_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .cloned()
        .collect();
    let non_system: Vec<_> = messages
        .iter()
        .filter(|m| m.role != Role::System)
        .cloned()
        .collect();

    let system_tokens: usize = system_msgs
        .iter()
        .map(|m| TOKENS_PER_MESSAGE_OVERHEAD + count_message_tokens(m, bpe))
        .sum();

    let budget = max_tokens.saturating_sub(system_tokens + 3);

    let mut turns = Vec::new();
    for message in non_system {
        if matches!(&message.content, MessageContent::User(_)) || turns.is_empty() {
            turns.push(Vec::new());
        }
        turns.last_mut().expect("turn exists").push(message);
    }

    let mut kept_turns = Vec::new();
    let mut used = 0;

    for turn in turns.iter().rev() {
        let turn_tokens = turn
            .iter()
            .map(|message| TOKENS_PER_MESSAGE_OVERHEAD + count_message_tokens(message, bpe))
            .sum::<usize>();
        if used + turn_tokens > budget {
            break;
        }
        used += turn_tokens;
        kept_turns.push(turn);
    }

    kept_turns.reverse();
    let kept = kept_turns
        .into_iter()
        .flat_map(|turn| turn.iter().cloned())
        .collect::<Vec<_>>();

    let non_system_count = turns.iter().map(Vec::len).sum::<usize>();
    let dropped = non_system_count - kept.len();
    if dropped > 0 {
        debug!(dropped_messages = dropped, "truncated old messages");
        let summary = Message::assistant_text(&format!(
            "[{dropped} earlier messages truncated to fit context window]"
        ));
        let mut result = system_msgs;
        result.push(summary);
        result.extend(kept);
        result
    } else {
        messages
    }
}

pub fn get_bpe_for_model(model: &str) -> CoreBPE {
    if model.contains("gpt-4o") || model.contains("o1") || model.contains("gpt-4.1") {
        tiktoken_rs::o200k_base().unwrap_or_else(|_| tiktoken_rs::cl100k_base().unwrap())
    } else {
        tiktoken_rs::cl100k_base().unwrap()
    }
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

    fn tool_result(id: ToolCallId) -> Message {
        Message {
            id: MessageId::new(),
            role: Role::User,
            content: MessageContent::ToolResult {
                id,
                result: Ok("contents".to_string()),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn compression_keeps_complete_turns_with_tool_results() {
        let bpe = get_bpe_for_model("test-model");
        let first_call = ToolCallId::from_provider("first");
        let second_call = ToolCallId::from_provider("second");
        let first_turn = vec![
            Message::user("old request"),
            tool_call(&first_call),
            tool_result(first_call),
            Message::assistant_text("old answer"),
        ];
        let second_turn = vec![
            Message::user("new request"),
            tool_call(&second_call),
            tool_result(second_call),
            Message::assistant_text("new answer"),
        ];
        let latest_tokens = second_turn
            .iter()
            .map(|message| TOKENS_PER_MESSAGE_OVERHEAD + count_message_tokens(message, &bpe))
            .sum::<usize>();
        let messages = first_turn.into_iter().chain(second_turn).collect();

        let compressed = compress_if_needed(messages, &bpe, latest_tokens + 3);

        assert_eq!(compressed.len(), 5);
        assert!(matches!(compressed[1].content, MessageContent::User(_)));
        assert!(matches!(
            compressed[2].content,
            MessageContent::Assistant(_)
        ));
        assert!(matches!(
            compressed[3].content,
            MessageContent::ToolResult { .. }
        ));
        assert!(matches!(
            compressed[4].content,
            MessageContent::Assistant(_)
        ));
    }
}
