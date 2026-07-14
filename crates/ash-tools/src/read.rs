use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// Absolute file path to read
    path: String,
    /// Start line (1-indexed)
    offset: Option<usize>,
    /// Number of lines to read
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "read",
        "Read file contents with optional line range",
        |ctx, args: ReadArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;

            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!("cannot read {}: {e}", path.display()))
            })?;

            let lines: Vec<&str> = content.lines().collect();
            let offset = args.offset.unwrap_or(1).saturating_sub(1);
            let limit = args.limit.unwrap_or(lines.len());
            let selected: Vec<&str> = lines.into_iter().skip(offset).take(limit).collect();

            let mut result = String::new();
            for (i, line) in selected.iter().enumerate() {
                result.push_str(&format!("{:>4}\t{line}\n", offset + i + 1));
            }
            Ok(result)
        },
    )
}
