use std::{path::Path, sync::Arc};

use ash_core::{ForkPoint, SessionSummary};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    block_layout::{layout_stack, StackItem},
    live_block::LiveBlock,
    menu::MenuView,
    scrollback::sanitize_single_line,
    slash_command::CommandCompletion,
    status_line::{compact_path, fit_status_left, format_token_count},
    text_width::truncate_end,
    SubagentView, SubagentViewState,
};

const FOOTER_ROWS: u16 = 1;
const MAX_COMPOSER_ROWS: u16 = 8;
const MENU_MAX_ROWS: usize = 8;
const SCREEN_SPACING: u16 = 1;
const COMPACT_STATUS_WIDTH: u16 = 32;
const SUBAGENTS_MAX_ROWS: usize = 4;
const FOOTER_SIDE_PADDING: u16 = 2;
const FOOTER_COLUMN_GAP: u16 = 3;
const FOOTER_MIN_LEFT_WIDTH: u16 = 3;
const CONTEXT_BAR_COLUMNS: usize = 8;
const COMMAND_NAME_PREFIX_COLUMNS: usize = 3;
const MENU_COLUMN_GAP: usize = 2;
const MENU_PREFIX_COLUMNS: usize = 2;
const SESSION_CREATED_MIN_LEFT_COLUMNS: usize = 8;
pub const COMPOSER_TEXT_COLUMN: u16 = 2;

pub fn drawable_width(terminal_width: u16) -> u16 {
    // Keep the final column free because writing into it can trigger an automatic wrap.
    terminal_width.saturating_sub(1).max(1)
}

#[derive(Clone, Copy)]
pub struct ViewportInput<'a> {
    pub(crate) terminal_width: u16,
    pub(crate) terminal_height: u16,
    pub(crate) transcript: &'a [LiveBlock],
    pub(crate) scroll_top: Option<u16>,
    pub(crate) busy: bool,
    pub(crate) interruptible: bool,
    pub(crate) status_header: &'a str,
    pub(crate) status_dots: &'a str,
    pub(crate) elapsed: &'a str,
    pub(crate) prompt_lines: &'a [String],
    pub(crate) prompt_cursor_row: u16,
    pub(crate) prompt_cursor_column: u16,
    pub(crate) menu: MenuView<'a>,
    pub(crate) model: &'a str,
    pub(crate) protocol: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) context_tokens: Option<u64>,
    pub(crate) context_limit: Option<u64>,
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

