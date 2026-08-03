use std::{cell::RefCell, path::PathBuf, sync::Arc};

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::{
    history_block::HistoryBlock,
    markdown::{render_markdown, RenderedLine, StreamingMarkdownCache},
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
    source_len: Option<usize>,
    expanded: bool,
    buffer: Arc<Buffer>,
    streaming_markdown: Option<StreamingMarkdownCache>,
}

#[derive(Clone, Debug)]
enum LiveBlockKind {
    Welcome(PathBuf),
    History(HistoryBlock),
    Assistant {
        source: String,
        streaming: bool,
    },
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
        Self::new(
            id,
            LiveBlockKind::Assistant {
                source,
                streaming: false,
            },
        )
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
        let LiveBlockKind::Assistant {
            source: current,
            streaming,
        } = &mut self.kind
        else {
            return false;
        };
        current.push_str(source);
        *streaming = true;
        true
    }

    pub(crate) fn finalize_markdown(&mut self) {
        let LiveBlockKind::Assistant { streaming, .. } = &mut self.kind else {
            return;
        };
        *streaming = false;
        self.invalidate();
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

    pub(crate) fn render(&self, width: u16, expanded: bool) -> Arc<Buffer> {
        let width = width.max(1);
        let source_len = self.source_len();
        if let Some(buffer) = self
            .cache
            .borrow()
            .as_ref()
            .filter(|cached| {
                cached.width == width
                    && cached.source_len == source_len
                    && cached.expanded == expanded
            })
            .map(|cached| Arc::clone(&cached.buffer))
        {
            return buffer;
        }

        let previous = self.cache.borrow_mut().take();
        let (buffer, streaming_markdown) = match &self.kind {
            LiveBlockKind::Assistant {
                source,
                streaming: true,
            } => {
                let previous = previous.filter(|cached| cached.width == width);
                let (previous_buffer, mut markdown) = previous.map_or_else(
                    || (None, StreamingMarkdownCache::default()),
                    |cached| {
                        (
                            Some(cached.buffer),
                            cached.streaming_markdown.unwrap_or_default(),
                        )
                    },
                );
                let dirty_from = markdown.stable_lines().len();
                let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
                let tail = markdown.update(source, content_width);
                let buffer = update_markdown_buffer(
                    previous_buffer,
                    markdown.stable_lines(),
                    &tail,
                    Style::default(),
                    width,
                    dirty_from,
                );
                (buffer, Some(markdown))
            }
            _ => (self.render_uncached(width, expanded), None),
        };
        let buffer = Arc::new(buffer);
        self.cache.replace(Some(RenderCache {
            width,
            source_len,
            expanded,
            buffer: Arc::clone(&buffer),
            streaming_markdown,
        }));
        buffer
    }

    pub(crate) fn clear_render_cache(&self) {
        self.cache.replace(None);
    }

    fn render_uncached(&self, width: u16, expanded: bool) -> Buffer {
        match &self.kind {
            LiveBlockKind::Welcome(working_dir) => render_welcome(width, working_dir),
            LiveBlockKind::History(block) => block.render(width),
            LiveBlockKind::Assistant { source, .. } => {
                render_markdown_block(source, Style::default(), width)
            }
            LiveBlockKind::Thought { elapsed_seconds } => render_thought(*elapsed_seconds, width),
            LiveBlockKind::ReadGroup(details) => render_read_group(details, width),
            LiveBlockKind::Tool {
                name,
                arguments,
                output,
                is_error,
            } => render_tool(name, arguments, output, *is_error, width, expanded),
        }
    }

    fn invalidate(&mut self) {
        self.cache.get_mut().take();
    }

    fn source_len(&self) -> Option<usize> {
        match &self.kind {
            LiveBlockKind::Assistant { source, .. } => Some(source.len()),
            _ => None,
        }
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
    let lines = render_markdown(source, content_width);
    render_markdown_lines(&[], &lines, style, width)
}

fn render_markdown_lines(
    stable: &[RenderedLine],
    tail: &[RenderedLine],
    style: Style,
    width: u16,
) -> Buffer {
    let height = u16::try_from(markdown_line_count(stable, tail))
        .unwrap_or(u16::MAX)
        .max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    render_markdown_rows(&mut buffer, stable, tail, style, width, 0);
    buffer
}

fn update_markdown_buffer(
    previous: Option<Arc<Buffer>>,
    stable: &[RenderedLine],
    tail: &[RenderedLine],
    style: Style,
    width: u16,
    dirty_from: usize,
) -> Buffer {
    let width = width.max(1);
    let height = u16::try_from(markdown_line_count(stable, tail))
        .unwrap_or(u16::MAX)
        .max(1);
    let (mut buffer, dirty_from) = match previous
        .and_then(|buffer| Arc::try_unwrap(buffer).ok())
        .filter(|buffer| buffer.area.width == width)
    {
        Some(buffer) => (buffer, dirty_from),
        None => (Buffer::empty(Rect::new(0, 0, width, height)), 0),
    };
    buffer.area = Rect::new(0, 0, width, height);
    buffer
        .content
        .resize(usize::from(width) * usize::from(height), Cell::EMPTY);

    let dirty_from = dirty_from.min(usize::from(height));
    let dirty_cell = dirty_from.saturating_mul(usize::from(width));
    for cell in &mut buffer.content[dirty_cell..] {
        *cell = Cell::EMPTY;
    }
    render_markdown_rows(&mut buffer, stable, tail, style, width, dirty_from);
    buffer
}

fn render_markdown_rows(
    buffer: &mut Buffer,
    stable: &[RenderedLine],
    tail: &[RenderedLine],
    style: Style,
    width: u16,
    start: usize,
) {
    let height = buffer.area.height;
    let line_count = markdown_line_count(stable, tail).min(usize::from(height));
    for index in start..line_count {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        let Some(rendered) = markdown_line(stable, tail, index) else {
            continue;
        };
        let mut spans = vec![if index == 0 {
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM))
        } else {
            Span::raw("  ")
        }];
        spans.extend(rendered.ratatui_line().spans.into_iter().map(|mut span| {
            span.style = span.style.patch(style);
            span
        }));
        buffer.set_line(0, y, &Line::from(spans), width);
    }
}

