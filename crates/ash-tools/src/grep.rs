use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::{define_tool, Tool, ToolError};
use ignore::{overrides::OverrideBuilder, WalkBuilder};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::path::SearchPath;

const MAX_MATCHES: usize = 100;
const MAX_LINE_CHARS: usize = 500;
const MAX_SEARCH_LINE_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regular expression used to search file contents
    pattern: String,
    /// File or directory to search; defaults to the working directory
    path: Option<String>,
    /// Glob pattern used to include files, such as *.rs or *.{ts,tsx}
    include: Option<String>,
}

#[derive(Debug)]
struct Match {
    path: PathBuf,
    line: usize,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchProgress {
    Complete,
    MatchLimitReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineRead {
    EndOfFile,
    Complete,
    Oversized,
}

pub fn tool(working_dir: Arc<PathBuf>) -> Arc<dyn Tool> {
    define_tool(
        "grep",
        "Search file contents with a regular expression inside the working directory. Optionally filters files by glob and returns at most 100 matching lines.",
        move |_ctx, args: GrepArgs| {
            let root = Arc::clone(&working_dir);
            async move {
            crate::path::run_blocking(move || {
                search(
                    &root,
                    args.path.as_deref().unwrap_or("."),
                    &args.pattern,
                    args.include.as_deref(),
                )
            })
            .await
            }
        },
    )
}

fn search(
    root: &Path,
    requested: &str,
    pattern: &str,
    include: Option<&str>,
) -> Result<String, ToolError> {
    if pattern.is_empty() {
        return Err(ToolError::Execution("pattern cannot be empty".into()));
    }
    let regex = Regex::new(pattern)
        .map_err(|error| ToolError::Execution(format!("invalid regular expression: {error}")))?;
    let search = SearchPath::new(root, requested)?;
    let mut matches = Vec::new();
    let mut progress = SearchProgress::Complete;

    if search.full_path().is_file() {
        let included = match include {
            Some(pattern) => file_matches(search.full_path(), pattern)?,
            None => true,
        };
        if included {
            progress = search_file(&search, search.full_path(), &regex, &mut matches)?;
        }
    } else if search.full_path().is_dir() {
        let mut builder = WalkBuilder::new(search.full_path());
        builder
            .follow_links(false)
            .require_git(false)
            .sort_by_file_path(|left, right| left.cmp(right));
        let include = if let Some(pattern) = include {
            let mut overrides = OverrideBuilder::new(search.full_path());
            overrides.add(pattern).map_err(|error| {
                ToolError::Execution(format!("invalid include pattern: {error}"))
            })?;
            Some(overrides.build().map_err(|error| {
                ToolError::Execution(format!("invalid include pattern: {error}"))
            })?)
        } else {
            None
        };
        for entry in builder.build() {
            let entry = entry
                .map_err(|error| ToolError::Execution(format!("cannot search files: {error}")))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if include
                .as_ref()
                .is_some_and(|matcher| !matcher.matched(entry.path(), false).is_whitelist())
            {
                continue;
            }
            if search_file(&search, entry.path(), &regex, &mut matches)?
                == SearchProgress::MatchLimitReached
            {
                progress = SearchProgress::MatchLimitReached;
                break;
            }
        }
    } else {
        return Err(ToolError::Execution(format!(
            "grep path is not a file or directory: {}",
            search.full_path().display()
        )));
    }

    if matches.is_empty() {
        return Ok("No matches found".into());
    }
    matches.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line.cmp(&right.line))
    });
    let mut output = format!(
        "Found {} matching lines{}",
        matches.len(),
        if progress == SearchProgress::MatchLimitReached {
            " (more available)"
        } else {
            ""
        }
    );
    let mut current = None;
    for found in matches {
        if current.as_ref() != Some(&found.path) {
            output.push_str(&format!("\n\n{}:", found.path.display()));
            current = Some(found.path.clone());
        }
        output.push_str(&format!("\n  Line {}: {}", found.line, found.text));
    }
    if progress == SearchProgress::MatchLimitReached {
        output.push_str(
            "\n\n[Results truncated at 100 matching lines. Use a narrower path, pattern, or include glob.]",
        );
    }
    Ok(output)
}

fn file_matches(path: &Path, pattern: &str) -> Result<bool, ToolError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut overrides = OverrideBuilder::new(parent);
    overrides
        .add(pattern)
        .map_err(|error| ToolError::Execution(format!("invalid include pattern: {error}")))?;
    let overrides = overrides
        .build()
        .map_err(|error| ToolError::Execution(format!("invalid include pattern: {error}")))?;
    Ok(overrides.matched(path, false).is_whitelist())
}

