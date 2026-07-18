use std::path::Path;

use ash_core::SessionSummary;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    block_layout::{layout_stack, stack_height, StackItem},
    live_block::LiveBlock,
    markdown::RenderedLine,
    slash_command::CommandCompletion,
    status_line::prompt_header_line,
    text_width::truncate_end,
};

const STATUS_ROWS: u16 = 1;
const PROMPT_HEADER_ROWS: u16 = 1;
const COMPOSER_ROWS: u16 = 1;
const SESSION_MENU_MAX_ROWS: usize = 8;
const COMPACT_STATUS_WIDTH: u16 = 32;
const COMMAND_NAME_PREFIX_COLUMNS: usize = 3;
const MENU_COLUMN_GAP: usize = 2;
const MENU_PREFIX_COLUMNS: usize = 2;
const SESSION_CREATED_MIN_LEFT_COLUMNS: usize = 8;
pub(crate) const COMPOSER_TEXT_COLUMN: u16 = 2;

pub(crate) fn drawable_width(terminal_width: u16) -> u16 {
    // Keep the final column free because writing into it can trigger an automatic wrap.
    terminal_width.saturating_sub(1).max(1)
}

#[derive(Clone, Copy)]
pub(crate) struct ViewportInput<'a> {
    pub(crate) terminal_width: u16,
    pub(crate) terminal_height: u16,
    pub(crate) pending_blocks: &'a [LiveBlock],
    pub(crate) busy: bool,
    pub(crate) active_lines: &'a [RenderedLine],
    pub(crate) status_header: &'a str,
    pub(crate) status_dots: &'a str,
    pub(crate) elapsed: &'a str,
    pub(crate) queued: &'a str,
    pub(crate) prompt: &'a str,
    pub(crate) prompt_cursor_column: u16,
    pub(crate) command_menu: &'a [CommandCompletion],
    pub(crate) command_menu_selected: usize,
    pub(crate) session_menu: &'a [SessionSummary],
    pub(crate) session_menu_selected: usize,
    pub(crate) model: &'a str,
    pub(crate) working_dir: &'a Path,
}

pub(crate) struct ViewportFrame {
    pub(crate) buffer: Buffer,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_column: u16,
}

pub(crate) fn render(input: ViewportInput<'_>) -> ViewportFrame {
    let width = drawable_width(input.terminal_width);
    let menu_rows = if input.session_menu.is_empty() {
        command_menu_rows(input.command_menu.len())
    } else {
        session_menu_rows(input.session_menu.len())
    };
    let pending = input
        .pending_blocks
        .iter()
        .map(|block| RenderedPendingBlock {
            buffer: block.render(width),
        })
        .collect::<Vec<_>>();
    let active = active_window(
        input.active_lines,
        input.terminal_height,
        &pending,
        input.busy,
        menu_rows,
    );
    let active_rows = active
        .as_ref()
        .map(|active| u16::try_from(active.lines.len()).unwrap_or(u16::MAX));
    let regions = viewport_regions(&pending, active_rows, input.busy, menu_rows);
    let items = regions.iter().map(|region| region.item).collect::<Vec<_>>();
    let layout = layout_stack(width, &items);
    let area = Rect::new(0, 0, width, layout.height);
    let mut buffer = Buffer::empty(area);
    let mut composer_input_area = Rect::default();

    for (region, area) in regions.iter().zip(&layout.areas) {
        match region.kind {
            ViewportRegion::Pending(index) => {
                if let Some(block) = pending.get(index) {
                    blit_buffer(&mut buffer, *area, &block.buffer);
                }
            }
            ViewportRegion::Active => {
                if let Some(active) = &active {
                    render_active(*area, active, &mut buffer);
                }
            }
            ViewportRegion::Status => render_status(*area, &input, &mut buffer),
            ViewportRegion::Composer => {
                let (header_area, input_area, menu_area) = composer_areas(*area, menu_rows);
                composer_input_area = input_area;
                render_prompt_header(header_area, &input, &mut buffer);
                render_composer(input_area, &input, &mut buffer);
                if !input.session_menu.is_empty() {
                    render_session_menu(menu_area, &input, &mut buffer);
                } else if !input.command_menu.is_empty() {
                    render_command_menu(menu_area, &input, &mut buffer);
                }
            }
        }
    }

    ViewportFrame {
        buffer,
        cursor_row: composer_input_area.y,
        cursor_column: COMPOSER_TEXT_COLUMN
            .saturating_add(input.prompt_cursor_column)
            .min(input.terminal_width.saturating_sub(1)),
    }
}

struct ActiveWindow<'a> {
    start: usize,
    lines: &'a [RenderedLine],
}

struct RenderedPendingBlock {
    buffer: Buffer,
}

