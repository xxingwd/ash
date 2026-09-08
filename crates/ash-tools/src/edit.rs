use std::{path::PathBuf, sync::Arc, time::Instant};

use ash_core::{define_tool, CancellationToken, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::path::Workspace;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Replacement {
    /// Exact text to replace; it must occur exactly once in the original file
    old_text: String,
    /// Replacement text
    new_text: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// File path, relative to the working directory or absolute
    path: String,
    /// Non-overlapping replacements matched against the original file
    edits: Vec<Replacement>,
}

#[derive(Debug)]
struct EditResult {
    path: PathBuf,
}

pub fn tool(working_dir: Arc<Workspace>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "edit",
        "Edit one file using one or more exact replacements. Every edits[].oldText must be unique and non-overlapping in the original file; replacements are not applied incrementally.",
        move |ctx, args: EditArgs| {
            let working_dir = Arc::clone(&working_dir);
            let deadline = ctx.require_deadline();
            let cancellation = ctx.cancellation;
            async move {
                let deadline = deadline?;
                let count = args.edits.len();
                let result =
                    edit_file(&working_dir, &args.path, args.edits, cancellation, deadline).await?;
                let text = format!(
                    "Successfully replaced {} block(s) in {}.",
                    count,
                    result.path.display()
                );
                Ok(ToolOutput::from(text))
            }
        },
    )
}

