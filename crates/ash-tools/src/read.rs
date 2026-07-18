use std::{path::Path, sync::Arc};

use ash_core::{define_tool, Content, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, LimitKind, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// File path, relative to the working directory or absolute within it
    path: String,
    /// First line to read (1-indexed)
    offset: Option<usize>,
    /// Maximum number of lines to read
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "read",
        "Read a text file or image. Text is truncated to 2000 lines or 50KB; use offset and limit to continue. Supported images: jpg, png, gif, webp, and bmp.",
        |ctx, args: ReadArgs| async move {
            let path = crate::path::existing(&ctx.working_dir, &args.path)?;
            read_file(&path, args.offset, args.limit).await
        },
    )
}

async fn read_file(
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolOutput, ToolError> {
    let bytes = tokio::fs::read(path).await.map_err(|error| {
        ToolError::Execution(format!("cannot read {}: {error}", path.display()))
    })?;
    if let Some(media_type) = image_media_type(path) {
        return Ok(ToolOutput::with_attachments(
            format!("Read image file [{media_type}]"),
            vec![Content::Image {
                media_type: media_type.to_string(),
                data: bytes,
            }],
        ));
    }

    let text = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    render_text(&text, offset, limit).map(Into::into)
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn render_text(
    content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    let offset = offset.unwrap_or(1);
    if offset == 0 {
        return Err(ToolError::Execution("offset must be at least 1".into()));
    }
    if limit == Some(0) {
        return Err(ToolError::Execution("limit must be at least 1".into()));
    }

    let lines = content.split('\n').collect::<Vec<_>>();
    let start = offset - 1;
    if start >= lines.len() {
        return Err(ToolError::Execution(format!(
            "offset {offset} is beyond end of file ({} lines total)",
            lines.len()
        )));
    }

    let requested_end = limit.map_or(lines.len(), |limit| start.saturating_add(limit));
    let end = requested_end.min(lines.len());
    let selected = lines[start..end].join("\n");
    let truncated = truncate::head(&selected, DEFAULT_MAX_LINES);
    if truncated.output_lines == 0 && truncated.truncated {
        return Ok(format!(
            "[Line {offset} exceeds the {} read limit. Use bash to inspect a byte range.]",
            truncate::format_size(DEFAULT_MAX_BYTES)
        ));
    }

    let mut output = truncated.content;
    if truncated.truncated {
        let last_line = offset + truncated.output_lines.saturating_sub(1);
        let next_offset = last_line + 1;
        let reason = match truncated.limited_by {
            Some(LimitKind::Lines) => format!("{} line limit", DEFAULT_MAX_LINES),
            Some(LimitKind::Bytes) => format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES)),
            None => String::new(),
        };
        output.push_str(&format!(
            "\n\n[Showing lines {offset}-{last_line} of {} ({reason}). Use offset={next_offset} to continue.]",
            lines.len()
        ));
    } else if end < lines.len() {
        output.push_str(&format!(
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            lines.len() - end,
            end + 1
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_reads_by_lines_and_reports_the_next_offset() {
        let content = (1..=2_001)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_text(&content, None, None).unwrap();

        assert!(rendered.contains("line 2000"));
        assert!(!rendered.contains("line 2001"));
        assert!(rendered.contains("offset=2001"));
    }

    #[test]
    fn keeps_offsets_one_based() {
        let rendered = render_text("one\ntwo\nthree", Some(2), Some(1)).unwrap();

        assert!(rendered.starts_with("two"));
        assert!(rendered.contains("offset=3"));
    }

    #[test]
    fn recognizes_supported_image_extensions_case_insensitively() {
        assert_eq!(image_media_type(Path::new("image.PNG")), Some("image/png"));
        assert_eq!(image_media_type(Path::new("image.svg")), None);
    }

    #[tokio::test]
    async fn returns_images_as_attachments() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("image.png");
        tokio::fs::write(&path, [0x89, 0x50, 0x4e, 0x47])
            .await
            .unwrap();

        let output = read_file(&path, None, None).await.unwrap();

        assert_eq!(output.text, "Read image file [image/png]");
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/png" && data == &[0x89, 0x50, 0x4e, 0x47]
        ));
    }
}
