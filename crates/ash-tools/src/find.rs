use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use globset::{GlobBuilder, GlobMatcher};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, DEFAULT_MAX_BYTES};

const DEFAULT_LIMIT: usize = 1_000;

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Glob pattern such as *.ts, **/*.json, or src/**/*.spec.ts
    pattern: String,
    /// Search root; defaults to the working directory
    path: Option<String>,
    /// Maximum number of results; defaults to 1000
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "find",
        "Find files by glob pattern. Returns paths relative to the search directory, respects .gitignore, and limits output by result count and 50KB.",
        |ctx, args: FindArgs| async move {
            let root = crate::path::existing(
                &ctx.working_dir,
                args.path.as_deref().unwrap_or("."),
            )?;
            tokio::task::spawn_blocking(move || find(&root, &args))
                .await
                .map_err(|error| ToolError::Execution(format!("find task failed: {error}")))?
        },
    )
}

fn find(root: &Path, args: &FindArgs) -> Result<String, ToolError> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return Err(ToolError::Execution("limit must be at least 1".into()));
    }
    let glob = build_glob(&args.pattern)?;
    let match_path = args.pattern.contains('/');
    let root_is_file = root.is_file();
    let mut results = crate::path::files(root)
        .filter_map(|path| {
            let relative = if root_is_file {
                path.file_name().map(Path::new).unwrap_or(path.as_path())
            } else {
                path.strip_prefix(root).unwrap_or(path.as_path())
            };
            let candidate = if match_path {
                relative
            } else {
                path.file_name().map(Path::new).unwrap_or(relative)
            };
            glob.is_match(candidate).then(|| {
                relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            })
        })
        .take(limit.saturating_add(1))
        .collect::<Vec<_>>();
    results.sort();

    if results.is_empty() {
        return Ok("No files found matching pattern".into());
    }
    let result_limit_reached = results.len() > limit;
    results.truncate(limit);
    let truncated = truncate::head(&results.join("\n"), usize::MAX);
    let mut output = truncated.content;
    let mut notices = Vec::new();
    if result_limit_reached {
        notices.push(format!(
            "{limit} results limit reached; increase limit or refine the pattern"
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

fn build_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::Execution(format!("invalid glob: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_basename_patterns_recursively_and_respects_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(root.path().join("ignored.rs"), "").unwrap();
        let args = FindArgs {
            pattern: "*.rs".into(),
            path: None,
            limit: None,
        };

        let output = find(root.path(), &args).unwrap();

        assert_eq!(output, "src/lib.rs");
    }

    #[test]
    fn supports_path_globs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/nested/file.json"), "").unwrap();
        let args = FindArgs {
            pattern: "src/**/*.json".into(),
            path: None,
            limit: None,
        };

        assert_eq!(find(root.path(), &args).unwrap(), "src/nested/file.json");
    }
}
