use std::{
    fmt::Write as _,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use ash_core::{define_tool, CancellationToken, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::{Child, Command};

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

pub fn tool(working_dir: Arc<PathBuf>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "bash",
        "Execute a bash command in the current working directory. Set `cwd` to run in a subdirectory instead of prefixing the command with `cd`. Returns stdout and stderr. Output keeps the last 2000 lines or 50KB; truncated output is saved to a temporary file.",
        move |ctx, args: BashArgs| {
            let working_dir = Arc::clone(&working_dir);
            let cancellation = ctx.cancellation;
            let deadline = ctx.deadline;
            async move { run_command(&working_dir, args, cancellation, deadline).await }
        },
    )
}

async fn run_command(
    working_dir: &Path,
    args: BashArgs,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    crate::path::ensure_running(&cancellation, deadline)?;
    let timeout = args
        .timeout
        .map(crate::timeout::parse_positive_seconds)
        .transpose()?;
    let cwd = match args.cwd {
        Some(requested) => {
            let working_dir = working_dir.to_path_buf();
            crate::path::run_blocking(move || resolve_cwd(&working_dir, &requested)).await?
        }
        None => working_dir.to_path_buf(),
    };
    crate::path::ensure_running(&cancellation, deadline)?;
    let stdout = tempfile::Builder::new()
        .prefix("ash-bash-stdout-")
        .tempfile()
        .map_err(|error| ToolError::Execution(format!("cannot capture stdout: {error}")))?;
    let stderr = tempfile::Builder::new()
        .prefix("ash-bash-stderr-")
        .tempfile()
        .map_err(|error| ToolError::Execution(format!("cannot capture stderr: {error}")))?;
    let mut command = Command::new("bash");
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

    #[cfg(unix)]
    command.process_group(0);
    let mut child = ManagedChild::spawn(&mut command)
        .map_err(|error| ToolError::Execution(format!("failed to execute bash: {error}")))?;

    let remaining = deadline.saturating_duration_since(Instant::now());
    let effective_timeout = timeout.map_or(remaining, |timeout| timeout.min(remaining));
    let output = wait_for_child(&mut child, effective_timeout, &cancellation).await?;
    let rendered = crate::path::run_blocking(move || render_files(stdout, stderr)).await?;
    if output.success() {
        Ok(rendered)
    } else {
        Err(ToolError::Execution(format!(
            "{rendered}\n\nCommand exited with {}",
            exit_description(output)
        )))
    }
}

async fn wait_for_child(
    child: &mut ManagedChild,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<std::process::ExitStatus, ToolError> {
    let wait = async { child.wait().await };
    tokio::select! {
        status = wait => status.map_err(|error| {
            ToolError::Execution(format!("failed to execute bash: {error}"))
        }),
        () = tokio::time::sleep(timeout) => {
            child.terminate().await;
            Err(ToolError::Timeout(timeout))
        }
        () = cancellation.cancelled() => {
            child.terminate().await;
            Err(ToolError::Cancelled)
        }
    }
}

fn exit_description(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    status
        .code()
        .map_or_else(|| "unknown".to_string(), |code| code.to_string())
}

struct ManagedChild {
    child: Child,
    #[cfg(unix)]
    process_group: Option<rustix::process::Pid>,
}

impl ManagedChild {
    fn spawn(command: &mut Command) -> std::io::Result<Self> {
        let child = command.spawn()?;
        #[cfg(unix)]
        let process_group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        Ok(Self {
            child,
            #[cfg(unix)]
            process_group,
        })
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        self.disarm();
        Ok(status)
    }

    async fn terminate(&mut self) {
        self.kill_processes();
        let _ = self.child.wait().await;
        self.disarm();
    }

    fn kill_processes(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            let _ =
                rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
    }

    const fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.process_group = None;
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.kill_processes();
    }
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
        return read_entire_output(&mut output);
    }

    let start = length.saturating_sub(DEFAULT_MAX_BYTES as u64);
    let (mut rendered, partial_line, limited_by) = render_tail_window(&mut output, start, length)?;
    let oversized_line = if partial_line {
        Some(oversized_line_length(&mut output, start, length)?)
    } else {
        None
    };
    let path = output
        .into_temp_path()
        .keep()
        .map_err(|error| ToolError::Execution(format!("cannot keep output file: {error}")))?;

    if let Some(line_length) = oversized_line {
        let _ = write!(
            rendered,
            "\n\n[Showing the last {} of an oversized line of {}. Full output: {}]",
            truncate::format_size(rendered.len()),
            truncate::format_size(line_length),
            path.display()
        );
        return Ok(rendered);
    }

    let shown_lines = line_count(rendered.as_bytes());
    let first_line = total_lines.saturating_sub(shown_lines).saturating_add(1);
    let reason = match limited_by {
        Some(LimitKind::Lines) => format!("{DEFAULT_MAX_LINES} line limit"),
        Some(LimitKind::Bytes) | None => {
            format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES))
        }
    };
    let _ = write!(
        rendered,
        "\n\n[Showing lines {first_line}-{} of {} ({reason}). Full output: {}]",
        total_lines,
        total_lines,
        path.display()
    );
    Ok(rendered)
}

