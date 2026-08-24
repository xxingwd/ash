use std::{
    fmt::Write as _,
    io::Read,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ash_core::{define_tool, CancellationToken, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{self, LimitKind, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// File path, relative to the working directory or absolute within it
    path: String,
    /// First line to read (1-indexed)
    offset: Option<NonZeroUsize>,
    /// Maximum number of lines to read
    limit: Option<NonZeroUsize>,
}

enum CurrentLine {
    Buffered(Vec<u8>),
    Oversized,
}

struct TextReadState {
    offset: usize,
    end: usize,
    line: usize,
    saw_input: bool,
    ends_with_line_break: bool,
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
            saw_input: false,
            ends_with_line_break: false,
            current: CurrentLine::Buffered(Vec::new()),
            selected: Vec::with_capacity(DEFAULT_MAX_BYTES.min(8 * 1024)),
            output_lines: 0,
            limited_by: None,
        }
    }

    const fn accepts_current_line(&self) -> bool {
        self.line >= self.offset && self.line < self.end && self.limited_by.is_none()
    }

    fn push(&mut self, byte: u8) {
        self.saw_input = true;
        self.ends_with_line_break = false;
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
        self.saw_input = true;
        self.ends_with_line_break = true;
        self.line = self.line.saturating_add(1);
    }

    fn finish_file(&mut self) {
        let output_lines = self.output_lines;
        let terminal_line_is_empty = !self.saw_input || self.ends_with_line_break;
        self.finish_line();
        if terminal_line_is_empty && self.output_lines > output_lines {
            self.output_lines -= 1;
        }
    }

    fn total_lines(&self) -> usize {
        if self.saw_input {
            self.line
                .saturating_sub(usize::from(self.ends_with_line_break))
        } else {
            0
        }
    }
}

pub fn tool(working_dir: Arc<PathBuf>) -> Result<Arc<dyn Tool>, ToolError> {
    define_tool(
        "read",
        "Read a text file or image. Text is truncated to 2000 lines or 50KB; use offset and limit to continue. Images are resized to 2000px / 5MB. Supported images: jpg, png, gif, webp, and bmp.",
        move |ctx, args: ReadArgs| {
            let working_dir = Arc::clone(&working_dir);
            let deadline = ctx.require_deadline();
            let cancellation = ctx.cancellation;
            async move {
                let deadline = deadline?;
                read_file(
                    &working_dir,
                    &args.path,
                    args.offset.map(NonZeroUsize::get),
                    args.limit.map(NonZeroUsize::get),
                    cancellation,
                    deadline,
                )
                .await
            }
        },
    )
}

async fn read_file(
    root: &Path,
    requested: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<ToolOutput, ToolError> {
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
                "cannot read {}: {error}",
                path.full_path().display()
            ))
        })?;
        if let Some(media_type) = image_media_type(path.full_path()) {
            let bytes = crate::path::read_limited(
                &mut file,
                path.full_path(),
                crate::image::MAX_IMAGE_INGEST_BYTES,
                &cancellation,
                deadline,
            )?;
            return crate::image::tool_output(media_type, &bytes);
        }
        render_reader(file, offset, limit, &cancellation, deadline).map(Into::into)
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
    render_reader(
        std::io::Cursor::new(content),
        offset,
        limit,
        &CancellationToken::new(),
        Instant::now() + std::time::Duration::from_mins(1),
    )
}

