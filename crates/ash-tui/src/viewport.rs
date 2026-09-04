use std::{path::Path, sync::Arc};

use ash_core::{SessionSummary, TurnActivity};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::AppState,
    block_layout::{layout_stack, StackItem},
    fork_picker::ForkOption,
    live_block::{render_grouped_tool, LiveBlock},
    menu::MenuView,
    operation::ActivityView,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    status_line::{
        compact_path, fit_status_left, format_elapsed, format_token_rate, format_token_usage,
    },
    text_width::truncate_end,
    SubagentView, SubagentViewState,
};

const FOOTER_ROWS: u16 = 1;
const MAX_COMPOSER_ROWS: u16 = 8;
const MENU_MAX_ROWS: usize = 8;
const SCREEN_SPACING: u16 = 1;
const TURN_STATS_STATUS_WIDTH: u16 = 80;
const SUBAGENTS_MAX_ROWS: usize = 4;
const FOOTER_SIDE_PADDING: u16 = 2;
const FOOTER_COLUMN_GAP: u16 = 3;
const FOOTER_MIN_LEFT_WIDTH: u16 = 3;
const COMMAND_NAME_PREFIX_COLUMNS: usize = 3;
const MENU_COLUMN_GAP: usize = 2;
const MENU_PREFIX_COLUMNS: usize = 2;
const SESSION_CREATED_MIN_LEFT_COLUMNS: usize = 8;
const TERMINAL_SAFE_COLUMN: u16 = 1;
pub const COMPOSER_TEXT_COLUMN: u16 = 2;

pub fn drawable_width(terminal_width: u16) -> u16 {
    // Keep the final column free because writing into it can trigger an automatic wrap.
    terminal_width.saturating_sub(1).max(1)
}

#[derive(Clone, Copy)]
struct ViewportInput<'a> {
    pub(crate) terminal_width: u16,
    pub(crate) terminal_height: u16,
    pub(crate) transcript: &'a [LiveBlock],
    pub(crate) scroll_top: Option<u16>,
    pub(crate) busy: bool,
    pub(crate) status_header: &'a str,
    pub(crate) elapsed: &'a str,
    pub(crate) turn_activity: Option<TurnActivity>,
    pub(crate) context_tokens: Option<u64>,
    pub(crate) context_limit: Option<u64>,
    pub(crate) prompt_lines: &'a [String],
    pub(crate) prompt_cursor_row: u16,
    pub(crate) prompt_cursor_column: u16,
    pub(crate) menu: MenuView<'a>,
    pub(crate) model: &'a str,
    pub(crate) protocol: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) tools_expanded: bool,
    pub(crate) subagents: &'a [SubagentView],
}

pub struct ViewportFrame {
    pub(crate) buffer: Buffer,
    pub(crate) viewport_height: u16,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_column: u16,
    pub(crate) scroll_top: u16,
    pub(crate) max_scroll_top: u16,
    pub(crate) page_rows: u16,
}

impl ViewportFrame {
    #[cfg(test)]
    pub(crate) fn for_test(buffer: Buffer, cursor: ratatui::layout::Position) -> Self {
        let area = buffer.area;
        Self {
            viewport_height: area.height.max(1),
            buffer,
            cursor_row: cursor.y,
            cursor_column: cursor.x,
            scroll_top: 0,
            max_scroll_top: 0,
            page_rows: area.height.max(1),
        }
    }
}

pub(crate) fn render(state: &AppState, width: u16, height: u16) -> ViewportFrame {
    let elapsed = format_elapsed(state.status.elapsed_seconds());
    let (busy, status_header) = match state.operation.activity_view() {
        ActivityView::Idle => (false, ""),
        ActivityView::Active { header, .. } => (true, header),
    };
    let status_header = sanitize_single_line(status_header);
    let model = sanitize_single_line(&state.model);
    let protocol = sanitize_single_line(&state.protocol);
    let prompt = state.input.view(composer_text_width(width));
    let subagents = state
        .subagents
        .iter()
        .filter(|agent| Some(agent.root_id) == state.session_id)
        .cloned()
        .collect::<Vec<_>>();
    render_view(ViewportInput {
        terminal_width: width,
        terminal_height: height,
        transcript: state.blocks.pending(),
        scroll_top: state.scroll_top,
        busy,
        status_header: &status_header,
        elapsed: &elapsed,
        turn_activity: state.turn_activity,
        context_tokens: state.context_tokens,
        context_limit: state.context_limit,
        prompt_lines: &prompt.lines,
        prompt_cursor_row: prompt.cursor_row,
        prompt_cursor_column: prompt.cursor_column,
        menu: state.menu.view(),
        model: &model,
        protocol: &protocol,
        working_dir: &state.working_dir,
        tools_expanded: state.tools_expanded,
        subagents: &subagents,
    })
}

pub(crate) fn composer_text_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_sub(COMPOSER_TEXT_COLUMN)
        .saturating_sub(TERMINAL_SAFE_COLUMN)
        .max(1)
}

