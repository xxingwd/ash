use std::{cell::RefCell, path::PathBuf, sync::Arc, time::Instant};

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use ash_core::{ToolCallId, TurnId};

use crate::{
    history_block::HistoryBlock,
    markdown::{render_markdown, RenderedLine, StreamingMarkdownCache},
    scrollback::{sanitize_terminal_text, wrap_text},
    tool_display::{tool_call_summary, tool_renderer, OutputPresentation, ToolRenderer},
    welcome_card::{welcome_card, WelcomeLine, WelcomeStyle},
};

const BULLET_PREFIX_COLUMNS: u16 = 2;

/// A complete transcript entry retained by Ash and re-rendered after updates.
#[derive(Clone, Debug)]
pub struct LiveBlock {
    id: u64,
    turn_id: Option<TurnId>,
    kind: LiveBlockKind,
    cache: RefCell<Option<RenderCache>>,
}

#[derive(Clone, Debug)]
struct RenderCache {
    width: u16,
    source_len: Option<usize>,
    elapsed_seconds: Option<u64>,
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
    Reasoning {
        source: String,
        state: ReasoningState,
    },
    Tool {
        name: String,
        arguments: Value,
        state: ToolState,
    },
}

#[derive(Clone, Debug)]
enum ReasoningState {
    Running { started_at: Instant },
    Finished { elapsed_seconds: u64 },
}

#[derive(Clone, Debug)]
enum ToolState {
    Running { id: ToolCallId },
    Finished { output: String, is_error: bool },
}

impl LiveBlock {
    pub(crate) const fn welcome(id: u64, working_dir: PathBuf) -> Self {
        Self::new(id, LiveBlockKind::Welcome(working_dir))
    }

    pub(crate) const fn history(id: u64, block: HistoryBlock) -> Self {
        Self::new(id, LiveBlockKind::History(block))
    }

    pub(crate) const fn assistant(id: u64, source: String) -> Self {
        Self::new(
            id,
            LiveBlockKind::Assistant {
                source,
                streaming: false,
            },
        )
    }

    pub(crate) const fn thought(id: u64, source: String, elapsed_seconds: u64) -> Self {
        Self::new(
            id,
            LiveBlockKind::Reasoning {
                source,
                state: ReasoningState::Finished { elapsed_seconds },
            },
        )
    }

    pub(crate) fn reasoning(id: u64) -> Self {
        Self::new(
            id,
            LiveBlockKind::Reasoning {
                source: String::new(),
                state: ReasoningState::Running {
                    started_at: Instant::now(),
                },
            },
        )
    }

    pub(crate) fn tool(
        id: u64,
        name: String,
        arguments: Value,
        output: String,
        is_error: bool,
    ) -> Self {
        Self::new(
            id,
            LiveBlockKind::Tool {
                name,
                arguments,
                state: ToolState::Finished { output, is_error },
            },
        )
    }

    pub(crate) fn running_tool(
        block_id: u64,
        id: ToolCallId,
        name: String,
        arguments: Value,
    ) -> Self {
        Self::new(
            block_id,
            LiveBlockKind::Tool {
                name,
                arguments,
                state: ToolState::Running { id },
            },
        )
    }

    const fn new(id: u64, kind: LiveBlockKind) -> Self {
        Self {
            id,
            turn_id: None,
            kind,
            cache: RefCell::new(None),
        }
    }

    pub(crate) const fn with_turn(mut self, turn_id: Option<TurnId>) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn belongs_to_turn(&self, turn_id: TurnId) -> bool {
        self.turn_id == Some(turn_id)
    }

    /// Blocks produced by live streaming for this turn (assistant text,
    /// reasoning, tool output). They are replaced by the canonical projection
    /// when the turn settles; user input and error blocks are kept.
    pub(crate) fn is_streamed_for_turn(&self, turn_id: TurnId) -> bool {
        self.belongs_to_turn(turn_id)
            && matches!(
                self.kind,
                LiveBlockKind::Assistant { .. }
                    | LiveBlockKind::Reasoning { .. }
                    | LiveBlockKind::Tool { .. }
            )
    }