fn active_window<'a>(
    active_lines: &'a [RenderedLine],
    terminal_height: u16,
    pending: &[RenderedPendingBlock],
    busy: bool,
    menu_rows: u16,
) -> Option<ActiveWindow<'a>> {
    if active_lines.is_empty() {
        return None;
    }

    let overhead = active_overhead(pending, busy, menu_rows);
    let max_lines = usize::from(terminal_height.saturating_sub(overhead));
    let skip = active_lines.len().saturating_sub(max_lines);
    Some(ActiveWindow {
        start: skip,
        lines: &active_lines[skip..],
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
    Pending(usize),
    Active,
    Status,
    Composer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegionSpec {
    kind: ViewportRegion,
    item: StackItem,
}

fn active_overhead(pending: &[RenderedPendingBlock], busy: bool, menu_rows: u16) -> u16 {
    let regions = viewport_regions(pending, Some(0), busy, menu_rows);
    let items = regions.iter().map(|region| region.item).collect::<Vec<_>>();
    stack_height(&items)
}

fn viewport_regions(
    pending: &[RenderedPendingBlock],
    active_rows: Option<u16>,
    busy: bool,
    menu_rows: u16,
) -> Vec<RegionSpec> {
    let mut regions = Vec::with_capacity(pending.len().saturating_add(3));
    for (index, block) in pending.iter().enumerate() {
        regions.push(RegionSpec {
            kind: ViewportRegion::Pending(index),
            item: StackItem::block(block.buffer.area.height),
        });
    }
    if let Some(height) = active_rows {
        regions.push(RegionSpec {
            kind: ViewportRegion::Active,
            item: StackItem::block(height),
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

fn blit_buffer(destination: &mut Buffer, area: Rect, source: &Buffer) {
    let height = area.height.min(source.area.height);
    let width = area.width.min(source.area.width);
    for y in 0..height {
        for x in 0..width {
            let Some(cell) = source.cell((source.area.x + x, source.area.y + y)) else {
                continue;
            };
            *destination
                .cell_mut((area.x + x, area.y + y))
                .expect("in bounds") = cell.clone();
        }
    }
}

fn composer_block_rows(menu_rows: u16) -> u16 {
    PROMPT_HEADER_ROWS
        .saturating_add(COMPOSER_ROWS)
        .saturating_add(menu_rows)
}

fn composer_areas(area: Rect, menu_rows: u16) -> (Rect, Rect, Rect) {
    let areas = Layout::vertical([
        Constraint::Length(PROMPT_HEADER_ROWS),
        Constraint::Length(COMPOSER_ROWS),
        Constraint::Length(menu_rows),
    ])
    .flex(Flex::Start)
    .spacing(0)
    .split(area);
    (areas[0], areas[1], areas[2])
}

fn render_status(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let line = if area.width < COMPACT_STATUS_WIDTH {
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
    if area.is_empty() {
        return;
    }
    let line = Line::from(vec![
        Span::styled("›", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(input.prompt.to_string()),
    ]);
    buffer.set_line(area.x, area.y, &line, area.width);
}

fn render_prompt_header(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    buffer.set_line(
        area.x,
        area.y,
        &prompt_header_line(input.model, input.working_dir, area.width),
        area.width,
    );
}

fn render_command_menu(area: Rect, input: &ViewportInput<'_>, buffer: &mut Buffer) {
    let name_width = input
        .command_menu
        .iter()
        .map(|item| UnicodeWidthStr::width(item.name))
        .max()
        .unwrap_or(1);
    let description_column = COMMAND_NAME_PREFIX_COLUMNS
        .saturating_add(name_width)
        .saturating_add(MENU_COLUMN_GAP);
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
                truncate_end(item.description, available),
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
                .saturating_sub(MENU_PREFIX_COLUMNS)
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ash_core::SessionId;

    use super::*;
    use crate::markdown::render_markdown;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| !cell.skip)
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn lays_out_active_status_and_prompt_once() {
        let active = render_markdown("answer", 80);
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            pending_blocks: &[],
            busy: true,
            active_lines: &active,
            status_header: "Working",
            status_dots: "...",
            elapsed: "2s",
            queued: "",
            prompt: "draft",
            prompt_cursor_column: 5,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(row_text(&frame.buffer, 0), "• answer");
        assert!(row_text(&frame.buffer, 2).contains("Working..."));
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› draft");
    }

    #[test]
    fn completion_menu_follows_the_prompt_without_extra_spacing() {
        let menu = [CommandCompletion {
            name: "clear",
            description: "start a new chat",
        }];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            pending_blocks: &[],
            busy: false,
            active_lines: &[],
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            queued: "",
            prompt: "/cl",
            prompt_cursor_column: 3,
            command_menu: &menu,
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "› /cl");
        assert!(row_text(&frame.buffer, frame.buffer.area.height - 1).contains("/clear"));
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
            pending_blocks: &[],
            busy: false,
            active_lines: &[],
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            queued: "",
            prompt: "",
            prompt_cursor_column: 0,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &sessions,
            session_menu_selected: 0,
            model: "mock",
            working_dir: Path::new("/tmp/ash"),
        });

        let menu = row_text(&frame.buffer, frame.buffer.area.height - 1);
        assert!(menu.contains("Inspect the session picker"));
        assert!(menu.contains("2026-07-15 12:30"));
        assert_eq!(session_menu_rows(20), 8);
    }

    #[test]
    fn active_output_starts_at_the_top_of_the_local_frame() {
        let active = render_markdown("Thinking (0s)", 80);
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            pending_blocks: &[],
            busy: true,
            active_lines: &active,
            status_header: "Thinking",
            status_dots: "...",
            elapsed: "0s",
            queued: "",
            prompt: "",
            prompt_cursor_column: 0,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(row_text(&frame.buffer, 0), "• Thinking (0s)");
    }

    #[test]
    fn pending_blocks_share_the_local_frame_with_the_composer() {
        let blocks = [LiveBlock::history(
            1,
            crate::history_block::HistoryBlock::info("restored output"),
        )];
        let frame = render(ViewportInput {
            terminal_width: 80,
            terminal_height: 24,
            pending_blocks: &blocks,
            busy: false,
            active_lines: &[],
            status_header: "",
            status_dots: "",
            elapsed: "0s",
            queued: "",
            prompt: "",
            prompt_cursor_column: 0,
            command_menu: &[],
            command_menu_selected: 0,
            session_menu: &[],
            session_menu_selected: 0,
            model: "mock",
            working_dir: Path::new("/tmp/ash"),
        });

        assert_eq!(row_text(&frame.buffer, 0), "• restored output");
        assert_eq!(row_text(&frame.buffer, frame.cursor_row), "›");
    }
}