fn render_reader(
    mut reader: impl Read,
    offset: Option<usize>,
    limit: Option<usize>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<String, ToolError> {
    crate::path::ensure_running(cancellation, deadline)?;
    let offset = offset.unwrap_or(1);
    let requested_lines = limit.unwrap_or(usize::MAX).min(DEFAULT_MAX_LINES);
    let end = offset.saturating_add(requested_lines);
    let mut state = TextReadState::new(offset, end);
    let mut skip_lf = false;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        crate::path::ensure_running(cancellation, deadline)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|error| ToolError::Execution(format!("cannot read file: {error}")))?;
        if count == 0 {
            state.finish_file();
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
    crate::path::ensure_running(cancellation, deadline)?;

    let total_lines = state.total_lines();
    if offset > total_lines.max(1) {
        return Err(ToolError::Execution(format!(
            "offset {offset} is beyond end of file ({total_lines} lines total)"
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
    let last_line = offset + state.output_lines.saturating_sub(1);
    if let Some(limited_by) = state.limited_by {
        let next_offset = last_line + 1;
        let reason = match limited_by {
            LimitKind::Lines => format!("{DEFAULT_MAX_LINES} line limit"),
            LimitKind::Bytes => format!("{} limit", truncate::format_size(DEFAULT_MAX_BYTES)),
        };
        let _ = write!(
            output,
            "\n\n[Showing lines {offset}-{last_line} of {total_lines} ({reason}). Use offset={next_offset} to continue.]"
        );
    } else if last_line < total_lines {
        let _ = write!(
            output,
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            total_lines - last_line,
            last_line + 1
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash_core::Content;
    use image::GenericImageView;
    use std::time::Duration;

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
    fn renders_empty_files_without_a_misleading_tail_message() {
        let rendered = render_text("", None, None).unwrap();
        assert_eq!(rendered, "");
    }

    #[test]
    fn renders_all_empty_lines_without_a_misleading_tail_message() {
        let rendered = render_text("\n\n\n", None, None).unwrap();
        assert_eq!(rendered, "\n\n\n");
    }

    #[test]
    fn keeps_offsets_one_based() {
        let rendered = render_text("one\ntwo\nthree", Some(2), Some(1)).unwrap();

        assert!(rendered.starts_with("two"));
        assert!(rendered.contains("offset=3"));
    }

    #[test]
    fn rejects_offset_after_a_trailing_newline() {
        let error = render_text("one\ntwo\n", Some(3), None).unwrap_err();

        assert!(matches!(
            error,
            ToolError::Execution(message)
                if message == "offset 3 is beyond end of file (2 lines total)"
        ));
    }

    #[test]
    fn does_not_report_a_phantom_line_after_a_trailing_newline() {
        let rendered = render_text("one\ntwo\n", Some(2), Some(1)).unwrap();

        assert_eq!(rendered, "two");
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

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        use image::{ImageFormat, Rgb, RgbImage};
        use std::io::Cursor;

        let image = RgbImage::from_pixel(width, height, Rgb([0xcc, 0x33, 0x00]));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode png");
        bytes.into_inner()
    }

    #[tokio::test]
    async fn returns_images_as_attachments() {
        let root = tempfile::tempdir().unwrap();
        let bytes = png_bytes(8, 4);
        tokio::fs::write(root.path().join("image.png"), &bytes)
            .await
            .unwrap();

        let output = read_file(
            root.path(),
            "image.png",
            None,
            None,
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert_eq!(output.text, "Read image file [image/png]");
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/png" && data == &bytes
        ));
    }

    #[tokio::test]
    async fn resizes_oversized_images_before_attaching_them() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("wide.png"), png_bytes(2_400, 800))
            .await
            .unwrap();

        let output = read_file(
            root.path(),
            "wide.png",
            None,
            None,
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap();

        assert!(output
            .text
            .contains("[Image converted from image/png to image/jpeg.]"));
        assert!(output
            .text
            .contains("[Image resized from 2400x800 to 2000x666.]"));
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/jpeg"
                    && image::load_from_memory(data).unwrap().dimensions() == (2000, 666)
        ));
    }

    #[tokio::test]
    async fn rejects_images_over_the_ingest_limit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("huge.png");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((crate::image::MAX_IMAGE_INGEST_BYTES as u64) + 1)
            .unwrap();

        let error = read_file(
            root.path(),
            "huge.png",
            None,
            None,
            CancellationToken::new(),
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            ToolError::Execution(message)
                if message.contains("exceeds the 20.0MB read limit")
        ));
    }

    #[tokio::test]
    async fn cancelled_read_does_not_open_the_file() {
        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("notes.txt"), "secret\n")
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = read_file(
            root.path(),
            "notes.txt",
            None,
            None,
            cancellation,
            Instant::now() + Duration::from_mins(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolError::Cancelled));
    }
}
