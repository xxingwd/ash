use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    layout::{Position, Rect},
    Terminal, TerminalOptions, Viewport,
};

use crate::viewport::ViewportFrame;

struct FrameWriter {
    stdout: Stdout,
    frame: Option<Vec<u8>>,
}

impl FrameWriter {
    fn new(stdout: Stdout) -> Self {
        Self {
            stdout,
            frame: None,
        }
    }

    fn begin_frame(&mut self) -> io::Result<()> {
        if self.frame.is_some() {
            return Err(io::Error::other("terminal frame is already active"));
        }
        self.frame = Some(Vec::new());
        Ok(())
    }

    fn commit_frame(&mut self) -> io::Result<()> {
        let frame = self
            .frame
            .take()
            .ok_or_else(|| io::Error::other("terminal frame is not active"))?;
        let mut stdout = self.stdout.lock();
        stdout.write_all(&frame)?;
        stdout.flush()
    }
}

impl Write for FrameWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(frame) = &mut self.frame {
            frame.extend_from_slice(buffer);
            Ok(buffer.len())
        } else {
            self.stdout.write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.frame.is_some() {
            Ok(())
        } else {
            self.stdout.flush()
        }
    }
}

type InlineTerminal = Terminal<CrosstermBackend<FrameWriter>>;

/// Owns an inline viewport while preserving the terminal's native scrollback.
pub(crate) struct InlineScreen {
    terminal: InlineTerminal,
    viewport_area: Rect,
    guard: TerminalGuard,
}

impl InlineScreen {
    pub(crate) fn enter() -> io::Result<Self> {
        let guard = TerminalGuard::enter()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;

        let (width, height) = terminal::size()?;
        if width == 0 || height == 0 {
            return Err(io::Error::other("terminal reported a zero-sized viewport"));
        }
        let mut backend = CrosstermBackend::new(FrameWriter::new(stdout));
        let viewport_area = reserve_initial_viewport(&mut backend, width, height)?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(viewport_area),
            },
        )?;
        terminal.clear()?;

        Ok(Self {
            terminal,
            viewport_area,
            guard,
        })
    }

    pub(crate) fn begin_synchronized(&mut self) -> io::Result<()> {
        let writer = self.terminal.backend_mut().writer_mut();
        writer.begin_frame()?;
        queue!(writer, BeginSynchronizedUpdate)
    }

    pub(crate) fn end_synchronized(&mut self) -> io::Result<()> {
        let writer = self.terminal.backend_mut().writer_mut();
        queue!(writer, EndSynchronizedUpdate)?;
        writer.commit_frame()
    }

    pub(crate) fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        let viewport_height = self.viewport_area.height.clamp(1, height);
        let current = Rect::new(
            0,
            self.viewport_area.y.min(height - viewport_height),
            width,
            viewport_height,
        );
        let screen = Rect::new(0, 0, width, height);
        apply_viewport_resize(
            &mut self.terminal,
            current,
            self.viewport_area.intersection(screen),
        )?;
        self.viewport_area = current;
        Ok(())
    }

    pub(crate) fn reset(&mut self) -> io::Result<()> {
        let writer = self.terminal.backend_mut().writer_mut();
        queue!(
            writer,
            Clear(ClearType::Purge),
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        writer.flush()?;
        self.viewport_area.y = 0;
        self.terminal.resize(self.viewport_area)?;
        Ok(())
    }

    pub(crate) fn render_frame(&mut self, frame: &ViewportFrame) -> io::Result<()> {
        self.set_viewport_height(
            frame.buffer.area.width,
            frame.buffer.area.height,
            frame.viewport_height,
        )?;
        self.terminal.draw(|terminal_frame| {
            let cursor = render_inline(terminal_frame.buffer_mut(), frame);
            terminal_frame.set_cursor_position(cursor);
        })?;
        Ok(())
    }

    pub(crate) fn insert_buffer(&mut self, buffer: &Buffer, gap_after: u16) -> io::Result<()> {
        insert_finalized_buffer(&mut self.terminal, buffer, gap_after)?;
        self.viewport_area = self.terminal.get_frame().area();
        Ok(())
    }

    pub(crate) fn leave_screen(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        self.terminal.show_cursor()?;
        self.guard.restore()
    }

    fn set_viewport_height(
        &mut self,
        width: u16,
        screen_height: u16,
        viewport_height: u16,
    ) -> io::Result<()> {
        let previous_area = self.viewport_area;
        let (area, scroll_by) =
            resized_viewport_area(previous_area, width, screen_height, viewport_height);
        if scroll_by > 0 {
            append_native_scrollback(&mut self.terminal, screen_height, scroll_by)?;
        }
        if area != previous_area {
            apply_viewport_resize(&mut self.terminal, area, previous_area)?;
            self.viewport_area = area;
        }
        Ok(())
    }
}

