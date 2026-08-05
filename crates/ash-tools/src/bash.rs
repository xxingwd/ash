use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, LimitKind, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

#[derive(Deserialize, JsonSchema)]
struct BashArgs {
    /// Bash command to execute
    command: String,
    /// Working directory for the command, relative to the session working
    /// directory or absolute within it; defaults to the session working
    /// directory
    cwd: Option<String>,
    /// Timeout in seconds; omitted means no tool-specific timeout
    timeout: Option<f64>,
}

pub fn tool(working_dir: Arc<PathBuf>) -> Arc<dyn Tool> {
    define_tool(
        "bash",
        "Execute a bash command in the current working directory. Set `cwd` to run in a subdirectory instead of prefixing the command with `cd`. Returns stdout and stderr. Output keeps the last 2000 lines or 50KB; truncated output is saved to a temporary file.",
        move |_ctx, args: BashArgs| {
            let working_dir = Arc::clone(&working_dir);
            async move {
            let timeout = args.timeout.map(parse_timeout).transpose()?;
            let cwd = match args.cwd {
                Some(requested) => {
                    let working_dir = Arc::clone(&working_dir);
                    crate::path::run_blocking(move || resolve_cwd(working_dir.as_path(), &requested)).await?
                }
                None => (*working_dir).clone(),
            };
            let stdout = tempfile::Builder::new()
                .prefix("ash-bash-stdout-")
                .tempfile()
                .map_err(|error| ToolError::Execution(format!("cannot capture stdout: {error}")))?;
            let stderr = tempfile::Builder::new()
                .prefix("ash-bash-stderr-")
                .tempfile()
                .map_err(|error| ToolError::Execution(format!("cannot capture stderr: {error}")))?;
            let mut command = tokio::process::Command::new("bash");
            command
                .args(["-c", &args.command])
                .current_dir(cwd)
                .stdout(Stdio::from(stdout.reopen().map_err(|error| {
                    ToolError::Execution(format!("cannot capture stdout: {error}"))
                })?))
                .stderr(Stdio::from(stderr.reopen().map_err(|error| {
                    ToolError::Execution(format!("cannot capture stderr: {error}"))
                })?))
                .kill_on_drop(true);

            let output = match timeout {
                Some(timeout) => tokio::time::timeout(timeout, command.status())
                    .await
                    .map_err(|_| {
                        ToolError::Execution(format!(
                            "command timed out after {} seconds",
                            timeout.as_secs_f64()
                        ))
                    })?,
                None => command.status().await,
            }
            .map_err(|error| ToolError::Execution(format!("failed to execute bash: {error}")))?;

            let rendered = crate::path::run_blocking(move || render_files(stdout, stderr)).await?;
            if output.success() {
                Ok(rendered)
            } else {
                let code = output
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                Err(ToolError::Execution(format!(
                    "{rendered}\n\nCommand exited with {code}"
                )))
            }
            }
        },
    )
}

