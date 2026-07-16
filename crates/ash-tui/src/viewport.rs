use std::path::Path;

use ash_core::SessionSummary;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Padding, Widget},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    block_layout::{layout_stack, stack_height, StackBoundary, StackFlow, StackItem},
    markdown::RenderedLine,
    palette::Rgb,
    slash_command::CommandCompletion,
    status_line::{compact_path, fit_status_left},
};

const STATUS_ROWS: u16 = 1;
const COMPOSER_ROWS: u16 = 3;
const FOOTER_ROWS: u16 = 1;
const SESSION_MENU_MAX_ROWS: usize = 8;
const COMPOSER_PADDING: Padding = Padding::new(0, 0, 1, 1);

pub(crate) struct ViewportInput<'a> {
    pub(crate) terminal_width: u16,
    pub(crate) terminal_height: u16,
    pub(crate) history_boundary: StackBoundary,
    pub(crate) busy: bool,
    pub(crate) active_start: usize,
    pub(crate) active_lines: &'a [RenderedLine],
    pub(crate) status_header: &'a str,
    pub(crate) status_dots: &'a str,
    pub(crate) elapsed: &'a str,
    pub(crate) queued: &'a str,
    pub(crate) prompt: &'a str,
    pub(crate) prompt_cursor_column: u16,
    pub(crate) composer_background: Option<Rgb>,
    pub(crate) command_menu: &'a [CommandCompletion],
    pub(crate) command_menu_selected: usize,
    pub(crate) session_menu: &'a [SessionSummary],
    pub(crate) session_menu_selected: usize,
    pub(crate) model: &'a str,
    pub(crate) protocol: &'a str,
    pub(crate) working_dir: &'a Path,
}

pub(crate) struct ViewportFrame {
    pub(crate) buffer: Buffer,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_column: u16,
    pub(crate) total_rows: u16,
}

pub(crate) fn render(input: ViewportInput<'_>) -> ViewportFrame {
    let width = input.terminal_width.saturating_sub(1).max(1);
    let menu_rows = if input.session_menu.is_empty() {
        command_menu_rows(input.command_menu.len())
    } else {
        session_menu_rows(input.session_menu.len())
    };
    let active = active_window(
        input.active_start,
        input.active_lines,
        input.terminal_height,
        input.history_boundary,
        input.busy,
        menu_rows,
    );
    let regions = viewport_regions(active.as_ref(), input.busy, menu_rows);
    let items = regions.iter().map(|region| region.item).collect::<Vec<_>>();
    let layout = layout_stack(width, input.history_boundary, &items);
    let area = Rect::new(0, 0, width, layout.height);
    let mut buffer = Buffer::empty(area);
    let mut composer_input_area = Rect::default();

    for (region, area) in regions.iter().zip(&layout.areas) {
        match region.kind {
            ViewportRegion::Active => {
                if let Some(active) = &active {
                    render_active(*area, active, &mut buffer);
                }
            }
            ViewportRegion::Status => render_status(*area, &input, &mut buffer),
            ViewportRegion::Composer => {
                let (input_area, bottom_area) = composer_areas(*area, menu_rows);
                composer_input_area = input_area;
                render_composer(input_area, &input, &mut buffer);
                if !input.session_menu.is_empty() {
                    render_session_menu(bottom_area, &input, &mut buffer);
                } else if input.command_menu.is_empty() {
                    render_footer(bottom_area, &input, &mut buffer);
                } else {
                    render_command_menu(bottom_area, &input, &mut buffer);
                }
            }
        }
    }

    ViewportFrame {
        buffer,
        cursor_row: composer_input_area.y.saturating_add(COMPOSER_PADDING.top),
        cursor_column: 2_u16
            .saturating_add(input.prompt_cursor_column)
            .min(input.terminal_width.saturating_sub(1)),
        total_rows: layout.height,
    }
}

struct ActiveWindow<'a> {
    start: usize,
    lines: &'a [RenderedLine],
    flow: StackFlow,
}

fn active_window<'a>(
    active_start: usize,
    active_lines: &'a [RenderedLine],
    terminal_height: u16,
    history_boundary: StackBoundary,
    busy: bool,
    menu_rows: u16,
) -> Option<ActiveWindow<'a>> {
    if active_lines.is_empty() {
        return None;
    }

    let starts_block = active_start == 0;
    let initial_flow = if starts_block {
        StackFlow::Block
    } else {
        StackFlow::Continuation
    };
    let initial_overhead = active_overhead(history_boundary, initial_flow, busy, menu_rows);
    let flow = if starts_block
        && active_lines
            .len()
            .saturating_add(usize::from(initial_overhead))
            <= usize::from(terminal_height)
    {
        StackFlow::Block
    } else {
        StackFlow::Continuation
    };
    let overhead = active_overhead(history_boundary, flow, busy, menu_rows);
    let max_lines = usize::from(terminal_height.saturating_sub(overhead));
    let skip = active_lines.len().saturating_sub(max_lines);
    Some(ActiveWindow {
        start: active_start + skip,
        lines: &active_lines[skip..],
        flow,
    })
}