fn reserve_initial_viewport<B: Backend>(
    backend: &mut B,
    width: u16,
    height: u16,
) -> Result<Rect, B::Error> {
    let width = width.max(1);
    let height = height.max(1);
    let cursor = backend.get_cursor_position()?;
    let lines_after_cursor = height.saturating_sub(1);
    backend.append_lines(lines_after_cursor)?;
    let available_lines = height.saturating_sub(cursor.y).saturating_sub(1);
    let missing_lines = lines_after_cursor.saturating_sub(available_lines);
    Ok(Rect::new(
        0,
        cursor.y.saturating_sub(missing_lines),
        width,
        height,
    ))
}

fn insert_finalized_buffer<B: Backend>(
    terminal: &mut Terminal<B>,
    buffer: &Buffer,
    gap_after: u16,
) -> Result<(), B::Error> {
    let current = terminal.get_frame().area();
    let screen_height = terminal.size()?.height.max(1);
    let height = buffer.area.height.saturating_add(gap_after).max(1);
    let mut prepared = Buffer::empty(Rect::new(0, 0, current.width, height));
    let target = prepared.area;
    crate::buffer::copy_rows_for_direct_draw(buffer, &mut prepared, 0, target);
    let mut cells = prepared.content.as_slice();

    let mut drawn_height = i32::from(current.top());
    let mut remaining_height = i32::from(height);
    let viewport_height = i32::from(current.height);
    let screen_height_i32 = i32::from(screen_height);
    while remaining_height + viewport_height > screen_height_i32 {
        let lines_to_draw = remaining_height.min(screen_height_i32);
        let scroll_up = 0.max(drawn_height + lines_to_draw - screen_height_i32);
        append_native_scrollback(terminal, screen_height, to_u16(scroll_up))?;
        cells = draw_lines(
            terminal,
            to_u16(drawn_height - scroll_up),
            to_u16(lines_to_draw),
            current.width,
            cells,
        )?;
        drawn_height += lines_to_draw - scroll_up;
        remaining_height -= lines_to_draw;
    }

    let scroll_up = 0.max(drawn_height + remaining_height + viewport_height - screen_height_i32);
    append_native_scrollback(terminal, screen_height, to_u16(scroll_up))?;
    draw_lines(
        terminal,
        to_u16(drawn_height - scroll_up),
        to_u16(remaining_height),
        current.width,
        cells,
    )?;
    drawn_height += remaining_height - scroll_up;

    terminal.resize(Rect {
        y: to_u16(drawn_height),
        ..current
    })
}

/// `i32` row positions are non-negative by construction in
/// `insert_finalized_buffer`; clamp to `u16` like the rest of the codebase.
fn to_u16(value: i32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn append_native_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    screen_height: u16,
    rows: u16,
) -> Result<(), B::Error> {
    if rows == 0 {
        return Ok(());
    }
    terminal.set_cursor_position(Position::new(0, screen_height.saturating_sub(1)))?;
    terminal.backend_mut().append_lines(rows)
}

fn draw_lines<'a, B: Backend>(
    terminal: &mut Terminal<B>,
    y_offset: u16,
    lines_to_draw: u16,
    width: u16,
    cells: &'a [ratatui::buffer::Cell],
) -> Result<&'a [ratatui::buffer::Cell], B::Error> {
    let cell_count = usize::from(width) * usize::from(lines_to_draw);
    // Never split past the buffer: the caller's loop arithmetic guarantees a
    // full buffer in practice, but a short one must degrade to drawing what is
    // available instead of panicking on an out-of-bounds `split_at`.
    let (to_draw, remainder) = cells.split_at(cell_count.min(cells.len()));
    if lines_to_draw > 0 {
        let width = usize::from(width);
        let updates = to_draw.iter().enumerate().map(|(index, cell)| {
            (
                u16::try_from(index % width).unwrap_or(u16::MAX),
                y_offset.saturating_add(u16::try_from(index / width).unwrap_or(u16::MAX)),
                cell,
            )
        });
        terminal.backend_mut().draw(updates)?;
        terminal.backend_mut().flush()?;
    }
    Ok(remainder)
}

