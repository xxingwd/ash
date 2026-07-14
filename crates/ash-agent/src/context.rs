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

    let mut kept = Vec::new();
    let mut used = 0;

    for msg in non_system.iter().rev() {
        let msg_tokens = TOKENS_PER_MESSAGE_OVERHEAD + count_message_tokens(msg, bpe);
        if used + msg_tokens > budget {
            break;
        }
        used += msg_tokens;
        kept.push(msg.clone());
    }

    kept.reverse();

    let dropped = non_system.len() - kept.len();
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