async fn edit_file(
    workspace: &Workspace,
    requested: &str,
    edits: Vec<Replacement>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<EditResult, ToolError> {
    let requested = requested.to_string();
    crate::path::ensure_running(&cancellation, deadline)?;
    let path = workspace.path(&requested)?;
    crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
        crate::path::ensure_running(&cancellation, deadline)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        crate::path::ensure_running(&cancellation, deadline)?;
        let mut file = path
            .open_with(&options)
            .map_err(|error| ToolError::Execution(format!("cannot open file: {error}")))?;

        let permissions = file
            .metadata()
            .map_err(|error| ToolError::Execution(format!("cannot inspect file: {error}")))?
            .permissions();
        let raw = crate::path::read_all(&mut file, &cancellation, deadline)?;
        let original = String::from_utf8(raw).map_err(|error| {
            ToolError::Execution(format!("cannot read file: {}", error.utf8_error()))
        })?;
        let (bom, content) = original
            .strip_prefix('\u{feff}')
            .map_or(("", original.as_str()), |content| ("\u{feff}", content));
        let line_ending = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let normalized = normalize_newlines(content);
        let edits = edits
            .iter()
            .map(|edit| Replacement {
                old_text: normalize_newlines(&edit.old_text),
                new_text: normalize_newlines(&edit.new_text),
            })
            .collect::<Vec<_>>();
        let edited = apply_edits(&normalized, &edits)?;
        let edited = if line_ending == "\r\n" {
            edited.replace('\n', "\r\n")
        } else {
            edited
        };
        let content = format!("{bom}{edited}");

        if original == content {
            return Err(ToolError::Execution(
                "edits did not change the resulting file".to_string(),
            ));
        }
        path.atomic_write(
            content.as_bytes(),
            Some(permissions),
            &cancellation,
            deadline,
        )?;
        Ok(EditResult {
            path: path.full_path().to_path_buf(),
        })
    })
    .await
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn apply_edits(content: &str, edits: &[Replacement]) -> Result<String, ToolError> {
    if edits.is_empty() {
        return Err(ToolError::Execution(
            "edits must contain at least one replacement".into(),
        ));
    }

    let mut matches = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(ToolError::Execution(format!(
                "edits[{index}].oldText must not be empty"
            )));
        }
        if edit.old_text == edit.new_text {
            return Err(ToolError::Execution(format!(
                "edits[{index}] does not change the file"
            )));
        }
        let occurrences = content
            .match_indices(&edit.old_text)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        if occurrences.len() > 1 {
            return Err(ToolError::Execution(format!(
                "edits[{index}].oldText matches {} locations; include more context",
                occurrences.len()
            )));
        }
        let start = occurrences
            .first()
            .copied()
            .ok_or_else(|| ToolError::Execution(format!("edits[{index}].oldText was not found")))?;
        matches.push((start, start + edit.old_text.len(), index));
    }

    matches.sort_unstable_by_key(|(start, _, _)| *start);
    if matches.windows(2).any(|pair| pair[1].0 < pair[0].1) {
        return Err(ToolError::Execution(
            "edits contain overlapping oldText regions; merge them into one replacement".into(),
        ));
    }

    let mut result = content.to_string();
    for (start, end, index) in matches.into_iter().rev() {
        result.replace_range(start..end, &edits[index].new_text);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn replacement(old_text: &str, new_text: &str) -> Replacement {
        Replacement {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }

    #[test]
    fn applies_disjoint_edits_against_the_original_file() {
        let edited = apply_edits(
            "one two three",
            &[replacement("one", "1"), replacement("three", "3")],
        )
        .unwrap();

        assert_eq!(edited, "1 two 3");
    }

    #[test]
    fn rejects_duplicate_and_overlapping_matches() {
        assert!(apply_edits("one one", &[replacement("one", "1")]).is_err());
        assert!(apply_edits(
            "one two",
            &[replacement("one two", "all"), replacement("two", "2")]
        )
        .is_err());
    }

    #[test]
    fn normalizes_line_endings_for_matching() {
        let content = normalize_newlines("one\r\ntwo\r\n");
        let edited = apply_edits(&content, &[replacement("one\ntwo", "three")]).unwrap();

        assert_eq!(edited, "three\n");
    }

    #[tokio::test]
    async fn edits_files_through_the_workspace_capability() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input.txt"), "one\r\ntwo\r\n").unwrap();

        let result = edit_file(
            &Workspace::new(root.path()).unwrap(),
            "input.txt",
            vec![replacement("one\ntwo", "three")],
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.path, root.path().join("input.txt"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
            "three\r\n"
        );
    }

    #[tokio::test]
    async fn large_edits_succeed() {
        let root = tempfile::tempdir().unwrap();
        let content = format!("needle\n{}", "x".repeat(64 * 1024));
        std::fs::write(root.path().join("large.txt"), content).unwrap();

        let result = edit_file(
            &Workspace::new(root.path()).unwrap(),
            "large.txt",
            vec![replacement("needle", "changed")],
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.path, root.path().join("large.txt"));
        assert!(std::fs::read_to_string(root.path().join("large.txt"))
            .unwrap()
            .starts_with("changed\n"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_file_permissions_during_atomic_replace() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("script.sh");
        std::fs::write(&path, "echo old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();

        edit_file(
            &Workspace::new(root.path()).unwrap(),
            "script.sh",
            vec![replacement("old", "new")],
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[tokio::test]
    async fn cancelled_edit_does_not_replace_the_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input.txt"), "old\n").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = edit_file(
            &Workspace::new(root.path()).unwrap(),
            "input.txt",
            vec![replacement("old", "new")],
            cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
        assert_eq!(
            std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn missing_file_error_omits_the_full_path() {
        let root = tempfile::tempdir().unwrap();
        let requested = "missing/input.txt";

        let error = edit_file(
            &Workspace::new(root.path()).unwrap(),
            requested,
            vec![replacement("old", "new")],
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap_err();

        let full_path = root.path().join(requested);
        assert!(matches!(
            error,
            ToolError::Execution(message)
                if message.starts_with("cannot open file:")
                    && !message.contains(&full_path.display().to_string())
                    && !message.contains(requested)
        ));
    }

    #[tokio::test]
    async fn edits_files_outside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file_path = outside.path().join("outside.txt");
        std::fs::write(&file_path, "one\r\ntwo\r\n").unwrap();

        let result = edit_file(
            &Workspace::new(root.path()).unwrap(),
            file_path.to_str().unwrap(),
            vec![replacement("one\ntwo", "three")],
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(result.path, file_path);
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "three\r\n");
    }
}