fn render_active(area: Rect, active: &ActiveWindow<'_>, buffer: &mut Buffer) {
    let mut y = area.y;
    for (offset, rendered) in active.lines.iter().enumerate() {
        if y >= area.bottom() {
            break;
        }
        let index = active.start + offset;
        let mut spans = vec![if index == 0 {
            Span::styled("• ", Style::default().add_modifier(Modifier::DIM))
        } else {
            Span::raw("  ")
        }];
        spans.extend(rendered.ratatui_line().spans);
        buffer.set_line(area.x, y, &Line::from(spans), area.width);
        y = y.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewportRegion {
    Active,
    Status,
    Composer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegionSpec {
    kind: ViewportRegion,
    item: StackItem,
}

fn active_overhead(
    history_boundary: StackBoundary,
    flow: StackFlow,
    busy: bool,
    menu_rows: u16,
) -> u16 {
    let active = ActiveWindow {
        start: 0,
        lines: &[],
        flow,
    };
    let regions = viewport_regions(Some(&active), busy, menu_rows);
    let items = regions.iter().map(|region| region.item).collect::<Vec<_>>();
    stack_height(history_boundary, &items)
}

fn viewport_regions(
    active: Option<&ActiveWindow<'_>>,
    busy: bool,
    menu_rows: u16,
) -> Vec<RegionSpec> {
    let mut regions = Vec::with_capacity(4);
    if let Some(active) = active {
        let height = u16::try_from(active.lines.len()).unwrap_or(u16::MAX);
        let item = match active.flow {
            StackFlow::Block => StackItem::block(height),
            StackFlow::Continuation => StackItem::continuation(height),
        };
        regions.push(RegionSpec {
            kind: ViewportRegion::Active,
            item,
        });
    }
    if busy {
        regions.push(RegionSpec {
            kind: ViewportRegion::Status,
            item: StackItem::block(STATUS_ROWS),
        });
    }
    regions.push(RegionSpec {
        kind: ViewportRegion::Composer,
        item: StackItem::block(composer_block_rows(menu_rows)),
    });
    regions
}

fn composer_block_rows(menu_rows: u16) -> u16 {
    COMPOSER_ROWS.saturating_add(if menu_rows == 0 {
        FOOTER_ROWS
    } else {
        menu_rows
    })
}

fn composer_areas(area: Rect, menu_rows: u16) -> (Rect, Rect) {
    let bottom_rows = if menu_rows == 0 {
        FOOTER_ROWS
    } else {
        menu_rows
    };
    let areas = Layout::vertical([
        Constraint::Length(COMPOSER_ROWS),
        Constraint::Length(bottom_rows),
    ])
    .flex(Flex::Start)
    .spacing(0)
    .split(area);
    (areas[0], areas[1])
}

fn render_status(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let line = if area.width < 32 {
        Line::from(vec![
            Span::styled(
                format!("{}{}", input.status_header, input.status_dots),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                input.queued.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
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
                format!(" ({} • esc to interrupt){}", input.elapsed, input.queued),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    };
    buffer.set_line(area.x, area.y, &line, area.width);
}

fn render_composer(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    let background = input
        .composer_background
        .map_or(Color::Reset, |(r, g, b)| Color::Rgb(r, g, b));
    let block = Block::default()
        .style(Style::default().bg(background))
        .padding(COMPOSER_PADDING);
    let inner = block.inner(area);
    block.render(area, buffer);
    if inner.is_empty() {
        return;
    }
    let line = Line::from(vec![
        Span::styled("›", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(input.prompt.to_string()),
    ]);
    buffer.set_line(inner.x, inner.y, &line, inner.width);
}

fn render_footer(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let path = compact_path(input.working_dir);
    let protocol_width = UnicodeWidthStr::width(input.protocol) as u16;
    let right_x = area.width.saturating_sub(protocol_width.saturating_add(2));
    let show_protocol = protocol_width > 0 && right_x > 3;
    let left_width = if show_protocol {
        right_x.saturating_sub(3)
    } else {
        area.width.saturating_sub(4)
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
    if show_protocol {
        buffer.set_line(
            area.x.saturating_add(right_x),
            area.y,
            &Line::styled(input.protocol.to_string(), Style::default().fg(Color::Cyan)),
            protocol_width,
        );
    }
}

fn render_command_menu(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    let name_width = input
        .command_menu
        .iter()
        .map(|item| UnicodeWidthStr::width(item.name))
        .max()
        .unwrap_or(1);
    let description_column = 3usize.saturating_add(name_width).saturating_add(2);
    for (index, item) in input.command_menu.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        let y = area.y.saturating_add(offset);
        if y >= area.bottom() {
            break;
        }
        let mut spans = if index == input.command_menu_selected {
            vec![Span::styled(
                format!("› /{:<name_width$}", item.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]
        } else {
            vec![Span::raw(format!("  /{:<name_width$}", item.name))]
        };
        if usize::from(area.width) > description_column {
            let available = usize::from(area.width) - description_column;
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                fit_menu_text(item.description, available),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        buffer.set_line(area.x, y, &Line::from(spans), area.width);
    }
}

fn render_session_menu(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    let visible = usize::from(area.height).min(input.session_menu.len());
    if visible == 0 {
        return;
    }
    let selected = input
        .session_menu_selected
        .min(input.session_menu.len().saturating_sub(1));
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(input.session_menu.len().saturating_sub(visible));
    for (offset, session) in input.session_menu[start..start + visible]
        .iter()
        .enumerate()
    {
        let index = start + offset;
        let y = area
            .y
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        let selected_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let prefix = if index == selected { "› " } else { "  " };
        let prefix_style = if index == selected {
            selected_style
        } else {
            Style::default()
        };
        let created_width = UnicodeWidthStr::width(session.created_at.as_str());
        let show_created = usize::from(area.width) > created_width.saturating_add(8);
        let title_width = if show_created {
            usize::from(area.width)
                .saturating_sub(2)
                .saturating_sub(created_width)
                .saturating_sub(2)
        } else {
            usize::from(area.width).saturating_sub(2)
        };
        let title = fit_menu_text(&session.title, title_width);
        let title_used = UnicodeWidthStr::width(title.as_str());
        let mut spans = vec![
            Span::styled(prefix, prefix_style),
            Span::styled(
                title,
                if index == selected {
                    selected_style
                } else {
                    Style::default()
                },
            ),
        ];
        if show_created {
            let spacing = usize::from(area.width)
                .saturating_sub(2)
                .saturating_sub(title_used)
                .saturating_sub(created_width);
            spans.push(Span::raw(" ".repeat(spacing)));
            spans.push(Span::styled(
                session.created_at.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        buffer.set_line(area.x, y, &Line::from(spans), area.width);
    }
}

pub(crate) fn command_menu_rows(item_count: usize) -> u16 {
    u16::try_from(item_count).unwrap_or(u16::MAX)
}

pub(crate) fn session_menu_rows(item_count: usize) -> u16 {
    u16::try_from(item_count.min(SESSION_MENU_MAX_ROWS)).unwrap_or(u16::MAX)
}

pub(crate) fn fit_menu_text(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    let available = width.saturating_sub(1);
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ash_core::SessionId;

    use super::*;
    use crate::{markdown::render_markdown, slash_command::SlashCommand};

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| !cell.skip)
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn welcome_boundary() -> StackBoundary {
        StackBoundary::default().after_block()
    }

    fn user_boundary() -> StackBoundary {
        StackBoundary::default().after_block()
    }

    #[test]
    fn lays_out_active_status_composer_and_footer_once() {
        let active = render_markdown("answer", 80);
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            history_boundary: user_boundary(),
            busy: true,
            active_start: 0,
            active_lines: &active,
            status_header: "Working",
            status_dots: "...",
            elapsed: "2s",
            queued: "",
            prompt: "draft",
            prompt_cursor_column: 5,
            composer_background: Some((30, 30, 30)),
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(frame.total_rows, 9);
        assert_eq!(row_text(&frame.buffer, 1), "• answer");
        assert!(row_text(&frame.buffer, 3).contains("Working..."));
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
    }

    #[test]
    fn completion_menu_replaces_the_footer_without_changing_composer_spacing() {
        let menu = [CommandCompletion {
            command: SlashCommand::Clear,
            name: "clear",
            description: "start a new chat",
        }];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            history_boundary: welcome_boundary(),
            busy: false,
            active_start: 0,
            active_lines: &[],
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            queued: "",
            prompt: "/cl",
            prompt_cursor_column: 3,
            composer_background: None,
            command_menu: &menu,
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(frame.total_rows, 5);
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› /cl");
        assert!(row_text(&frame.buffer, frame.total_rows - 1).contains("/clear"));
    }

    #[test]
    fn session_menu_shows_the_title_and_creation_time() {
        let sessions = [SessionSummary {
            session_id: SessionId::new(),
            title: "Inspect the session picker".to_string(),
            created_at: "2026-07-15 12:30".to_string(),
        }];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            history_boundary: welcome_boundary(),
            busy: false,
            active_start: 0,
            active_lines: &[],
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            queued: "",
            prompt: "",
            prompt_cursor_column: 0,
            composer_background: None,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &sessions,
            session_menu_selected: 0,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
        });

        let menu = row_text(&frame.buffer, frame.total_rows - 1);
        assert!(menu.contains("Inspect the session picker"));
        assert!(menu.contains("2026-07-15 12:30"));
        assert_eq!(session_menu_rows(20), 8);
    }

    #[test]
    fn flex_owns_the_gap_before_the_first_active_block() {
        let active = render_markdown("Thinking (0s)", 80);
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            history_boundary: user_boundary(),
            busy: true,
            active_start: 0,
            active_lines: &active,
            status_header: "Thinking",
            status_dots: "...",
            elapsed: "0s",
            queued: "",
            prompt: "",
            prompt_cursor_column: 0,
            composer_background: None,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            protocol: "openai",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(row_text(&frame.buffer, 1), "• Thinking (0s)");
    }
}
