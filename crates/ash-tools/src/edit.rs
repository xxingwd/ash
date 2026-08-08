use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ash_core::{define_tool, CancellationToken, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

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
    /// File path, relative to the working directory or absolute within it
    path: String,
    /// Non-overlapping replacements matched against the original file
    edits: Vec<Replacement>,
}

pub fn tool(working_dir: Arc<PathBuf>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "edit",
        "Edit one file using one or more exact replacements. Every edits[].oldText must be unique and non-overlapping in the original file; replacements are not applied incrementally.",
        move |ctx, args: EditArgs| {
            let working_dir = Arc::clone(&working_dir);
            let cancellation = ctx.cancellation;
            let deadline = ctx.deadline;
            async move {
                let count = args.edits.len();
                let path =
                    edit_file(&working_dir, &args.path, args.edits, cancellation, deadline).await?;
                Ok(format!(
                    "Successfully replaced {} block(s) in {}.",
                    count,
                    path.display()
                ))
            }
        },
    )
}

async fn edit_file(
    root: &Path,
    requested: &str,
    edits: Vec<Replacement>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<std::path::PathBuf, ToolError> {
    let root = root.to_path_buf();
    let requested = requested.to_string();
    crate::path::run_tool_blocking(cancellation, deadline, move |cancellation, deadline| {
        crate::path::ensure_running(&cancellation, deadline)?;
        let path = crate::path::WorkspacePath::new(&root, &requested)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        crate::path::ensure_running(&cancellation, deadline)?;
        let mut file = path.open_with(&options).map_err(|error| {
            ToolError::Execution(format!(
                "cannot open {}: {error}",
                path.full_path().display()
            ))
        })?;

        let permissions = file
            .metadata()
            .map_err(|error| {
                ToolError::Execution(format!(
                    "cannot inspect {}: {error}",
                    path.full_path().display()
                ))
            })?
            .permissions();
        let raw = crate::path::read_all(&mut file, path.full_path(), &cancellation, deadline)?;
        let raw = String::from_utf8(raw).map_err(|error| {
            ToolError::Execution(format!(
                "cannot read {}: {}",
                path.full_path().display(),
                error.utf8_error()
            ))
        })?;
        let (bom, content) = raw
            .strip_prefix('\u{feff}')
            .map_or(("", raw.as_str()), |content| ("\u{feff}", content));
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

        path.atomic_write(
            content.as_bytes(),
            Some(permissions),
            &cancellation,
            deadline,
        )?;

        Ok(path.full_path().to_path_buf())
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

        edit_file(
            root.path(),
            "input.txt",
            vec![replacement("one\ntwo", "three")],
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
            "three\r\n"
        );
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
            root.path(),
            "script.sh",
            vec![replacement("old", "new")],
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(60),
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
            root.path(),
            "input.txt",
            vec![replacement("old", "new")],
            cancellation,
            Instant::now() + Duration::from_secs(60),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
        assert_eq!(
            std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
            "old\n"
        );
    }
}
