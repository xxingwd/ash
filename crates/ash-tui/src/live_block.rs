use std::{cell::RefCell, path::PathBuf, sync::Arc};

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
    scrollback::{sanitize_terminal_text, wrap_text},
    tool_display::{read_group_detail, read_group_summary, tool_call_summary},
    welcome_card::{welcome_card, WelcomeLine, WelcomeStyle},
};

const BULLET_PREFIX_COLUMNS: u16 = 2;
const CHANGE_PREVIEW_MAX_LINES: usize = 12;

/// A complete transcript entry retained by Ash and re-rendered after updates.
#[derive(Clone, Debug)]
pub(crate) struct LiveBlock {
    id: u64,
    turn_id: Option<u64>,
    kind: LiveBlockKind,
    cache: RefCell<Option<RenderCache>>,
}

#[derive(Clone, Debug)]
struct RenderCache {
    width: u16,
    buffer: Arc<Buffer>,
}

#[derive(Clone, Debug)]
enum LiveBlockKind {
    Welcome(PathBuf),
    History(HistoryBlock),
    Assistant(String),
    Thought {
        elapsed_seconds: u64,
    },
    ReadGroup(Vec<String>),
    Tool {
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
    },
}

impl LiveBlock {
    pub(crate) fn welcome(id: u64, working_dir: PathBuf) -> Self {
        Self::new(id, LiveBlockKind::Welcome(working_dir))
    }

    pub(crate) fn history(id: u64, block: HistoryBlock) -> Self {
        Self::new(id, LiveBlockKind::History(block))
    }

    pub(crate) fn assistant(id: u64, source: String) -> Self {
        Self::new(id, LiveBlockKind::Assistant(source))
    }

    pub(crate) fn thought(id: u64, elapsed_seconds: u64) -> Self {
        Self::new(id, LiveBlockKind::Thought { elapsed_seconds })
    }

    pub(crate) fn tool(
        id: u64,
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
    ) -> Self {
        if !is_error {
            if let Some(detail) = read_group_detail(&name, &arguments) {
                return Self::new(id, LiveBlockKind::ReadGroup(vec![detail]));
            }
        }
        Self::new(
            id,
            LiveBlockKind::Tool {
                name,
                arguments,
                output,
                is_error,
            },
        )
    }

    fn new(id: u64, kind: LiveBlockKind) -> Self {
        Self {
            id,
            turn_id: None,
            kind,
            cache: RefCell::new(None),
        }
    }

    pub(crate) fn with_turn(mut self, turn_id: Option<u64>) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn belongs_to_turn(&self, turn_id: u64) -> bool {
        self.turn_id == Some(turn_id)
    }

    pub(crate) const fn turn_id(&self) -> Option<u64> {
        self.turn_id
    }

    pub(crate) fn is_response_for_turn(&self, turn_id: u64) -> bool {
        self.belongs_to_turn(turn_id)
            && !matches!(self.kind, LiveBlockKind::History(HistoryBlock::User(_)))
    }

    pub(crate) fn append_markdown_source(&mut self, source: &str) -> bool {
        let LiveBlockKind::Assistant(current) = &mut self.kind else {
            return false;
        };
        current.push_str(source);
        self.invalidate();
        true
    }

    pub(crate) fn try_append_tool(
        &mut self,
        name: &str,
        arguments: &Value,
        is_error: bool,
    ) -> bool {
        if is_error {
            return false;
        }
        let Some(detail) = read_group_detail(name, arguments) else {
            return false;
        };
        let LiveBlockKind::ReadGroup(details) = &mut self.kind else {
            return false;
        };
        details.push(detail);
        self.invalidate();
        true
    }

    pub(crate) fn render(&self, width: u16) -> Arc<Buffer> {
        let width = width.max(1);
        if let Some(buffer) = self
            .cache
            .borrow()
            .as_ref()
            .filter(|cached| cached.width == width)
            .map(|cached| Arc::clone(&cached.buffer))
        {
            return buffer;
        }

        let buffer = Arc::new(self.render_uncached(width));
        self.cache.replace(Some(RenderCache {
            width,
            buffer: Arc::clone(&buffer),
        }));
        buffer
    }

    pub(crate) fn clear_render_cache(&self) {
        self.cache.replace(None);
    }

    fn render_uncached(&self, width: u16) -> Buffer {
        match &self.kind {
            LiveBlockKind::Welcome(working_dir) => render_welcome(width, working_dir),
            LiveBlockKind::History(block) => block.render(width),
            LiveBlockKind::Assistant(source) => {
                render_markdown_block(source, Style::default(), width)
            }
            LiveBlockKind::Thought { elapsed_seconds } => render_thought(*elapsed_seconds, width),
            LiveBlockKind::ReadGroup(details) => render_read_group(details, width),
            LiveBlockKind::Tool {
                name,
                arguments,
                output,
                is_error,
            } => render_tool(name, arguments, output, *is_error, width),
        }
    }

    fn invalidate(&mut self) {
        self.cache.get_mut().take();
    }
}