    pub(crate) fn is_unfinished_response_for_turn(&self, turn_id: TurnId) -> bool {
        self.belongs_to_turn(turn_id)
            && matches!(
                &self.kind,
                LiveBlockKind::Assistant {
                    streaming: true,
                    ..
                } | LiveBlockKind::Reasoning {
                    state: ReasoningState::Running { .. },
                    ..
                } | LiveBlockKind::Tool {
                    state: ToolState::Running { .. },
                    ..
                }
            )
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

    pub(crate) fn append_reasoning_source(&mut self, delta: &str) -> bool {
        let LiveBlockKind::Reasoning {
            source,
            state: ReasoningState::Running { .. },
        } = &mut self.kind
        else {
            return false;
        };
        source.push_str(&sanitize_terminal_text(delta));
        true
    }

    pub(crate) fn finish_reasoning(&mut self) -> bool {
        let LiveBlockKind::Reasoning { source, state } = &mut self.kind else {
            return false;
        };
        let ReasoningState::Running { started_at } = state else {
            return false;
        };
        *state = ReasoningState::Finished {
            elapsed_seconds: started_at.elapsed().as_secs(),
        };
        let has_content = !source.trim().is_empty();
        self.invalidate();
        has_content
    }

    pub(crate) fn finish_tool(&mut self, id: &ToolCallId, output: String, is_error: bool) -> bool {
        let LiveBlockKind::Tool { state, .. } = &mut self.kind else {
            return false;
        };
        let ToolState::Running { id: running_id } = state else {
            return false;
        };
        if running_id != id {
            return false;
        }
        *state = ToolState::Finished { output, is_error };
        self.invalidate();
        true
    }

    pub(crate) fn is_running_tool(&self, id: &ToolCallId) -> bool {
        matches!(
            &self.kind,
            LiveBlockKind::Tool {
                state: ToolState::Running { id: running_id },
                ..
            } if running_id == id
        )
    }

    pub(crate) fn grouped_tool_name(&self, expanded: bool) -> Option<&str> {
        let LiveBlockKind::Tool { name, state, .. } = &self.kind else {
            return None;
        };
        if matches!(state, ToolState::Finished { is_error: true, .. }) {
            return None;
        }
        crate::tool_display::is_groupable_tool(name, expanded).then_some(name.as_str())
    }

    pub(crate) fn grouped_tool_detail(&self, expanded: bool) -> Option<(String, String)> {
        let LiveBlockKind::Tool {
            name, arguments, ..
        } = &self.kind
        else {
            return None;
        };
        self.grouped_tool_name(expanded)
            .map(|_| tool_call_summary(name, arguments))
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(
            self.kind,
            LiveBlockKind::Tool {
                state: ToolState::Running { .. },
                ..
            }
        )
    }

    #[cfg(test)]
    pub(crate) fn render(&self, width: u16, expanded: bool) -> Arc<Buffer> {
        self.render_in(width, expanded, None)
    }

    pub(crate) fn render_in(
        &self,
        width: u16,
        expanded: bool,
        working_dir: Option<&std::path::Path>,
    ) -> Arc<Buffer> {
        let width = width.max(1);
        let source_len = self.source_len();
        let elapsed_seconds = self.running_elapsed_seconds();
        if let Some(buffer) = self
            .cache
            .borrow()
            .as_ref()
            .filter(|cached| {
                cached.width == width
                    && cached.source_len == source_len
                    && cached.elapsed_seconds == elapsed_seconds
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
            LiveBlockKind::Reasoning {
                source,
                state: ReasoningState::Running { .. },
            } => {
                let previous = previous.filter(|cached| cached.width == width);
                let mut markdown = previous
                    .and_then(|cached| cached.streaming_markdown)
                    .unwrap_or_default();
                let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
                let tail = markdown.update(source, content_width);
                let buffer = render_running_reasoning(
                    elapsed_seconds.unwrap_or_default(),
                    width,
                    expanded,
                    &markdown,
                    &tail,
                );
                (buffer, Some(markdown))
            }
            _ => (self.render_uncached(width, expanded, working_dir), None),
        };
        let buffer = Arc::new(buffer);
        self.cache.replace(Some(RenderCache {
            width,
            source_len,
            elapsed_seconds,
            expanded,
            buffer: Arc::clone(&buffer),
            streaming_markdown,
        }));
        buffer
    }

    pub(crate) fn clear_render_cache(&self) {
        self.cache.replace(None);
    }

    fn render_uncached(
        &self,
        width: u16,
        expanded: bool,
        working_dir: Option<&std::path::Path>,
    ) -> Buffer {
        match &self.kind {
            LiveBlockKind::Welcome(working_dir) => render_welcome(width, working_dir),
            LiveBlockKind::History(block) => block.render(width),
            LiveBlockKind::Assistant { source, .. } => {
                render_markdown_block(source, Style::default(), width)
            }
            LiveBlockKind::Reasoning {
                source,
                state: ReasoningState::Finished { elapsed_seconds },
            } => render_thought(source, *elapsed_seconds, width, expanded),
            LiveBlockKind::Reasoning {
                source,
                state: ReasoningState::Running { started_at },
            } => {
                let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
                let mut markdown = StreamingMarkdownCache::default();
                let tail = markdown.update(source, content_width);
                render_running_reasoning(
                    started_at.elapsed().as_secs(),
                    width,
                    expanded,
                    &markdown,
                    &tail,
                )
            }
            LiveBlockKind::Tool {
                name,
                arguments,
                state: ToolState::Running { .. },
            } => render_running_tool(name, arguments, width, expanded, working_dir),
            LiveBlockKind::Tool {
                name,
                arguments,
                state: ToolState::Finished { output, is_error },
            } => render_tool(
                name,
                arguments,
                output,
                *is_error,
                width,
                expanded,
                working_dir,
            ),
        }
    }

    fn invalidate(&mut self) {
        self.cache.get_mut().take();
    }

    const fn source_len(&self) -> Option<usize> {
        match &self.kind {
            LiveBlockKind::Assistant { source, .. } | LiveBlockKind::Reasoning { source, .. } => {
                Some(source.len())
            }
            _ => None,
        }
    }

    fn running_elapsed_seconds(&self) -> Option<u64> {
        match &self.kind {
            LiveBlockKind::Reasoning {
                state: ReasoningState::Running { started_at },
                ..
            } => Some(started_at.elapsed().as_secs()),
            _ => None,
        }
    }
}

fn render_running_reasoning(
    elapsed_seconds: u64,
    width: u16,
    expanded: bool,
    markdown: &StreamingMarkdownCache,
    tail: &[RenderedLine],
) -> Buffer {
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    let mut header = render_markdown(
        &format!(
            "Thinking ({})",
            crate::status_line::format_elapsed(elapsed_seconds)
        ),
        width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1),
    );
    let limit = if expanded {
        usize::MAX
    } else {
        crate::ansi::COLLAPSED_MAX_LINES
    };
    let mut body = markdown.latest_lines(tail, limit);
    while body.last().is_some_and(RenderedLine::is_blank) {
        body.pop();
    }
    render_markdown_lines(
        &[],
        &[header.as_mut_slice(), body.as_mut_slice()].concat(),
        style,
        width,
    )
}

fn render_thought(source: &str, elapsed_seconds: u64, width: u16, expanded: bool) -> Buffer {
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    // Compact mode keeps the summary line; expanded mode reveals the full
    // reasoning text below it, mirroring the tool output toggle (`Ctrl+o`).
    if !expanded {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), 1));
        buffer.set_line(
            0,
            0,
            &Line::styled(
                format!(
                    "• Thought for {}",
                    crate::status_line::format_elapsed(elapsed_seconds)
                ),
                style,
            ),
            width,
        );
        return buffer;
    }
    let content_width = width.saturating_sub(BULLET_PREFIX_COLUMNS).max(1);
    let mut lines = render_markdown(source, content_width);
    while lines.last().is_some_and(RenderedLine::is_blank) {
        lines.pop();
    }
    for line in &mut lines {
        line.patch_style(style);
    }
    let height = u16::try_from(lines.len() + 1).unwrap_or(u16::MAX).max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    buffer.set_line(
        0,
        0,
        &Line::styled(
            format!(
                "• Thought for {} — expanded",
                crate::status_line::format_elapsed(elapsed_seconds)
            ),
            style,
        ),
        width,
    );
    for (offset, line) in lines.into_iter().enumerate() {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.ratatui_line().spans);
        buffer.set_line(
            0,
            u16::try_from(offset).unwrap_or(u16::MAX).saturating_add(1),
            &Line::from(spans),
            width,
        );
    }
    buffer
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
    let (mut buffer, dirty_from) = previous
        .and_then(|buffer| Arc::try_unwrap(buffer).ok())
        .filter(|buffer| buffer.area.width == width)
        .map_or_else(
            || (Buffer::empty(Rect::new(0, 0, width, height)), 0),
            |buffer| (buffer, dirty_from),
        );
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
        let mut spans = crate::scrollback::content_row_prefix(index == 0);
        spans.extend(rendered.ratatui_line().spans.into_iter().map(|mut span| {
            span.style = span.style.patch(style);
            span
        }));
        buffer.set_line(0, y, &Line::from(spans), width);
    }
}