fn markdown_line_count(stable: &[RenderedLine], tail: &[RenderedLine]) -> usize {
    stable
        .len()
        .saturating_add(usize::from(!stable.is_empty() && !tail.is_empty()))
        .saturating_add(tail.len())
}

fn markdown_line<'a>(
    stable: &'a [RenderedLine],
    tail: &'a [RenderedLine],
    index: usize,
) -> Option<&'a RenderedLine> {
    if index < stable.len() {
        return stable.get(index);
    }
    let has_gap = !stable.is_empty() && !tail.is_empty();
    if has_gap && index == stable.len() {
        return None;
    }
    let tail_index = index
        .saturating_sub(stable.len())
        .saturating_sub(usize::from(has_gap));
    tail.get(tail_index)
}

/// Stack multiple row buffers vertically into one buffer, copying cells from
/// each row in order. Shared by every tool renderer so the stacking logic
/// lives in one place.
fn stack_rows(rows: &[Buffer], width: u16) -> Buffer {
    let total_height: u16 = rows.iter().map(|row| row.area.height).sum();
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), total_height.max(1)));
    let mut y = 0;
    for row in rows {
        for row_y in 0..row.area.height {
            for x in 0..row.area.width {
                if let Some(cell) = row.cell((x, row_y)) {
                    if let Some(target) = buffer.cell_mut((x, y + row_y)) {
                        *target = cell.clone();
                    }
                }
            }
        }
        y += row.area.height;
    }
    buffer
}

fn render_tool(
    name: &str,
    arguments: &Value,
    output: &str,
    is_error: bool,
    width: u16,
    expanded: bool,
) -> Buffer {
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
    if name == "bash" {
        return render_bash_tool(name, arguments, output, is_error, width, expanded);
    }
    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, detail) = tool_call_summary(name, arguments, is_error, detail_width);
    let title = render_tool_title(action, detail, is_error, width);
    if output.is_empty() {
        return title;
    }
    let mut rows = vec![title];
    // read/edit/write have dedicated previews (ReadGroup / change preview);
    // every other tool shows its output (errors included, like bash).
    if name != "read" && name != "edit" && name != "write" {
        rows.push(render_tool_output(output, width, expanded));
    }
    stack_rows(&rows, width)
}