fn render_thought(elapsed_seconds: u64, width: u16) -> Buffer {
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), 1));
    buffer.set_line(
        0,
        0,
        &Line::styled(
            format!(
                "• Thought for {}",
                crate::stream_state::format_elapsed(elapsed_seconds)
            ),
            style,
        ),
        width,
    );
    buffer
}

fn render_read_group(details: &[String], width: u16) -> Buffer {
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, detail) = read_group_summary(details, detail_width);
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
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    for (index, rendered) in lines.iter().take(usize::from(height)).enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        let mut spans = vec![if index == 0 {
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM))
        } else {
            Span::raw("  ")
        }];
        spans.extend(rendered.ratatui_line().spans);
        buffer.set_line(0, y, &Line::from(spans), width);
    }
    buffer
}

fn render_tool(name: &str, arguments: &Value, output: &str, is_error: bool, width: u16) -> Buffer {
    if !is_error {
        let preview = match name {
            "edit" if !output.is_empty() => Some(output.to_string()),
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
        let block = LiveBlock::assistant(1, "a long line that must wrap".to_string());

        let narrow = block.render(10);
        let wide = block.render(40);

        assert!(narrow.area.height > wide.area.height);
    }

    #[test]
    fn completed_blocks_reuse_rendering_until_the_content_changes() {
        let mut block = LiveBlock::assistant(1, "hello".to_string());

        let first = block.render(40);
        let second = block.render(40);
        assert!(std::sync::Arc::ptr_eq(&first, &second));

        assert!(block.append_markdown_source(" world"));
        let updated = block.render(40);
        assert!(!std::sync::Arc::ptr_eq(&first, &updated));
    }

    #[test]
    fn committed_blocks_can_drop_their_render_cache_before_replay() {
        let block = LiveBlock::assistant(1, "hello".to_string());

        let committed = block.render(40);
        block.clear_render_cache();
        let replayed = block.render(40);

        assert!(!std::sync::Arc::ptr_eq(&committed, &replayed));
        assert_eq!(row_text(&committed, 0), row_text(&replayed, 0));
    }

    #[test]
    fn blocks_keep_turn_ownership() {
        let response = LiveBlock::history(1, HistoryBlock::info("done")).with_turn(Some(7));
        let input = LiveBlock::history(2, HistoryBlock::user("question")).with_turn(Some(7));

        assert!(response.belongs_to_turn(7));
        assert!(response.is_response_for_turn(7));
        assert!(!input.is_response_for_turn(7));
    }

    #[test]
    fn completed_thoughts_render_as_a_single_summary_line() {
        let block = LiveBlock::thought(1, 3);
        let rendered = block.render(40);

        assert_eq!(rendered.area.height, 1);
        assert_eq!(row_text(&rendered, 0), "• Thought for 3s");
    }

    #[test]
    fn consecutive_successful_matching_tools_share_one_summary() {
        let mut block = LiveBlock::tool(
            1,
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/inline.rs"}),
            String::new(),
            false,
        );

        assert!(block.try_append_tool(
            "read",
            &serde_json::json!({"path": "/workspace/src/viewport.rs"}),
            false,
        ));
        assert!(!block.try_append_tool(
            "bash",
            &serde_json::json!({"command": "rg ToolGroup src"}),
            false,
        ));
        assert!(!block.try_append_tool(
            "read",
            &serde_json::json!({"path": "/workspace/src/live_block.rs"}),
            true,
        ));

        let buffer = block.render(80);
        let rendered = (0..buffer.area.width)
            .filter_map(|column| buffer.cell((column, 0)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("• Read inline.rs, viewport.rs"));
    }

    #[test]
    fn non_groupable_tools_remain_independent() {
        let mut block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "cargo test"}),
            String::new(),
            false,
        );

        assert!(!block.try_append_tool(
            "bash",
            &serde_json::json!({"command": "cargo clippy"}),
            false,
        ));
    }

    #[test]
    fn edit_and_write_render_different_change_previews() {
        let edit = LiveBlock::tool(
            1,
            "edit".to_string(),
            serde_json::json!({"path": "/workspace/src/main.rs"}),
            "--- before\n+++ after\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            false,
        )
        .render(60);
        let write = LiveBlock::tool(
            2,
            "write".to_string(),
            serde_json::json!({"path": "/workspace/src/new.rs", "content": "one\ntwo"}),
            String::new(),
            false,
        )
        .render(60);

        assert_eq!(edit.cell((2, 2)).expect("deleted line").fg, Color::Red);
        assert_eq!(edit.cell((2, 3)).expect("added line").fg, Color::Green);
        assert_eq!(write.cell((2, 1)).expect("written line").fg, Color::Green);

        let tiny = LiveBlock::tool(
            3,
            "write".to_string(),
            serde_json::json!({"path": "new.rs", "content": "one"}),
            String::new(),
            false,
        )
        .render(1);
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

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| !crate::buffer::cell_is_skipped(cell))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}
