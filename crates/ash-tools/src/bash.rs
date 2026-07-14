use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct BashArgs {
    /// Shell command to execute
    command: String,
    /// Working directory (defaults to project root)
    cwd: Option<String>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "bash",
        "Execute a shell command",
        |ctx, args: BashArgs| async move {
            let cwd = match args.cwd.as_deref() {
                Some(path) => crate::path::existing(&ctx.working_dir, path)?,
                None => crate::path::existing(&ctx.working_dir, ".")?,
            };

            let mut command = tokio::process::Command::new("bash");
            command
                .args(["-c", &args.command])
                .current_dir(&cwd)
                .kill_on_drop(true);
            let output = tokio::select! {
                _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
                result = tokio::time::timeout(ctx.timeout, command.output()) => {
                    result
                        .map_err(|_| ToolError::Timeout(ctx.timeout))?
                        .map_err(|error| ToolError::Execution(format!("failed to execute: {error}")))?
                }
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("stderr: ");
                result.push_str(&stderr);
            }

            if output.status.success() {
                Ok(result)
            } else {
                let code = output.status.code().unwrap_or(-1);
                Err(ToolError::Execution(format!("exit code {code}\n{result}")))
            }
        },
    )
}
