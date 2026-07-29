use std::{io::Read, path::Path, sync::Arc};

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

enum CurrentLine {
    Buffered(Vec<u8>),
    Oversized,
}

struct TextReadState {
    offset: usize,
    end: usize,
    line: usize,
    current: CurrentLine,
    selected: Vec<u8>,
    output_lines: usize,
    limited_by: Option<LimitKind>,
}

impl TextReadState {
    fn new(offset: usize, end: usize) -> Self {
        Self {
            offset,
            end,
            line: 1,
            current: CurrentLine::Buffered(Vec::new()),
            selected: Vec::with_capacity(DEFAULT_MAX_BYTES.min(8 * 1024)),
            output_lines: 0,
            limited_by: None,
        }
    }

    fn accepts_current_line(&self) -> bool {
        self.line >= self.offset && self.line < self.end && self.limited_by.is_none()
    }

    fn push(&mut self, byte: u8) {
        if !self.accepts_current_line() {
            return;
        }
        let CurrentLine::Buffered(current) = &mut self.current else {
            return;
        };
        if current.len() < DEFAULT_MAX_BYTES {
            current.push(byte);
        } else {
            self.current = CurrentLine::Oversized;
        }
    }

    fn finish_line(&mut self) {
        let current = std::mem::replace(&mut self.current, CurrentLine::Buffered(Vec::new()));
        if !self.accepts_current_line() {
            return;
        }
        let CurrentLine::Buffered(mut current) = current else {
            self.limited_by = Some(LimitKind::Bytes);
            return;
        };
        let separator = usize::from(self.output_lines > 0);
        if self
            .selected
            .len()
            .saturating_add(separator)
            .saturating_add(current.len())
            > DEFAULT_MAX_BYTES
        {
            self.limited_by = Some(LimitKind::Bytes);
            return;
        }
        if separator != 0 {
            self.selected.push(b'\n');
        }
        self.selected.append(&mut current);
        self.output_lines = self.output_lines.saturating_add(1);
    }

    fn next_line(&mut self) {
        self.finish_line();
        self.line = self.line.saturating_add(1);
    }
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "read",
        "Read a text file or image. Text is truncated to 2000 lines or 50KB; use offset and limit to continue. Supported images: jpg, png, gif, webp, and bmp.",
        |ctx, args: ReadArgs| async move {
            read_file(&ctx.working_dir, &args.path, args.offset, args.limit).await
        },
    )
}

async fn read_file(
    root: &Path,
    requested: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolOutput, ToolError> {
    let root = root.to_path_buf();
    let requested = requested.to_string();
    crate::path::run_blocking(move || {
        let path = crate::path::WorkspacePath::new(&root, &requested)?;
        if let Some(media_type) = image_media_type(path.full_path()) {
            let bytes = path.read().map_err(|error| {
                ToolError::Execution(format!(
                    "cannot read {}: {error}",
                    path.full_path().display()
                ))
            })?;
            return Ok(ToolOutput::with_attachments(
                format!("Read image file [{media_type}]"),
                vec![Content::Image {
                    media_type: media_type.to_string(),
                    data: bytes,
                }],
            ));
        }

        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        let file = path.open_with(&options).map_err(|error| {
            ToolError::Execution(format!(
                "cannot read {}: {error}",
                path.full_path().display()
            ))
        })?;
        render_reader(file, offset, limit).map(Into::into)
    })
    .await
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

#[cfg(test)]
fn render_text(
    content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    render_reader(std::io::Cursor::new(content), offset, limit)
}

fn render_reader(
    mut reader: impl Read,
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

    let requested_lines = limit.unwrap_or(usize::MAX).min(DEFAULT_MAX_LINES);
    let end = offset.saturating_add(requested_lines);
    let mut state = TextReadState::new(offset, end);
    let mut skip_lf = false;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| ToolError::Execution(format!("cannot read file: {error}")))?;
        if count == 0 {
            state.finish_line();
            break;
        }
        for &byte in &buffer[..count] {
            if skip_lf {
                skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\n' | b'\r') {
                state.next_line();
                skip_lf = byte == b'\r';
            } else {
                state.push(byte);
            }
        }
    }

    let total_lines = state.line;
    if offset > total_lines {
        return Err(ToolError::Execution(format!(
            "offset {offset} is beyond end of file ({} lines total)",
            total_lines
        )));
    }
    if state.limited_by.is_none()
        && limit.unwrap_or(usize::MAX) > DEFAULT_MAX_LINES
        && total_lines >= end
    {
        state.limited_by = Some(LimitKind::Lines);
    }
    if state.output_lines == 0 && state.limited_by == Some(LimitKind::Bytes) {
        return Ok(format!(
            "[Line {offset} exceeds the {} read limit. Use bash to inspect a byte range.]",
            truncate::format_size(DEFAULT_MAX_BYTES)
        ));
    }

    let mut output = String::from_utf8_lossy(&state.selected).into_owned();
    if let Some(limited_by) = state.limited_by {
        let last_line = offset + state.output_lines.saturating_sub(1);
        let next_offset = last_line + 1;
        let reason = match limited_by {
            LimitKind::Lines => format!("{} line limit", DEFAULT_MAX_LINES),
            LimitKind::Bytes => format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES)),
        };
        output.push_str(&format!(
            "\n\n[Showing lines {offset}-{last_line} of {} ({reason}). Use offset={next_offset} to continue.]",
            total_lines
        ));
    } else {
        let last_line = offset + state.output_lines.saturating_sub(1);
        if last_line < total_lines {
            output.push_str(&format!(
                "\n\n[{} more lines in file. Use offset={} to continue.]",
                total_lines - last_line,
                last_line + 1
            ));
        }
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
    fn bounds_memory_for_an_oversized_text_line() {
        let content = "x".repeat(DEFAULT_MAX_BYTES * 4);
        let rendered = render_text(&content, None, None).unwrap();

        assert!(rendered.contains("Line 1 exceeds the 50.0KB read limit"));
        assert!(rendered.len() < 256);
    }

    #[test]
    fn recognizes_supported_image_extensions_case_insensitively() {
        assert_eq!(image_media_type(Path::new("image.PNG")), Some("image/png"));
        assert_eq!(image_media_type(Path::new("image.svg")), None);
    }

    #[tokio::test]
    async fn returns_images_as_attachments() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("image.png"), [0x89, 0x50, 0x4e, 0x47])
            .await
            .unwrap();

        let output = read_file(root.path(), "image.png", None, None)
            .await
            .unwrap();

        assert_eq!(output.text, "Read image file [image/png]");
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/png" && data == &[0x89, 0x50, 0x4e, 0x47]
        ));
    }
}