/// Render a bash tool call: the highlighted command (the "input") inline on
/// the title row, continuation lines indented below, then a short preview of
/// the output. The command is syntax highlighted and the output is truncated
/// at the display layer to a few head/tail lines so the block stays compact.
fn render_bash_tool(
    name: &str,
    arguments: &Value,
    output: &str,
    is_error: bool,
    width: u16,
    expanded: bool,
) -> Buffer {
    let (title, continuation, _command_height) =
        render_bash_command_line(name, arguments, is_error, width, expanded);
    let mut rows: Vec<Buffer> = Vec::new();
    rows.push(title);
    if let Some(continuation) = continuation {
        rows.push(continuation);
    }
    if !output.is_empty() {
        rows.push(render_tool_output(output, width, expanded));
    }
    stack_rows(&rows, width)
}

/// Build the title row with the highlighted command merged inline (like
/// codex: `• Ran <command>` on one line), plus a separate row for the
/// continuation lines of a multi-line command.
fn render_bash_command_line(
    name: &str,
    arguments: &Value,
    is_error: bool,
    width: u16,
    expanded: bool,
) -> (Buffer, Option<Buffer>, u16) {
    use crate::ansi::highlight_bash_command;

    let detail_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let (action, _) = tool_call_summary(name, arguments, is_error, detail_width);
    let bullet_style = if is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    let mut header_spans = vec![Span::styled("•", bullet_style), Span::raw(" ")];
    header_spans.push(Span::styled(
        action,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    header_spans.push(Span::raw(" "));

    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut highlighted = highlight_bash_command(command);
    if highlighted.is_empty() {
        highlighted.push(Line::default());
    }

    // The first highlighted line goes on the title row. If it is too wide it
    // is wrapped (like the input box does) instead of being truncated by
    // `set_line`; extra wrapped rows become continuation lines.
    let first = highlighted.remove(0);
    let header_prefix: String = header_spans
        .iter()
        .map(|span| span.content.to_string())
        .collect();
    let header_prefix_width = UnicodeWidthStr::width(header_prefix.as_str());
    let header_content_width = usize::from(width.max(1))
        .saturating_sub(header_prefix_width)
        .max(1);
    let wrapped_first = crate::ansi::wrap_highlighted_line(&first, header_content_width);
    let mut iter = wrapped_first.into_iter();
    let mut first_spans: Vec<Span<'static>> = Vec::new();
    if let Some(first_row) = iter.next() {
        let mut spans = header_spans.clone();
        spans.extend(first_row.spans);
        first_spans = spans;
    }
    // Any remaining wrapped rows become continuation lines, in order.
    let mut wrapped_tail: Vec<Line<'static>> = iter.collect();
    wrapped_tail.extend(highlighted);
    highlighted = wrapped_tail;

    let header_width = width.max(1);
    let mut title = Buffer::empty(Rect::new(0, 0, header_width, 1));
    title.set_line(0, 0, &Line::from(first_spans), header_width);

    if highlighted.is_empty() {
        (title, None, 1)
    } else {
        const CONTINUATION_PREFIX: &str = "  │ ";
        let prefix_width = UnicodeWidthStr::width(CONTINUATION_PREFIX);
        let content_width = width.saturating_sub(prefix_width as u16).max(1);
        let shown = truncate_command_lines(&mut highlighted, expanded);
        let mut continuation = Buffer::empty(Rect::new(0, 0, width.max(1), shown as u16));
        for (offset, line) in highlighted.drain(..).enumerate() {
            // Dim the pipe prefix so it matches the `└` output corner; the
            // command text itself keeps its syntax colors.
            let mut spans = vec![Span::styled(
                CONTINUATION_PREFIX,
                Style::default().add_modifier(Modifier::DIM),
            )];
            spans.extend(line.spans);
            continuation.set_line(0, offset as u16, &Line::from(spans), content_width);
        }
        (title, Some(continuation), 1)
    }
}

/// Cap the continuation lines of a multi-line command. In compact mode only a
/// few head/tail lines with an ellipsis marker are kept; in expanded mode the
/// full command is shown. The first command line already lives on the title
/// row, so this applies to the remaining lines only.
fn truncate_command_lines(lines: &mut Vec<Line<'static>>, expanded: bool) -> usize {
    use ratatui::style::Modifier as RtModifier;

    const COMMAND_MAX_LINES: usize = 5;
    const COMMAND_EXPANDED_MAX_LINES: usize = 50;
    let limit = if expanded {
        COMMAND_EXPANDED_MAX_LINES
    } else {
        COMMAND_MAX_LINES
    };
    let total = lines.len();
    if total <= limit {
        return total;
    }
    // Reserve one row for the ellipsis marker, then keep an equal head/tail.
    let remaining = limit - 1;
    let half = remaining / 2;
    let omitted = total - remaining;
    let mut selected: Vec<Line<'static>> = lines.drain(..half).collect();
    let mut ellipsis = Line::from(format!("… +{omitted} lines (truncated for display)"));
    for span in &mut ellipsis.spans {
        span.style = span.style.add_modifier(RtModifier::DIM);
    }
    selected.push(ellipsis);
    // Drain the tail after the head was removed; the remaining vector now
    // holds the middle plus tail, so take the last `half` of it.
    let remaining_after_head = lines.len();
    selected.extend(lines.drain(remaining_after_head - half..));
    *lines = selected;
    limit
}

/// Render a short preview of tool output: a few head/tail lines with an
/// ellipsis between them when the output is longer. Expanded mode shows many
/// more lines. ANSI colors from the tool (e.g. colored bash output) survive
/// into the rendered spans.
fn render_tool_output(output: &str, width: u16, expanded: bool) -> Buffer {
    use crate::ansi::split_output;

    const FIRST_PREFIX: &str = "  └ ";
    const SUBSEQUENT_PREFIX: &str = "    ";
    let limit = if expanded {
        crate::ansi::TOOL_OUTPUT_EXPANDED_MAX_LINES
    } else {
        crate::ansi::TOOL_OUTPUT_MAX_LINES
    };
    let half = limit / 2;
    let lines = split_output(output, half, half, FIRST_PREFIX, SUBSEQUENT_PREFIX, true);
    if lines.is_empty() {
        return Buffer::empty(Rect::new(0, 0, width.max(1), 0));
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), lines.len() as u16));
    for (offset, line) in lines.into_iter().enumerate() {
        buffer.set_line(0, offset as u16, &line, width);
    }
    buffer
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
    let line = Line::from(spans);
    // Wrap long titles (e.g. a long grep pattern or path) instead of letting
    // `set_line` truncate them, consistent with the bash command line.
    let rows = crate::ansi::wrap_highlighted_line(&line, usize::from(width.max(1)));
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), rows.len() as u16));
    for (offset, row) in rows.into_iter().enumerate() {
        buffer.set_line(0, offset as u16, &row, width);
    }
    buffer
}

