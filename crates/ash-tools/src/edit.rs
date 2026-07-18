use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
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

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "edit",
        "Edit one file using one or more exact replacements. Every edits[].oldText must be unique and non-overlapping in the original file; replacements are not applied incrementally.",
        |ctx, args: EditArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;
            let raw = tokio::fs::read_to_string(&path).await.map_err(|error| {
                ToolError::Execution(format!("cannot read {}: {error}", path.display()))
            })?;
            let (bom, content) = raw
                .strip_prefix('\u{feff}')
                .map_or(("", raw.as_str()), |content| ("\u{feff}", content));
            let line_ending = if content.contains("\r\n") { "\r\n" } else { "\n" };
            let normalized = normalize_newlines(content);
            let edits = args
                .edits
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
            tokio::fs::write(&path, format!("{bom}{edited}"))
                .await
                .map_err(|error| {
                    ToolError::Execution(format!("cannot write {}: {error}", path.display()))
                })?;

            Ok(format!(
                "Successfully replaced {} block(s) in {}.",
                edits.len(),
                args.path
            ))
        },
    )
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
        if occurrences.is_empty() {
            return Err(ToolError::Execution(format!(
                "edits[{index}].oldText was not found"
            )));
        }
        if occurrences.len() > 1 {
            return Err(ToolError::Execution(format!(
                "edits[{index}].oldText matches {} locations; include more context",
                occurrences.len()
            )));
        }
        let start = occurrences[0];
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
}
