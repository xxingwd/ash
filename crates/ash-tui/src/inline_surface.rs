use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{position, MoveTo, Show},
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

type ManagedTerminal = Terminal<CrosstermBackend<FrameWriter>>;

#[derive(Clone, Copy)]
struct PendingResize {
    area: Rect,
    inline_top: Option<u16>,
}

/// Starts at the shell cursor and promotes once, when resize/reset/history
/// first needs it, to ownership of the entire visible primary screen.
pub(crate) struct InlineSurface {
    terminal: ManagedTerminal,
    fullscreen: bool,
    cursor_row: u16,
    rendered_rows: u16,
    pending_resize: Option<PendingResize>,
    _guard: TerminalGuard,
}

impl InlineSurface {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;
        let (_, top) = position()?;
        let (width, height) = terminal::size()?;
        let initial_area = Rect::new(0, top.min(height.saturating_sub(1)), width.max(1), 1);

        let backend = CrosstermBackend::new(FrameWriter::new(stdout));
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(initial_area),
            },
        )?;

        Ok(Self {
            terminal,
            fullscreen: false,
            cursor_row: 0,
            rendered_rows: 0,
            pending_resize: None,
            _guard: guard,
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
        let area = Rect::new(0, 0, width.max(1), height.max(1));
        let inline_top = if self.fullscreen {
            None
        } else {
            let (_, cursor_y) = position()?;
            Some(cursor_y.saturating_sub(self.cursor_row))
        };
        self.pending_resize = Some(PendingResize { area, inline_top });
        Ok(())
    }

    pub(crate) fn reset(&mut self) -> io::Result<()> {
        self.apply_pending_resize()?;
        let (width, height) = terminal::size()?;
        let current = self.terminal.current_buffer_mut().area;
        let top = if self.fullscreen {
            0
        } else {
            current.y.min(height.saturating_sub(1))
        };
        clear_from_row(self.terminal.backend_mut().writer_mut(), top)?;
        self.terminal
            .set_viewport_area(Rect::new(0, top, width.max(1), 1));
        self.terminal.force_redraw();
        self.fullscreen = false;
        self.cursor_row = 0;
        self.rendered_rows = 0;
        Ok(())
    }

    pub(crate) fn render_frame(&mut self, frame: &ViewportFrame) -> io::Result<()> {
        self.apply_pending_resize()?;
        if !self.fullscreen {
            self.prepare_inline_area(frame)?;
        }
        let area_height = self.terminal.current_buffer_mut().area.height;
        self.cursor_row = frame
            .cursor_row
            .saturating_sub(frame.buffer.area.height.saturating_sub(area_height));
        self.rendered_rows = frame.buffer.area.height.min(area_height);
        self.terminal.draw(|terminal_frame| {
            let cursor = render_area(terminal_frame.buffer_mut(), frame);
            terminal_frame.set_cursor_position(cursor);
        })?;
        Ok(())
    }

    pub(crate) fn insert_history(
        &mut self,
        buffer: &Buffer,
        leading_blank: bool,
    ) -> io::Result<()> {
        self.apply_pending_resize()?;
        let (width, height) = terminal::size()?;
        let width = width.max(1);
        let height = height.max(1);
        let mut history = Buffer::empty(Rect::new(
            0,
            0,
            width,
            buffer.area.height.saturating_add(u16::from(leading_blank)),
        ));
        let target_y = u16::from(leading_blank);
        copy_buffer(buffer, &mut history, 0, target_y, buffer.area.height);
        self.insert_before_viewport(&history, height)
    }

    pub(crate) fn leave_screen(&mut self) -> io::Result<()> {
        self.apply_pending_resize()?;
        let height = terminal::size()?.1.max(1);
        let area = self.terminal.current_buffer_mut().area;
        let next_row = area.y.saturating_add(self.rendered_rows).min(height);
        if next_row < height {
            clear_from_row(self.terminal.backend_mut().writer_mut(), next_row)?;
            queue!(
                self.terminal.backend_mut().writer_mut(),
                MoveTo(0, next_row)
            )
        } else {
            let writer = self.terminal.backend_mut().writer_mut();
            queue!(
                writer,
                ResetColor,
                SetAttribute(Attribute::Reset),
                MoveTo(0, height.saturating_sub(1))
            )?;
            write!(writer, "\r\n")
        }
    }

    fn prepare_inline_area(&mut self, frame: &ViewportFrame) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let width = width.max(1);
        let height = height.max(1);
        let current = self.terminal.current_buffer_mut().area;
        if frame.buffer.area.height > height {
            self.promote_to_fullscreen(current.y, Rect::new(0, 0, width, height))?;
            return Ok(());
        }

        let next_height = frame.buffer.area.height.max(1);
        let scroll_rows = current.y.saturating_add(next_height).saturating_sub(height);
        let next = Rect::new(0, current.y.saturating_sub(scroll_rows), width, next_height);
        if current != next {
            clear_from_row(self.terminal.backend_mut().writer_mut(), current.y)?;
            if scroll_rows > 0 {
                let backend = self.terminal.backend_mut();
                backend.set_cursor_position(Position::new(0, height.saturating_sub(1)))?;
                backend.append_lines(scroll_rows)?;
            }
            self.terminal.set_viewport_area(next);
            self.terminal.force_redraw();
        }
        Ok(())
    }

    fn insert_before_viewport(&mut self, history: &Buffer, screen_height: u16) -> io::Result<()> {
        let mut source_y = 0;
        let mut remaining = history.area.height;
        let mut area = self.terminal.current_buffer_mut().area;

        if area.height == screen_height {
            return self.insert_fullscreen_history(history);
        }

        if area.bottom() < screen_height {
            let rows = remaining.min(screen_height.saturating_sub(area.bottom()));
            if rows > 0 {
                self.terminal
                    .backend_mut()
                    .scroll_region_down(area.top()..area.bottom().saturating_add(rows), rows)?;
                self.draw_history_rows(history, source_y, area.top(), rows)?;
                source_y = source_y.saturating_add(rows);
                remaining = remaining.saturating_sub(rows);
                area.y = area.y.saturating_add(rows);
                self.terminal.set_viewport_area(area);
            }
        }

        while remaining > 0 {
            let rows = remaining.min(area.top());
            if rows == 0 {
                return self.insert_fullscreen_history_rows(history, source_y, remaining);
            }
            self.terminal
                .backend_mut()
                .scroll_region_up(0..area.top(), rows)?;
            self.draw_history_rows(history, source_y, area.top().saturating_sub(rows), rows)?;
            source_y = source_y.saturating_add(rows);
            remaining = remaining.saturating_sub(rows);
        }

        self.terminal.force_redraw();
        Ok(())
    }

    fn insert_fullscreen_history(&mut self, history: &Buffer) -> io::Result<()> {
        self.insert_fullscreen_history_rows(history, 0, history.area.height)
    }

    fn insert_fullscreen_history_rows(
        &mut self,
        history: &Buffer,
        source_y: u16,
        rows: u16,
    ) -> io::Result<()> {
        let width = self.terminal.current_buffer_mut().area.width.max(1);
        // A full-screen viewport has no rows above it to scroll. Borrow its top
        // row, draw one history row there, then scroll only that one-row region
        // into terminal scrollback. This is the same bounded-scroll strategy as
        // Ratatui's `insert_before`, kept here because InlineSurface owns a
        // custom Fixed viewport and the promotion/reflow state around it.
        for y in 0..rows {
            let source_y = source_y.saturating_add(y);
            let backend = self.terminal.backend_mut();
            backend.draw(
                (0..width).filter_map(|x| history.cell((x, source_y)).map(|cell| (x, 0, cell))),
            )?;
            backend.scroll_region_up(0..1, 1)?;
        }
        self.terminal.force_redraw();
        Ok(())
    }

    fn draw_history_rows(
        &mut self,
        history: &Buffer,
        source_y: u16,
        target_y: u16,
        rows: u16,
    ) -> io::Result<()> {
        let width = self.terminal.current_buffer_mut().area.width.max(1);
        self.terminal.backend_mut().draw((0..rows).flat_map(|row| {
            (0..width).filter_map(move |x| {
                history
                    .cell((x, source_y.saturating_add(row)))
                    .map(|cell| (x, target_y.saturating_add(row), cell))
            })
        }))
    }

    fn apply_pending_resize(&mut self) -> io::Result<()> {
        let Some(resize) = self.pending_resize.take() else {
            return Ok(());
        };
        if let Some(inline_top) = resize.inline_top {
            let current_height = self
                .terminal
                .current_buffer_mut()
                .area
                .height
                .min(resize.area.height)
                .max(1);
            let top = inline_top.min(resize.area.height.saturating_sub(1));
            clear_from_row(self.terminal.backend_mut().writer_mut(), top)?;
            self.terminal
                .set_viewport_area(Rect::new(0, top, resize.area.width, current_height));
            self.terminal.force_redraw();
            self.fullscreen = false;
        } else {
            clear_from_row(self.terminal.backend_mut().writer_mut(), 0)?;
            self.terminal.set_viewport_area(resize.area);
            self.terminal.force_redraw();
        }
        Ok(())
    }

    fn promote_to_fullscreen(&mut self, inline_top: u16, area: Rect) -> io::Result<()> {
        let height = area.height.max(1);
        let inline_top = inline_top.min(height.saturating_sub(1));
        clear_from_row(self.terminal.backend_mut().writer_mut(), inline_top)?;
        if inline_top > 0 {
            let backend = self.terminal.backend_mut();
            backend.set_cursor_position(Position::new(0, height.saturating_sub(1)))?;
            backend.append_lines(inline_top)?;
        }
        self.terminal.set_viewport_area(area);
        self.terminal.force_redraw();
        self.fullscreen = true;
        self.cursor_row = 0;
        self.rendered_rows = 0;
        Ok(())
    }
}

