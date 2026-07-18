use std::path::PathBuf;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use crate::{
    history_block::HistoryBlock,
    markdown::render_markdown,
    palette::Rgb,
    scrollback::{sanitize_terminal_text, wrap_text},
    tool_display::{read_group_summary, tool_call_summary},
    welcome_card::{welcome_card, WelcomeLine, WelcomeStyle},
};

const BULLET_PREFIX_COLUMNS: u16 = 2;
const CHANGE_PREVIEW_MAX_LINES: usize = 12;

/// A complete piece of output Ash still owns and can therefore re-render.
///
/// Once an entry is emitted to stdout it is dropped from the live queue. The
/// terminal's scrollback is deliberately not modeled here: it belongs to the
/// terminal and cannot be safely reflowed after a resize.
#[derive(Clone, Debug)]
pub(crate) struct LiveBlock {
    kind: LiveBlockKind,
}

#[derive(Clone, Debug)]
enum LiveBlockKind {
    Welcome(PathBuf),
    History(HistoryBlock),
    Assistant(String),
    Thought(String),
    ReadGroup {
        arguments: Vec<Value>,
    },
    Tool {
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
    },
}

impl LiveBlock {
    pub(crate) fn welcome(working_dir: PathBuf) -> Self {
        Self::new(LiveBlockKind::Welcome(working_dir))
    }

    pub(crate) fn history(block: HistoryBlock) -> Self {
        Self::new(LiveBlockKind::History(block))
    }

    pub(crate) fn assistant(source: String) -> Self {
        Self::new(LiveBlockKind::Assistant(source))
    }

    pub(crate) fn thought(source: String) -> Self {
        Self::new(LiveBlockKind::Thought(source))
    }

    pub(crate) fn tool(name: String, arguments: Value, output: String, is_error: bool) -> Self {
        if name == "read" && !is_error {
            return Self::new(LiveBlockKind::ReadGroup {
                arguments: vec![arguments],
            });
        }
        Self::new(LiveBlockKind::Tool {
            name,
            arguments,
            output,
            is_error,
        })
    }

    fn new(kind: LiveBlockKind) -> Self {
        Self { kind }
    }

    pub(crate) fn render(&self, width: u16, composer_background: Option<Rgb>) -> Buffer {
        match &self.kind {
            LiveBlockKind::Welcome(working_dir) => render_welcome(width, working_dir),
            LiveBlockKind::History(block) => block.render(width, composer_background),
            LiveBlockKind::Assistant(source) => {
                render_markdown_block(source, Style::default(), width)
            }
            LiveBlockKind::Thought(source) => render_markdown_block(
                source,
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
                width,
            ),
            LiveBlockKind::ReadGroup { arguments } => render_read_group(arguments, width),
            LiveBlockKind::Tool {
                name,
                arguments,
                output,
                is_error,
            } => render_tool(name, arguments, output, *is_error, width),
        }
    }
}

fn render_read_group(arguments: &[Value], width: u16) -> Buffer {
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, detail) = read_group_summary(arguments, detail_width);
    render_tool_title(action, detail, false, width)
}

fn render_welcome(width: u16, working_dir: &std::path::Path) -> Buffer {
    let lines = welcome_card(width, working_dir);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, line) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        buffer.set_line(0, y, &styled_welcome_line(line), width);
    }
    buffer
}

fn styled_welcome_line(line: &WelcomeLine) -> Line<'static> {
    let frame_style = Style::default().fg(Color::Cyan);
    let content_style = match line.style {
        WelcomeStyle::Frame => frame_style,
        WelcomeStyle::Subtitle => Style::default().add_modifier(Modifier::DIM),
        WelcomeStyle::Logo | WelcomeStyle::Title => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    };

    let Some(content) = line
        .text
        .strip_prefix('│')
        .and_then(|content| content.strip_suffix('│'))
    else {
        return Line::styled(line.text.clone(), content_style);
    };

    Line::from(vec![
        Span::styled("│", frame_style),
        Span::styled(content.to_string(), content_style),
        Span::styled("│", frame_style),
    ])
}

fn render_markdown_block(source: &str, style: Style, width: u16) -> Buffer {
    let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let mut lines = render_markdown(source, content_width);
    for line in &mut lines {
        line.patch_style(style);
    }
    render_markdown_lines(&lines, true, width)
}

pub(crate) fn render_assistant_lines(
    lines: &[crate::markdown::RenderedLine],
    first_line: bool,
    width: u16,
) -> Buffer {
    render_markdown_lines(lines, first_line, width)
}

fn render_markdown_lines(
    lines: &[crate::markdown::RenderedLine],
    first_line: bool,
    width: u16,
) -> Buffer {
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, rendered) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        let mut spans = vec![if index == 0 && first_line {
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM))
        } else {
            Span::raw("  ")
        }];
        spans.extend(rendered.ratatui_line().spans);
        buffer.set_line(0, y, &Line::from(spans), width);
    }
    buffer
}

fn render_tool(name: &str, arguments: &Value, _output: &str, is_error: bool, width: u16) -> Buffer {
    if !is_error {
        let preview = match name {
            "edit" => edit_preview(arguments),
            "write" => arguments
                .get("content")
                .and_then(Value::as_str)
                .map(write_preview),
            _ => None,
        };
        if let Some(preview) = preview {
            return render_change_preview(name, arguments, &preview, width);
        }
    }
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, detail) = tool_call_summary(name, arguments, is_error, detail_width);
    render_tool_title(action, detail, is_error, width)
}