fn read_entire_output(output: &mut tempfile::NamedTempFile) -> Result<String, ToolError> {
    let length = output.as_file().metadata().map_err(output_error)?.len();
    output
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(output_error)?;
    let mut bytes = Vec::with_capacity(output_length_usize(length)?);
    output
        .as_file_mut()
        .read_to_end(&mut bytes)
        .map_err(output_error)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads the final `DEFAULT_MAX_BYTES` bytes and trims it to a line-boundary
/// tail. Returns the rendered tail, whether its first line was cut mid-line
/// (an oversized line), and which limit triggered the truncation.
fn render_tail_window(
    output: &mut tempfile::NamedTempFile,
    start: u64,
    length: u64,
) -> Result<(String, bool, Option<LimitKind>), ToolError> {
    output
        .as_file_mut()
        .seek(SeekFrom::Start(start))
        .map_err(output_error)?;
    let mut bytes = Vec::with_capacity(output_length_usize(length - start)?);
    output
        .as_file_mut()
        .read_to_end(&mut bytes)
        .map_err(output_error)?;
    let suffix = String::from_utf8_lossy(&bytes);
    let truncated = truncate::tail(&suffix);
    let mut rendered = truncated.content;
    let mut partial_line = truncated.partial_line;
    let limited_by = truncated.limited_by;
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
    Ok((rendered, partial_line, limited_by))
}

/// Byte length of the oversized line that begins at or before `start` and runs
/// until the next newline (or end of file).
fn oversized_line_length(
    output: &mut tempfile::NamedTempFile,
    start: u64,
    file_length: u64,
) -> Result<usize, ToolError> {
    let line_start = previous_newline_offset(output, start)?;
    let line_end = next_newline_offset(output, start, file_length)?;
    output_length_usize(line_end - line_start)
}

/// Offset just after the last newline strictly before `offset` (or 0).
fn previous_newline_offset(
    output: &mut tempfile::NamedTempFile,
    offset: u64,
) -> Result<u64, ToolError> {
    let mut position = offset;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let window_start = position.saturating_sub(buffer.len() as u64);
        // The window is at most `buffer.len()` bytes, so this cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let window_len = (position - window_start) as usize;
        if window_len == 0 {
            return Ok(0);
        }
        output
            .as_file_mut()
            .seek(SeekFrom::Start(window_start))
            .map_err(output_error)?;
        output
            .as_file_mut()
            .read_exact(&mut buffer[..window_len])
            .map_err(output_error)?;
        if let Some(index) = buffer[..window_len].iter().rposition(|byte| *byte == b'\n') {
            return Ok(window_start + index as u64 + 1);
        }
        position = window_start;
    }
}

