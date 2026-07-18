use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use globset::{GlobBuilder, GlobMatcher};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, DEFAULT_MAX_BYTES};

const DEFAULT_LIMIT: usize = 100;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    /// Search pattern, interpreted as a regular expression unless literal is true
    pattern: String,
    /// File or directory to search; defaults to the working directory
    path: Option<String>,
    /// Glob used to filter files, for example *.ts or **/*.spec.ts
    glob: Option<String>,
    /// Perform a case-insensitive search
    ignore_case: Option<bool>,
    /// Treat pattern as literal text instead of a regular expression
    literal: Option<bool>,
    /// Number of context lines before and after each match
    context: Option<usize>,
    /// Maximum number of matches; defaults to 100
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "grep",
        "Search file contents by regex or literal text. Respects .gitignore and returns matching paths, line numbers, and optional context. Output is limited by match count and 50KB.",
        |ctx, args: GrepArgs| async move {
            let root = crate::path::existing(
                &ctx.working_dir,
                args.path.as_deref().unwrap_or("."),
            )?;
            tokio::task::spawn_blocking(move || search(&root, &args))
                .await
                .map_err(|error| ToolError::Execution(format!("grep task failed: {error}")))?
        },
    )
}

fn search(root: &Path, args: &GrepArgs) -> Result<String, ToolError> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return Err(ToolError::Execution("limit must be at least 1".into()));
    }
    let pattern = if args.literal.unwrap_or(false) {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case.unwrap_or(false))
        .build()
        .map_err(|error| ToolError::Execution(format!("invalid regex: {error}")))?;
    let glob = args.glob.as_deref().map(build_glob).transpose()?;
    let root_is_file = root.is_file();
    let mut matches = 0;
    let mut limit_reached = false;
    let mut lines_truncated = false;
    let mut output = Vec::new();

    'files: for path in crate::path::files(root) {
        if !matches_glob(&path, root, glob.as_ref()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines = content.lines().collect::<Vec<_>>();
        for (line_index, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if matches >= limit {
                limit_reached = true;
                break 'files;
            }
            matches += 1;
            append_match(
                &mut output,
                display_path(&path, root, root_is_file),
                &lines,
                line_index,
                args.context.unwrap_or(0),
                &mut lines_truncated,
            );
        }
    }

    if output.is_empty() {
        return Ok("No matches found".into());
    }
    let truncated = truncate::head(&output.join("\n"), usize::MAX);
    let mut rendered = truncated.content;
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} matches limit reached; increase limit or refine the pattern"
        ));
    }
    if truncated.truncated {
        notices.push(format!(
            "{} output limit reached",
            truncate::format_size(DEFAULT_MAX_BYTES)
        ));
    }
    if lines_truncated {
        notices.push("some lines were truncated; use read for the full line".into());
    }
    if !notices.is_empty() {
        rendered.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(rendered)
}

fn build_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::Execution(format!("invalid glob: {error}")))
}

fn matches_glob(path: &Path, root: &Path, glob: Option<&GlobMatcher>) -> bool {
    let Some(glob) = glob else {
        return true;
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    glob.is_match(relative) || path.file_name().is_some_and(|name| glob.is_match(name))
}

fn display_path(path: &Path, root: &Path, root_is_file: bool) -> String {
    let display = if root_is_file {
        path.file_name().map(Path::new).unwrap_or(path)
    } else {
        path.strip_prefix(root).unwrap_or(path)
    };
    display
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn append_match(
    output: &mut Vec<String>,
    path: String,
    lines: &[&str],
    line_index: usize,
    context: usize,
    lines_truncated: &mut bool,
) {
    let start = line_index.saturating_sub(context);
    let end = line_index
        .saturating_add(context)
        .saturating_add(1)
        .min(lines.len());
    for (index, source) in lines.iter().enumerate().take(end).skip(start) {
        let (line, truncated) = truncate::truncate_line(source);
        *lines_truncated |= truncated;
        let separator = if index == line_index { ':' } else { '-' };
        output.push(format!("{path}{separator}{}{separator} {line}", index + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pattern: &str) -> GrepArgs {
        GrepArgs {
            pattern: pattern.into(),
            path: None,
            glob: None,
            ignore_case: None,
            literal: None,
            context: None,
            limit: None,
        }
    }

    #[test]
    fn respects_gitignore_and_reports_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(root.path().join("kept.txt"), "needle\n").unwrap();

        let output = search(root.path(), &args("needle")).unwrap();

        assert!(output.contains("kept.txt:1: needle"));
        assert!(!output.contains("ignored.txt"));
    }

    #[test]
    fn supports_literal_case_insensitive_search_with_context() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), "before\nA.B\nafter\n").unwrap();
        let mut args = args("a.b");
        args.literal = Some(true);
        args.ignore_case = Some(true);
        args.context = Some(1);

        let output = search(root.path(), &args).unwrap();

        assert!(output.contains("file.txt-1- before"));
        assert!(output.contains("file.txt:2: A.B"));
        assert!(output.contains("file.txt-3- after"));
    }
}