fn render_tool_title(action: String, detail: String, is_error: bool, width: u16) -> Buffer {
    let bullet_style = if is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled("•", bullet_style), Span::raw(" ")];
    spans.push(Span::styled(
        action,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(detail));
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), 1));
    buffer.set_line(0, 0, &Line::from(spans), width);
    buffer
}

fn edit_preview(arguments: &Value) -> Option<String> {
    let edits = arguments.get("edits")?.as_array()?;
    let mut lines = Vec::new();
    for edit in edits {
        lines.extend(
            edit.get("oldText")?
                .as_str()?
                .lines()
                .map(|line| format!("-{line}")),
        );
        lines.extend(
            edit.get("newText")?
                .as_str()?
                .lines()
                .map(|line| format!("+{line}")),
        );
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn write_preview(content: &str) -> String {
    sanitize_terminal_text(content)
        .lines()
        .map(|line| format!("+{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_change_preview(name: &str, arguments: &Value, preview: &str, width: u16) -> Buffer {
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, detail) = tool_call_summary(name, arguments, false, detail_width);
    let sanitized = sanitize_terminal_text(preview);
    let source_lines = sanitized
        .lines()
        .filter(|line| !matches!(*line, "--- before" | "+++ after"))
        .collect::<Vec<_>>();
    let shown = source_lines.len().min(CHANGE_PREVIEW_MAX_LINES);
    let content_x = if width > BULLET_PREFIX_COLUMNS {
        BULLET_PREFIX_COLUMNS
    } else {
        0
    };
    let content_width = width.saturating_sub(content_x).max(1);
    let mut rendered = Vec::new();
    for line in source_lines.iter().take(shown) {
        let style = change_line_style(line);
        for row in wrap_text(line, content_width) {
            rendered.push((row, style));
        }
    }
    if shown < source_lines.len() {
        rendered.push((
            format!("… {} more lines", source_lines.len() - shown),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let height = u16::try_from(rendered.len().saturating_add(1))
        .unwrap_or(u16::MAX)
        .max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    let mut title = vec![
        Span::styled(
            "•",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(action, Style::default().add_modifier(Modifier::BOLD)),
    ];
    if !detail.is_empty() {
        title.push(Span::raw(" "));
        title.push(Span::raw(detail));
    }
    buffer.set_line(0, 0, &Line::from(title), width);
    for (index, (line, style)) in rendered.iter().enumerate() {
        let Ok(y) = u16::try_from(index.saturating_add(1)) else {
            break;
        };
        buffer.set_string(content_x, y, line, *style);
    }
    buffer
}

fn change_line_style(line: &str) -> Style {
    if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_blocks_reflow_at_the_current_width() {
        let block = LiveBlock::assistant("a long line that must wrap".to_string());

        let narrow = block.render(10, None);
        let wide = block.render(40, None);

        assert!(narrow.area.height > wide.area.height);
    }

    #[test]
    fn successful_reads_render_a_summary() {
        let block = LiveBlock::tool(
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/inline.rs"}),
            String::new(),
            false,
        );

        let buffer = block.render(80, None);
        let rendered = (0..buffer.area.width)
            .filter_map(|column| buffer.cell((column, 0)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("• Read inline.rs"));
    }

    #[test]
    fn edit_and_write_render_different_change_previews() {
        let edit = LiveBlock::tool(
            "edit".to_string(),
            serde_json::json!({
                "path": "/workspace/src/main.rs",
                "edits": [{"oldText": "old", "newText": "new"}]
            }),
            "Successfully replaced 1 block(s).".to_string(),
            false,
        )
        .render(60, None);
        let write = LiveBlock::tool(
            "write".to_string(),
            serde_json::json!({"path": "/workspace/src/new.rs", "content": "one\ntwo"}),
            String::new(),
            false,
        )
        .render(60, None);

        assert_eq!(edit.cell((2, 1)).expect("deleted line").fg, Color::Red);
        assert_eq!(edit.cell((2, 2)).expect("added line").fg, Color::Green);
        assert_eq!(write.cell((2, 1)).expect("written line").fg, Color::Green);

        let tiny = LiveBlock::tool(
            "write".to_string(),
            serde_json::json!({"path": "new.rs", "content": "one"}),
            String::new(),
            false,
        )
        .render(1, None);
        assert_eq!(tiny.area.width, 1);
    }

    #[test]
    fn welcome_frame_stays_cyan_around_dim_content() {
        let buffer = render_welcome(40, std::path::Path::new("/workspace/ash"));
        let subtitle_row = (0..buffer.area.height)
            .find(|&row| {
                (0..buffer.area.width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("TERMINAL CODING AGENT")
            })
            .expect("subtitle row");

        assert_eq!(
            buffer.cell((0, subtitle_row)).expect("left frame").fg,
            Color::Cyan
        );
        assert_eq!(
            buffer
                .cell((buffer.area.width - 1, subtitle_row))
                .expect("right frame")
                .fg,
            Color::Cyan
        );
        assert!(buffer
            .cell((2, subtitle_row))
            .expect("subtitle")
            .modifier
            .contains(Modifier::DIM));
    }
}
