use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// New file path within the workspace
    path: String,
    /// Content to write
    content: String,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "write",
        "Create a new file with complete content. Fails if the file already exists.",
        |ctx, args: WriteArgs| async move {
            let path = write_new_file(&ctx.working_dir, &args.path, &args.content).await?;

            Ok(format!(
                "wrote {} bytes to {}",
                args.content.len(),
                path.display()
            ))
        },
    )
}

async fn write_new_file(
    root: &Path,
    requested: &str,
    content: &str,
) -> Result<std::path::PathBuf, ToolError> {
    let path = crate::path::for_write(root, requested)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| ToolError::Execution(format!("cannot create dirs: {error}")))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
        .map_err(|error| {
            let message = if error.kind() == std::io::ErrorKind::AlreadyExists {
                "file already exists; use edit for existing files".to_string()
            } else {
                format!("cannot create {}: {error}", path.display())
            };
            ToolError::Execution(message)
        })?;
    use tokio::io::AsyncWriteExt;
    file.write_all(content.as_bytes()).await.map_err(|error| {
        ToolError::Execution(format!("cannot write {}: {error}", path.display()))
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_new_files_but_refuses_to_overwrite() {
        let root = tempfile::tempdir().unwrap();
        write_new_file(root.path(), "src/new.rs", "first")
            .await
            .unwrap();

        let error = write_new_file(root.path(), "src/new.rs", "second")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("src/new.rs"))
                .await
                .unwrap(),
            "first"
        );
    }
}
