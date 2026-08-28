use ash_core::{Content, Input, Item, ModelContext, Step, ToolDefinition};

use crate::agent::COMPACTION_TRIGGER_PERCENT;

const CHARS_PER_TOKEN: usize = 4;
const MESSAGE_OVERHEAD: usize = 16;

#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

#[must_use]
pub fn estimate_request_tokens(
    system: Option<&str>,
    context: &ModelContext,
    tools: &[ToolDefinition],
) -> usize {
    let mut characters = system.map_or(0, str::len);
    characters = characters.saturating_add(context.summary().map_or(0, str::len));
    for turn in context.turns() {
        characters = characters.saturating_add(input_characters(&turn.input));
        characters = characters.saturating_add(steps_characters(&turn.steps));
    }
    if let Some((input, steps)) = context.current() {
        characters = characters.saturating_add(input_characters(input));
        characters = characters.saturating_add(steps_characters(steps));
    }
    characters = characters.saturating_add(
        tools
            .iter()
            .map(|tool| {
                tool.name
                    .len()
                    .saturating_add(tool.description.len())
                    .saturating_add(tool.parameters_schema.to_string().len())
            })
            .sum::<usize>(),
    );
    characters
        .div_ceil(CHARS_PER_TOKEN)
        .saturating_add(context.turns().len().saturating_mul(MESSAGE_OVERHEAD))
        .saturating_add(usize::from(context.current().is_some()) * MESSAGE_OVERHEAD)
}

#[must_use]
pub const fn needs_compaction(estimated_tokens: usize, max_context_tokens: usize) -> bool {
    estimated_tokens.saturating_mul(100)
        >= max_context_tokens.saturating_mul(COMPACTION_TRIGGER_PERCENT)
}

fn input_characters(input: &Input) -> usize {
    input.content.iter().map(content_characters).sum()
}

fn content_characters(content: &Content) -> usize {
    match content {
        Content::Text(text) => text.len(),
        Content::Image { media_type, data } => {
            media_type.len().saturating_add(data.len().div_ceil(3) * 4)
        }
    }
}

fn steps_characters(steps: &[Step]) -> usize {
    steps
        .iter()
        .flat_map(|step| &step.items)
        .map(|item| match item {
            Item::Text(text) | Item::Thought { text, .. } => text.len(),
            Item::ToolCall(call) => {
                let result = match &call.result {
                    Ok(output) => output
                        .text
                        .len()
                        .saturating_add(output.attachments.iter().map(content_characters).sum()),
                    Err(error) => error.len(),
                };
                call.name
                    .len()
                    .saturating_add(call.arguments.to_string().len())
                    .saturating_add(result)
            }
        })
        .sum()
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
    fn compaction_starts_at_eighty_percent() {
        assert!(!needs_compaction(159_999, 200_000));
        assert!(needs_compaction(160_000, 200_000));
    }
}