fn markdown_line_count(stable: &[RenderedLine], tail: &[RenderedLine]) -> usize {
    crate::markdown::combined_line_count(stable, tail)
}

fn markdown_line<'a>(
    stable: &'a [RenderedLine],
    tail: &'a [RenderedLine],
    index: usize,
) -> Option<&'a RenderedLine> {
    crate::markdown::combined_line(stable, tail, index)
}

/// Stack multiple row buffers vertically into one buffer, copying cells from
/// each row in order. Shared by every tool renderer so the stacking logic
/// lives in one place.
fn stack_rows(rows: &[Buffer], width: u16) -> Buffer {
    let total_height = rows
        .iter()
        .fold(0_u16, |height, row| height.saturating_add(row.area.height));
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), total_height.max(1)));
    let mut y = 0;
    for row in rows {
        if y >= total_height {
            break;
        }
        for row_y in 0..row.area.height {
            let Some(target_y) = y
                .checked_add(row_y)
                .filter(|target_y| *target_y < total_height)
            else {
                break;
            };
            for x in 0..row.area.width {
                if let Some(cell) = row.cell((x, row_y)) {
                    if let Some(target) = buffer.cell_mut((x, target_y)) {
                        *target = cell.clone();
                    }
                }
            }
        }
        y = y.saturating_add(row.area.height).min(total_height);
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
    working_dir: Option<&std::path::Path>,
) -> Buffer {
    let output_presentation = match tool_renderer(name, is_error) {
        ToolRenderer::Bash => {
            return render_bash_tool(name, arguments, output, is_error, width, expanded);
        }
        ToolRenderer::Edit => {
            if let Some(rendered) = render_edit_tool(arguments, width, working_dir) {
                return rendered;
            }
            OutputPresentation::Preview
        }
        ToolRenderer::Write => {
            if let Some(rendered) = render_write_tool(arguments, width, working_dir) {
                return rendered;
            }
            OutputPresentation::Preview
        }
        ToolRenderer::Generic(presentation) => presentation,
    };
    let (action, detail) = tool_call_summary(name, arguments);
    let title = render_tool_title(&action, &detail, is_error, width);
    if output.is_empty() {
        return title;
    }
    let mut rows = vec![title];
    if let Some(output) = render_presented_tool_output(output, output_presentation, width, expanded)
    {
        rows.push(output);
    }
    stack_rows(&rows, width)
}

