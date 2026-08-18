use std::{
    fmt::Write as _,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ash_core::{define_tool, CancellationToken, Tool, ToolError};
use ignore::overrides::OverrideBuilder;
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

pub fn tool(working_dir: Arc<PathBuf>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "grep",
        "Search file contents with a regular expression inside the working directory. Optionally filters files by glob and returns at most 100 matching lines.",
        move |ctx, args: GrepArgs| {
            let root = Arc::clone(&working_dir);
            let cancellation = ctx.cancellation;
            let deadline = ctx.deadline;
            async move {
                crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
                    search(
                        &root,
                        args.path.as_deref().unwrap_or("."),
                        &args.pattern,
                        args.include.as_deref(),
                        &cancellation,
                        deadline,
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
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    crate::path::ensure_running(cancellation, deadline)?;
    if pattern.is_empty() {
        return Err(ToolError::Execution("pattern cannot be empty".into()));
    }
    let regex = Regex::new(pattern)
        .map_err(|error| ToolError::Execution(format!("invalid regular expression: {error}")))?;
    let search = SearchPath::new(root, requested)?;
    let mut matches = Vec::new();
    let progress = collect_matches(
        &search,
        &regex,
        include,
        &mut matches,
        cancellation,
        deadline,
    )?;
    render_matches(matches, progress, cancellation, deadline)
}

/// Collects up to `MAX_MATCHES` matching lines from a single file or a whole
/// directory tree, reporting whether the match limit was reached.
fn collect_matches(
    search: &SearchPath,
    regex: &Regex,
    include: Option<&str>,
    matches: &mut Vec<Match>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<SearchProgress, ToolError> {
    let full = search.full_path();
    if full.is_file() {
        let included = match include {
            Some(pattern) => file_matches(full, pattern)?,
            None => true,
        };
        if !included {
            return Ok(SearchProgress::Complete);
        }
        return search_file(search, full, regex, matches, cancellation, deadline);
    }
    if !full.is_dir() {
        return Err(ToolError::Execution(format!(
            "grep path is not a file or directory: {}",
            full.display()
        )));
    }
    let builder = crate::path::file_walker(full);
    let include =
        if let Some(pattern) = include {
            let mut overrides = OverrideBuilder::new(full);
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
        crate::path::ensure_running(cancellation, deadline)?;
        let entry =
            entry.map_err(|error| ToolError::Execution(format!("cannot search files: {error}")))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if include
            .as_ref()
            .is_some_and(|matcher| !matcher.matched(entry.path(), false).is_whitelist())
        {
            continue;
        }
        if search_file(search, entry.path(), regex, matches, cancellation, deadline)?
            == SearchProgress::MatchLimitReached
        {
            return Ok(SearchProgress::MatchLimitReached);
        }
    }
    Ok(SearchProgress::Complete)
}

fn render_matches(
    matches: Vec<Match>,
    progress: SearchProgress,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    crate::path::ensure_running(cancellation, deadline)?;
    if matches.is_empty() {
        return Ok("No matches found".into());
    }
    let mut matches = matches;
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
    for chunk in matches
        .as_slice()
        .chunk_by(|left, right| left.path == right.path)
    {
        let _ = write!(output, "\n\n{}:", chunk[0].path.display());
        for found in chunk {
            let _ = write!(output, "\n  Line {}: {}", found.line, found.text);
        }
    }
    if progress == SearchProgress::MatchLimitReached {
        let _ = write!(
            output,
            "\n\n[Results truncated at {MAX_MATCHES} matching lines. Use a narrower path, pattern, or include glob.]"
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
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<SearchProgress, ToolError> {
    crate::path::ensure_running(cancellation, deadline)?;
    let file = search.open_file(path).map_err(|error| {
        ToolError::Execution(format!("cannot read {}: {error}", path.display()))
    })?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut line = 0;
    loop {
        crate::path::ensure_running(cancellation, deadline)?;
        let line_read = read_line(&mut reader, &mut buffer, path, cancellation, deadline)?;
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

/// Reads one line into `output`, consuming it from `reader`. The line is fully
/// consumed even when oversized; `output` then holds only the truncated prefix
/// (up to `MAX_SEARCH_LINE_BYTES`), which the caller must skip via the
/// `LineRead::Oversized` result.
fn read_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    path: &Path,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<LineRead, ToolError> {
    output.clear();
    let mut truncated = false;
    loop {
        crate::path::ensure_running(cancellation, deadline)?;
        let available = reader.fill_buf().map_err(|error| {
            ToolError::Execution(format!("cannot read {}: {error}", path.display()))
        })?;
        if available.is_empty() {
            // EOF: an empty output means the reader was already exhausted, so
            // no line (not even a trailing partial one) was read.
            return Ok(if output.is_empty() {
                LineRead::EndOfFile
            } else if truncated {
                LineRead::Oversized
            } else {
                LineRead::Complete
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.unwrap_or(consumed);
        let remaining = MAX_SEARCH_LINE_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&available[..content.min(remaining)]);
        truncated |= content > remaining;
        reader.consume(consumed);
        if newline.is_some() {
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
    use std::time::Duration;

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

        let output = search(
            root.path(),
            "src",
            r"needle \d",
            Some("*.rs"),
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert!(output.contains("Found 2 matching lines"));
        assert!(output.contains("src/lib.rs:"));
        assert!(output.contains("Line 2: needle 1"));
        assert!(!output.contains("needle 3"));
    }

    #[test]
    fn supports_exact_file_paths_and_rejects_invalid_regexes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "one\ntwo\n").unwrap();

        let output = search(
            root.path(),
            "notes.txt",
            "two",
            None,
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert!(output.contains("notes.txt:"));
        assert!(output.contains("Line 2: two"));
        assert!(search(
            root.path(),
            ".",
            "[",
            None,
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .is_err());
    }

    #[test]
    fn truncates_large_match_sets() {
        let root = tempfile::tempdir().unwrap();
        let content = (0..=MAX_MATCHES)
            .map(|index| format!("match {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.path().join("matches.txt"), content).unwrap();

        let output = search(
            root.path(),
            ".",
            "match",
            None,
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

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
        let cancellation = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_mins(1);

        assert_eq!(
            read_line(
                &mut reader,
                &mut buffer,
                Path::new("input"),
                &cancellation,
                deadline,
            )
            .unwrap(),
            LineRead::Oversized
        );
        assert!(buffer.len() <= MAX_SEARCH_LINE_BYTES);
        assert_eq!(
            read_line(
                &mut reader,
                &mut buffer,
                Path::new("input"),
                &cancellation,
                deadline,
            )
            .unwrap(),
            LineRead::Complete
        );
        assert_eq!(buffer, b"needle");
        assert_eq!(
            read_line(
                &mut reader,
                &mut buffer,
                Path::new("input"),
                &cancellation,
                deadline,
            )
            .unwrap(),
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

        let output = search(
            root.path(),
            ".",
            "needle",
            None,
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();

        assert_eq!(output, "No matches found");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_files_that_escape_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            root.path().join("link.txt"),
        )
        .unwrap();

        // The file is a symlink to a path outside the workspace, so it must
        // not be readable through the capability-backed open even when a
        // traversal swapped it in after the walker enumerated it.
        let search_path = SearchPath::new(root.path(), ".").unwrap();
        let link = root.path().join("link.txt");
        assert!(search_path.open_file(&link).is_err());

        let output = search(
            root.path(),
            ".",
            "needle",
            None,
            &CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap();
        assert_eq!(output, "No matches found");
    }

    #[test]
    fn cancelled_search_returns_cancelled() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "needle\n").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = search(
            root.path(),
            ".",
            "needle",
            None,
            &cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }
}
