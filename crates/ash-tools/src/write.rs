use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// File path, relative to the working directory or absolute within it
    path: String,
    /// Complete file content
    content: String,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "write",
        "Write complete content to a file. Creates missing parent directories and overwrites an existing file; use edit for local changes.",
        |ctx, args: WriteArgs| async move {
            let path = write_file(&ctx.working_dir, &args.path, &args.content).await?;
            Ok(format!(
                "Wrote {} bytes to {}.",
                args.content.len(),
                path.display()
            ))
        },
    )
}

async fn write_file(
    root: &Path,
    requested: &str,
    content: &str,
) -> Result<std::path::PathBuf, ToolError> {
    let path = crate::path::for_write(root, requested)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| ToolError::Execution(format!("cannot create directories: {error}")))?;
    }
    tokio::fs::write(&path, content).await.map_err(|error| {
        ToolError::Execution(format!("cannot write {}: {error}", path.display()))
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_parent_directories_and_overwrites_files() {
        let root = tempfile::tempdir().unwrap();
        write_file(root.path(), "src/new.rs", "first")
            .await
            .unwrap();
        write_file(root.path(), "src/new.rs", "second")
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(root.path().join("src/new.rs"))
                .await
                .unwrap(),
            "second"
        );
    }
}
