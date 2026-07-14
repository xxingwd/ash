use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use similar::TextDiff;

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// File path to edit
    path: String,
    /// Exact text to search for (must match uniquely)
    old: String,
    /// Replacement text
    new: String,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "edit",
        "Edit a file by search/replace. The old text must match uniquely.",
        |ctx, args: EditArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;

            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!("cannot read {}: {e}", path.display()))
            })?;

            let count = content.matches(&args.old).count();
            if count == 0 {
                return Err(ToolError::Execution("search text not found in file".into()));
            }
            if count > 1 {
                return Err(ToolError::Execution(format!(
                    "search text matches {count} locations, must be unique"
                )));
            }

            let new_content = content.replacen(&args.old, &args.new, 1);

            let diff = TextDiff::from_lines(&content, &new_content);
            let unified = diff.unified_diff().header("before", "after").to_string();

            tokio::fs::write(&path, &new_content).await.map_err(|e| {
                ToolError::Execution(format!("cannot write {}: {e}", path.display()))
            })?;

            Ok(unified)
        },
    )
}
