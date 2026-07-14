use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// Absolute file path to write
    path: String,
    /// Content to write
    content: String,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "write",
        "Write content to a file (creates parent dirs)",
        |ctx, args: WriteArgs| async move {
            let path = crate::path::for_write(&ctx.working_dir, &args.path)?;

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ToolError::Execution(format!("cannot create dirs: {e}")))?;
            }

            tokio::fs::write(&path, &args.content).await.map_err(|e| {
                ToolError::Execution(format!("cannot write {}: {e}", path.display()))
            })?;

            Ok(format!(
                "wrote {} bytes to {}",
                args.content.len(),
                path.display()
            ))
        },
    )
}