fn resolve_cwd(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let candidate = crate::path::WorkspacePath::new(root, requested)?
        .full_path()
        .to_path_buf();
    let workspace = std::fs::canonicalize(root).map_err(|error| {
        ToolError::Execution(format!(
            "cannot resolve working directory {}: {error}",
            root.display()
        ))
    })?;
    let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
        ToolError::Execution(format!(
            "cannot access working directory {}: {error}",
            candidate.display()
        ))
    })?;
    if !resolved.starts_with(&workspace) {
        return Err(ToolError::Execution(format!(
            "working directory is outside the session working directory: {}",
            resolved.display()
        )));
    }
    if !resolved.is_dir() {
        return Err(ToolError::Execution(format!(
            "working directory is not a directory: {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn parse_timeout(seconds: f64) -> Result<Duration, ToolError> {
    crate::timeout::parse_positive_seconds(seconds)
}

fn render_files(
    mut stdout: tempfile::NamedTempFile,
    mut stderr: tempfile::NamedTempFile,
) -> Result<String, ToolError> {
    let stdout_len = stdout.as_file().metadata().map_err(output_error)?.len();
    let stderr_len = stderr.as_file().metadata().map_err(output_error)?.len();
    stdout
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(output_error)?;
    stderr
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(output_error)?;
    let mut combined = tempfile::Builder::new()
        .prefix("ash-bash-")
        .tempfile()
        .map_err(output_error)?;
    std::io::copy(stdout.as_file_mut(), combined.as_file_mut()).map_err(output_error)?;
    if stdout_len > 0 && stderr_len > 0 {
        combined.write_all(b"\n").map_err(output_error)?;
    }
    std::io::copy(stderr.as_file_mut(), combined.as_file_mut()).map_err(output_error)?;
    combined.flush().map_err(output_error)?;
    render_output(combined)
}

fn render_output(mut output: tempfile::NamedTempFile) -> Result<String, ToolError> {
    let length = output.as_file().metadata().map_err(output_error)?.len();
    if length == 0 {
        return Ok("(no output)".into());
    }
    let total_lines = count_lines(output.as_file_mut())?;
    if length <= DEFAULT_MAX_BYTES as u64 && total_lines <= DEFAULT_MAX_LINES {
        output
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(output_error)?;
        let mut bytes = Vec::with_capacity(length as usize);
        output
            .as_file_mut()
            .read_to_end(&mut bytes)
            .map_err(output_error)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }

    let start = length.saturating_sub(DEFAULT_MAX_BYTES as u64);
    output
        .as_file_mut()
        .seek(SeekFrom::Start(start))
        .map_err(output_error)?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    output
        .as_file_mut()
        .read_to_end(&mut bytes)
        .map_err(output_error)?;
    let suffix = String::from_utf8_lossy(&bytes);
    let truncated = truncate::tail(&suffix);
    let mut rendered = truncated.content;
    let mut partial_line = truncated.partial_line;
    if start > 0 && !truncated.truncated {
        if let Some(newline) = rendered.find('\n') {
            if newline + 1 < rendered.len() {
                rendered.drain(..=newline);
            } else {
                partial_line = true;
            }
        } else {
            partial_line = true;
        }
    }
    let output_lines = line_count(rendered.as_bytes());
    let path = output
        .into_temp_path()
        .keep()
        .map_err(|error| ToolError::Execution(format!("cannot keep output file: {error}")))?;
    if partial_line {
        rendered.push_str(&format!(
            "\n\n[Showing the last {} of an oversized line. Full output: {}]",
            truncate::format_size(rendered.len()),
            path.display()
        ));
    } else {
        let first_line = total_lines.saturating_sub(output_lines).saturating_add(1);
        let reason = match truncated.limited_by {
            Some(LimitKind::Lines) => format!("{} line limit", DEFAULT_MAX_LINES),
            Some(LimitKind::Bytes) | None => {
                format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES))
            }
        };
        rendered.push_str(&format!(
            "\n\n[Showing lines {first_line}-{} of {} ({reason}). Full output: {}]",
            total_lines,
            total_lines,
            path.display()
        ));
    }
    Ok(rendered)
}

fn count_lines(file: &mut std::fs::File) -> Result<usize, ToolError> {
    file.seek(SeekFrom::Start(0)).map_err(output_error)?;
    let mut buffer = [0_u8; 8 * 1024];
    let mut newlines = 0;
    let mut last = None;
    loop {
        let count = file.read(&mut buffer).map_err(output_error)?;
        if count == 0 {
            break;
        }
        newlines += buffer[..count]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        last = Some(buffer[count - 1]);
    }
    Ok(newlines + usize::from(last.is_some_and(|byte| byte != b'\n')))
}

fn line_count(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    content.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(content.last() != Some(&b'\n'))
}

fn output_error(error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("cannot process command output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{AgentToolContext, CancellationToken, ThreadId, ToolContext, TreeId, TurnId};

    fn test_context() -> ToolContext {
        ToolContext {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            cancellation: CancellationToken::new(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            agent: AgentToolContext {
                tree_id: TreeId::new(),
                path: String::new(),
                messages: Vec::new(),
            },
        }
    }

    async fn run_in(root: &Path, command: &str, cwd: Option<&str>) -> Result<String, ToolError> {
        let tool = tool(Arc::new(root.to_path_buf()));
        let mut args = serde_json::Map::new();
        args.insert("command".into(), serde_json::Value::String(command.into()));
        if let Some(cwd) = cwd {
            args.insert("cwd".into(), serde_json::Value::String(cwd.into()));
        }
        let output = tool
            .execute(test_context(), serde_json::Value::Object(args))
            .await?;
        Ok(output.text)
    }

    #[tokio::test]
    async fn defaults_to_the_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let expected = std::fs::canonicalize(root.path()).unwrap();

        let output = run_in(root.path(), "pwd", None).await.unwrap();

        assert!(
            output.contains(expected.to_str().unwrap()),
            "unexpected output: {output}"
        );
    }

    #[tokio::test]
    async fn runs_in_the_requested_cwd() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();

        let output = run_in(root.path(), "pwd", Some("src")).await.unwrap();

        assert!(output.contains("src"), "unexpected output: {output}");
    }

    #[tokio::test]
    async fn resolves_absolute_cwd_within_working_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();

        let output = run_in(
            root.path(),
            "pwd",
            Some(root.path().join("src").to_str().unwrap()),
        )
        .await
        .unwrap();

        assert!(output.contains("src"), "unexpected output: {output}");
    }

    #[tokio::test]
    async fn rejects_cwd_outside_working_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        let error = run_in(root.path(), "pwd", Some(outside.path().to_str().unwrap()))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("outside working directory"));
    }

    #[tokio::test]
    async fn rejects_cwd_escaping_via_parent() {
        let root = tempfile::tempdir().unwrap();

        let error = run_in(root.path(), "pwd", Some("../elsewhere"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("escapes working directory"));
    }

    #[tokio::test]
    async fn rejects_cwd_that_is_not_a_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();

        let error = run_in(root.path(), "pwd", Some("file.txt"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not a directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_cwd_symlink_that_resolves_outside_working_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();

        let error = run_in(root.path(), "pwd", Some("outside"))
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("outside the session working directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn allows_cwd_symlink_that_resolves_inside_working_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::os::unix::fs::symlink("src", root.path().join("source")).unwrap();
        let expected = std::fs::canonicalize(root.path().join("src")).unwrap();

        let output = run_in(root.path(), "pwd", Some("source")).await.unwrap();

        assert_eq!(output.trim(), expected.to_str().unwrap());
    }

    #[test]
    fn rejects_invalid_timeouts() {
        assert!(parse_timeout(0.0).is_err());
        assert!(parse_timeout(f64::INFINITY).is_err());
        assert_eq!(parse_timeout(0.5).unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn saves_full_output_when_truncated() {
        let output = (0..=DEFAULT_MAX_LINES)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(output.as_bytes()).unwrap();
        let rendered = render_output(file).unwrap();

        assert!(rendered.contains("Full output:"));
        assert!(!rendered.starts_with("0\n"));
        let path = rendered
            .rsplit_once("Full output: ")
            .map(|(_, path)| path.trim_end_matches(']'))
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn renders_a_bounded_tail_for_very_large_output() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..1024 {
            file.write_all(&[b'x'; 8 * 1024]).unwrap();
        }

        let rendered = render_output(file).unwrap();

        assert!(rendered.len() < DEFAULT_MAX_BYTES + 512);
        let path = rendered
            .rsplit_once("Full output: ")
            .map(|(_, path)| path.trim_end_matches(']'))
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
