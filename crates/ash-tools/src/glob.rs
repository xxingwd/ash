use std::{fmt::Write as _, sync::Arc, time::Instant};

use ash_core::{define_tool, CancellationToken, Tool, ToolError};
use ignore::overrides::OverrideBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::path::Workspace;

const MAX_RESULTS: usize = 100;

#[derive(Deserialize, JsonSchema)]
struct GlobArgs {
    /// Glob pattern used to match files
    pattern: String,
    /// Directory to search; defaults to the working directory
    path: Option<String>,
}

pub fn tool(working_dir: Arc<Workspace>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "glob",
        "Find files by glob pattern inside the working directory. Respects ignore files and returns at most 100 workspace-relative paths.",
        move |ctx, args: GlobArgs| {
            let root = Arc::clone(&working_dir);
            let deadline = ctx.require_deadline();
            let cancellation = ctx.cancellation;
            async move {
                let deadline = deadline?;
                crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
                    find_files_in_workspace(
                        &root,
                        args.path.as_deref().unwrap_or("."),
                        &args.pattern,
                        &cancellation,
                        deadline,
                    )
                })
                .await
            }
        },
    )
}

fn find_files_in_workspace(
    workspace: &Workspace,
    requested: &str,
    pattern: &str,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    crate::path::ensure_running(cancellation, deadline)?;
    if pattern.is_empty() {
        return Err(ToolError::Execution("pattern cannot be empty".into()));
    }
    let search = workspace.search_path(requested)?;
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
    let builder = crate::path::file_walker(search.full_path());

    let mut files = Vec::new();
    for entry in builder.build() {
        crate::path::ensure_running(cancellation, deadline)?;
        let entry =
            entry.map_err(|error| ToolError::Execution(format!("cannot search files: {error}")))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !overrides.matched(entry.path(), false).is_whitelist() {
            continue;
        }
        files.push(search.relative(entry.path()));
        // `file_walker` traverses in sorted order, so stopping at
        // `MAX_RESULTS + 1` yields the lexicographically first results without
        // collecting the whole tree; the extra entry only distinguishes
        // "exactly full" from "truncated".
        if files.len() > MAX_RESULTS {
            break;
        }
    }
    files.sort();
    let truncated = files.len() > MAX_RESULTS;
    files.truncate(MAX_RESULTS);
    crate::path::ensure_running(cancellation, deadline)?;

    if files.is_empty() {
        return Ok("No files found".into());
    }
    let mut output = format!("Found {} files", files.len());
    for path in &files {
        let _ = write!(output, "\n{}", path.display());
    }
    if truncated {
        let _ = write!(
            output,
            "\n\n[Results truncated at {MAX_RESULTS} files. Use a narrower path or pattern.]"
        );
    }
    Ok(output)
}

#[cfg(test)]
fn find_files(
    root: &std::path::Path,
    requested: &str,
    pattern: &str,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    let workspace = Workspace::new(root)?;
    find_files_in_workspace(&workspace, requested, pattern, cancellation, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn matches_files_and_respects_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(root.path().join("src/nested/mod.rs"), "").unwrap();
        std::fs::write(root.path().join("src/ignored.rs"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "src/ignored.rs\n").unwrap();

        let output = find_files(
            root.path(),
            ".",
            "*.rs",
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert!(output.contains("Found 2 files"));
        assert!(output.contains("src/lib.rs"));
        assert!(output.contains("src/nested/mod.rs"));
        assert!(!output.contains("ignored.rs"));
    }

    #[test]
    fn rejects_files_as_search_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), "").unwrap();

        assert!(find_files(
            root.path(),
            "file.rs",
            "*.rs",
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .is_err());
    }

    #[test]
    fn truncates_large_result_sets() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_RESULTS {
            std::fs::write(root.path().join(format!("{index:03}.rs")), "").unwrap();
        }

        let output = find_files(
            root.path(),
            ".",
            "*.rs",
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert!(output.starts_with("Found 100 files"));
        assert_eq!(
            output
                .lines()
                .filter(|line| std::path::Path::new(line)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rs")))
                .count(),
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

        let output = find_files(
            root.path(),
            ".",
            "*.rs",
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert_eq!(output, "No files found");
    }

    #[test]
    fn cancelled_search_returns_cancelled() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = find_files(
            root.path(),
            ".",
            "*.rs",
            &cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }
}