fn clear_from_row(writer: &mut impl Write, row: u16) -> io::Result<()> {
    queue!(
        writer,
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveTo(0, row),
        Clear(ClearType::FromCursorDown)
    )
}

fn render_area(screen: &mut Buffer, frame: &ViewportFrame) -> Position {
    let visible_rows = frame.buffer.area.height.min(screen.area.height);
    let hidden_rows = frame.buffer.area.height.saturating_sub(visible_rows);
    copy_buffer(&frame.buffer, screen, hidden_rows, 0, visible_rows);

    Position::new(
        screen
            .area
            .x
            .saturating_add(frame.cursor_column.min(screen.area.width.saturating_sub(1))),
        screen
            .area
            .y
            .saturating_add(frame.cursor_row.saturating_sub(hidden_rows))
            .min(screen.area.bottom().saturating_sub(1)),
    )
}

fn copy_buffer(
    source: &Buffer,
    destination: &mut Buffer,
    source_y: u16,
    target_y: u16,
    row_count: u16,
) {
    let height = row_count.min(source.area.height.saturating_sub(source_y));
    let width = source.area.width.min(destination.area.width);
    for y in 0..height {
        let destination_y = target_y.saturating_add(y);
        if destination_y >= destination.area.height {
            break;
        }
        for x in 0..width {
            let Some(cell) = source.cell((
                source.area.x.saturating_add(x),
                source.area.y.saturating_add(source_y).saturating_add(y),
            )) else {
                continue;
            };
            *destination
                .cell_mut((
                    destination.area.x.saturating_add(x),
                    destination.area.y.saturating_add(destination_y),
                ))
                .expect("destination cell is in bounds") = cell.clone();
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

    use super::*;

    #[test]
    fn fullscreen_render_top_aligns_the_viewport() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 2));
        source.set_string(0, 0, "first", Style::default());
        source.set_string(0, 1, "last", Style::default());
        let frame = ViewportFrame {
            buffer: source,
            cursor_row: 1,
            cursor_column: 2,
        };
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 5));

        let cursor = render_area(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "first");
        assert_eq!(row_text(&screen, 1), "last");
        assert_eq!(cursor, Position::new(2, 1));
    }

    #[test]
    fn fullscreen_render_keeps_only_the_bottom_rows() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 4));
        for (row, text) in ["one", "two", "three", "four"].into_iter().enumerate() {
            source.set_string(0, row as u16, text, Style::default());
        }
        let frame = ViewportFrame {
            buffer: source,
            cursor_row: 3,
            cursor_column: 1,
        };
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 2));

        let cursor = render_area(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "three");
        assert_eq!(row_text(&screen, 1), "four");
        assert_eq!(cursor, Position::new(1, 1));
    }

    #[test]
    fn synchronized_frame_holds_newlines_in_memory_until_commit() {
        let mut writer = FrameWriter::new(io::stdout());
        writer.begin_frame().unwrap();
        writer.write_all(b"first\r\nsecond").unwrap();
        writer.flush().unwrap();

        assert_eq!(writer.frame.as_deref(), Some(b"first\r\nsecond".as_slice()));
    }

    #[test]
    fn fixed_render_uses_the_viewport_origin() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 1));
        source.set_string(0, 0, "frame", Style::default());
        let frame = ViewportFrame {
            buffer: source,
            cursor_row: 0,
            cursor_column: 2,
        };
        let mut screen = Buffer::empty(Rect::new(0, 2, 6, 1));

        let cursor = render_area(&mut screen, &frame);

        assert_eq!(row_text(&screen, 2), "frame");
        assert_eq!(cursor, Position::new(2, 2));
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| !cell.skip)
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}
