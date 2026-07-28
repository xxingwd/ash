use std::{
    fmt,
    io::{self, Stdout, Write},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use crossterm::{
    cursor::{MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{Attribute, Print, ResetColor, SetAttribute},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
    Command,
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
        self.viewport_area =
            insert_finalized_buffer(&mut self.terminal, self.viewport_area, buffer, gap_after)?;
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
        let previous_area = self.viewport_area;
        let (area, scroll_by) =
            resized_viewport_area(previous_area, width, screen_height, viewport_height);
        if scroll_by > 0 {
            scroll_history_for_viewport_expansion(&mut self.terminal, previous_area, scroll_by)?;
        }
        if area != previous_area {
            apply_viewport_resize(&mut self.terminal, area, previous_area)?;
            self.viewport_area = area;
        }
        Ok(())
    }
}

/// Finalized history and live viewport movement deliberately use different terminal operations.
/// A CRLF at the bottom of a top-anchored scroll region enters native scrollback in terminals and
/// multiplexers where an explicit `CSI S` region scroll only changes the visible grid.
fn insert_finalized_buffer<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    mut viewport: Rect,
    buffer: &Buffer,
    gap_after: u16,
) -> io::Result<Rect> {
    let screen = terminal.size()?;
    let inserted_height = buffer.area.height.saturating_add(gap_after).max(1);

    if viewport.top() == 0 && viewport.height == screen.height {
        insert_finalized_buffer_at_screen_bottom(
            terminal.backend_mut(),
            screen.width,
            screen.height,
            buffer,
            inserted_height,
        )?;
        terminal.force_redraw();
        return Ok(viewport);
    }

    // History starts at the old viewport edge; reverse-index only opens space below it.
    let cursor_top = viewport.top().saturating_sub(1);
    let space_below = screen.height.saturating_sub(viewport.bottom());
    let move_down = inserted_height.min(space_below);
    if move_down > 0 {
        let backend = terminal.backend_mut();
        queue!(
            backend,
            SetScrollRegion::new(viewport.top().saturating_add(1), screen.height),
            MoveTo(0, viewport.top())
        )?;
        for _ in 0..move_down {
            queue!(backend, Print("\x1bM"))?;
        }
        queue!(backend, ResetScrollRegion)?;
        viewport.y = viewport.y.saturating_add(move_down);
    }

    let history_bottom = viewport.top();
    if history_bottom == 0 {
        return Err(io::Error::other(
            "inline viewport leaves no row available for finalized history",
        ));
    }

    let region_bottom = history_bottom - 1;
    let backend = terminal.backend_mut();
    queue!(
        backend,
        SetScrollRegion::new(1, history_bottom),
        MoveTo(0, cursor_top)
    )?;
    for row in 0..inserted_height {
        queue!(backend, Print("\r\n"))?;
        let target_y = cursor_top
            .saturating_add(row)
            .saturating_add(1)
            .min(region_bottom);
        draw_finalized_row(backend, buffer, row, target_y, screen.width)?;
    }
    queue!(backend, ResetScrollRegion)?;
    Backend::flush(backend)?;

    terminal.set_viewport_area(viewport);
    terminal.force_redraw();
    Ok(viewport)
}

fn insert_finalized_buffer_at_screen_bottom<B: Backend + Write>(
    backend: &mut B,
    screen_width: u16,
    screen_height: u16,
    buffer: &Buffer,
    inserted_height: u16,
) -> io::Result<()> {
    let bottom = screen_height.saturating_sub(1);
    let mut target_y = 0u16;
    queue!(
        backend,
        ResetScrollRegion,
        MoveTo(0, 0),
        Clear(ClearType::FromCursorDown)
    )?;
    for row in 0..inserted_height {
        if row > 0 {
            queue!(backend, Print("\r\n"))?;
            target_y = target_y.saturating_add(1).min(bottom);
        }
        draw_finalized_row(backend, buffer, row, target_y, screen_width)?;
    }
    for _ in 0..screen_height {
        queue!(backend, Print("\r\n"), Clear(ClearType::CurrentLine))?;
    }
    Backend::flush(backend)
}

fn draw_finalized_row<B: Backend + Write>(
    backend: &mut B,
    source: &Buffer,
    source_row: u16,
    target_y: u16,
    screen_width: u16,
) -> io::Result<()> {
    queue!(backend, Clear(ClearType::CurrentLine))?;

    let width = source.area.width.min(screen_width).max(1);
    let area = Rect::new(0, target_y, width, 1);
    let empty = Buffer::empty(area);
    let mut row = Buffer::empty(area);
    if source_row < source.area.height {
        crate::buffer::copy_rows_for_direct_draw(source, &mut row, source_row, area);
    }
    backend.draw(empty.diff(&row).into_iter())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SetScrollRegion {
    top: u16,
    bottom: u16,
}

impl SetScrollRegion {
    const fn new(top: u16, bottom: u16) -> Self {
        Self { top, bottom }
    }
}

impl Command for SetScrollRegion {
    fn write_ansi(&self, output: &mut impl fmt::Write) -> fmt::Result {
        write!(output, "\x1b[{};{}r", self.top, self.bottom)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scroll regions require ANSI terminal support",
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, output: &mut impl fmt::Write) -> fmt::Result {
        write!(output, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scroll regions require ANSI terminal support",
        ))
    }
}