pub fn render(input: ViewportInput<'_>) -> ViewportFrame {
    let terminal_width = input.terminal_width.max(1);
    let terminal_height = input.terminal_height.max(1);
    let width = drawable_width(input.terminal_width);
    let requested_composer_rows = u16::try_from(input.prompt_lines.len())
        .unwrap_or(u16::MAX)
        .clamp(1, MAX_COMPOSER_ROWS);

    let rendered_blocks = input
        .transcript
        .iter()
        .map(|block| block.render(width, input.tools_expanded))
        .collect::<Vec<_>>();
    let items = rendered_blocks
        .iter()
        .map(|block| StackItem::block(block.area.height))
        .collect::<Vec<_>>();
    let layout = layout_stack(width, &items);
    let menu_rows = u16::try_from(input.menu.item_count().min(MENU_MAX_ROWS)).unwrap_or(u16::MAX);
    let active_subagents = input
        .subagents
        .iter()
        .filter(|subagent| subagent.state.is_active())
        .collect::<Vec<_>>();
    let (screen_rows, fitted_height) = fit_screen_rows(
        ScreenRows {
            transcript: layout.height,
            status: u16::from(input.busy),
            composer: requested_composer_rows,
            menu: menu_rows,
            subagents: subagent_rows(&active_subagents),
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
        &rendered_blocks,
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
        render_subagents(screen.subagents, &active_subagents, &mut buffer);
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
enum ScreenRegion {
    Transcript,
    Status,
    Composer,
    Subagents,
    Footer,
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
    let mut regions = Vec::with_capacity(5);
    let mut constraints = Vec::with_capacity(5);
    for (region, rows) in [
        (ScreenRegion::Transcript, rows.transcript),
        (ScreenRegion::Status, rows.status),
        (
            ScreenRegion::Composer,
            rows.composer.saturating_add(rows.menu),
        ),
        (ScreenRegion::Subagents, rows.subagents),
        (ScreenRegion::Footer, rows.footer),
    ] {
        if rows > 0 {
            regions.push(region);
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
    for (region, area) in regions.into_iter().zip(layout.iter().copied()) {
        match region {
            ScreenRegion::Transcript => areas.transcript = area,
            ScreenRegion::Status => areas.status = area,
            ScreenRegion::Composer if rows.menu > 0 => {
                let input_surface = Layout::vertical([
                    Constraint::Length(rows.composer),
                    Constraint::Length(rows.menu),
                ])
                .split(area);
                areas.composer = input_surface[0];
                areas.menu = input_surface[1];
            }
            ScreenRegion::Composer => areas.composer = area,
            ScreenRegion::Subagents => areas.subagents = area,
            ScreenRegion::Footer => areas.footer = area,
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
    blocks: &[Arc<Buffer>],
    scroll_top: u16,
    visible_rows: u16,
    buffer: &mut Buffer,
) {
    let visible_bottom = scroll_top.saturating_add(visible_rows);
    for (block, area) in blocks.iter().zip(areas) {
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
        crate::buffer::copy_rows(block, buffer, source_y, target);
    }
}

fn subagent_rows(subagents: &[&SubagentView]) -> u16 {
    if subagents.is_empty() {
        return 0;
    }
    let rows = u16::try_from(subagents.len().min(SUBAGENTS_MAX_ROWS)).unwrap_or(u16::MAX);
    rows.saturating_add(u16::from(subagents.len() > SUBAGENTS_MAX_ROWS))
}

fn render_subagents(area: Rect, subagents: &[&SubagentView], buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let visible = subagents.len().min(SUBAGENTS_MAX_ROWS);
    for (index, subagent) in subagents.iter().take(visible).enumerate() {
        let state_symbol = subagent_state_symbol(subagent.state);
        let state_color = subagent_state_color(subagent.state);
        let message = sanitize_single_line(&subagent.last_message);
        let line = Line::from(vec![
            Span::styled(
                format!("{state_symbol} "),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                subagent.name.as_str(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" (", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(subagent.profile.as_str(), Style::default().fg(state_color)),
            Span::styled(")", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(" · ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(message),
            Span::styled(" (", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                subagent_state_label(subagent.state),
                Style::default().fg(state_color),
            ),
            Span::styled(")", Style::default().add_modifier(Modifier::DIM)),
        ]);
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
                format!("  … and {more} more"),
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

const fn subagent_state_label(state: SubagentViewState) -> &'static str {
    match state {
        SubagentViewState::Idle => "idle",
        SubagentViewState::Running => "running",
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
    let line = if area.width < COMPACT_STATUS_WIDTH {
        Line::from(Span::styled(
            format!("{}{}", input.status_header, input.status_dots),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                input.status_header.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                input.status_dots.to_string(),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                if input.interruptible {
                    format!(" ({} • esc to interrupt)", input.elapsed)
                } else {
                    format!(" ({})", input.elapsed)
                },
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    };
    buffer.set_line(area.x, area.y, &line, area.width);
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
    let protocol = (!input.protocol.is_empty()).then_some(input.protocol);
    let detailed_context = context_display(
        input.context_tokens,
        input.context_limit,
        ContextDisplayMode::Detailed,
    );
    let compact_context = context_display(
        input.context_tokens,
        input.context_limit,
        ContextDisplayMode::Compact,
    );
    let candidates = [
        (detailed_context.clone(), protocol),
        (compact_context.clone(), protocol),
        (detailed_context, None),
        (compact_context, None),
        (None, protocol),
    ];
    let (context, protocol) = candidates
        .into_iter()
        .find(|(context, protocol)| footer_right_fits(area, context.as_ref(), *protocol))
        .unwrap_or((None, None));
    let right_width = footer_right_width(context.as_ref(), protocol);
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
    if right_width > 0 {
        let mut spans = Vec::new();
        let has_context = context.is_some();
        if let Some(context) = context {
            spans.push(Span::styled(
                context.text,
                Style::default().fg(context.color),
            ));
        }
        if has_context && protocol.is_some() {
            spans.push(Span::styled(
                " · ",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        if let Some(protocol) = protocol {
            spans.push(Span::styled(protocol, Style::default().fg(Color::Cyan)));
        }
        let right_x = area.x.saturating_add(
            area.width
                .saturating_sub(right_width.saturating_add(FOOTER_SIDE_PADDING)),
        );
        buffer.set_line(right_x, area.y, &Line::from(spans), right_width);
    }
}

#[derive(Clone)]
struct ContextDisplay {
    text: String,
    color: Color,
}

#[derive(Clone, Copy)]
enum ContextDisplayMode {
    Detailed,
    Compact,
}

fn context_display(
    context_tokens: Option<u64>,
    context_limit: Option<u64>,
    mode: ContextDisplayMode,
) -> Option<ContextDisplay> {
    let tokens = context_tokens?;
    let Some(limit) = context_limit.filter(|limit| *limit > 0) else {
        return Some(ContextDisplay {
            text: format!("ctx {}", format_token_count(tokens)),
            color: Color::Green,
        });
    };
    let percent = tokens.saturating_mul(100) / limit;
    let percent_text = if percent > 100 {
        "100%+".to_string()
    } else {
        format!("{percent}%")
    };
    let color = match percent {
        0..=69 => Color::Green,
        70..=84 => Color::Yellow,
        _ => Color::Red,
    };
    let text = match mode {
        ContextDisplayMode::Detailed => {
            let filled = usize::try_from(
                tokens
                    .saturating_mul(CONTEXT_BAR_COLUMNS as u64)
                    .saturating_add(limit.saturating_sub(1))
                    / limit,
            )
            .unwrap_or(CONTEXT_BAR_COLUMNS)
            .min(CONTEXT_BAR_COLUMNS);
            format!(
                "ctx {}{} {percent_text}",
                "█".repeat(filled),
                "░".repeat(CONTEXT_BAR_COLUMNS - filled),
            )
        }
        ContextDisplayMode::Compact => format!("ctx {percent_text}"),
    };
    Some(ContextDisplay { text, color })
}

fn footer_right_fits(area: Rect, context: Option<&ContextDisplay>, protocol: Option<&str>) -> bool {
    let right_width = footer_right_width(context, protocol);
    right_width > 0
        && area.width
            >= right_width
                .saturating_add(FOOTER_SIDE_PADDING)
                .saturating_add(FOOTER_COLUMN_GAP)
                .saturating_add(FOOTER_MIN_LEFT_WIDTH)
}

fn footer_right_width(context: Option<&ContextDisplay>, protocol: Option<&str>) -> u16 {
    let context_width = context.map_or(0, |context| {
        u16::try_from(UnicodeWidthStr::width(context.text.as_str())).unwrap_or(u16::MAX)
    });
    let protocol_width = protocol.map_or(0, |protocol| {
        u16::try_from(UnicodeWidthStr::width(protocol)).unwrap_or(u16::MAX)
    });
    context_width
        .saturating_add(protocol_width)
        .saturating_add(u16::from(context_width > 0 && protocol_width > 0) * 3)
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

fn render_fork_menu(area: Rect, points: &[ForkPoint], selected: usize, buffer: &mut Buffer) {
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

    use ash_core::{MessageId, SessionId};

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
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            interruptible: true,
            status_header: "Working",
            status_dots: "...",
            elapsed: "2s",
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(frame.buffer.area, Rect::new(0, 0, 80, 24));
        assert_eq!(frame.viewport_height, 7);
        assert_eq!(row_text(&frame.buffer, 0), "• answer");
        assert!(row_text(&frame.buffer, 2).contains("Working..."));
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
    fn subagents_render_below_the_composer_and_above_the_footer() {
        let blocks = [LiveBlock::assistant(1, "answer".to_string())];
        let subagents = [
            SubagentView {
                name: "inspect_glob".to_string(),
                profile: "explorer".to_string(),
                state: SubagentViewState::Running,
                last_message: "Inspect the glob API".to_string(),
            },
            SubagentView {
                name: "fix_bash".to_string(),
                profile: "worker".to_string(),
                state: SubagentViewState::Running,
                last_message: "Add cwd to bash".to_string(),
            },
        ];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            interruptible: true,
            status_header: "Working",
            status_dots: "...",
            elapsed: "2s",
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &subagents,
        });

        assert_eq!(frame.viewport_height, 10);
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
        assert!(row_text(&frame.buffer, 6).contains("inspect_glob"));
        assert!(row_text(&frame.buffer, 6).contains("explorer"));
        assert!(row_text(&frame.buffer, 6).contains("Inspect the glob API"));
        assert!(row_text(&frame.buffer, 6).contains("running"));
        assert!(row_text(&frame.buffer, 7).contains("fix_bash"));
        assert!(row_text(&frame.buffer, 7).contains("worker"));
        assert!(row_text(&frame.buffer, 7).contains("running"));
    }

    #[test]
    fn idle_subagents_do_not_occupy_a_row() {
        let blocks = [LiveBlock::assistant(1, "answer".to_string())];
        let subagents = [SubagentView {
            name: "inspect_glob".to_string(),
            profile: "explorer".to_string(),
            state: SubagentViewState::Idle,
            last_message: "Inspect the glob API".to_string(),
        }];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &subagents,
        });

        assert_eq!(frame.viewport_height, 5);
        assert!(!row_text(&frame.buffer, 2).contains("explorer"));
        assert!(!row_text(&frame.buffer, 4).contains("explorer"));
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
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &["/".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 1,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &[],
        };
        let baseline = render(input);
        let frame = render(ViewportInput {
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
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &[],
        };
        let baseline = render(input);
        let frame = render(ViewportInput {
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
            ForkPoint {
                message_id: MessageId::new(),
                prompt: "latest prompt\ncontinued".to_string(),
            },
            ForkPoint {
                message_id: MessageId::new(),
                prompt: "older prompt".to_string(),
            },
        ];
        let frame = render(ViewportInput {
            terminal_width: 50,
            terminal_height: 10,
            transcript: &[],
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
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
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: true,
            interruptible: false,
            status_header: "Thinking",
            status_dots: "...",
            elapsed: "0s",
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &prompt,
            prompt_cursor_row: 2,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 40,
            terminal_height: 16,
            transcript: &[],
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &prompt,
            prompt_cursor_row: 9,
            prompt_cursor_column: 6,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 40,
            terminal_height: 10,
            transcript: &blocks,
            scroll_top: None,
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &["draft".to_string()],
            prompt_cursor_row: 0,
            prompt_cursor_column: 5,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
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
        let frame = render(ViewportInput {
            terminal_width: 40,
            terminal_height: 10,
            transcript: &blocks,
            scroll_top: Some(4),
            busy: false,
            interruptible: false,
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            prompt_lines: &[],
            prompt_cursor_row: 0,
            prompt_cursor_column: 0,
            menu: MenuView::None,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
            context_tokens: None,

            context_limit: None,
            tools_expanded: false,
            subagents: &[],
        });

        assert_eq!(frame.scroll_top, 4);
        assert_eq!(row_text(&frame.buffer, 0), "• entry 2");
        assert_eq!(row_text(&frame.buffer, 4), "• entry 4");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "›");
    }

    #[test]
    fn context_display_uses_a_fixed_bar_and_compact_fallback() {
        let detailed = context_display(Some(50), Some(100), ContextDisplayMode::Detailed)
            .expect("detailed meter");
        assert_eq!(detailed.text, "ctx ████░░░░ 50%");
        assert_eq!(detailed.color, Color::Green);

        let compact = context_display(Some(85), Some(100), ContextDisplayMode::Compact)
            .expect("compact meter");
        assert_eq!(compact.text, "ctx 85%");
        assert_eq!(compact.color, Color::Red);

        let unknown = context_display(Some(12_345), None, ContextDisplayMode::Detailed)
            .expect("token display");
        assert_eq!(unknown.text, "ctx 12.3k");
    }
}