fn render_presented_tool_output(
    output: &str,
    presentation: OutputPresentation,
    width: u16,
    expanded: bool,
) -> Option<Buffer> {
    match (presentation, expanded) {
        (OutputPresentation::Omitted, _) | (OutputPresentation::Expandable, false) => None,
        (OutputPresentation::Summary, false) => render_tool_output_summary(output, width),
        (
            OutputPresentation::Expandable
            | OutputPresentation::Summary
            | OutputPresentation::Preview,
            true,
        )
        | (OutputPresentation::Preview, false) => Some(render_tool_output(output, width, expanded)),
    }
}

fn render_tool_output_summary(output: &str, width: u16) -> Option<Buffer> {
    output
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|summary| render_tool_output(summary, width, false))
}

fn render_running_tool(
    name: &str,
    arguments: &Value,
    width: u16,
    expanded: bool,
    working_dir: Option<&std::path::Path>,
) -> Buffer {
    let (action, mut detail) = tool_call_summary(name, arguments);
    if matches!(
        tool_renderer(name, false),
        ToolRenderer::Edit | ToolRenderer::Write
    ) {
        if let Some(working_dir) = working_dir {
            detail = crate::tool_display::workspace_path(&detail, working_dir);
        }
    }
    if matches!(tool_renderer(name, false), ToolRenderer::Bash) {
        let (title, continuation, _) =
            render_bash_command_line_with_action(arguments, &action, Color::Cyan, width, expanded);
        return continuation.map_or(title.clone(), |continuation| {
            stack_rows(&[title, continuation], width)
        });
    }
    render_tool_title_with_color(&action, &detail, Color::Cyan, width)
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
/// codex: `• bash <command>` on one line), plus a separate row for the
/// continuation lines of a multi-line command.
fn render_bash_command_line(
    name: &str,
    arguments: &Value,
    is_error: bool,
    width: u16,
    expanded: bool,
) -> (Buffer, Option<Buffer>, u16) {
    let (action, _) = tool_call_summary(name, arguments);
    let color = if is_error { Color::Red } else { Color::Green };
    render_bash_command_line_with_action(arguments, &action, color, width, expanded)
}

fn render_bash_command_line_with_action(
    arguments: &Value,
    action: &str,
    color: Color,
    width: u16,
    expanded: bool,
) -> (Buffer, Option<Buffer>, u16) {
    use crate::ansi::highlight_bash_command;

    let bullet_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut header_spans = vec![Span::styled("•", bullet_style), Span::raw(" ")];
    header_spans.push(Span::styled(
        display_label(action),
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
    let first_spans: Vec<Span<'static>> = if let Some(first_row) = iter.next() {
        let mut spans = header_spans.clone();
        spans.extend(first_row.spans);
        spans
    } else {
        Vec::new()
    };
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
        let content_width = width
            .saturating_sub(u16::try_from(prefix_width).unwrap_or(u16::MAX))
            .max(1);
        let mut wrapped = Vec::new();
        for line in highlighted {
            wrapped.extend(crate::ansi::wrap_highlighted_line(
                &line,
                usize::from(content_width),
            ));
        }
        let shown = truncate_command_lines(&mut wrapped, expanded);
        let mut continuation = Buffer::empty(Rect::new(
            0,
            0,
            width.max(1),
            u16::try_from(shown).unwrap_or(u16::MAX),
        ));
        for (offset, line) in wrapped.into_iter().enumerate() {
            // Dim the pipe prefix so it matches the `└` output corner; the
            // command text itself keeps its syntax colors.
            let mut spans = vec![Span::styled(
                CONTINUATION_PREFIX,
                Style::default().add_modifier(Modifier::DIM),
            )];
            spans.extend(line.spans);
            continuation.set_line(
                0,
                u16::try_from(offset).unwrap_or(u16::MAX),
                &Line::from(spans),
                width.max(1),
            );
        }
        (title, Some(continuation), 1)
    }
}

/// Cap the continuation lines of a multi-line command. In compact mode only a
/// few head/tail lines with an ellipsis marker are kept; in expanded mode the
/// full command is shown. The first command line already lives on the title
/// row, so this applies to the remaining lines only.
fn truncate_command_lines(lines: &mut Vec<Line<'static>>, expanded: bool) -> usize {
    let total = lines.len();
    if expanded {
        return total;
    }
    let limit = crate::ansi::COLLAPSED_MAX_LINES;
    if total <= limit {
        return total;
    }
    // Reserve one row for the ellipsis marker, then keep an equal head/tail.
    let remaining = limit - 1;
    let half = remaining / 2;
    let omitted = total - remaining;
    let mut ellipsis = Line::from(format!("… +{omitted} lines (truncated for display)"));
    for span in &mut ellipsis.spans {
        span.style = span.style.add_modifier(Modifier::DIM);
    }
    *lines = crate::ansi::split_with_ellipsis(std::mem::take(lines), half, half, ellipsis);
    limit
}

/// Render tool output as a bounded head/tail preview or in full when expanded.
/// ANSI colors from the tool (e.g. colored bash output) survive into the
/// rendered spans.
fn render_tool_output(output: &str, width: u16, expanded: bool) -> Buffer {
    use crate::ansi::split_output;

    const FIRST_PREFIX: &str = "  └ ";
    const SUBSEQUENT_PREFIX: &str = "    ";
    let lines = if expanded {
        split_output(output, usize::MAX, 0, FIRST_PREFIX, SUBSEQUENT_PREFIX, true)
    } else {
        let half = crate::ansi::COLLAPSED_MAX_LINES / 2;
        split_output(output, half, half, FIRST_PREFIX, SUBSEQUENT_PREFIX, true)
    };
    if lines.is_empty() {
        return Buffer::empty(Rect::new(0, 0, width.max(1), 0));
    }
    let wrap_width = usize::from(width.max(1));
    let rows: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|line| crate::ansi::wrap_highlighted_line(&line, wrap_width))
        .collect();
    let mut buffer = Buffer::empty(Rect::new(
        0,
        0,
        width.max(1),
        u16::try_from(rows.len()).unwrap_or(u16::MAX),
    ));
    for (offset, line) in rows.into_iter().enumerate() {
        buffer.set_line(0, u16::try_from(offset).unwrap_or(u16::MAX), &line, width);
    }
    buffer
}