fn render_view(input: ViewportInput<'_>) -> ViewportFrame {
    let terminal_width = input.terminal_width.max(1);
    let terminal_height = input.terminal_height.max(1);
    let width = drawable_width(input.terminal_width);
    let requested_composer_rows = u16::try_from(input.prompt_lines.len())
        .unwrap_or(u16::MAX)
        .clamp(1, MAX_COMPOSER_ROWS);

    let rendered_groups = grouped_transcript(
        input.transcript,
        width,
        input.tools_expanded,
        Some(input.working_dir),
    );
    let items = rendered_groups
        .iter()
        .map(|group| StackItem::block(group.buffer.area.height))
        .collect::<Vec<_>>();
    let layout = layout_stack(width, &items);
    let menu_rows = u16::try_from(input.menu.item_count().min(MENU_MAX_ROWS)).unwrap_or(u16::MAX);
    let (screen_rows, fitted_height) = fit_screen_rows(
        ScreenRows {
            transcript: layout.height,
            status: u16::from(input.busy),
            composer: requested_composer_rows,
            menu: menu_rows,
            subagents: subagent_rows(input.subagents),
            footer: if menu_rows == 0 { FOOTER_ROWS } else { 0 },
        },
        terminal_height,
    );
    let screen = layout_screen(width, screen_rows, fitted_height);
    let transcript_view_rows = screen.transcript.height;

    let max_scroll_top = layout.height.saturating_sub(transcript_view_rows);
    let scroll_top = input
        .scroll_top
        .unwrap_or(max_scroll_top)
        .min(max_scroll_top);
    let mut buffer = Buffer::empty(Rect::new(0, 0, terminal_width, terminal_height));
    render_transcript(
        &layout.areas,
        &rendered_groups,
        scroll_top,
        transcript_view_rows,
        &mut buffer,
    );

    if !screen.status.is_empty() {
        render_status(screen.status, &input, &mut buffer);
    }

    let prompt = prompt_window(&input, screen.composer.height);
    render_composer(screen.composer, &prompt, &mut buffer);
    if !screen.subagents.is_empty() {
        render_subagents(screen.subagents, input.subagents, &mut buffer);
    }
    match input.menu {
        MenuView::None => render_footer(screen.footer, &input, &mut buffer),
        MenuView::Commands { items, selected } => {
            render_command_menu(screen.menu, items, selected, &mut buffer);
        }
        MenuView::Sessions { items, selected } => {
            render_session_menu(screen.menu, items, selected, &mut buffer);
        }
        MenuView::ForkPoints { items, selected } => {
            render_fork_menu(screen.menu, items, selected, &mut buffer);
        }
    }

    ViewportFrame {
        buffer,
        viewport_height: u16::try_from(fitted_height)
            .unwrap_or(u16::MAX)
            .min(terminal_height)
            .max(1),
        cursor_row: screen.composer.y.saturating_add(prompt.cursor_row),
        cursor_column: COMPOSER_TEXT_COLUMN
            .saturating_add(prompt.cursor_column)
            .min(input.terminal_width.saturating_sub(1)),
        scroll_top,
        max_scroll_top,
        page_rows: transcript_view_rows.max(1),
    }
}

pub(crate) struct RenderedTranscriptGroup<'a> {
    pub(crate) source: &'a [LiveBlock],
    pub(crate) buffer: Arc<Buffer>,
}

