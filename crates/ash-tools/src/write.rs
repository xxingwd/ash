use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ash_core::{define_tool, CancellationToken, FileChange, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

const MAX_DIFF_SOURCE_BYTES: usize = 64 * 1024;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// File path, relative to the working directory or absolute within it
    path: String,
    /// Complete file content
    content: String,
}

#[derive(Debug)]
struct WriteResult {
    path: PathBuf,
    change: Option<FileChange>,
}

pub fn tool(working_dir: Arc<PathBuf>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "write",
        "Write complete content to a file. Creates missing parent directories and overwrites an existing file; use edit for local changes.",
        move |ctx, args: WriteArgs| {
            let working_dir = Arc::clone(&working_dir);
            let cancellation = ctx.cancellation;
            let deadline = ctx.deadline;
            async move {
                let result =
                    write_file(&working_dir, &args.path, &args.content, cancellation, deadline)
                        .await?;
                let text = format!(
                    "Wrote {} bytes to {}.",
                    args.content.len(),
                    result.path.display()
                );
                Ok(match result.change {
                    Some(change) => ToolOutput::with_file_change(text, change),
                    None => ToolOutput::from(text),
                })
            }
        },
    )
}

async fn write_file(
    root: &Path,
    requested: &str,
    content: &str,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WriteResult, ToolError> {
    let root = root.to_path_buf();
    let requested = requested.to_string();
    let content = content.to_string();
    crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
        crate::path::ensure_running(&cancellation, deadline)?;
        let path = crate::path::WorkspacePath::new(&root, &requested)?;
        let previous = read_existing_text(&path, &cancellation, deadline);
        path.atomic_write(content.as_bytes(), None, &cancellation, deadline)?;
        let display_path = path.relative_path().to_path_buf();
        let change = match previous {
            ExistingText::Missing => Some(crate::change::added(display_path, content)),
            ExistingText::Text(previous) => {
                crate::change::updated(display_path, &previous, &content)
            }
            ExistingText::Unavailable => None,
        };
        Ok(WriteResult {
            path: path.full_path().to_path_buf(),
            change,
        })
    })
    .await
}

enum ExistingText {
    Missing,
    Text(String),
    Unavailable,
}

fn read_existing_text(
    path: &crate::path::WorkspacePath,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> ExistingText {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    let file = match path.open_with(&options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExistingText::Missing;
        }
        Err(_) => return ExistingText::Unavailable,
    };
    let limit = u64::try_from(MAX_DIFF_SOURCE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    crate::path::read_all(
        &mut file.take(limit),
        path.full_path(),
        cancellation,
        deadline,
    )
    .ok()
    .filter(|content| content.len() <= MAX_DIFF_SOURCE_BYTES)
    .and_then(|content| String::from_utf8(content).ok())
    .map_or(ExistingText::Unavailable, ExistingText::Text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn creates_parent_directories_and_overwrites_files() {
        let root = tempfile::tempdir().unwrap();
        let added = write_file(
            root.path(),
            "src/new.rs",
            "first",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();
        let updated = write_file(
            root.path(),
            "src/new.rs",
            "second",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(
            added.change,
            Some(FileChange::Add {
                path: PathBuf::from("src/new.rs"),
                content: "first".to_string(),
            })
        );
        let Some(FileChange::Update { path, unified_diff }) = updated.change else {
            panic!("overwrite must report an update");
        };
        assert_eq!(path, Path::new("src/new.rs"));
        assert!(unified_diff.contains("-first"));
        assert!(unified_diff.contains("+second"));
        assert_eq!(
            tokio::fs::read_to_string(root.path().join("src/new.rs"))
                .await
                .unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn omits_a_change_when_the_written_content_is_identical() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("same.txt"), "same\n").unwrap();

        let result = write_file(
            root.path(),
            "same.txt",
            "same\n",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.change, None);
    }

    #[tokio::test]
    async fn large_overwrites_succeed_without_reading_an_unbounded_preview() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("large.txt"),
            vec![b'x'; MAX_DIFF_SOURCE_BYTES + 1],
        )
        .unwrap();

        let result = write_file(
            root.path(),
            "large.txt",
            "replacement",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.change, None);
        assert_eq!(
            std::fs::read_to_string(root.path().join("large.txt")).unwrap(),
            "replacement"
        );
    }

    #[tokio::test]
    async fn large_new_files_report_the_complete_addition() {
        let root = tempfile::tempdir().unwrap();
        let content = "x".repeat(MAX_DIFF_SOURCE_BYTES + 1);

        let result = write_file(
            root.path(),
            "large.txt",
            &content,
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(
            result.change,
            Some(FileChange::Add {
                path: PathBuf::from("large.txt"),
                content,
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_permissions_when_overwriting() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("existing.txt");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o640)
            .open(&path)
            .unwrap();

        write_file(
            root.path(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_writes_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link.txt")).unwrap();

        assert!(write_file(
            root.path(),
            "link.txt",
            "changed",
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "secret");
    }

    #[tokio::test]
    async fn cancelled_write_does_not_create_the_file() {
        let root = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = write_file(
            root.path(),
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
