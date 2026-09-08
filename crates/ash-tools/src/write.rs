use std::{path::PathBuf, sync::Arc, time::Instant};

use ash_core::{define_tool, CancellationToken, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::path::Workspace;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// File path, relative to the working directory or absolute
    path: String,
    /// Complete file content
    content: String,
}

#[derive(Debug)]
struct WriteResult {
    path: PathBuf,
}

pub fn tool(working_dir: Arc<Workspace>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "write",
        "Write complete content to a file. Creates missing parent directories and overwrites an existing file; use edit for local changes.",
        move |ctx, args: WriteArgs| {
            let working_dir = Arc::clone(&working_dir);
            let deadline = ctx.require_deadline();
            let cancellation = ctx.cancellation;
            async move {
                let deadline = deadline?;
                let result =
                    write_file(&working_dir, &args.path, &args.content, cancellation, deadline)
                        .await?;
                let text = format!(
                    "Wrote {} bytes to {}.",
                    args.content.len(),
                    result.path.display()
                );
                Ok(ToolOutput::from(text))
            }
        },
    )
}

async fn write_file(
    workspace: &Workspace,
    requested: &str,
    content: &str,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WriteResult, ToolError> {
    let requested = requested.to_string();
    let content = content.to_string();
    crate::path::ensure_running(&cancellation, deadline)?;
    let path = workspace.path(&requested)?;
    crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
        crate::path::ensure_running(&cancellation, deadline)?;
        path.atomic_write(content.as_bytes(), None, &cancellation, deadline)?;
        Ok(WriteResult {
            path: path.full_path().to_path_buf(),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn creates_parent_directories_and_overwrites_files() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let added = write_file(
            &workspace,
            "src/new.rs",
            "first",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();
        let updated = write_file(
            &workspace,
            "src/new.rs",
            "second",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(added.path, root.path().join("src/new.rs"));
        assert_eq!(updated.path, root.path().join("src/new.rs"));
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("src/new.rs"))
                .await
                .unwrap(),
            "second"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_permissions_when_overwriting() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let path = root.path().join("existing.txt");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&path)
            .unwrap();

        write_file(
            &workspace,
            "existing.txt",
            "changed",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[tokio::test]
    async fn allows_writes_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside.txt");

        let result = write_file(
            &workspace,
            target.to_str().unwrap(),
            "outside content",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.path, target);
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "outside content"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn allows_symlink_writes_overwriting_link() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link.txt")).unwrap();

        let result = write_file(
            &workspace,
            "link.txt",
            "changed",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.path, root.path().join("link.txt"));
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("link.txt"))
                .await
                .unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn cancelled_write_does_not_create_the_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = write_file(
            &workspace,
            "new.txt",
            "content",
            cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
        assert!(!root.path().join("new.txt").exists());
    }
}
