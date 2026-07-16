use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_READ_LIMIT: usize = 2_000;
const MAX_READ_LIMIT: usize = 2_000;

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// Existing file path within the workspace
    path: String,
    /// Start line (1-indexed)
    offset: Option<usize>,
    /// Number of lines to read, capped at 2000
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "read",
        "Read up to 2000 lines from an existing text file",
        |ctx, args: ReadArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;

            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!("cannot read {}: {e}", path.display()))
            })?;

            Ok(render_range(&content, args.offset, args.limit))
        },
    )
}

fn render_range(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    let start = offset.unwrap_or(1).max(1).saturating_sub(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT);
    let selected = lines.iter().skip(start).take(limit).collect::<Vec<_>>();
    let mut result = String::new();
    for (index, line) in selected.iter().enumerate() {
        result.push_str(&format!("{:>4}\t{line}\n", start + index + 1));
    }
    let next = start.saturating_add(selected.len());
    if next < lines.len() {
        result.push_str(&format!(
            "\nShowing lines {}-{} of {}. Continue with offset={}.\n",
            start + 1,
            next,
            lines.len(),
            next + 1
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_default_reads_and_reports_the_next_offset() {
        let content = (1..=2_001)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_range(&content, None, None);

        assert!(rendered.contains("2000\tline 2000"));
        assert!(!rendered.contains("2001\tline 2001"));
        assert!(rendered.contains("Continue with offset=2001"));
    }

    #[test]
    fn keeps_offsets_one_based() {
        let rendered = render_range("one\ntwo\nthree", Some(2), Some(1));

        assert!(rendered.starts_with("   2\ttwo\n"));
        assert!(rendered.contains("Continue with offset=3"));
    }
}