fn scroll_history_for_viewport_expansion<B: Backend>(
    terminal: &mut Terminal<B>,
    viewport: Rect,
    rows: u16,
) -> io::Result<()> {
    terminal
        .backend_mut()
        .scroll_region_up(0..viewport.top(), rows)
}

fn apply_viewport_resize<B: Backend>(
    terminal: &mut Terminal<B>,
    current: Rect,
    previous: Rect,
) -> io::Result<()> {
    // The rows exposed by a viewport move are not represented in Ratatui's diff buffers.
    // Clear the old managed area before changing coordinates so blank cells in the next frame
    // cannot reveal stale terminal contents.
    clear_viewport_rows(terminal, previous)?;
    terminal.set_viewport_area(current);
    terminal.force_redraw();
    Ok(())
}

fn clear_viewport_rows<B: Backend>(terminal: &mut Terminal<B>, area: Rect) -> io::Result<()> {
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
        backend::{ClearType as BackendClearType, CrosstermBackend, TestBackend, WindowSize},
        buffer::{Buffer, Cell},
        layout::{Rect, Size},
        style::Style,
        widgets::{Paragraph, Widget},
    };

    use super::*;

    struct Vt100Backend {
        inner: CrosstermBackend<vt100::Parser>,
    }

    impl Vt100Backend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: CrosstermBackend::new(vt100::Parser::new(height, width, 100)),
            }
        }

        fn rows(&self) -> Vec<String> {
            let (height, _) = self.inner.writer().screen().size();
            self.inner.writer().screen().rows(0, height).collect()
        }
    }

    impl Write for Vt100Backend {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.inner.writer_mut().write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.writer_mut().flush()
        }
    }

    impl Backend for Vt100Backend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.inner.writer().screen().cursor_position().into())
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: BackendClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn append_lines(&mut self, line_count: u16) -> io::Result<()> {
            self.inner.append_lines(line_count)
        }

        fn size(&self) -> io::Result<Size> {
            let (height, width) = self.inner.writer().screen().size();
            Ok(Size::new(width, height))
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size()?,
                pixels: Size::default(),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.writer_mut().flush()
        }

        fn scroll_region_up(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> io::Result<()> {
            self.inner.scroll_region_up(region, line_count)
        }

        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> io::Result<()> {
            self.inner.scroll_region_down(region, line_count)
        }
    }

    struct RecordingBackend {
        size: Size,
        cursor: Position,
        output: Vec<u8>,
        scroll_region_up_calls: usize,
    }

    impl RecordingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                size: Size::new(width, height),
                cursor: Position::ORIGIN,
                output: Vec::new(),
                scroll_region_up_calls: 0,
            }
        }
    }

    impl Write for RecordingBackend {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Backend for RecordingBackend {
        fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.cursor)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.cursor = position.into();
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size::default(),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn scroll_region_up(
            &mut self,
            _region: std::ops::Range<u16>,
            _line_count: u16,
        ) -> io::Result<()> {
            self.scroll_region_up_calls += 1;
            Ok(())
        }

        fn scroll_region_down(
            &mut self,
            _region: std::ops::Range<u16>,
            _line_count: u16,
        ) -> io::Result<()> {
            Ok(())
        }
    }

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
    fn viewport_expansion_scrolls_only_the_history_above_it() {
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

        scroll_history_for_viewport_expansion(&mut terminal, previous, 1).unwrap();
        apply_viewport_resize(&mut terminal, current, previous).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("live\n\ninput").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let screen = terminal.backend().buffer();
        assert_eq!(row_text(screen, 0), "history2");
        assert_eq!(row_text(screen, 1), "live");
        assert_eq!(row_text(screen, 2), "");
        assert_eq!(row_text(screen, 3), "input");
    }

    #[test]
    fn finalized_history_uses_crlf_instead_of_explicit_region_scroll() {
        let viewport = Rect::new(0, 0, 8, 2);
        let backend = RecordingBackend::new(8, 4);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(viewport),
            },
        )
        .unwrap();
        let mut history = Buffer::empty(Rect::new(0, 0, 7, 2));
        history.set_string(0, 0, "first", Style::default());
        history.set_string(0, 1, "second", Style::default());

        let next = insert_finalized_buffer(&mut terminal, viewport, &history, 1).unwrap();

        let backend = terminal.backend();
        let output = String::from_utf8_lossy(&backend.output);
        assert_eq!(next, Rect::new(0, 2, 8, 2));
        assert_eq!(output.matches("\r\n").count(), 3);
        assert_eq!(backend.scroll_region_up_calls, 0);
        assert!(!output.contains("\x1b[1S"));
    }

    #[test]
    fn finalized_history_clears_a_full_screen_viewport_before_scrolling() {
        let viewport = Rect::new(0, 0, 8, 4);
        let backend = RecordingBackend::new(8, 4);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(viewport),
            },
        )
        .unwrap();
        let mut history = Buffer::empty(Rect::new(0, 0, 7, 1));
        history.set_string(0, 0, "history", Style::default());

        let next = insert_finalized_buffer(&mut terminal, viewport, &history, 1).unwrap();

        let backend = terminal.backend();
        let output = String::from_utf8_lossy(&backend.output);
        assert_eq!(next, viewport);
        assert!(output.starts_with("\x1b[r\x1b[1;1H\x1b[J"));
        assert_eq!(output.matches("\r\n").count(), 5);
        assert_eq!(backend.scroll_region_up_calls, 0);
    }

    #[test]
    fn committing_full_screen_live_output_keeps_history_next_to_the_viewport() {
        let width = 20;
        let height = 10;
        let full = Rect::new(0, 0, width, height);
        let backend = Vt100Backend::new(width, height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(full),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new(
                    (0..height)
                        .map(|row| format!("live{row:02}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
                .render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let compact = Rect::new(0, 0, width, 3);
        apply_viewport_resize(&mut terminal, compact, full).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let mut history = Buffer::empty(Rect::new(0, 0, width - 1, 14));
        for row in 0..history.area.height {
            history.set_string(0, row, format!("history{row:02}"), Style::default());
        }
        let viewport = insert_finalized_buffer(&mut terminal, compact, &history, 1).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let rows = terminal.backend().rows();
        let viewport_top = usize::from(viewport.top());
        assert_eq!(viewport, Rect::new(0, 7, width, 3));
        assert_eq!(rows[viewport_top - 2].trim_end(), "history13");
        assert_eq!(rows[viewport_top - 1].trim_end(), "");
        assert_eq!(rows[viewport_top].trim_end(), "input");
    }

    #[test]
    fn committing_multiple_live_blocks_does_not_accumulate_blank_rows() {
        let width = 20;
        let height = 10;
        let full = Rect::new(0, 0, width, height);
        let backend = Vt100Backend::new(width, height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(full),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("live output").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let mut viewport = Rect::new(0, 0, width, 3);
        apply_viewport_resize(&mut terminal, viewport, full).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        for (label, block_height) in [
            ("user", 1),
            ("thought", 1),
            ("tool", 2),
            ("answer", 8),
            ("worked", 1),
        ] {
            let mut block = Buffer::empty(Rect::new(0, 0, width - 1, block_height));
            for row in 0..block_height {
                block.set_string(0, row, format!("{label}{row:02}"), Style::default());
            }
            viewport = insert_finalized_buffer(&mut terminal, viewport, &block, 1).unwrap();
        }
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let rows = terminal.backend().rows();
        let viewport_top = usize::from(viewport.top());
        assert_eq!(viewport, Rect::new(0, 7, width, 3));
        assert_eq!(
            rows[..viewport_top]
                .iter()
                .map(|row| row.trim_end())
                .collect::<Vec<_>>(),
            ["answer04", "answer05", "answer06", "answer07", "", "worked00", ""]
        );
        assert_eq!(rows[viewport_top].trim_end(), "input");
    }

    #[test]
    fn committing_short_blocks_preserves_the_live_block_spacing() {
        let width = 20;
        let height = 10;
        let viewport = Rect::new(0, 0, width, 3);
        let backend = Vt100Backend::new(width, height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(viewport),
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
        let viewport = insert_finalized_buffer(&mut terminal, viewport, &first, 1).unwrap();
        let mut second = Buffer::empty(Rect::new(0, 0, width - 1, 1));
        second.set_string(0, 0, "thought", Style::default());
        let viewport = insert_finalized_buffer(&mut terminal, viewport, &second, 1).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("input\n\nmodel").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let rows = terminal.backend().rows();
        assert_eq!(viewport, Rect::new(0, 4, width, 3));
        assert_eq!(
            rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
            ["user", "", "thought", "", "input", "", "model", "", "", ""]
        );
    }

    #[test]
    fn shrinking_a_viewport_clears_the_removed_rows() {
        let backend = TestBackend::new(8, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                Paragraph::new("first\nsecond\nthird").render(frame.area(), frame.buffer_mut());
            })
            .unwrap();

        let previous = Rect::new(0, 0, 8, 3);
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
    fn shrinking_a_viewport_clears_stale_menu_borders_from_retained_rows() {
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
