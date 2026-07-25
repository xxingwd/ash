use std::io::{self, Stdout, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute, queue,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::CrosstermBackend,
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

/// Owns the alternate screen for the lifetime of the interactive UI.
pub(crate) struct AlternateScreen {
    terminal: FullscreenTerminal,
    guard: TerminalGuard,
}

impl AlternateScreen {
    pub(crate) fn enter() -> io::Result<Self> {
        let guard = TerminalGuard::enter()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            Show
        )?;

        let backend = CrosstermBackend::new(FrameWriter::new(stdout));
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        terminal.force_redraw();

        Ok(Self { terminal, guard })
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
        self.terminal.force_redraw();
        Ok(())
    }

    pub(crate) fn reset(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        self.terminal.force_redraw();
        Ok(())
    }

    pub(crate) fn render_frame(&mut self, frame: &ViewportFrame) -> io::Result<()> {
        self.terminal.draw(|terminal_frame| {
            let cursor = render_fullscreen(terminal_frame.buffer_mut(), frame);
            terminal_frame.set_cursor_position(cursor);
        })?;
        Ok(())
    }

    pub(crate) fn copy_to_clipboard(&mut self, text: &str) -> io::Result<()> {
        let writer = self.terminal.backend_mut().writer_mut();
        writer.write_all(osc52_sequence(text).as_bytes())?;
        writer.flush()
    }

    pub(crate) fn leave_screen(&mut self) -> io::Result<()> {
        self.terminal.show_cursor()?;
        self.guard.restore()
    }
}

fn render_fullscreen(screen: &mut Buffer, frame: &ViewportFrame) -> Position {
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
        frame.cursor_column.min(screen.area.width.saturating_sub(1)),
        frame.cursor_row.min(screen.area.height.saturating_sub(1)),
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
            DisableMouseCapture,
            DisableBracketedPaste,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
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
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

    use super::*;

    #[test]
    fn fullscreen_render_top_aligns_the_viewport() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 2));
        source.set_string(0, 0, "first", Style::default());
        source.set_string(0, 1, "last", Style::default());
        let frame = ViewportFrame::for_test(source, Position::new(2, 1));
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 5));

        let cursor = render_fullscreen(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "first");
        assert_eq!(row_text(&screen, 1), "last");
        assert_eq!(cursor, Position::new(2, 1));
    }

    #[test]
    fn fullscreen_render_clips_rows_below_the_screen() {
        let mut source = Buffer::empty(Rect::new(0, 0, 6, 4));
        for (row, text) in ["one", "two", "three", "four"].into_iter().enumerate() {
            source.set_string(0, row as u16, text, Style::default());
        }
        let frame = ViewportFrame::for_test(source, Position::new(1, 3));
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 2));

        let cursor = render_fullscreen(&mut screen, &frame);

        assert_eq!(row_text(&screen, 0), "one");
        assert_eq!(row_text(&screen, 1), "two");
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
