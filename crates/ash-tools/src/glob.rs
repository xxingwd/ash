use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use ignore::{overrides::OverrideBuilder, WalkBuilder};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::path::SearchPath;

const MAX_RESULTS: usize = 100;

#[derive(Deserialize, JsonSchema)]
struct GlobArgs {
    /// Glob pattern used to match files
    pattern: String,
    /// Directory to search; defaults to the working directory
    path: Option<String>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "glob",
        "Find files by glob pattern inside the working directory. Respects ignore files and returns at most 100 workspace-relative paths.",
        |ctx, args: GlobArgs| async move {
            let root = ctx.working_dir;
            crate::path::run_blocking(move || {
                find_files(&root, args.path.as_deref().unwrap_or("."), &args.pattern)
            })
            .await
        },
    )
}

fn find_files(root: &Path, requested: &str, pattern: &str) -> Result<String, ToolError> {
    if pattern.is_empty() {
        return Err(ToolError::Execution("pattern cannot be empty".into()));
    }
    let search = SearchPath::new(root, requested)?;
    if !search.full_path().is_dir() {
        return Err(ToolError::Execution(format!(
            "glob path must be a directory: {}",
            search.full_path().display()
        )));
    }

    let mut overrides = OverrideBuilder::new(search.full_path());
    overrides
        .add(pattern)
        .map_err(|error| ToolError::Execution(format!("invalid glob pattern: {error}")))?;
    let overrides = overrides
        .build()
        .map_err(|error| ToolError::Execution(format!("invalid glob pattern: {error}")))?;
    let mut builder = WalkBuilder::new(search.full_path());
    builder
        .follow_links(false)
        .require_git(false)
        .sort_by_file_path(|left, right| left.cmp(right));

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry =
            entry.map_err(|error| ToolError::Execution(format!("cannot search files: {error}")))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !overrides.matched(entry.path(), false).is_whitelist() {
            continue;
        }
        files.push(search.relative(entry.path()));
        if files.len() > MAX_RESULTS {
            break;
        }
    }
    files.sort();
    let truncated = files.len() > MAX_RESULTS;
    files.truncate(MAX_RESULTS);

    if files.is_empty() {
        return Ok("No files found".into());
    }
    let mut output = files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if truncated {
        output.push_str("\n\n[Results truncated at 100 files. Use a narrower path or pattern.]");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_files_and_respects_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(root.path().join("src/nested/mod.rs"), "").unwrap();
        std::fs::write(root.path().join("src/ignored.rs"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "src/ignored.rs\n").unwrap();

        let output = find_files(root.path(), ".", "*.rs").unwrap();

        assert!(output.contains("src/lib.rs"));
        assert!(output.contains("src/nested/mod.rs"));
        assert!(!output.contains("ignored.rs"));
    }

    #[test]
    fn rejects_files_as_search_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), "").unwrap();

        assert!(find_files(root.path(), "file.rs", "*.rs").is_err());
    }

    #[test]
    fn truncates_large_result_sets() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_RESULTS {
            std::fs::write(root.path().join(format!("{index:03}.rs")), "").unwrap();
        }

        let output = find_files(root.path(), ".", "*.rs").unwrap();

        assert_eq!(
            output.lines().filter(|line| line.ends_with(".rs")).count(),
            MAX_RESULTS
        );
        assert!(output.contains("Results truncated at 100 files"));
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();

        let output = find_files(root.path(), ".", "*.rs").unwrap();

        assert_eq!(output, "No files found");
    }
}