/// Append the bash tool output below the tool title: the first 25 lines and
/// the last 25 lines (with an ellipsis marker between them when truncated),
/// parsed so ANSI colors from the command output survive into the TUI.
/// Truncation happens at the display layer only; the agent still receives the
/// full output for reasoning.
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

        let narrow = block.render(10, false);
        let wide = block.render(40, false);

        assert!(narrow.area.height > wide.area.height);
    }

    #[test]
    fn completed_blocks_reuse_rendering_until_the_content_changes() {
        let mut block = LiveBlock::assistant(1, "hello".to_string());

        let first = block.render(40, false);
        let second = block.render(40, false);
        assert!(std::sync::Arc::ptr_eq(&first, &second));

        assert!(block.append_markdown_source(" world"));
        let updated = block.render(40, false);
        assert!(!std::sync::Arc::ptr_eq(&first, &updated));
    }

    #[test]
    fn streaming_markdown_matches_full_render_after_each_append() {
        let chunks = [
            "## Head",
            "ing\n\n",
            "A paragraph with **bo",
            "ld** and `code`.\n\n",
            "- first item\n",
            "- second item\n\n",
            "```rust\nfn main() {}\n",
            "```\n\n",
            "| Name | State |\n|---|---|\n",
            "| ash | ready |",
        ];
        let mut source = String::new();
        let mut block = LiveBlock::assistant(1, String::new());

        for chunk in chunks {
            source.push_str(chunk);
            assert!(block.append_markdown_source(chunk));
            assert_eq!(
                block.render(80, false).as_ref(),
                &render_markdown_block(&source, Style::default(), 80)
            );
        }

        let cache = block.cache.borrow();
        assert!(cache
            .as_ref()
            .and_then(|cache| cache.streaming_markdown.as_ref())
            .is_some_and(|cache| !cache.stable_lines().is_empty()));
    }

    #[test]
    fn streaming_markdown_falls_back_when_a_previous_buffer_is_still_borrowed() {
        let mut block = LiveBlock::assistant(1, String::new());
        assert!(block.append_markdown_source("First paragraph.\n\nSecond paragraph."));
        let retained = block.render(80, false);
        assert!(block.append_markdown_source("\n\nThird paragraph."));

        let updated = block.render(80, false);

        assert!(!Arc::ptr_eq(&retained, &updated));
        assert_eq!(
            updated.as_ref(),
            &render_markdown_block(
                "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.",
                Style::default(),
                80,
            )
        );
    }

    #[test]
    fn finalizing_stream_reparses_cross_block_reference_links() {
        let mut block = LiveBlock::assistant(1, String::new());
        assert!(block.append_markdown_source("Read [the docs][docs].\n\nNext paragraph."));
        let _ = block.render(80, false);
        assert!(block.append_markdown_source("\n\n[docs]: https://example.com/docs"));
        let _ = block.render(80, false);

        block.finalize_markdown();

        assert_eq!(
            block.render(80, false).as_ref(),
            &render_markdown_block(
                "Read [the docs][docs].\n\nNext paragraph.\n\n[docs]: https://example.com/docs",
                Style::default(),
                80,
            )
        );
    }

    #[test]
    fn committed_blocks_can_drop_their_render_cache_before_replay() {
        let block = LiveBlock::assistant(1, "hello".to_string());

        let committed = block.render(40, false);
        block.clear_render_cache();
        let replayed = block.render(40, false);

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
        let rendered = block.render(40, false);

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

        let buffer = block.render(80, false);
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
    fn bash_output_renders_head_tail_with_ansi_colors() {
        let output = (1..=120)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "seq 120"}),
            output,
            false,
        );
        let rendered = block.render(40, false);

        // Title row with the (single-line) command inline, then 2 head +
        // 1 ellipsis + 2 tail output lines.
        assert_eq!(rendered.area.height, 1 + 2 + 1 + 2);
        assert!(row_text(&rendered, 0).contains("seq 120"), "command inline");
        assert!(row_text(&rendered, 1).contains("line 1"), "head row");
        assert!(row_text(&rendered, 3).contains("+116 lines"));
        assert!(row_text(&rendered, 5).contains("line 120"), "tail row");
    }

    #[test]
    fn expanded_bash_output_shows_more_lines() {
        let output = (1..=120)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "seq 120"}),
            output,
            false,
        );
        // Collapsed: 5 output lines (2 head + 1 ellipsis + 2 tail).
        assert_eq!(block.render(40, false).area.height, 6);
        // Expanded: 50 output lines (25 head + 1 ellipsis + 24 tail).
        let expanded = block.render(40, true);
        assert_eq!(expanded.area.height, 1 + 51);
        assert!(row_text(&expanded, 1).contains("line 1"), "head");
        assert!(row_text(&expanded, 26).contains("+70 lines"), "ellipsis");
        assert!(row_text(&expanded, 51).contains("line 120"), "tail");
        // The two render modes are independent of any per-block state.
        assert_eq!(block.render(40, false).area.height, 6);
    }

    #[test]
    fn bash_output_preserves_ansi_colors() {
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "ls --color"}),
            "\x1b[31mred.txt\x1b[0m\nplain".to_string(),
            false,
        );
        let rendered = block.render(40, false);
        // Title row (command inline) + two output lines.
        assert_eq!(rendered.area.height, 3);
        assert!(row_text(&rendered, 0).contains("ls --color"));
        // The colored span keeps its foreground.
        let cells = (0..rendered.area.width)
            .filter_map(|column| rendered.cell((column, 1)))
            .collect::<Vec<_>>();
        assert!(cells.iter().any(|cell| cell.symbol() == "r"));
        assert!(row_text(&rendered, 2).contains("plain"));
    }

    #[test]
    fn multi_line_command_truncates_like_output() {
        let command = (1..=20)
            .map(|i| format!("echo step {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": command}),
            "done".to_string(),
            false,
        );
        let rendered = block.render(40, false);
        // Title (first line inline) + 2 head + 1 ellipsis + 2 tail continuations
        // + 1 output line.
        assert_eq!(rendered.area.height, 1 + 5 + 1);
        assert!(row_text(&rendered, 0).contains("echo step 1"));
        assert!(row_text(&rendered, 1).contains("echo step 2"));
        assert!(row_text(&rendered, 3).contains("+15 lines"));
        assert!(row_text(&rendered, 5).contains("echo step 20"));
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
        .render(60, false);
        let write = LiveBlock::tool(
            2,
            "write".to_string(),
            serde_json::json!({"path": "/workspace/src/new.rs", "content": "one\ntwo"}),
            String::new(),
            false,
        )
        .render(60, false);

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
        .render(1, false);
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

