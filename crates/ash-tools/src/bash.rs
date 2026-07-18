use std::{path::PathBuf, sync::Arc, time::Duration};

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, LimitKind, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

#[derive(Deserialize, JsonSchema)]
struct BashArgs {
    /// Bash command to execute
    command: String,
    /// Timeout in seconds; omitted means no tool-specific timeout
    timeout: Option<f64>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "bash",
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output keeps the last 2000 lines or 50KB; truncated output is saved to a temporary file.",
        |ctx, args: BashArgs| async move {
            let timeout = args.timeout.map(parse_timeout).transpose()?;
            let mut command = tokio::process::Command::new("bash");
            command
                .args(["-c", &args.command])
                .current_dir(&ctx.working_dir)
                .kill_on_drop(true);

            let output = match timeout {
                Some(timeout) => tokio::time::timeout(timeout, command.output())
                    .await
                    .map_err(|_| {
                        ToolError::Execution(format!(
                            "command timed out after {} seconds",
                            args.timeout.unwrap_or_default()
                        ))
                    })?,
                None => command.output().await,
            }
            .map_err(|error| ToolError::Execution(format!("failed to execute bash: {error}")))?;

            let combined = combine_output(&output.stdout, &output.stderr);
            let rendered = render_output(&combined)?;
            if output.status.success() {
                Ok(rendered)
            } else {
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                Err(ToolError::Execution(format!(
                    "{rendered}\n\nCommand exited with {code}"
                )))
            }
        },
    )
}

fn parse_timeout(seconds: f64) -> Result<Duration, ToolError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ToolError::Execution(
            "timeout must be a positive finite number of seconds".into(),
        ));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| ToolError::Execution("timeout is too large".into()))
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.into_owned(),
        (true, false) => stderr.into_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn render_output(output: &str) -> Result<String, ToolError> {
    if output.is_empty() {
        return Ok("(no output)".into());
    }
    let truncated = truncate::tail(output);
    if !truncated.truncated {
        return Ok(truncated.content);
    }

    let path = persist_full_output(output)?;
    let mut rendered = truncated.content;
    if truncated.partial_line {
        rendered.push_str(&format!(
            "\n\n[Showing the last {} of an oversized line. Full output: {}]",
            truncate::format_size(truncated.output_bytes),
            path.display()
        ));
    } else {
        let start = truncated
            .total_lines
            .saturating_sub(truncated.output_lines)
            .saturating_add(1);
        let reason = match truncated.limited_by {
            Some(LimitKind::Lines) => format!("{} line limit", DEFAULT_MAX_LINES),
            Some(LimitKind::Bytes) => format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES)),
            None => String::new(),
        };
        rendered.push_str(&format!(
            "\n\n[Showing lines {start}-{} of {} ({reason}). Full output: {}]",
            truncated.total_lines,
            truncated.total_lines,
            path.display()
        ));
    }
    Ok(rendered)
}

fn persist_full_output(output: &str) -> Result<PathBuf, ToolError> {
    let file = tempfile::Builder::new()
        .prefix("ash-bash-")
        .tempfile()
        .map_err(|error| ToolError::Execution(format!("cannot create output file: {error}")))?;
    let path = file
        .into_temp_path()
        .keep()
        .map_err(|error| ToolError::Execution(format!("cannot keep output file: {error}")))?;
    std::fs::write(&path, output)
        .map_err(|error| ToolError::Execution(format!("cannot save full output: {error}")))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let rendered = render_output(&output).unwrap();

        assert!(rendered.contains("Full output:"));
        assert!(!rendered.starts_with("0\n"));
        let path = rendered
            .rsplit_once("Full output: ")
            .map(|(_, path)| path.trim_end_matches(']'))
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
