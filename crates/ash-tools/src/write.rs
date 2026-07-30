use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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

pub fn tool(working_dir: Arc<PathBuf>) -> Arc<dyn Tool> {
    define_tool(
        "write",
        "Write complete content to a file. Creates missing parent directories and overwrites an existing file; use edit for local changes.",
        move |_ctx, args: WriteArgs| {
            let working_dir = Arc::clone(&working_dir);
            async move {
            let path = write_file(&working_dir, &args.path, &args.content).await?;
            Ok(format!(
                "Wrote {} bytes to {}.",
                args.content.len(),
                path.display()
            ))
            }
        },
    )
}

async fn write_file(
    root: &Path,
    requested: &str,
    content: &str,
) -> Result<std::path::PathBuf, ToolError> {
    let root = root.to_path_buf();
    let requested = requested.to_string();
    let content = content.as_bytes().to_vec();
    crate::path::run_blocking(move || {
        let path = crate::path::WorkspacePath::new(&root, &requested)?;
        path.write(&content).map_err(|error| {
            ToolError::Execution(format!(
                "cannot write {}: {error}",
                path.full_path().display()
            ))
        })?;
        Ok(path.full_path().to_path_buf())
    })
    .await
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

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_writes_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link.txt")).unwrap();

        assert!(write_file(root.path(), "link.txt", "changed")
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "secret");
    }
}