#[cfg(test)]
mod generic_output_tests {
    use super::*;

    fn row_text(buffer: &Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|column| buffer.cell((column, row)))
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn long_tool_title_wraps_instead_of_truncating() {
        let pattern = "very long pattern ".repeat(8);
        let block = LiveBlock::tool(
            1,
            "grep".to_string(),
            serde_json::json!({"pattern": pattern, "path": "src"}),
            "match".to_string(),
            false,
        );
        // Non-bash titles are pre-truncated by fit_action_and_detail, so they
        // stay on one row (title + output).
        let rendered = block.render(30, false);
        assert_eq!(rendered.area.height, 2);
        // The bash command line, by contrast, wraps instead of truncating.
        let bash_block = LiveBlock::tool(
            2,
            "bash".to_string(),
            serde_json::json!({"command": format!("cargo {}", "x".repeat(60))}),
            "ok".to_string(),
            false,
        );
        assert!(bash_block.render(30, false).area.height > 2);
    }

    #[test]
    fn error_tool_output_is_shown_for_all_tools() {
        // A failed grep keeps its error message visible below the title.
        let block = LiveBlock::tool(
            1,
            "grep".to_string(),
            serde_json::json!({"pattern": "[", "path": "src"}),
            "invalid regular expression".to_string(),
            true,
        );
        let rendered = block.render(40, false);
        assert_eq!(rendered.area.height, 2);
        assert!(row_text(&rendered, 0).contains("Failed"), "title");
        assert!(row_text(&rendered, 1).contains("invalid regular"));
    }