fn render_tool_title(action: &str, detail: &str, is_error: bool, width: u16) -> Buffer {
    let color = if is_error { Color::Red } else { Color::Green };
    render_tool_title_with_color(action, detail, color, width)
}

pub(crate) fn render_grouped_tool(
    name: &str,
    details: &[String],
    running: bool,
    width: u16,
) -> Buffer {
    let (action, detail) = crate::tool_display::grouped_tool_summary(name, details);
    let color = if running { Color::Cyan } else { Color::Green };
    render_tool_title_with_color(&action, &detail, color, width)
}

/// How a tool name is shown in a title: underscores become spaces and the
/// first character is uppercase. Tool names themselves stay lowercase
/// everywhere; this is presentation only.
fn display_label(label: &str) -> String {
    let label = label.replace('_', " ");
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |first| {
        format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
    })
}

fn render_tool_title_with_color(action: &str, detail: &str, color: Color, width: u16) -> Buffer {
    let bullet_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled("•", bullet_style), Span::raw(" ")];
    spans.push(Span::styled(
        display_label(action),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(detail.to_string()));
    }
    let line = Line::from(spans);
    // Wrap long titles (e.g. a long grep pattern or path) instead of letting
    // `set_line` truncate them, consistent with the bash command line.
    let rows = crate::ansi::wrap_highlighted_line(&line, usize::from(width.max(1)));
    let mut buffer = Buffer::empty(Rect::new(
        0,
        0,
        width.max(1),
        u16::try_from(rows.len()).unwrap_or(u16::MAX),
    ));
    for (offset, row) in rows.into_iter().enumerate() {
        buffer.set_line(0, u16::try_from(offset).unwrap_or(u16::MAX), &row, width);
    }
    buffer
}

fn render_edit_tool(
    arguments: &Value,
    width: u16,
    working_dir: Option<&std::path::Path>,
) -> Option<Buffer> {
    let path = tool_argument(arguments, "path")?;
    let path = working_dir.map_or(path.clone(), |working_dir| {
        crate::tool_display::workspace_path(&path, working_dir)
    });
    let edits = arguments.get("edits")?.as_array()?;
    let mut lines = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let old_text = tool_argument(edit, "oldText")?;
        let new_text = tool_argument(edit, "newText")?;
        if index > 0 {
            lines.push("@@".to_string());
        }
        extend_change_lines(&mut lines, '-', &old_text);
        extend_change_lines(&mut lines, '+', &new_text);
    }
    let (added, removed) = changed_line_counts(&lines);
    Some(render_file_change(
        "edit", &path, added, removed, &lines, width,
    ))
}

fn render_write_tool(
    arguments: &Value,
    width: u16,
    working_dir: Option<&std::path::Path>,
) -> Option<Buffer> {
    let path = tool_argument(arguments, "path")?;
    let path = working_dir.map_or(path.clone(), |working_dir| {
        crate::tool_display::workspace_path(&path, working_dir)
    });
    let content = tool_argument(arguments, "content")?;
    let mut lines = Vec::new();
    extend_change_lines(&mut lines, '+', &content);
    let added = lines.len();
    Some(render_file_change("write", &path, added, 0, &lines, width))
}

fn tool_argument(arguments: &Value, name: &str) -> Option<String> {
    arguments.get(name)?.as_str().map(sanitize_terminal_text)
}

fn extend_change_lines(lines: &mut Vec<String>, prefix: char, text: &str) {
    lines.extend(text.lines().map(|line| format!("{prefix}{line}")));
}

