use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use similar::TextDiff;

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// Existing file path within the workspace
    path: String,
    /// Exact text to search for (must match uniquely)
    old: String,
    /// Replacement text
    new: String,
    /// Replace every exact match instead of requiring one unique match
    #[serde(default)]
    replace_all: bool,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "edit",
        "Edit an existing file by exact search/replace. Matches must be unique unless replace_all is true.",
        |ctx, args: EditArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;

            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!("cannot read {}: {e}", path.display()))
            })?;

            let new_content = replace_exact(&content, &args.old, &args.new, args.replace_all)?;

            let diff = TextDiff::from_lines(&content, &new_content);
            let unified = diff.unified_diff().header("before", "after").to_string();

            tokio::fs::write(&path, &new_content).await.map_err(|e| {
                ToolError::Execution(format!("cannot write {}: {e}", path.display()))
            })?;

            Ok(unified)
        },
    )
}

fn replace_exact(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, ToolError> {
    if old.is_empty() {
        return Err(ToolError::Execution("old text cannot be empty".into()));
    }
    if old == new {
        return Err(ToolError::Execution(
            "old and new text must be different".into(),
        ));
    }
    let count = content.matches(old).count();
    if count == 0 {
        return Err(ToolError::Execution("search text not found in file".into()));
    }
    if !replace_all && count > 1 {
        return Err(ToolError::Execution(format!(
            "search text matches {count} locations; make it unique or set replace_all"
        )));
    }
    Ok(if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_unique_matches_unless_replace_all_is_enabled() {
        assert!(replace_exact("one one", "one", "two", false).is_err());
        assert_eq!(
            replace_exact("one one", "one", "two", true).unwrap(),
            "two two"
        );
    }

    #[test]
    fn rejects_empty_or_unchanged_replacements() {
        assert!(replace_exact("one", "", "two", false).is_err());
        assert!(replace_exact("one", "one", "one", false).is_err());
    }
}