    #[test]
    fn bash_error_title_says_failed_and_shows_output() {
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "false"}),
            "exit code 1".to_string(),
            true,
        );
        let rendered = block.render(40, false);
        assert!(
            row_text(&rendered, 0).contains("Failed"),
            "title: {:?}",
            row_text(&rendered, 0)
        );
        assert!(row_text(&rendered, 1).contains("exit code 1"));
    }

    #[test]
    fn grep_tool_shows_output_preview() {
        let output = "src/main.rs:\n  Line 12: let x = 1;\n  Line 34: let y = 2;";
        let block = LiveBlock::tool(
            1,
            "grep".to_string(),
            serde_json::json!({"pattern": "let x", "path": "src"}),
            output.to_string(),
            false,
        );
        let rendered = block.render(50, false);
        assert_eq!(rendered.area.height, 1 + 3);
        assert!(row_text(&rendered, 0).contains("Searched"));
        assert!(row_text(&rendered, 1).contains("src/main.rs"));
        assert!(row_text(&rendered, 2).contains("Line 12"));
    }
}

#[cfg(test)]
mod toggle_tests {
    use super::*;

    #[test]
    fn toggle_expanded_changes_rendered_height() {
        let output = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "seq 30"}),
            output,
            false,
        );
        // collapsed: 1 title + 5 output (2+1+2)
        assert_eq!(block.render(40, false).area.height, 6);
        // expanded: 1 title + 26 output (13+1+12)... wait 30 lines with 50 limit → all 30 shown
        assert!(
            block.render(40, true).area.height > 6,
            "expanded should be taller, got {}",
            block.render(40, true).area.height
        );
    }
}