fn search_file(
    search: &SearchPath,
    path: &Path,
    regex: &Regex,
    matches: &mut Vec<Match>,
) -> Result<SearchProgress, ToolError> {
    let file = File::open(path).map_err(|error| {
        ToolError::Execution(format!("cannot read {}: {error}", path.display()))
    })?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut line = 0;
    loop {
        let line_read = read_line(&mut reader, &mut buffer).map_err(|error| {
            ToolError::Execution(format!("cannot read {}: {error}", path.display()))
        })?;
        if line_read == LineRead::EndOfFile {
            return Ok(SearchProgress::Complete);
        }
        if buffer.contains(&0) {
            return Ok(SearchProgress::Complete);
        }
        line += 1;
        if line_read == LineRead::Oversized {
            continue;
        }
        while matches!(buffer.last(), Some(b'\n' | b'\r')) {
            buffer.pop();
        }
        let text = String::from_utf8_lossy(&buffer);
        if !regex.is_match(&text) {
            continue;
        }
        if matches.len() == MAX_MATCHES {
            return Ok(SearchProgress::MatchLimitReached);
        }
        let text = text.chars().take(MAX_LINE_CHARS).collect::<String>();
        matches.push(Match {
            path: search.relative(path),
            line,
            text,
        });
    }
}

fn read_line(reader: &mut impl BufRead, output: &mut Vec<u8>) -> std::io::Result<LineRead> {
    output.clear();
    let mut read_any = false;
    let mut truncated = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if !read_any {
                LineRead::EndOfFile
            } else if truncated {
                LineRead::Oversized
            } else {
                LineRead::Complete
            });
        }
        read_any = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.map_or(consumed, |index| index);
        let remaining = MAX_SEARCH_LINE_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&available[..content.min(remaining)]);
        truncated |= content > remaining;
        let complete = newline.is_some();
        reader.consume(consumed);
        if complete {
            return Ok(if truncated {
                LineRead::Oversized
            } else {
                LineRead::Complete
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_regexes_and_filters_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "first\nneedle 1\nneedle 2\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.txt"), "needle 3\n").unwrap();

        let output = search(root.path(), "src", r"needle \d", Some("*.rs")).unwrap();

        assert!(output.contains("Found 2 matching lines"));
        assert!(output.contains("src/lib.rs:"));
        assert!(output.contains("Line 2: needle 1"));
        assert!(!output.contains("needle 3"));
    }

    #[test]
    fn supports_exact_file_paths_and_rejects_invalid_regexes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "one\ntwo\n").unwrap();

        let output = search(root.path(), "notes.txt", "two", None).unwrap();

        assert!(output.contains("notes.txt:"));
        assert!(output.contains("Line 2: two"));
        assert!(search(root.path(), ".", "[", None).is_err());
    }

    #[test]
    fn truncates_large_match_sets() {
        let root = tempfile::tempdir().unwrap();
        let content = (0..=MAX_MATCHES)
            .map(|index| format!("match {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.path().join("matches.txt"), content).unwrap();

        let output = search(root.path(), ".", "match", None).unwrap();

        assert!(output.starts_with("Found 100 matching lines (more available)"));
        assert_eq!(
            output.lines().filter(|line| line.contains("Line ")).count(),
            MAX_MATCHES
        );
        assert!(output.contains("Results truncated at 100 matching lines"));
    }

    #[test]
    fn skips_oversized_lines_without_hiding_later_matches() {
        let input = format!("{}\nneedle\n", "x".repeat(MAX_SEARCH_LINE_BYTES + 1));
        let mut reader = BufReader::new(input.as_bytes());
        let mut buffer = Vec::new();

        assert_eq!(
            read_line(&mut reader, &mut buffer).unwrap(),
            LineRead::Oversized
        );
        assert!(buffer.len() <= MAX_SEARCH_LINE_BYTES);
        assert_eq!(
            read_line(&mut reader, &mut buffer).unwrap(),
            LineRead::Complete
        );
        assert_eq!(buffer, b"needle");
        assert_eq!(
            read_line(&mut reader, &mut buffer).unwrap(),
            LineRead::EndOfFile
        );
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();

        let output = search(root.path(), ".", "needle", None).unwrap();

        assert_eq!(output, "No matches found");
    }
}
