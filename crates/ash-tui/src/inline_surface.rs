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
    Terminal,
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

type FullscreenTerminal = Terminal<CrosstermBackend<FrameWriter>>;

/// Owns the entire visible primary screen. Rows leave this surface only through
/// `insert_history`, which moves them into the terminal's scrollback buffer.
pub(crate) struct InlineSurface {
    terminal: FullscreenTerminal,
    pending_resize: Option<Rect>,
    _guard: TerminalGuard,
}

impl InlineSurface {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;
        take_over_visible_screen(&mut stdout)?;

        let backend = CrosstermBackend::new(FrameWriter::new(stdout));
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        terminal.force_redraw();

        Ok(Self {
            terminal,
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

    pub(crate) fn resize(&mut self, width: u16, height: u16) {
        self.pending_resize = Some(Rect::new(0, 0, width.max(1), height.max(1)));
    }

    pub(crate) fn reset(&mut self) -> io::Result<()> {
        self.apply_pending_resize()?;
        self.terminal.clear()?;
        self.terminal.force_redraw();
        Ok(())
    }

    pub(crate) fn render_frame(&mut self, frame: &ViewportFrame) -> io::Result<()> {
        self.apply_pending_resize()?;
        self.terminal.draw(|terminal_frame| {
            let cursor = render_fullscreen(terminal_frame.buffer_mut(), frame);
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
        let width = self.terminal.current_buffer_mut().area.width.max(1);
        let mut history = Buffer::empty(Rect::new(
            0,
            0,
            width,
            buffer.area.height.saturating_add(u16::from(leading_blank)),
        ));
        let target_y = u16::from(leading_blank);
        copy_buffer(buffer, &mut history, 0, target_y, buffer.area.height);

        // This is the full-screen case of Ratatui's inline `insert_before`:
        // borrow the top row, draw one history row into it, then scroll that
        // one-row region into the primary screen's scrollback.
        for y in 0..history.area.height {
            let backend = self.terminal.backend_mut();
            backend
                .draw((0..width).filter_map(|x| history.cell((x, y)).map(|cell| (x, 0, cell))))?;
            backend.scroll_region_up(0..1, 1)?;
        }
        self.terminal.force_redraw();
        Ok(())
    }

    pub(crate) fn leave_screen(&mut self) -> io::Result<()> {
        self.apply_pending_resize()?;
        let height = self.terminal.current_buffer_mut().area.height.max(1);
        let writer = self.terminal.backend_mut().writer_mut();
        queue!(
            writer,
            ResetColor,
            SetAttribute(Attribute::Reset),
            MoveTo(0, height.saturating_sub(1)),
            Clear(ClearType::CurrentLine)
        )?;
        write!(writer, "\r\n")
    }

    fn apply_pending_resize(&mut self) -> io::Result<()> {
        let Some(area) = self.pending_resize.take() else {
            return Ok(());
        };
        self.terminal.resize(area)?;
        self.terminal.force_redraw();
        Ok(())
    }
}

fn take_over_visible_screen(stdout: &mut Stdout) -> io::Result<()> {
    let (_, height) = terminal::size()?;
    let height = height.max(1);
    queue!(
        stdout,
        BeginSynchronizedUpdate,
        MoveTo(0, height.saturating_sub(1))
    )?;
    // Preserve the shell's current screen by moving it into scrollback once.
    // After this point every visible row belongs to Ash.
    for _ in 0..height {
        write!(stdout, "\r\n")?;
    }
    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        EndSynchronizedUpdate
    )?;
    stdout.flush()
}

fn render_fullscreen(screen: &mut Buffer, frame: &ViewportFrame) -> Position {
    let visible_rows = frame.buffer.area.height.min(screen.area.height);
    let hidden_rows = frame.buffer.area.height.saturating_sub(visible_rows);
    let target_y = screen.area.height.saturating_sub(visible_rows);
    copy_buffer(&frame.buffer, screen, hidden_rows, target_y, visible_rows);

    Position::new(
        frame.cursor_column.min(screen.area.width.saturating_sub(1)),
        target_y
            .saturating_add(frame.cursor_row.saturating_sub(hidden_rows))
            .min(screen.area.height.saturating_sub(1)),
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
    fn fullscreen_render_bottom_aligns_the_viewport() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 2));
        source.set_string(0, 0, "first", Style::default());
        source.set_string(0, 1, "last", Style::default());
        let frame = ViewportFrame {
            buffer: source,
            cursor_row: 1,
            cursor_column: 2,
            total_rows: 2,
        };
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 5));

        let cursor = render_fullscreen(&mut screen, &frame);

        assert_eq!(row_text(&screen, 3), "first");
        assert_eq!(row_text(&screen, 4), "last");
        assert_eq!(cursor, Position::new(2, 4));
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
            total_rows: 4,
        };
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 2));

        let cursor = render_fullscreen(&mut screen, &frame);

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