fn render_file_change(
    tool_name: &str,
    path: &str,
    added: usize,
    removed: usize,
    lines: &[String],
    width: u16,
) -> Buffer {
    let title = Line::from(vec![
        Span::styled(
            "•",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            display_label(tool_name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(path.to_string()),
        Span::raw(" ("),
        Span::styled(format!("+{added}"), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)),
        Span::raw(")"),
    ]);
    render_change_block(&title, lines, width)
}

fn changed_line_counts(lines: &[String]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(added, removed), line| {
        if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

fn render_change_block(title: &Line<'static>, lines: &[String], width: u16) -> Buffer {
    let title_rows = crate::ansi::wrap_highlighted_line(title, usize::from(width.max(1)));
    let content_x = if width > BULLET_PREFIX_COLUMNS {
        BULLET_PREFIX_COLUMNS
    } else {
        0
    };
    let content_width = width.saturating_sub(content_x).max(1);
    let mut rendered = Vec::new();
    for line in lines {
        let style = change_line_style(line);
        for row in wrap_text(line, content_width) {
            rendered.push((row, style));
        }
    }

    let height = u16::try_from(rendered.len().saturating_add(title_rows.len()))
        .unwrap_or(u16::MAX)
        .max(1);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), height));
    let title_height = title_rows.len();
    for (index, row) in title_rows.into_iter().enumerate() {
        let Ok(y) = u16::try_from(index) else {
            break;
        };
        buffer.set_line(0, y, &row, width);
    }
    for (index, (line, style)) in rendered.iter().enumerate() {
        let Ok(y) = u16::try_from(index.saturating_add(title_height)) else {
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

    fn test_turn(n: u128) -> TurnId {
        TurnId::from_u128(n)
    }

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
        let response =
            LiveBlock::history(1, HistoryBlock::info("done")).with_turn(Some(test_turn(7)));

        assert!(response.belongs_to_turn(test_turn(7)));
        assert!(!response.belongs_to_turn(test_turn(8)));
    }

    #[test]
    fn completed_thoughts_render_as_a_single_summary_line() {
        let block = LiveBlock::thought(1, "detail".to_string(), 3);
        let rendered = block.render(40, false);

        assert_eq!(rendered.area.height, 1);
        assert_eq!(row_text(&rendered, 0), "• Thought for 3s");
    }

    #[test]
    fn running_reasoning_is_a_live_transcript_block() {
        let mut block = LiveBlock::reasoning(1).with_turn(Some(test_turn(7)));
        assert!(block.append_reasoning_source("inspect first"));

        let rendered = block.render(40, false);
        assert_eq!(row_text(&rendered, 0), "• Thinking (0s)");
        assert_eq!(row_text(&rendered, 1), "  inspect first");
        assert!(block.is_unfinished_response_for_turn(test_turn(7)));

        assert!(block.finish_reasoning());
        assert_eq!(row_text(&block.render(40, false), 0), "• Thought for 0s");
        assert!(!block.is_unfinished_response_for_turn(test_turn(7)));
    }

    #[test]
    fn running_reasoning_has_a_safe_uncached_render() {
        let mut block = LiveBlock::reasoning(1);
        assert!(block.append_reasoning_source("inspect first"));

        let rendered = block.render_uncached(40, false, None);

        assert_eq!(row_text(&rendered, 0), "• Thinking (0s)");
        assert_eq!(row_text(&rendered, 1), "  inspect first");
    }

    #[test]
    fn stacking_rows_saturates_at_the_buffer_height_limit() {
        let rows = [
            Buffer::empty(Rect::new(0, 0, 1, 40_000)),
            Buffer::empty(Rect::new(0, 0, 1, 40_000)),
        ];

        let rendered = stack_rows(&rows, 1);

        assert_eq!(rendered.area.height, u16::MAX);
    }

    #[test]
    fn expanded_running_reasoning_shows_every_line() {
        let source = (1..=80)
            .map(|line| format!("- item {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut block = LiveBlock::reasoning(1);
        assert!(block.append_reasoning_source(&source));

        let collapsed = block.render(80, false);
        assert_eq!(collapsed.area.height, 6);

        let expanded = block.render(80, true);
        assert_eq!(expanded.area.height, 81);
        assert!(row_text(&expanded, 80).contains("item 80"));
    }

    #[test]
    fn running_tool_becomes_completed_in_the_same_block() {
        let call_id = ToolCallId::from_provider("call-1");
        let mut block = LiveBlock::running_tool(
            1,
            call_id.clone(),
            "bash".to_string(),
            serde_json::json!({"command": "cargo test"}),
        )
        .with_turn(Some(test_turn(7)));

        let running = block.render(60, false);
        assert_eq!(row_text(&running, 0), "• Bash cargo test");
        assert!(block.is_running_tool(&call_id));
        assert!(block.is_unfinished_response_for_turn(test_turn(7)));

        assert!(block.finish_tool(&call_id, "ok".to_string(), false));
        let finished = block.render(60, false);
        assert_eq!(row_text(&finished, 0), "• Bash cargo test");
        assert_eq!(row_text(&finished, 1), "  └ ok");
        assert!(!block.is_unfinished_response_for_turn(test_turn(7)));
    }

    #[test]
    fn tool_completion_requires_the_matching_call_id() {
        let call_id = ToolCallId::from_provider("call-1");
        let mut block = LiveBlock::running_tool(
            1,
            call_id.clone(),
            "bash".to_string(),
            serde_json::json!({"command": "pwd"}),
        );

        assert!(!block.finish_tool(
            &ToolCallId::from_provider("call-2"),
            "wrong".to_string(),
            false,
        ));
        assert!(block.is_running_tool(&call_id));
        assert_eq!(row_text(&block.render(40, false), 0), "• Bash pwd");
    }

    #[test]
    fn expanded_thoughts_reveal_the_full_reasoning_text() {
        let block = LiveBlock::thought(1, "first\nsecond".to_string(), 3);
        let rendered = block.render(40, true);

        assert_eq!(rendered.area.height, 3);
        assert_eq!(row_text(&rendered, 0), "• Thought for 3s — expanded");
        assert_eq!(row_text(&rendered, 1), "  first");
        assert_eq!(row_text(&rendered, 2), "  second");
    }

    #[test]
    fn outputless_tools_expose_a_groupable_detail() {
        let read = LiveBlock::tool(
            1,
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/inline.rs"}),
            String::new(),
            false,
        );
        let running = LiveBlock::running_tool(
            2,
            ToolCallId::from_provider("call-2"),
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/viewport.rs"}),
        );
        let bash = LiveBlock::tool(
            3,
            "bash".to_string(),
            serde_json::json!({"command": "pwd"}),
            String::new(),
            false,
        );
        let skill = LiveBlock::tool(
            4,
            "skill".to_string(),
            serde_json::json!({"name": "review"}),
            "full instructions".to_string(),
            false,
        );
        let failed = LiveBlock::tool(
            5,
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/missing.rs"}),
            "file not found".to_string(),
            true,
        );

        assert_eq!(
            read.grouped_tool_detail(false),
            Some(("read".to_string(), "inline.rs".to_string()))
        );
        assert_eq!(
            running.grouped_tool_detail(false),
            Some(("read".to_string(), "viewport.rs".to_string()))
        );
        assert_eq!(
            skill.grouped_tool_detail(false),
            Some(("skill".to_string(), "review".to_string()))
        );
        assert_eq!(skill.grouped_tool_detail(true), None);
        assert_eq!(bash.grouped_tool_detail(false), None);
        assert_eq!(failed.grouped_tool_detail(false), None);
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
    fn expanded_bash_output_shows_every_line() {
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
        let expanded = block.render(40, true);
        assert_eq!(expanded.area.height, 1 + 120);
        assert!(row_text(&expanded, 1).contains("line 1"), "head");
        assert!(row_text(&expanded, 120).contains("line 120"), "tail");
        assert!(!(0..expanded.area.height)
            .any(|row| row_text(&expanded, row).contains("truncated for display")));
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

        let expanded = block.render(40, true);
        assert_eq!(expanded.area.height, 1 + 19 + 1);
        assert!(row_text(&expanded, 19).contains("echo step 20"));
        assert!(row_text(&expanded, 20).contains("done"));
    }

    #[test]
    fn edit_and_write_render_from_tool_arguments() {
        let edit = LiveBlock::tool(
            1,
            "edit".to_string(),
            serde_json::json!({
                "path": "/workspace/src/main.rs",
                "edits": [{"oldText": "old", "newText": "new"}]
            }),
            "Successfully replaced 1 block".to_string(),
            false,
        )
        .render(60, false);
        let write = LiveBlock::tool(
            2,
            "write".to_string(),
            serde_json::json!({"path": "/workspace/src/new.rs", "content": "one\ntwo"}),
            "Wrote 7 bytes".to_string(),
            false,
        )
        .render(60, false);

        assert_eq!(row_text(&edit, 0), "• Edit /workspace/src/main.rs (+1 -1)");
        assert_eq!(row_text(&write, 0), "• Write /workspace/src/new.rs (+2 -0)");

        let relative = LiveBlock::tool(
            4,
            "edit".to_string(),
            serde_json::json!({
                "path": "/workspace/src/main.rs",
                "edits": [{"oldText": "old", "newText": "new"}]
            }),
            String::new(),
            false,
        )
        .render_in(80, false, Some(std::path::Path::new("/workspace")));
        assert_eq!(row_text(&relative, 0), "• Edit src/main.rs (+1 -1)");
        assert_eq!(edit.cell((2, 1)).expect("deleted line").fg, Color::Red);
        assert_eq!(edit.cell((2, 2)).expect("added line").fg, Color::Green);
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
    fn edit_content_that_resembles_diff_headers_is_not_hidden() {
        let block = LiveBlock::tool(
            1,
            "edit".to_string(),
            serde_json::json!({
                "path": "markers.txt",
                "edits": [{"oldText": "-- before", "newText": "++ after"}]
            }),
            String::new(),
            false,
        );

        let rendered = block.render(60, false);

        assert_eq!(row_text(&rendered, 0), "• Edit markers.txt (+1 -1)");
        assert_eq!(row_text(&rendered, 1), "  --- before");
        assert_eq!(row_text(&rendered, 2), "  +++ after");
    }

    #[test]
    fn long_write_arguments_render_completely() {
        let content = (1..=30)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block = LiveBlock::tool(
            1,
            "write".to_string(),
            serde_json::json!({"path": "long.txt", "content": content}),
            String::new(),
            false,
        );

        let collapsed = block.render(80, false);
        assert_eq!(collapsed.area.height, 31);
        assert!(row_text(&collapsed, 30).contains("line 30"));
        assert!(!(0..collapsed.area.height)
            .any(|row| row_text(&collapsed, row).contains("truncated for display")));

        let expanded = block.render(80, true);
        assert_eq!(expanded.area.height, 31);
        assert!(row_text(&expanded, 30).contains("line 30"));
    }

    #[test]
    fn malformed_specialized_tool_arguments_fall_back_to_generic_output() {
        let block = LiveBlock::tool(
            1,
            "write".to_string(),
            serde_json::json!({"path": "same.txt"}),
            "Wrote 5 bytes".to_string(),
            false,
        );

        let rendered = block.render(60, false);

        assert_eq!(rendered.area.height, 2);
        assert_eq!(row_text(&rendered, 0), "• Write same.txt");
        assert_eq!(row_text(&rendered, 1), "  └ Wrote 5 bytes");
    }

    #[test]
    fn welcome_frame_stays_cyan_around_dim_content() {
        let buffer = render_welcome(40, std::path::Path::new("/workspace/ash"));
        let subtitle_row = (0..buffer.area.height)
            .find(|&row| {
                (0..buffer.area.width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(ratatui::buffer::Cell::symbol)
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
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

#[cfg(test)]
mod generic_output_tests {
    use super::*;

    fn test_turn(n: u128) -> TurnId {
        TurnId::from_u128(n)
    }

    fn row_text(buffer: &Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|column| buffer.cell((column, row)))
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn finalized_assistant_does_not_count_as_a_completed_tool() {
        let mut block = LiveBlock::assistant(1, String::new()).with_turn(Some(test_turn(7)));
        assert!(block.append_markdown_source("partial"));
        assert!(block.is_unfinished_response_for_turn(test_turn(7)));

        block.finalize_markdown();
        assert!(!block.is_unfinished_response_for_turn(test_turn(7)));
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
        // Generic titles wrap at the column width, like bash command lines.
        let rendered = block.render(30, false);
        assert!(rendered.area.height > 2);
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
        assert!(row_text(&rendered, 0).contains("Grep"), "title");
        assert!(row_text(&rendered, 1).contains("invalid regular"));
    }

    #[test]
    fn bash_error_title_is_red_and_shows_output() {
        let block = LiveBlock::tool(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "false"}),
            "exit code 1".to_string(),
            true,
        );
        let rendered = block.render(40, false);
        assert!(
            row_text(&rendered, 0).contains("Bash false"),
            "title: {:?}",
            row_text(&rendered, 0)
        );
        assert!(
            (0..rendered.area.width).any(|x| rendered[(x, 0)].style().fg == Some(Color::Red)),
            "title is red"
        );
        assert!(row_text(&rendered, 1).contains("exit code 1"));
    }

    #[test]
    fn successful_search_tools_summarize_then_expand_fully() {
        let cases = [
            (
                "grep",
                serde_json::json!({"pattern": "let x", "path": "src"}),
                "Found 2 matching lines\n\nsrc/main.rs:\n  Line 12: let x = 1;",
            ),
            (
                "glob",
                serde_json::json!({"pattern": "**/*.rs"}),
                "Found 1 file\nsrc/main.rs",
            ),
        ];

        for (name, arguments, output) in cases {
            let block = LiveBlock::tool(1, name.to_string(), arguments, output.to_string(), false);

            let collapsed = block.render(80, false);
            let collapsed_text = rendered_to_string(&collapsed);
            assert_eq!(collapsed.area.height, 2, "{name}");
            assert!(collapsed_text.contains(output.lines().next().unwrap()));
            assert!(!collapsed_text.contains("src/main.rs"));

            let expanded_text = rendered_to_string(&block.render(80, true));
            assert!(expanded_text.contains("src/main.rs"), "{name} expanded");
        }
    }

    #[test]
    fn successful_webfetch_summarizes_then_expands_fully() {
        let block = LiveBlock::tool(
            1,
            "webfetch".to_string(),
            serde_json::json!({"url": "https://example.com/docs"}),
            "HTTP 200 OK · 42 chars\n\n# Documentation\n\nFull fetched page content.".to_string(),
            false,
        );

        let collapsed = block.render(80, false);
        let collapsed_text = rendered_to_string(&collapsed);
        assert_eq!(collapsed.area.height, 2);
        assert!(collapsed_text.contains("HTTP 200 OK · 42 chars"));
        assert!(!collapsed_text.contains("Documentation"));

        let expanded = block.render(80, true);
        let expanded_text = rendered_to_string(&expanded);
        assert!(expanded_text.contains("HTTP 200 OK · 42 chars"));
        assert!(expanded_text.contains("Documentation"));
        assert!(expanded_text.contains("Full fetched page content."));
    }

    fn rendered_to_string(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|row| row_text(buffer, row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn expandable_tools_hide_success_until_expanded_but_errors_surface() {
        let snapshot = r#"{"completions":[{"name":"research","turn_id":"…"}]}"#;
        let done = LiveBlock::tool(
            1,
            "wait_agent".to_string(),
            serde_json::json!({}),
            snapshot.to_string(),
            false,
        );
        let rendered = done.render(50, false);
        assert_eq!(rendered.area.height, 1);
        assert!(row_text(&rendered, 0).contains("Wait agent"));

        let expanded = done.render(80, true);
        assert!(rendered_to_string(&expanded).contains("completions"));

        let failed = LiveBlock::tool(
            1,
            "wait_agent".to_string(),
            serde_json::json!({}),
            "wait failed".to_string(),
            true,
        );
        let rendered = failed.render(50, false);
        assert!(rendered_to_string(&rendered).contains("wait failed"));
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
