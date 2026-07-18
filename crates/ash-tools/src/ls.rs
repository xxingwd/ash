use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, DEFAULT_MAX_BYTES};

const DEFAULT_LIMIT: usize = 500;

#[derive(Deserialize, JsonSchema)]
struct LsArgs {
    /// Directory to list; defaults to the working directory
    path: Option<String>,
    /// Maximum number of entries; defaults to 500
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "ls",
        "List a directory, including dotfiles. Entries are sorted alphabetically, directories end with '/', and output is limited by entry count and 50KB.",
        |ctx, args: LsArgs| async move {
            let path = crate::path::existing(
                &ctx.working_dir,
                args.path.as_deref().unwrap_or("."),
            )?;
            list(&path, args.limit).await
        },
    )
}

async fn list(path: &Path, limit: Option<usize>) -> Result<String, ToolError> {
    if !path.is_dir() {
        return Err(ToolError::Execution(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return Err(ToolError::Execution("limit must be at least 1".into()));
    }

    let mut directory = tokio::fs::read_dir(path).await.map_err(|error| {
        ToolError::Execution(format!("cannot read directory {}: {error}", path.display()))
    })?;
    let mut entries = Vec::new();
    while let Some(entry) = directory.next_entry().await.map_err(|error| {
        ToolError::Execution(format!("cannot read directory {}: {error}", path.display()))
    })? {
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            name.push('/');
        }
        entries.push(name);
    }
    entries.sort_by_cached_key(|entry| entry.to_lowercase());

    if entries.is_empty() {
        return Ok("(empty directory)".into());
    }
    let entry_limit_reached = entries.len() > limit;
    entries.truncate(limit);
    let truncated = truncate::head(&entries.join("\n"), usize::MAX);
    let mut output = truncated.content;
    let mut notices = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{limit} entries limit reached; increase limit for more"
        ));
    }
    if truncated.truncated {
        notices.push(format!(
            "{} output limit reached",
            truncate::format_size(DEFAULT_MAX_BYTES)
        ));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn includes_dotfiles_sorts_entries_and_marks_directories() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("z.txt"), "")
            .await
            .unwrap();
        tokio::fs::write(root.path().join(".hidden"), "")
            .await
            .unwrap();
        tokio::fs::create_dir(root.path().join("Alpha"))
            .await
            .unwrap();

        let output = list(root.path(), None).await.unwrap();

        assert_eq!(output, ".hidden\nAlpha/\nz.txt");
    }
}
