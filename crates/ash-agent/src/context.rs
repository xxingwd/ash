use ash_core::{Content, Input, Item, ModelContext, Step, ToolDefinition};
use std::sync::Arc;

use crate::agent::COMPACTION_TRIGGER_PERCENT;

const BYTES_PER_TOKEN: usize = 4;
const MESSAGE_OVERHEAD: usize = 16;

#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN)
}

#[must_use]
pub fn estimate_request_tokens(
    system: Option<&str>,
    context: &ModelContext,
    tools: &[ToolDefinition],
) -> usize {
    let history_bytes = context
        .turns()
        .iter()
        .map(|turn| input_bytes(&turn.input).saturating_add(steps_bytes(&turn.steps)))
        .chain(
            context
                .current()
                .map(|(input, steps)| input_bytes(input).saturating_add(steps_bytes(steps))),
        )
        .fold(0, usize::saturating_add);
    let tool_bytes = tools
        .iter()
        .map(|tool| {
            tool.name
                .len()
                .saturating_add(tool.description.len())
                .saturating_add(tool.parameters_schema.to_string().len())
        })
        .fold(0, usize::saturating_add);
    system
        .map_or(0, str::len)
        .saturating_add(context.summary().map_or(0, str::len))
        .saturating_add(history_bytes)
        .saturating_add(tool_bytes)
        .div_ceil(BYTES_PER_TOKEN)
        .saturating_add(context.turns().len().saturating_mul(MESSAGE_OVERHEAD))
        .saturating_add(usize::from(context.current().is_some()) * MESSAGE_OVERHEAD)
}

#[must_use]
pub const fn needs_compaction(estimated_tokens: usize, max_context_tokens: usize) -> bool {
    estimated_tokens.saturating_mul(100)
        >= max_context_tokens.saturating_mul(COMPACTION_TRIGGER_PERCENT)
}

fn input_bytes(input: &Input) -> usize {
    input
        .content
        .iter()
        .map(content_bytes)
        .fold(0, usize::saturating_add)
}

fn content_bytes(content: &Content) -> usize {
    match content {
        Content::Text(text) => text.len(),
        Content::Image { media_type, data } => media_type
            .len()
            .saturating_add(data.len().div_ceil(3).saturating_mul(4)),
    }
}

fn steps_bytes(steps: &[Arc<Step>]) -> usize {
    steps
        .iter()
        .flat_map(|step| &step.items)
        .map(|item| match item {
            Item::Text(text) | Item::Thought { text, .. } => text.len(),
            Item::ToolCall(call) => {
                let result = match &call.result {
                    Ok(output) => output.text.len().saturating_add(
                        output
                            .attachments
                            .iter()
                            .map(content_bytes)
                            .fold(0, usize::saturating_add),
                    ),
                    Err(error) => error.len(),
                };
                call.name
                    .len()
                    .saturating_add(call.arguments.to_string().len())
                    .saturating_add(result)
            }
        })
        .fold(0, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("1234"), 1);
        assert_eq!(estimate_tokens("12345"), 2);
    }

    #[test]
    fn text_and_request_estimates_use_the_same_utf8_byte_units() {
        let input = "你好世界";
        let context = ModelContext::default().with_current(Input::user(input), Vec::new());
        assert_eq!(estimate_tokens(input), 3);
        assert_eq!(
            estimate_request_tokens(None, &context, &[]),
            estimate_tokens(input) + MESSAGE_OVERHEAD
        );
    }

    #[test]
    fn compaction_starts_at_eighty_percent() {
        assert!(!needs_compaction(159_999, 200_000));
        assert!(needs_compaction(160_000, 200_000));
    }
}