pub(crate) fn grouped_transcript<'a>(
    blocks: &'a [LiveBlock],
    width: u16,
    expanded: bool,
    working_dir: Option<&Path>,
) -> Vec<RenderedTranscriptGroup<'a>> {
    blocks
        .chunk_by(|left, right| {
            left.grouped_tool_name(expanded)
                .zip(right.grouped_tool_name(expanded))
                .is_some_and(|(left, right)| left == right)
        })
        .map(|source| {
            let buffer = if let [block] = source {
                block.render_in(width, expanded, working_dir)
            } else {
                let name = source
                    .first()
                    .and_then(|block| block.grouped_tool_name(expanded))
                    .unwrap_or_default();
                let details = source
                    .iter()
                    .filter_map(|block| block.grouped_tool_detail(expanded))
                    .map(|(_, detail)| detail)
                    .collect::<Vec<_>>();
                Arc::new(render_grouped_tool(
                    name,
                    &details,
                    source.iter().any(LiveBlock::is_running),
                    width,
                ))
            };
            RenderedTranscriptGroup { source, buffer }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct ScreenRows {
    transcript: u16,
    status: u16,
    composer: u16,
    menu: u16,
    subagents: u16,
    footer: u16,
}

#[derive(Default)]
struct ScreenAreas {
    transcript: Rect,
    status: Rect,
    composer: Rect,
    menu: Rect,
    subagents: Rect,
    footer: Rect,
}

#[derive(Clone, Copy)]
enum ScreenPart {
    Transcript,
    Status,
    Composer,
    Menu,
    Subagents,
    Footer,
}

// Order defines which sections yield space first as the terminal shrinks.
const SHRINK_ORDER: [ScreenPart; 5] = [
    ScreenPart::Transcript,
    ScreenPart::Footer,
    ScreenPart::Menu,
    ScreenPart::Composer,
    ScreenPart::Subagents,
];
const REMOVE_ORDER: [ScreenPart; 5] = [
    ScreenPart::Status,
    ScreenPart::Subagents,
    ScreenPart::Transcript,
    ScreenPart::Footer,
    ScreenPart::Menu,
];

impl ScreenRows {
    const fn rows_mut(&mut self, part: ScreenPart) -> &mut u16 {
        match part {
            ScreenPart::Transcript => &mut self.transcript,
            ScreenPart::Status => &mut self.status,
            ScreenPart::Composer => &mut self.composer,
            ScreenPart::Menu => &mut self.menu,
            ScreenPart::Subagents => &mut self.subagents,
            ScreenPart::Footer => &mut self.footer,
        }
    }

    fn shrink_to_fit(&mut self, part: ScreenPart, minimum: u16, height: u16) {
        let overflow = screen_overflow(*self, height);
        let rows = self.rows_mut(part);
        *rows = rows.saturating_sub(overflow.min(rows.saturating_sub(minimum)));
    }

    fn remove_if_needed(&mut self, part: ScreenPart, height: u16) {
        if screen_height(*self) > u32::from(height) {
            *self.rows_mut(part) = 0;
        }
    }
}

/// Fit the screen rows into the available terminal height, returning the
/// fitted rows together with their total screen height so callers and
/// `layout_screen` do not recompute it.
fn fit_screen_rows(mut rows: ScreenRows, height: u16) -> (ScreenRows, u32) {
    for part in SHRINK_ORDER {
        rows.shrink_to_fit(part, 1, height);
    }
    for part in REMOVE_ORDER {
        rows.remove_if_needed(part, height);
    }
    let fitted_height = screen_height(rows);
    (rows, fitted_height)
}

fn screen_overflow(rows: ScreenRows, height: u16) -> u16 {
    u16::try_from(screen_height(rows).saturating_sub(u32::from(height))).unwrap_or(u16::MAX)
}

fn screen_height(rows: ScreenRows) -> u32 {
    let values = [
        rows.transcript,
        rows.status,
        rows.composer.saturating_add(rows.menu),
        rows.subagents,
        rows.footer,
    ];
    let content = values
        .iter()
        .fold(0_u32, |total, rows| total + u32::from(*rows));
    let gaps = u32::try_from(
        values
            .iter()
            .filter(|rows| **rows > 0)
            .count()
            .saturating_sub(1),
    )
    .unwrap_or(u32::MAX);
    content + gaps * u32::from(SCREEN_SPACING)
}

fn layout_screen(width: u16, rows: ScreenRows, fitted_height: u32) -> ScreenAreas {
    let mut parts = Vec::with_capacity(5);
    let mut constraints = Vec::with_capacity(5);
    for (part, rows) in [
        (ScreenPart::Transcript, rows.transcript),
        (ScreenPart::Status, rows.status),
        (
            ScreenPart::Composer,
            rows.composer.saturating_add(rows.menu),
        ),
        (ScreenPart::Subagents, rows.subagents),
        (ScreenPart::Footer, rows.footer),
    ] {
        if rows > 0 {
            parts.push(part);
            constraints.push(Constraint::Length(rows));
        }
    }

    let layout = Layout::vertical(constraints)
        .flex(Flex::Start)
        .spacing(SCREEN_SPACING)
        .split(Rect::new(
            0,
            0,
            width,
            u16::try_from(fitted_height).unwrap_or(u16::MAX),
        ));
    let mut areas = ScreenAreas::default();
    for (part, area) in parts.into_iter().zip(layout.iter().copied()) {
        match part {
            ScreenPart::Transcript => areas.transcript = area,
            ScreenPart::Status => areas.status = area,
            ScreenPart::Composer if rows.menu > 0 => {
                let input_surface = Layout::vertical([
                    Constraint::Length(rows.composer),
                    Constraint::Length(rows.menu),
                ])
                .split(area);
                areas.composer = input_surface[0];
                areas.menu = input_surface[1];
            }
            ScreenPart::Composer => areas.composer = area,
            ScreenPart::Menu => areas.menu = area,
            ScreenPart::Subagents => areas.subagents = area,
            ScreenPart::Footer => areas.footer = area,
        }
    }
    areas
}

struct PromptWindow<'a> {
    lines: &'a [String],
    cursor_row: u16,
    cursor_column: u16,
}

fn prompt_window<'a>(input: &'a ViewportInput<'_>, rows: u16) -> PromptWindow<'a> {
    let total = input.prompt_lines.len();
    if total == 0 {
        return PromptWindow {
            lines: &[],
            cursor_row: 0,
            cursor_column: 0,
        };
    }

    let visible = usize::from(rows.max(1)).min(total);
    let cursor = usize::from(input.prompt_cursor_row).min(total - 1);
    let start = cursor
        .saturating_add(1)
        .saturating_sub(visible)
        .min(total - visible);
    PromptWindow {
        lines: &input.prompt_lines[start..start + visible],
        cursor_row: u16::try_from(cursor - start).unwrap_or(u16::MAX),
        cursor_column: input.prompt_cursor_column,
    }
}

fn render_transcript(
    areas: &[Rect],
    groups: &[RenderedTranscriptGroup<'_>],
    scroll_top: u16,
    visible_rows: u16,
    buffer: &mut Buffer,
) {
    let visible_bottom = scroll_top.saturating_add(visible_rows);
    for (group, area) in groups.iter().zip(areas) {
        let top = area.y.max(scroll_top);
        let bottom = area.bottom().min(visible_bottom);
        if top >= bottom {
            continue;
        }
        let target = Rect::new(
            area.x,
            top.saturating_sub(scroll_top),
            area.width,
            bottom.saturating_sub(top),
        );
        let source_y = top.saturating_sub(area.y);
        crate::buffer::copy_rows(&group.buffer, buffer, source_y, target);
    }
}

fn subagent_rows(subagents: &[SubagentView]) -> u16 {
    if subagents.is_empty() {
        return 0;
    }
    let rows = u16::try_from(subagents.len().min(SUBAGENTS_MAX_ROWS)).unwrap_or(u16::MAX);
    rows.saturating_add(u16::from(subagents.len() > SUBAGENTS_MAX_ROWS))
}

fn render_subagents(area: Rect, subagents: &[SubagentView], buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let visible = subagents.len().min(SUBAGENTS_MAX_ROWS);
    for (index, subagent) in subagents.iter().take(visible).enumerate() {
        let state_symbol = subagent_state_symbol(subagent.state);
        let state_color = subagent_state_color(subagent.state);
        let metrics = activity_metrics(subagent.activity);
        let context = context_display(subagent.context_tokens, subagent.context_limit)
            .map(|context| format!("ctx {}", context.text));
        let metrics = match (metrics.is_empty(), context) {
            (true, Some(context)) => context,
            (false, Some(context)) => format!("{metrics} · {context}"),
            _ => metrics,
        };
        let metrics = (!metrics.is_empty()).then(|| format!("  {metrics}"));
        let prefix_width = UnicodeWidthStr::width(state_symbol).saturating_add(1);
        let available = usize::from(area.width).saturating_sub(prefix_width);
        let fixed = metrics.map_or_else(
            || truncate_end(&subagent.name, available),
            |metrics| {
                let metrics_width = UnicodeWidthStr::width(metrics.as_str());
                let name_width = UnicodeWidthStr::width(subagent.name.as_str());
                if name_width.saturating_add(metrics_width) <= available {
                    format!("{}{metrics}", subagent.name)
                } else {
                    truncate_end(&subagent.name, available)
                }
            },
        );
        let spans = vec![
            Span::styled(
                format!("{state_symbol} "),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(fixed, Style::default().add_modifier(Modifier::BOLD)),
        ];
        let line = Line::from(spans);
        buffer.set_line(
            area.x,
            area.y
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
            &line,
            area.width,
        );
    }
    if subagents.len() > SUBAGENTS_MAX_ROWS {
        let more = subagents.len() - SUBAGENTS_MAX_ROWS;
        buffer.set_line(
            area.x,
            area.y
                .saturating_add(u16::try_from(SUBAGENTS_MAX_ROWS).unwrap_or(u16::MAX)),
            &Line::from(Span::styled(
                format!("  +{more} more"),
                Style::default().add_modifier(Modifier::DIM),
            )),
            area.width,
        );
    }
}

const fn subagent_state_symbol(state: SubagentViewState) -> &'static str {
    match state {
        SubagentViewState::Idle => "○",
        SubagentViewState::Running => "●",
    }
}

const fn subagent_state_color(state: SubagentViewState) -> Color {
    match state {
        SubagentViewState::Idle => Color::DarkGray,
        SubagentViewState::Running => Color::Cyan,
    }
}

fn render_status(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let mut spans = vec![
        Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            input.status_header.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", input.elapsed),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    if input.terminal_width >= TURN_STATS_STATUS_WIDTH {
        let metrics = activity_metrics(input.turn_activity.unwrap_or_default());
        if !metrics.is_empty() {
            spans.push(Span::styled(
                format!("  {metrics}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }
    buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
}

fn activity_metrics(activity: TurnActivity) -> String {
    if activity == TurnActivity::default() {
        return String::new();
    }
    let stats = activity.stats;
    let mut metrics = format!(
        "{} · {} tools",
        format_token_usage(stats.input_tokens, stats.output_tokens),
        activity.completed_tool_calls,
    );
    if let Some(rate) = format_token_rate(stats.output_tokens, stats.generation_ms) {
        metrics.push_str(" · ");
        metrics.push_str(&rate);
    }
    metrics
}

fn render_composer(area: Rect, prompt: &PromptWindow<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }

    if prompt.lines.is_empty() {
        buffer.set_string(
            area.x,
            area.y,
            "›",
            Style::default().add_modifier(Modifier::BOLD),
        );
        return;
    }

    for (index, text) in prompt
        .lines
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let prefix = if index == 0 {
            Span::styled("› ", Style::default().add_modifier(Modifier::BOLD))
        } else {
            Span::raw("  ")
        };
        buffer.set_line(
            area.x,
            area.y
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
            &Line::from(vec![prefix, Span::raw(text)]),
            area.width,
        );
    }
}

fn render_footer(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let path = compact_path(input.working_dir);
    let context = context_display(input.context_tokens, input.context_limit);
    let protocol = (!input.protocol.is_empty()).then_some(input.protocol);
    let context_text = context
        .as_ref()
        .map(|context| format!("ctx {}", context.text));
    let (context, protocol) = if footer_right_fits(area, context_text.as_deref(), protocol) {
        (context, protocol)
    } else if footer_right_fits(area, context_text.as_deref(), None) {
        (context, None)
    } else if footer_right_fits(area, None, protocol) {
        (None, protocol)
    } else {
        (None, None)
    };
    let displayed_context = context
        .as_ref()
        .map(|_| context_text.as_deref().unwrap_or_default());
    let right_width = footer_right_width(displayed_context, protocol);
    let left_width = if right_width > 0 {
        area.width
            .saturating_sub(right_width)
            .saturating_sub(FOOTER_SIDE_PADDING)
            .saturating_sub(FOOTER_COLUMN_GAP)
    } else {
        area.width.saturating_sub(FOOTER_SIDE_PADDING * 2)
    };
    let (model, path) = fit_status_left(input.model, &path, left_width);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(model, Style::default().fg(Color::Cyan)),
    ];
    if let Some(path) = path {
        spans.push(Span::styled(
            " · ",
            Style::default().add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(path, Style::default().fg(Color::Green)));
    }
    buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
    if context.is_some() || protocol.is_some() {
        let right_x = area.x.saturating_add(
            area.width
                .saturating_sub(right_width.saturating_add(FOOTER_SIDE_PADDING)),
        );
        let mut spans = Vec::new();
        if let Some(context) = context {
            spans.push(Span::styled(
                context_text.as_deref().unwrap_or_default(),
                Style::default().fg(context.color),
            ));
        }
        if let Some(protocol) = protocol {
            if !spans.is_empty() {
                spans.push(Span::styled(
                    " · ",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(Span::styled(protocol, Style::default().fg(Color::Cyan)));
        }
        buffer.set_line(right_x, area.y, &Line::from(spans), right_width);
    }
}

#[derive(Clone)]
struct ContextDisplay {
    text: String,
    color: Color,
}

fn context_display(
    context_tokens: Option<u64>,
    context_limit: Option<u64>,
) -> Option<ContextDisplay> {
    let tokens = context_tokens?;
    let limit = context_limit.filter(|limit| *limit > 0)?;
    let percent = tokens.saturating_mul(100) / limit;
    Some(ContextDisplay {
        text: if percent > 100 {
            "100%+".to_string()
        } else {
            format!("{percent}%")
        },
        color: match percent {
            0..=69 => Color::Green,
            70..=84 => Color::Yellow,
            _ => Color::Red,
        },
    })
}

fn footer_right_fits(area: Rect, context: Option<&str>, protocol: Option<&str>) -> bool {
    let right_width = footer_right_width(context, protocol);
    right_width > 0
        && area.width
            >= right_width
                .saturating_add(FOOTER_SIDE_PADDING)
                .saturating_add(FOOTER_COLUMN_GAP)
                .saturating_add(FOOTER_MIN_LEFT_WIDTH)
}

fn footer_right_width(context: Option<&str>, protocol: Option<&str>) -> u16 {
    let context_width = context.map_or(0, UnicodeWidthStr::width);
    let protocol_width = protocol.map_or(0, UnicodeWidthStr::width);
    let separator_width = u16::from(context.is_some() && protocol.is_some()) * 3;
    u16::try_from(context_width)
        .unwrap_or(u16::MAX)
        .saturating_add(u16::try_from(protocol_width).unwrap_or(u16::MAX))
        .saturating_add(separator_width)
}

fn render_command_menu(
    area: Rect,
    items: &[CommandCompletion],
    selected: usize,
    buffer: &mut Buffer,
) {
    let name_width = items
        .iter()
        .map(|item| UnicodeWidthStr::width(item.name))
        .max()
        .unwrap_or(1);
    let description_column = COMMAND_NAME_PREFIX_COLUMNS
        .saturating_add(name_width)
        .saturating_add(MENU_COLUMN_GAP);
    render_menu_rows(
        area,
        items,
        selected,
        buffer,
        |_index, style, prefix, item| {
            let mut spans = vec![Span::styled(
                format!("{prefix}/{:<name_width$}", item.name),
                style,
            )];
            if usize::from(area.width) > description_column {
                let available = usize::from(area.width) - description_column;
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    truncate_end(item.description, available),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            spans
        },
    );
}

fn render_session_menu(
    area: Rect,
    sessions: &[SessionSummary],
    selected: usize,
    buffer: &mut Buffer,
) {
    render_menu_rows(
        area,
        sessions,
        selected,
        buffer,
        |_index, style, prefix, session| {
            let created_width = UnicodeWidthStr::width(session.created_at.as_str());
            let show_created = usize::from(area.width)
                > created_width.saturating_add(SESSION_CREATED_MIN_LEFT_COLUMNS);
            let title_width = if show_created {
                usize::from(area.width)
                    .saturating_sub(MENU_PREFIX_COLUMNS)
                    .saturating_sub(created_width)
                    .saturating_sub(MENU_COLUMN_GAP)
            } else {
                usize::from(area.width).saturating_sub(MENU_PREFIX_COLUMNS)
            };
            let title = truncate_end(&session.title, title_width);
            let title_used = UnicodeWidthStr::width(title.as_str());
            let mut spans = vec![Span::styled(prefix, style), Span::styled(title, style)];
            if show_created {
                let spacing = usize::from(area.width)
                    .saturating_sub(MENU_PREFIX_COLUMNS)
                    .saturating_sub(title_used)
                    .saturating_sub(created_width);
                spans.push(Span::raw(" ".repeat(spacing)));
                spans.push(Span::styled(
                    session.created_at.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            spans
        },
    );
}

fn render_fork_menu(area: Rect, points: &[ForkOption], selected: usize, buffer: &mut Buffer) {
    let prompt_width = usize::from(area.width).saturating_sub(MENU_PREFIX_COLUMNS);
    render_menu_rows(
        area,
        points,
        selected,
        buffer,
        |_index, style, prefix, point| {
            let prompt = sanitize_single_line(&point.prompt);
            vec![
                Span::styled(prefix, style),
                Span::styled(truncate_end(&prompt, prompt_width), style),
            ]
        },
    );
}

fn render_menu_rows<T>(
    area: Rect,
    items: &[T],
    selected: usize,
    buffer: &mut Buffer,
    row: impl Fn(usize, Style, &'static str, &T) -> Vec<Span<'static>>,
) {
    let Some(window) = menu_window(items.len(), selected, usize::from(area.height)) else {
        return;
    };
    for (offset, item) in items[window.start..window.end()].iter().enumerate() {
        let index = window.start + offset;
        let selected_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let style = if index == window.selected {
            selected_style
        } else {
            Style::default()
        };
        let prefix = if index == window.selected {
            "› "
        } else {
            "  "
        };
        let spans = row(index, style, prefix, item);
        let y = area
            .y
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        buffer.set_line(area.x, y, &Line::from(spans), area.width);
    }
}

struct MenuWindow {
    start: usize,
    visible: usize,
    selected: usize,
}

impl MenuWindow {
    const fn end(&self) -> usize {
        self.start + self.visible
    }
}

fn menu_window(items_len: usize, selected: usize, max_visible: usize) -> Option<MenuWindow> {
    let visible = max_visible.min(items_len);
    if visible == 0 {
        return None;
    }
    let selected = selected.min(items_len.saturating_sub(1));
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(items_len.saturating_sub(visible));
    Some(MenuWindow {
        start,
        visible,
        selected,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ash_core::{SessionId, TurnId};

    use super::*;
    fn row_text(buffer: &Buffer, y: u16) -> String {
        let mut continuation_columns = 0usize;
        let mut text = String::new();
        for x in 0..buffer.area.width {
            let Some(cell) = buffer.cell((x, y)) else {
                continue;
            };
            if continuation_columns > 0 || crate::buffer::cell_is_skipped(cell) {
                continuation_columns = continuation_columns.saturating_sub(1);
                continue;
            }
            text.push_str(cell.symbol());
            continuation_columns = UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
        }
        text.trim_end().to_string()
    }

    fn status_text(width: u16, header: &str) -> String {
        status_text_with_stats(width, header, None)
    }

    fn status_text_with_stats(
        width: u16,
        header: &str,
        turn_activity: Option<TurnActivity>,
    ) -> String {
        let frame = render_view(ViewportInput {
            terminal_width: width,
            terminal_height: 8,
            transcript: &[],
            scroll_top: None,
            busy: true,
            status_header: header,
            elapsed: "2s",
            turn_activity,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });
        (0..frame.buffer.area.height)
            .map(|row| row_text(&frame.buffer, row))
            .find(|row| row.starts_with(&format!("• {header}")))
            .expect("activity status")
    }

    #[test]
    fn screen_layout_handles_saturated_transcript_heights() {
        let (rows, fitted_height) = fit_screen_rows(
            ScreenRows {
                transcript: u16::MAX,
                status: 1,
                composer: 1,
                menu: 0,
                subagents: 0,
                footer: 1,
            },
            24,
        );

        assert_eq!(rows.transcript, 18);
        assert_eq!(fitted_height, 24);
        assert_eq!(screen_height(rows), 24);
    }

    #[test]
    fn screen_layout_keeps_the_composer_in_a_one_row_terminal() {
        let (rows, fitted_height) = fit_screen_rows(
            ScreenRows {
                transcript: 10,
                status: 1,
                composer: 4,
                menu: 6,
                subagents: 0,
                footer: 1,
            },
            1,
        );

        assert_eq!(rows.composer, 1);
        assert_eq!(fitted_height, 1);
        assert_eq!(screen_height(rows), 1);
    }

    #[test]
    fn places_status_composer_and_footer_after_short_content() {
        let blocks = [LiveBlock::assistant(1, "answer".to_string())];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            status_header: "Working",
            elapsed: "2s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(frame.buffer.area, Rect::new(0, 0, 80, 24));
        assert_eq!(frame.viewport_height, 7);
        assert_eq!(row_text(&frame.buffer, 0), "• answer");
        assert_eq!(row_text(&frame.buffer, 2), "• Working (2s)");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
        assert_eq!(frame.cursor_row, 4);
        assert_eq!(
            frame
                .buffer
                .cell((0, frame.cursor_row))
                .expect("composer")
                .bg,
            Color::Reset
        );
    }

    #[test]
    fn activity_status_shows_the_current_operation() {
        assert_eq!(status_text(80, "Working"), "• Working (2s)");
        assert_eq!(status_text(80, "Compacting"), "• Compacting (2s)");
    }

    #[test]
    fn activity_status_shows_live_metrics_only_when_they_fit() {
        let activity = TurnActivity {
            stats: ash_core::TurnStats {
                input_tokens: 120,
                output_tokens: 25,
                generation_ms: 200,
            },
            completed_tool_calls: 3,
        };

        assert_eq!(
            status_text_with_stats(79, "Working", Some(activity)),
            "• Working (2s)"
        );
        assert_eq!(
            status_text_with_stats(80, "Working", Some(activity)),
            "• Working (2s)  120 in / 25 out · 3 tools · 125 tok/s"
        );
    }

    #[test]
    fn context_display_uses_bounded_percentage_and_threshold_colors() {
        let green = context_display(Some(500), Some(1_000)).expect("context");
        assert_eq!(green.text, "50%");
        assert_eq!(green.color, Color::Green);

        let yellow = context_display(Some(800), Some(1_000)).expect("context");
        assert_eq!(yellow.color, Color::Yellow);

        let red = context_display(Some(1_100), Some(1_000)).expect("context");
        assert_eq!(red.text, "100%+");
        assert_eq!(red.color, Color::Red);
        assert!(context_display(Some(1), Some(0)).is_none());
    }

    #[test]
    fn footer_shows_context_percentage_alongside_protocol() {
        let frame = render_view(ViewportInput {
            terminal_width: 60,
            terminal_height: 8,
            transcript: &[],
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: Some(500),
            context_limit: Some(1_000),
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert!((0..frame.buffer.area.height)
            .map(|row| row_text(&frame.buffer, row))
            .any(|row| row.contains("ctx 50%") && row.contains("openai")));
    }

    #[test]
    fn subagents_render_below_the_composer_and_above_the_footer() {
        let blocks = [LiveBlock::assistant(1, "answer".to_string())];
        let subagents = [
            SubagentView {
                root_id: ash_core::SessionId::new(),
                session_id: ash_core::SessionId::new(),
                name: "inspect_glob".to_string(),
                state: SubagentViewState::Running,
                activity: TurnActivity::default(),
                context_tokens: None,
                context_limit: None,
                active_turn: None,
            },
            SubagentView {
                root_id: ash_core::SessionId::new(),
                session_id: ash_core::SessionId::new(),
                name: "fix_bash".to_string(),
                state: SubagentViewState::Running,
                activity: TurnActivity::default(),
                context_tokens: None,
                context_limit: None,
                active_turn: None,
            },
        ];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            status_header: "Working",
            elapsed: "2s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &subagents,
        });

        assert_eq!(frame.viewport_height, 10);
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
        assert!(row_text(&frame.buffer, 6).contains("inspect_glob"));
        assert!(row_text(&frame.buffer, 7).contains("fix_bash"));
    }

    #[test]
    fn idle_subagents_remain_visible() {
        let blocks = [LiveBlock::assistant(1, "answer".to_string())];
        let subagents = [SubagentView {
            root_id: ash_core::SessionId::new(),
            session_id: ash_core::SessionId::new(),
            name: "inspect_glob".to_string(),
            state: SubagentViewState::Idle,
            activity: TurnActivity::default(),
            context_tokens: None,
            context_limit: None,
            active_turn: None,
        }];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &subagents,
        });

        assert_eq!(frame.viewport_height, 7);
        assert!(row_text(&frame.buffer, 4).contains("inspect_glob"));
    }

    #[test]
    fn subagent_rows_preserve_snapshot_order() {
        let root_id = ash_core::SessionId::new();
        let agents = [
            SubagentView {
                root_id,
                session_id: ash_core::SessionId::new(),
                name: "idle".to_string(),
                state: SubagentViewState::Idle,
                activity: TurnActivity::default(),
                context_tokens: None,
                context_limit: None,
                active_turn: None,
            },
            SubagentView {
                root_id,
                session_id: ash_core::SessionId::new(),
                name: "running".to_string(),
                state: SubagentViewState::Running,
                activity: TurnActivity::default(),
                context_tokens: None,
                context_limit: None,
                active_turn: None,
            },
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 38, 2));

        render_subagents(Rect::new(0, 0, 38, 2), &agents, &mut buffer);

        assert!(row_text(&buffer, 0).starts_with("○ idle"));
        assert!(row_text(&buffer, 1).starts_with("● running"));
        assert!(!row_text(&buffer, 1).contains("very long task"));
    }

    #[test]
    fn subagent_rows_preserve_names_when_they_fit() {
        let agent = SubagentView {
            root_id: ash_core::SessionId::new(),
            session_id: ash_core::SessionId::new(),
            name: "a_very_long_agent_name_that_would_hide_usage".to_string(),
            state: SubagentViewState::Idle,
            activity: TurnActivity::default(),
            context_tokens: None,
            context_limit: None,
            active_turn: None,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 54, 1));

        render_subagents(Rect::new(0, 0, 54, 1), &[agent], &mut buffer);

        let row = row_text(&buffer, 0);
        assert!(row.contains("a_very_long_agent_name_that_would_hide_usage"));
    }

    #[test]
    fn subagent_rows_show_live_metrics_when_they_fit() {
        let agent = SubagentView {
            root_id: ash_core::SessionId::new(),
            session_id: ash_core::SessionId::new(),
            name: "worker".to_string(),
            state: SubagentViewState::Running,
            activity: TurnActivity {
                stats: ash_core::TurnStats {
                    input_tokens: 120,
                    output_tokens: 25,
                    generation_ms: 200,
                },
                completed_tool_calls: 2,
            },
            context_tokens: Some(500),
            context_limit: Some(1_000),
            active_turn: Some(TurnId::new()),
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 1));

        render_subagents(Rect::new(0, 0, 80, 1), &[agent], &mut buffer);

        assert_eq!(
            row_text(&buffer, 0),
            "● worker  120 in / 25 out · 2 tools · 125 tok/s · ctx 50%"
        );
    }

    #[test]
    fn subagent_rows_cap_visible_agents() {
        let root_id = ash_core::SessionId::new();
        let agents = (0..5)
            .map(|index| SubagentView {
                root_id,
                session_id: ash_core::SessionId::new(),
                name: format!("agent_{index}"),
                state: SubagentViewState::Idle,
                activity: TurnActivity::default(),
                context_tokens: None,
                context_limit: None,
                active_turn: None,
            })
            .collect::<Vec<_>>();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 5));

        render_subagents(Rect::new(0, 0, 60, 5), &agents, &mut buffer);

        assert!(row_text(&buffer, 3).contains("agent_3"));
        assert!(row_text(&buffer, 4).contains("+1 more"));
    }

    #[test]
    fn command_completion_uses_the_same_menu_below_the_composer() {
        let menu = [
            CommandCompletion {
                name: "new",
                description: "start a new chat",
            },
            CommandCompletion {
                name: "clear",
                description: "start a new chat",
            },
        ];
        let input = ViewportInput {
            terminal_width: 80,
            terminal_height: 12,
            transcript: &[],
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &["/".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 1,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        };
        let baseline = render_view(input);
        let frame = render_view(ViewportInput {
            menu: MenuView::Commands {
                items: &menu,
                selected: 1,
            },
            ..input
        });

        assert_eq!(frame.cursor_row, baseline.cursor_row);
        assert_eq!(frame.page_rows, baseline.page_rows);
        assert_eq!(frame.scroll_top, baseline.scroll_top);
        assert_eq!(frame.max_scroll_top, baseline.max_scroll_top);
        assert_eq!(baseline.viewport_height, 3);
        assert_eq!(frame.viewport_height, 3);
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› /");
        assert!(row_text(&frame.buffer, 1).contains("/new"));
        assert!(row_text(&frame.buffer, 2).contains("/clear"));
        assert!(row_text(&frame.buffer, 2).contains("start a new chat"));
    }

    #[test]
    fn session_picker_renders_multiple_rows_below_the_composer() {
        let sessions = [
            SessionSummary {
                session_id: SessionId::new(),
                title: "继续这个中文会话".to_string(),
                created_at: "2026-07-14 09:00".to_string(),
            },
            SessionSummary {
                session_id: SessionId::new(),
                title: "Inspect the session picker".to_string(),
                created_at: "2026-07-15 12:30".to_string(),
            },
            SessionSummary {
                session_id: SessionId::new(),
                title: "Third saved chat".to_string(),
                created_at: "2026-07-16 18:45".to_string(),
            },
        ];
        let input = ViewportInput {
            terminal_width: 80,
            terminal_height: 12,
            transcript: &[],
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        };
        let baseline = render_view(input);
        let frame = render_view(ViewportInput {
            menu: MenuView::Sessions {
                items: &sessions,
                selected: 1,
            },
            ..input
        });

        assert_eq!(frame.cursor_row, baseline.cursor_row);
        assert_eq!(frame.page_rows, baseline.page_rows);
        assert_eq!(frame.scroll_top, baseline.scroll_top);
        assert_eq!(frame.max_scroll_top, baseline.max_scroll_top);
        assert_eq!(baseline.viewport_height, 3);
        assert_eq!(frame.viewport_height, 4);
        let title_row = row_text(&frame.buffer, 1);
        assert!(
            title_row.contains("继续这个中文会话"),
            "unexpected session title row: {title_row:?}"
        );
        assert!(row_text(&frame.buffer, 2).contains("Inspect the session picker"));
        assert!(row_text(&frame.buffer, 2).contains("2026-07-15 12:30"));
        assert!(row_text(&frame.buffer, 3).contains("Third saved chat"));
    }

    #[test]
    fn fork_picker_renders_prompts_as_single_lines() {
        let points = [
            ForkOption {
                turn_id: TurnId::new(),
                prompt: "latest prompt\ncontinued".to_string(),
            },
            ForkOption {
                turn_id: TurnId::new(),
                prompt: "older prompt".to_string(),
            },
        ];
        let frame = render_view(ViewportInput {
            terminal_width: 50,
            terminal_height: 10,
            transcript: &[],
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::ForkPoints {
                items: &points,
                selected: 0,
            },
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert!(row_text(&frame.buffer, 1).contains("latest prompt continued"));
        assert!(row_text(&frame.buffer, 2).contains("older prompt"));
    }

    #[test]
    fn running_reasoning_starts_at_the_top_of_the_transcript() {
        let mut reasoning = LiveBlock::reasoning(1);
        assert!(reasoning.append_reasoning_source("inspect first"));
        let blocks = [reasoning];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            status_header: "Thinking",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "• Thinking (0s)");
    }

    #[test]
    fn transcript_blocks_share_the_screen_with_the_composer() {
        let blocks = [LiveBlock::history(
            1,
            crate::history_block::HistoryBlock::info("restored output"),
        )];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "• restored output");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "›");
        assert_eq!(frame.cursor_row, 2);
        assert_eq!(frame.viewport_height, 5);
        assert_eq!(row_text(&frame.buffer, 1), "");
        assert_eq!(row_text(&frame.buffer, 3), "");
    }

    #[test]
    fn submitted_input_uses_content_height_and_shared_block_spacing() {
        let blocks = [
            LiveBlock::history(1, crate::history_block::HistoryBlock::user("hello")),
            LiveBlock::history(2, crate::history_block::HistoryBlock::info("after")),
        ];
        let frame = render_view(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "› hello");
        assert_eq!(row_text(&frame.buffer, 1), "");
        assert_eq!(row_text(&frame.buffer, 2), "• after");
        assert_eq!(row_text(&frame.buffer, 3), "");
        assert_eq!(frame.cursor_row, 4);
    }

    #[test]
    fn multiline_composer_grows_between_the_transcript_and_footer() {
        let blocks = [LiveBlock::history(
            1,
            crate::history_block::HistoryBlock::info("before"),
        )];
        let prompt = ["one".to_string(), "two".to_string(), "three".to_string()];
        let frame = render_view(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &prompt,
            prompt_cursor_row: 2,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "• before");
        assert_eq!(row_text(&frame.buffer, 1), "");
        assert_eq!(row_text(&frame.buffer, 2), "› one");
        assert_eq!(row_text(&frame.buffer, 3), "  two");
        assert_eq!(row_text(&frame.buffer, 4), "  three");
        assert_eq!(row_text(&frame.buffer, 5), "");
        assert_eq!(frame.cursor_row, 4);
        assert_eq!(frame.cursor_column, 7);
    }

    #[test]
    fn multiline_composer_follows_the_cursor_after_eight_rows() {
        let prompt = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let frame = render_view(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &[],
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &prompt,
            prompt_cursor_row: 9,
            prompt_cursor_column: 6,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "› line 2");
        assert_eq!(row_text(&frame.buffer, 7), "  line 9");
        assert_eq!(row_text(&frame.buffer, 8), "");
        assert_eq!(frame.cursor_row, 7);
        assert_eq!(frame.cursor_column, 8);
    }

    #[test]
    fn transcript_follows_the_bottom_and_keeps_the_composer_fixed() {
        let blocks = (0..10)
            .map(|index| {
                LiveBlock::history(
                    index + 1,
                    crate::history_block::HistoryBlock::info(&format!("entry {index}")),
                )
            })
            .collect::<Vec<_>>();
        let frame = render_view(ViewportInput {
            terminal_width: 40,
            terminal_height: 10,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(frame.scroll_top, 13);
        assert_eq!(frame.max_scroll_top, 13);
        assert_eq!(frame.page_rows, 6);
        assert_eq!(row_text(&frame.buffer, 0), "");
        assert_eq!(row_text(&frame.buffer, 1), "• entry 7");
        assert_eq!(row_text(&frame.buffer, 5), "• entry 9");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
        assert_eq!(frame.cursor_row, 7);
    }

    #[test]
    fn explicit_scroll_top_keeps_older_transcript_rows_visible() {
        let blocks = (0..10)
            .map(|index| {
                LiveBlock::history(
                    index + 1,
                    crate::history_block::HistoryBlock::info(&format!("entry {index}")),
                )
            })
            .collect::<Vec<_>>();
        let frame = render_view(ViewportInput {
            terminal_width: 40,
            terminal_height: 10,
            transcript: &blocks,
            scroll_top: Some(4),
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(frame.scroll_top, 4);
        assert_eq!(row_text(&frame.buffer, 0), "• entry 2");
        assert_eq!(row_text(&frame.buffer, 4), "• entry 4");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "›");
    }

    #[test]
    fn consecutive_outputless_tools_group_only_while_collapsed() {
        let blocks = [
            LiveBlock::tool(
                1,
                "read".to_string(),
                serde_json::json!({"path": "/workspace/src/inline.rs"}),
                String::new(),
                false,
            ),
            LiveBlock::running_tool(
                2,
                ash_core::ToolCallId::from_provider("call-2"),
                "read".to_string(),
                serde_json::json!({"path": "/workspace/src/viewport.rs"}),
            ),
            LiveBlock::tool(
                3,
                "skill".to_string(),
                serde_json::json!({"name": "review"}),
                "review instructions".to_string(),
                false,
            ),
            LiveBlock::tool(
                4,
                "skill".to_string(),
                serde_json::json!({"name": "explore"}),
                "explore instructions".to_string(),
                false,
            ),
            LiveBlock::tool(
                5,
                "bash".to_string(),
                serde_json::json!({"command": "pwd"}),
                "/tmp".to_string(),
                false,
            ),
        ];
        let source_lengths = grouped_transcript(&blocks, 79, false, None)
            .into_iter()
            .map(|group| group.source.len())
            .collect::<Vec<_>>();
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 16,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            status_header: "",
            elapsed: "0s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(source_lengths, [2, 2, 1]);
        assert_eq!(
            grouped_transcript(&blocks, 79, true, None)
                .into_iter()
                .map(|group| group.source.len())
                .collect::<Vec<_>>(),
            [2, 1, 1, 1]
        );
        assert_eq!(row_text(&frame.buffer, 0), "• Read inline.rs, viewport.rs");
        assert_eq!(frame.buffer.cell((0, 0)).expect("bullet").fg, Color::Cyan);
        assert_eq!(row_text(&frame.buffer, 2), "• Skill review, explore");
        assert_eq!(row_text(&frame.buffer, 4), "• Bash pwd");
        assert_eq!(row_text(&frame.buffer, 5), "  └ /tmp");
    }

    #[test]
    fn failed_silent_tool_breaks_a_group_and_keeps_its_output() {
        let blocks = [
            LiveBlock::tool(
                1,
                "read".to_string(),
                serde_json::json!({"path": "/workspace/src/inline.rs"}),
                String::new(),
                false,
            ),
            LiveBlock::tool(
                2,
                "read".to_string(),
                serde_json::json!({"path": "/workspace/src/missing.rs"}),
                "file not found".to_string(),
                true,
            ),
            LiveBlock::tool(
                3,
                "read".to_string(),
                serde_json::json!({"path": "/workspace/src/viewport.rs"}),
                String::new(),
                false,
            ),
        ];

        let groups = grouped_transcript(&blocks, 79, false, None);

        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|group| group.source.len() == 1));
        assert_eq!(row_text(&groups[1].buffer, 0), "• Read missing.rs");
        assert_eq!(row_text(&groups[1].buffer, 1), "  └ file not found");
        assert_eq!(
            groups[1].buffer.cell((0, 0)).expect("bullet").fg,
            Color::Red
        );
    }

    #[test]
    fn a_single_running_read_keeps_its_own_row() {
        let blocks = [LiveBlock::running_tool(
            1,
            ash_core::ToolCallId::from_provider("call-1"),
            "read".to_string(),
            serde_json::json!({"path": "/workspace/src/inline.rs"}),
        )];
        let frame = render_view(ViewportInput {
            terminal_width: 80,
            terminal_height: 8,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            status_header: "Working",
            elapsed: "1s",
            turn_activity: None,
            context_tokens: None,
            context_limit: None,
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(row_text(&frame.buffer, 0), "• Read inline.rs");
        assert_eq!(frame.buffer.cell((0, 0)).expect("bullet").fg, Color::Cyan);
    }
}