fn apply_viewport_resize<B: Backend>(
    terminal: &mut Terminal<B>,
    current: Rect,
    previous: Rect,
) -> Result<(), B::Error> {
    // Rows exposed or shifted by a viewport move are not represented in Ratatui's diff buffers.
    // Clear both managed areas before changing coordinates so the next frame cannot reveal stale
    // terminal contents around short lines or removed menus.
    clear_viewport_rows(terminal, previous.union(current))?;
    terminal.resize(current)
}

fn clear_viewport_rows<B: Backend>(terminal: &mut Terminal<B>, area: Rect) -> Result<(), B::Error> {
    for y in area.top()..area.bottom() {
        terminal
            .backend_mut()
            .set_cursor_position(Position::new(area.left(), y))?;
        terminal
            .backend_mut()
            .clear_region(ratatui::backend::ClearType::CurrentLine)?;
    }
    Ok(())
}

fn resized_viewport_area(
    current: Rect,
    width: u16,
    screen_height: u16,
    viewport_height: u16,
) -> (Rect, u16) {
    let screen_height = screen_height.max(1);
    let mut area = current;
    area.width = width.max(1);
    area.height = viewport_height.clamp(1, screen_height);
    let scroll_by = area.bottom().saturating_sub(screen_height);
    if scroll_by > 0 {
        area.y = screen_height - area.height;
    }
    (area, scroll_by)
}