/// Offset of the first newline at or after `offset` (or end of file).
fn next_newline_offset(
    output: &mut tempfile::NamedTempFile,
    offset: u64,
    file_length: u64,
) -> Result<u64, ToolError> {
    let mut position = offset;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if position >= file_length {
            return Ok(file_length);
        }
        output
            .as_file_mut()
            .seek(SeekFrom::Start(position))
            .map_err(output_error)?;
        let count = output
            .as_file_mut()
            .read(&mut buffer)
            .map_err(output_error)?;
        if count == 0 {
            return Ok(position);
        }
        if let Some(index) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            return Ok(position + index as u64);
        }
        position += count as u64;
    }
}

// Counting newlines via `filter` is fine for command output sizes; adding a
// `memchr`/`bytecount` dependency is not worth it here.
#[allow(clippy::naive_bytecount)]
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

// Counting newlines via `filter` is fine for command output sizes; adding a
// `memchr`/`bytecount` dependency is not worth it here.
#[allow(clippy::naive_bytecount)]
fn line_count(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    content.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(content.last() != Some(&b'\n'))
}

// Takes the error by value so it can be passed directly to `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn output_error(error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("cannot process command output: {error}"))
}

/// Convert a file-derived `u64` length to `usize`. Files too large to address
/// on a 32-bit target cannot be read into memory anyway, so reject them
/// explicitly instead of silently truncating.
fn output_length_usize(length: u64) -> Result<usize, ToolError> {
    usize::try_from(length).map_err(|_| {
        ToolError::Execution("command output is too large for this platform".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::{
        CancellationToken, SessionId, SessionIdentity, SessionToolContext, ToolContext, TurnId,
    };

    fn test_context() -> ToolContext {
        test_context_with(CancellationToken::new())
    }

    fn test_context_with(cancellation: CancellationToken) -> ToolContext {
        ToolContext {
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            cancellation,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            session: SessionToolContext {
                identity: SessionIdentity::root(SessionId::new()),
                messages: Vec::new(),
            },
        }
    }

    async fn run_in(root: &Path, command: &str, cwd: Option<&str>) -> Result<String, ToolError> {
        run_with_timeout(root, command, cwd, None).await
    }

    async fn run_with_timeout(
        root: &Path,
        command: &str,
        cwd: Option<&str>,
        timeout: Option<f64>,
    ) -> Result<String, ToolError> {
        let tool = tool(Arc::new(root.to_path_buf())).unwrap();
        let mut args = serde_json::Map::new();
        args.insert("command".into(), serde_json::Value::String(command.into()));
        if let Some(cwd) = cwd {
            args.insert("cwd".into(), serde_json::Value::String(cwd.into()));
        }
        if let Some(timeout) = timeout {
            args.insert("timeout".into(), serde_json::Value::from(timeout));
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
        assert!(crate::timeout::parse_positive_seconds(0.0).is_err());
        assert!(crate::timeout::parse_positive_seconds(f64::INFINITY).is_err());
        assert_eq!(
            crate::timeout::parse_positive_seconds(0.5).unwrap(),
            Duration::from_millis(500)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_background_descendants() {
        let root = tempfile::tempdir().unwrap();
        let error = run_with_timeout(
            root.path(),
            "(sleep 0.2; printf survived > descendant) & wait",
            None,
            Some(0.05),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("timeout"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!root.path().join("descendant").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_execution_terminates_background_descendants() {
        let root = tempfile::tempdir().unwrap();
        let tool = tool(Arc::new(root.path().to_path_buf())).unwrap();
        let task = tokio::spawn(async move {
            tool.execute(
                test_context(),
                serde_json::json!({
                    "command": "printf started > started; (sleep 0.2; printf survived > descendant) & wait"
                }),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !root.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!root.path().join("descendant").exists());
    }

    #[tokio::test]
    async fn cancelled_execution_terminates_the_command() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let tool = tool(Arc::new(root_path.clone())).unwrap();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let spawned = tokio::spawn(async move {
            let ctx = test_context_with(task_cancellation);
            tool.execute(
                ctx,
                serde_json::json!({ "command": "printf started > started; sleep 30" }),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !root_path.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        cancellation.cancel();

        let error = spawned.await.unwrap().unwrap_err();
        assert!(matches!(error, ToolError::Cancelled));
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
