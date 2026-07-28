use std::io::{self, Stdout, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
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
        let backend = CrosstermBackend::new(FrameWriter::new(stdout));
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(1)),
            },
        )?;
        terminal.clear()?;
        terminal.force_redraw();
        let viewport_area = terminal.get_frame().area();

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
        self.terminal
            .resize(Rect::new(0, 0, width.max(1), height.max(1)))?;
        self.viewport_area = self.terminal.get_frame().area();
        self.terminal.force_redraw();
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
        self.terminal.set_viewport_area(self.viewport_area);
        self.terminal.clear()?;
        self.terminal.force_redraw();
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
        let height = buffer.area.height.saturating_add(gap_after).max(1);
        self.terminal.insert_before(height, |target| {
            crate::buffer::copy_rows_for_direct_draw(buffer, target, 0, target.area);
        })?;
        self.viewport_area = self.terminal.get_frame().area();
        Ok(())
    }

    pub(crate) fn copy_to_clipboard(&mut self, text: &str) -> io::Result<()> {
        let writer = self.terminal.backend_mut().writer_mut();
        writer.write_all(osc52_sequence(text).as_bytes())?;
        writer.flush()
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
        let (area, scroll_by) =
            resized_viewport_area(self.viewport_area, width, screen_height, viewport_height);
        if scroll_by > 0 {
            // Newlines at the bottom of the primary screen enter native scrollback reliably.
            // Region-scrolling sequences can discard those rows in multiplexers such as Zellij.
            append_native_scrollback(&mut self.terminal, screen_height, scroll_by)?;
        }
        if area != self.viewport_area {
            self.terminal.set_viewport_area(area);
            self.terminal.clear()?;
            self.terminal.force_redraw();
            self.viewport_area = area;
        }
        Ok(())
    }
}

fn append_native_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    screen_height: u16,
    rows: u16,
) -> io::Result<()> {
    terminal.set_cursor_position(Position::new(0, screen_height.saturating_sub(1)))?;
    terminal.backend_mut().append_lines(rows)
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

fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
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
            source.set_string(0, row as u16, text, Style::default());
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
        let backend = TestBackend::new(8, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("first\nsecond\nthird").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        append_native_scrollback(&mut terminal, 3, 2).unwrap();

        let scrollback = terminal.backend().scrollback();
        assert_eq!(scrollback.area.height, 2);
        assert_eq!(row_text(scrollback, 0), "first");
        assert_eq!(row_text(scrollback, 1), "second");
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
    fn osc52_copies_utf8_text_through_the_terminal() {
        assert_eq!(osc52_sequence("你好"), "\x1b]52;c;5L2g5aW9\x07");
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