fn render_inline(screen: &mut Buffer, frame: &ViewportFrame) -> Position {
    let visible_rows = frame.buffer.area.height.min(screen.area.height);
    crate::buffer::copy_rows(
        &frame.buffer,
        screen,
        0,
        Rect::new(
            screen.area.x,
            screen.area.y,
            screen.area.width,
            visible_rows,
        ),
    );

    Position::new(
        screen.area.x + frame.cursor_column.min(screen.area.width.saturating_sub(1)),
        screen.area.y + frame.cursor_row.min(screen.area.height.saturating_sub(1)),
    )
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self { active: true })
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let mut stdout = io::stdout();
        let screen_result = execute!(
            stdout,
            DisableBracketedPaste,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show
        );
        let raw_mode_result = terminal::disable_raw_mode();
        if screen_result.is_ok() && raw_mode_result.is_ok() {
            self.active = false;
        }
        screen_result.and(raw_mode_result)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        layout::Rect,
        style::Style,
        widgets::{Paragraph, Widget},
    };

    use super::*;

    #[test]
    fn inline_render_top_aligns_the_viewport() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 2));
        source.set_string(0, 0, "first", Style::default());
        source.set_string(0, 1, "last", Style::default());
        let frame = ViewportFrame::for_test(source, Position::new(2, 1));
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 5));

        let cursor = render_inline(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "first");
        assert_eq!(row_text(&screen, 1), "last");
        assert_eq!(cursor, Position::new(2, 1));
    }

    #[test]
    fn inline_render_clips_rows_below_the_screen() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 4));
        for (row, text) in ["one", "two", "three", "four"].into_iter().enumerate() {
            source.set_string(
                0,
                u16::try_from(row).unwrap_or(u16::MAX),
                text,
                Style::default(),
            );
        }
        let frame = ViewportFrame::for_test(source, Position::new(1, 3));
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 2));

        let cursor = render_inline(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "one");
        assert_eq!(row_text(&screen, 1), "two");
        assert_eq!(cursor, Position::new(1, 1));
    }

    #[test]
    fn inline_render_offsets_the_cursor_to_the_viewport_origin() {
        let source = Buffer::empty(Rect::new(0, 0, 6, 2));
        let frame = ViewportFrame::for_test(source, Position::new(2, 1));
        let mut screen = Buffer::empty(Rect::new(3, 4, 6, 2));

        let cursor = render_inline(&mut screen, &frame);

        assert_eq!(cursor, Position::new(5, 5));
    }

    #[test]
    fn shrinking_the_viewport_keeps_history_above_it_visible() {
        let (area, scroll_by) = resized_viewport_area(Rect::new(0, 8, 80, 16), 80, 24, 3);

        assert_eq!(area, Rect::new(0, 8, 80, 3));
        assert_eq!(scroll_by, 0);
    }

    #[test]
    fn expanding_the_viewport_scrolls_only_the_rows_it_needs() {
        let (area, scroll_by) = resized_viewport_area(Rect::new(0, 8, 80, 3), 80, 24, 24);

        assert_eq!(area, Rect::new(0, 0, 80, 24));
        assert_eq!(scroll_by, 8);
    }

    #[test]
    fn viewport_expansion_moves_rows_into_native_scrollback() {
        let previous = Rect::new(0, 2, 8, 2);
        let current = Rect::new(0, 1, 8, 3);
        let backend = TestBackend::with_lines(["history1", "history2", "stale1", "stale2"]);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(previous),
            },
        )
        .unwrap();

        append_native_scrollback(&mut terminal, 4, 1).unwrap();
        apply_viewport_resize(&mut terminal, current, previous).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("live\n\ninput").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let scrollback = terminal.backend().scrollback();
        assert_eq!(scrollback.area.height, 1);
        assert_eq!(row_text(scrollback, 0), "history1");
        let screen = terminal.backend().buffer();
        assert_eq!(row_text(screen, 0), "history2");
        assert_eq!(row_text(screen, 1), "live");
        assert_eq!(row_text(screen, 2), "");
        assert_eq!(row_text(screen, 3), "input");
    }

    #[test]
    fn finalized_history_moves_overflow_into_native_scrollback() {
        let width = 20;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, 3)),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let mut history = Buffer::empty(Rect::new(0, 0, width - 1, 14));
        for row in 0..history.area.height {
            history.set_string(0, row, format!("history{row:02}"), Style::default());
        }
        insert_finalized_buffer(&mut terminal, &history, 1).unwrap();
        let viewport = terminal.get_frame().area();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        assert_eq!(viewport, Rect::new(0, 7, width, 3));
        let scrollback = terminal.backend().scrollback();
        assert_eq!(scrollback.area.height, 8);
        assert_eq!(row_text(scrollback, 0), "history00");
        assert_eq!(row_text(scrollback, 7), "history07");
        let screen = terminal.backend().buffer();
        assert_eq!(
            (0..10).map(|row| row_text(screen, row)).collect::<Vec<_>>(),
            [
                "history08",
                "history09",
                "history10",
                "history11",
                "history12",
                "history13",
                "",
                "input",
                "",
                "model",
            ]
        );
    }

    #[test]
    fn committing_short_blocks_preserves_the_live_block_spacing() {
        let width = 20;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, 3)),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let mut first = Buffer::empty(Rect::new(0, 0, width - 1, 1));
        first.set_string(0, 0, "user", Style::default());
        insert_finalized_buffer(&mut terminal, &first, 1).unwrap();
        let mut second = Buffer::empty(Rect::new(0, 0, width - 1, 1));
        second.set_string(0, 0, "thought", Style::default());
        insert_finalized_buffer(&mut terminal, &second, 1).unwrap();
        let viewport = terminal.get_frame().area();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        assert_eq!(viewport, Rect::new(0, 4, width, 3));
        let screen = terminal.backend().buffer();
        assert_eq!(
            (0..10).map(|row| row_text(screen, row)).collect::<Vec<_>>(),
            ["user", "", "thought", "", "input", "", "model", "", "", ""]
        );
    }

    #[test]
    fn shrinking_a_viewport_clears_the_removed_rows() {
        let backend = TestBackend::new(8, 3);
        let previous = Rect::new(0, 0, 8, 3);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(previous),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("first\nsecond\nthird").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        apply_viewport_resize(&mut terminal, Rect::new(0, 0, 8, 1), previous).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("first").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let screen = terminal.backend().buffer();
        assert_eq!(row_text(screen, 0), "first");
        assert_eq!(row_text(screen, 1), "");
        assert_eq!(row_text(screen, 2), "");
    }

    #[test]
    fn shrinking_a_viewport_clears_stale_menu_rows_from_retained_rows() {
        let previous = Rect::new(0, 0, 8, 5);
        let current = Rect::new(0, 0, 8, 3);
        let backend = TestBackend::new(8, 5);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(previous),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\n--------\nchoice")
                    .render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        apply_viewport_resize(&mut terminal, current, previous).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let screen = terminal.backend().buffer();
        assert_eq!(row_text(screen, 2), "model");
        assert_eq!(row_text(screen, 3), "");
        assert_eq!(row_text(screen, 4), "");
    }

    #[test]
    fn synchronized_frame_holds_newlines_in_memory_until_commit() {
        let mut writer = FrameWriter::new(io::stdout());
        writer.begin_frame().unwrap();
        writer.write_all(b"first\r\nsecond").unwrap();
        writer.flush().unwrap();

        assert_eq!(writer.frame.as_deref(), Some(b"first\r\nsecond".as_slice()));
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
