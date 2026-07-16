use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{MoveTo, MoveToColumn, MoveToNextLine, MoveToPreviousLine, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::{
        Attribute, Color as CrosstermColor, ResetColor, SetAttribute, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use ratatui::{
    buffer::{Buffer, Cell},
    style::{Color as RatatuiColor, Modifier},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    palette::Rgb,
    viewport::{ViewportFrame, COMPOSER_TEXT_COLUMN},
};

#[derive(Debug, Default)]
struct ViewportGeometry {
    rows: u16,
    cursor_row: u16,
    cursor_column: u16,
    history_rows: u16,
    line_widths: Vec<u16>,
}

pub(crate) struct InlineSurface {
    stdout: Stdout,
    viewport: Option<ViewportGeometry>,
    reusable_rows: u16,
    prompt_width: u16,
    rendered_input: String,
    _guard: TerminalGuard,
}

impl InlineSurface {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste, Show)?;
        Ok(Self {
            stdout,
            viewport: None,
            reusable_rows: 0,
            prompt_width: 0,
            rendered_input: String::new(),
            _guard: TerminalGuard,
        })
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.viewport.is_some()
    }

    pub(crate) const fn prompt_width(&self) -> u16 {
        self.prompt_width
    }

    pub(crate) fn reset(&mut self) {
        self.viewport = None;
        self.reusable_rows = 0;
        self.prompt_width = 0;
        self.rendered_input.clear();
    }

    pub(crate) fn begin_synchronized(&mut self) -> io::Result<()> {
        queue!(self.stdout, BeginSynchronizedUpdate)
    }

    pub(crate) fn end_synchronized(&mut self) -> io::Result<()> {
        queue!(self.stdout, EndSynchronizedUpdate)?;
        self.stdout.flush()
    }

    pub(crate) fn clear_viewport(&mut self) -> io::Result<()> {
        let Some(viewport) = &self.viewport else {
            self.reusable_rows = 0;
            return Ok(());
        };
        queue!(self.stdout, MoveToPreviousLine(viewport.cursor_row))?;
        for row in 0..viewport.rows {
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            if row + 1 < viewport.rows {
                queue!(self.stdout, MoveToNextLine(1))?;
            }
        }
        if viewport.rows > 1 {
            queue!(self.stdout, MoveToPreviousLine(viewport.rows - 1))?;
        }
        self.reusable_rows = viewport.rows;
        Ok(())
    }

    pub(crate) fn push_history_to_scrollback(&mut self) -> io::Result<()> {
        let Some(viewport) = &self.viewport else {
            return Ok(());
        };
        let height = terminal::size()?.1.max(1);
        let (rows_below_cursor, rows_to_advance) = history_scroll_geometry(
            height,
            viewport.rows,
            viewport.history_rows,
            viewport.cursor_row,
        );
        queue!(
            self.stdout,
            SetAttribute(Attribute::Reset),
            ResetColor,
            MoveToColumn(0),
            MoveToNextLine(rows_below_cursor)
        )?;
        for _ in 0..rows_to_advance {
            writeln!(self.stdout)?;
        }
        queue!(self.stdout, Clear(ClearType::All), MoveTo(0, 0))
    }

    pub(crate) fn render_frame(
        &mut self,
        frame: &ViewportFrame,
        width: u16,
        prompt: &str,
    ) -> io::Result<()> {
        let line_widths = viewport_line_widths(&frame.buffer);
        self.write_buffer(&frame.buffer)?;
        self.viewport = Some(ViewportGeometry {
            rows: frame.total_rows,
            cursor_row: frame.cursor_row,
            cursor_column: frame.cursor_column,
            history_rows: frame.history_rows,
            line_widths,
        });
        self.prompt_width = width;
        self.rendered_input.clear();
        self.rendered_input.push_str(prompt);
        self.reusable_rows = 0;
        queue!(
            self.stdout,
            ResetColor,
            MoveToPreviousLine(
                frame
                    .total_rows
                    .saturating_sub(1)
                    .saturating_sub(frame.cursor_row)
            ),
            MoveToColumn(frame.cursor_column)
        )
    }

    pub(crate) fn update_input(
        &mut self,
        prompt: &str,
        cursor_column: u16,
        background: Option<Rgb>,
    ) -> io::Result<()> {
        if self.rendered_input != prompt {
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            let background =
                background.map_or(RatatuiColor::Reset, |(r, g, b)| RatatuiColor::Rgb(r, g, b));
            if background != RatatuiColor::Reset {
                queue!(
                    self.stdout,
                    SetBackgroundColor(crossterm_color(background)),
                    Clear(ClearType::CurrentLine),
                    ResetColor
                )?;
            }
            self.write_style(RatatuiColor::Reset, background, Modifier::BOLD)?;
            write!(self.stdout, "›")?;
            if !prompt.is_empty() {
                queue!(self.stdout, MoveToColumn(COMPOSER_TEXT_COLUMN))?;
                self.write_style(RatatuiColor::Reset, background, Modifier::empty())?;
                write!(self.stdout, "{prompt}")?;
            }
            queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            self.rendered_input.clear();
            self.rendered_input.push_str(prompt);
            if let Some(viewport) = &mut self.viewport {
                if let Some(line_width) = viewport
                    .line_widths
                    .get_mut(usize::from(viewport.cursor_row))
                {
                    let prompt_width =
                        u16::try_from(UnicodeWidthStr::width(prompt)).unwrap_or(u16::MAX);
                    *line_width = if prompt_width == 0 {
                        1
                    } else {
                        COMPOSER_TEXT_COLUMN.saturating_add(prompt_width)
                    };
                }
            }
        }
        queue!(
            self.stdout,
            MoveToColumn(COMPOSER_TEXT_COLUMN + cursor_column)
        )?;
        self.stdout.flush()
    }

    pub(crate) fn handle_resize(&mut self, width: u16, height: u16) {
        let Some(viewport) = &mut self.viewport else {
            return;
        };
        let (rows, cursor_row) = resize_reflow_geometry(
            &viewport.line_widths,
            viewport.cursor_row,
            viewport.cursor_column,
            width.max(1),
            height.max(1),
        );
        viewport.rows = rows;
        viewport.cursor_row = cursor_row;
        self.prompt_width = 0;
    }

    pub(crate) fn write_buffer(&mut self, buffer: &Buffer) -> io::Result<()> {
        self.write_buffer_rows(buffer, buffer.area.height)
    }

    pub(crate) fn write_buffer_rows(&mut self, buffer: &Buffer, height: u16) -> io::Result<()> {
        let height = height.min(buffer.area.height);
        let width = buffer.area.width;
        for y in 0..height {
            let background = uniform_row_background(buffer, y);
            queue!(
                self.stdout,
                MoveToColumn(0),
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::CurrentLine)
            )?;
            if background != RatatuiColor::Reset {
                queue!(
                    self.stdout,
                    SetBackgroundColor(crossterm_color(background)),
                    Clear(ClearType::CurrentLine),
                    ResetColor
                )?;
            }
            let mut current_style = None;
            let mut current_column = 0;
            for x in 0..width {
                let Some(cell) = buffer.cell((
                    buffer.area.x.saturating_add(x),
                    buffer.area.y.saturating_add(y),
                )) else {
                    continue;
                };
                if !cell_needs_write(cell, background) {
                    continue;
                }
                if current_column != x {
                    queue!(self.stdout, MoveToColumn(x))?;
                }
                let style = (cell.fg, cell.bg, cell.modifier);
                if current_style != Some(style) {
                    self.write_style(cell.fg, cell.bg, cell.modifier)?;
                    current_style = Some(style);
                }
                write!(self.stdout, "{}", cell.symbol())?;
                current_column = x.saturating_add(cell_display_width(cell));
            }
            queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            if y + 1 < height {
                self.next_row()?;
            }
        }
        Ok(())
    }

    pub(crate) fn next_row(&mut self) -> io::Result<()> {
        if self.reusable_rows > 1 {
            self.reusable_rows -= 1;
            queue!(self.stdout, MoveToNextLine(1))?;
        } else {
            self.reusable_rows = 0;
            write!(self.stdout, "\r\n")?;
        }
        Ok(())
    }

    pub(crate) fn clear_current_line(&mut self) -> io::Result<()> {
        queue!(
            self.stdout,
            MoveToColumn(0),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::CurrentLine)
        )
    }

    fn write_style(
        &mut self,
        foreground: RatatuiColor,
        background: RatatuiColor,
        modifiers: Modifier,
    ) -> io::Result<()> {
        queue!(self.stdout, SetAttribute(Attribute::Reset), ResetColor)?;
        if foreground != RatatuiColor::Reset {
            queue!(self.stdout, SetForegroundColor(crossterm_color(foreground)))?;
        }
        if background != RatatuiColor::Reset {
            queue!(self.stdout, SetBackgroundColor(crossterm_color(background)))?;
        }
        for (modifier, attribute) in [
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::HIDDEN, Attribute::Hidden),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
            (Modifier::SLOW_BLINK, Attribute::SlowBlink),
            (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        ] {
            if modifiers.contains(modifier) {
                queue!(self.stdout, SetAttribute(attribute))?;
            }
        }
        Ok(())
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

fn viewport_line_widths(buffer: &Buffer) -> Vec<u16> {
    (0..buffer.area.height)
        .map(|y| {
            let background = uniform_row_background(buffer, y);
            (0..buffer.area.width).fold(0, |line_width, x| {
                let Some(cell) = buffer.cell((
                    buffer.area.x.saturating_add(x),
                    buffer.area.y.saturating_add(y),
                )) else {
                    return line_width;
                };
                if cell_needs_write(cell, background) {
                    line_width.max(x.saturating_add(cell_display_width(cell)))
                } else {
                    line_width
                }
            })
        })
        .collect()
}

fn uniform_row_background(buffer: &Buffer, row: u16) -> RatatuiColor {
    let mut background = None;
    for x in 0..buffer.area.width {
        let Some(cell) = buffer.cell((
            buffer.area.x.saturating_add(x),
            buffer.area.y.saturating_add(row),
        )) else {
            continue;
        };
        match background {
            None => background = Some(cell.bg),
            Some(current) if current == cell.bg => {}
            Some(_) => return RatatuiColor::Reset,
        }
    }
    background
        .filter(|background| *background != RatatuiColor::Reset)
        .unwrap_or(RatatuiColor::Reset)
}

fn cell_needs_write(cell: &Cell, row_background: RatatuiColor) -> bool {
    !cell.skip
        && (cell.symbol() != " "
            || cell.fg != RatatuiColor::Reset
            || !cell.modifier.is_empty()
            || cell.bg != row_background)
}

fn cell_display_width(cell: &Cell) -> u16 {
    u16::try_from(UnicodeWidthStr::width(cell.symbol()))
        .unwrap_or(u16::MAX)
        .max(1)
}

fn resize_reflow_geometry(
    line_widths: &[u16],
    cursor_row: u16,
    cursor_column: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> (u16, u16) {
    let terminal_width = terminal_width.max(1);
    let terminal_height = terminal_height.max(1);
    if line_widths.is_empty() {
        return (1, 0);
    }

    let visual_rows = line_widths
        .iter()
        .map(|line_width| visual_row_count(*line_width, terminal_width))
        .collect::<Vec<_>>();
    let total_rows = visual_rows
        .iter()
        .copied()
        .fold(0u16, u16::saturating_add)
        .max(1);
    let cursor_row = usize::from(cursor_row).min(visual_rows.len() - 1);
    let rows_before_cursor = visual_rows[..cursor_row]
        .iter()
        .copied()
        .fold(0u16, u16::saturating_add);
    let cursor_line_offset = cursor_column
        .saturating_div(terminal_width)
        .min(visual_rows[cursor_row].saturating_sub(1));
    let cursor_visual_row = rows_before_cursor.saturating_add(cursor_line_offset);
    let hidden_rows = total_rows.saturating_sub(terminal_height);
    let visible_rows = total_rows.min(terminal_height);
    let visible_cursor_row = cursor_visual_row
        .saturating_sub(hidden_rows)
        .min(visible_rows.saturating_sub(1));

    (visible_rows, visible_cursor_row)
}

fn visual_row_count(line_width: u16, terminal_width: u16) -> u16 {
    if line_width == 0 {
        1
    } else {
        line_width
            .saturating_sub(1)
            .saturating_div(terminal_width.max(1))
            .saturating_add(1)
    }
}

fn history_scroll_geometry(
    terminal_height: u16,
    total_rows: u16,
    history_rows: u16,
    cursor_row: u16,
) -> (u16, u16) {
    let bottom_rows = total_rows.saturating_sub(history_rows);
    (
        total_rows.saturating_sub(1).saturating_sub(cursor_row),
        terminal_height.saturating_sub(bottom_rows),
    )
}

fn crossterm_color(color: RatatuiColor) -> CrosstermColor {
    match color {
        RatatuiColor::Reset => CrosstermColor::Reset,
        RatatuiColor::Black => CrosstermColor::Black,
        RatatuiColor::Red => CrosstermColor::DarkRed,
        RatatuiColor::Green => CrosstermColor::DarkGreen,
        RatatuiColor::Yellow => CrosstermColor::DarkYellow,
        RatatuiColor::Blue => CrosstermColor::DarkBlue,
        RatatuiColor::Magenta => CrosstermColor::DarkMagenta,
        RatatuiColor::Cyan => CrosstermColor::DarkCyan,
        RatatuiColor::Gray => CrosstermColor::Grey,
        RatatuiColor::DarkGray => CrosstermColor::DarkGrey,
        RatatuiColor::LightRed => CrosstermColor::Red,
        RatatuiColor::LightGreen => CrosstermColor::Green,
        RatatuiColor::LightYellow => CrosstermColor::Yellow,
        RatatuiColor::LightBlue => CrosstermColor::Blue,
        RatatuiColor::LightMagenta => CrosstermColor::Magenta,
        RatatuiColor::LightCyan => CrosstermColor::Cyan,
        RatatuiColor::White => CrosstermColor::White,
        RatatuiColor::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
        RatatuiColor::Indexed(value) => CrosstermColor::AnsiValue(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_width_ignores_trailing_default_cells() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 12, 1));
        buffer.set_string(0, 0, "ash", ratatui::style::Style::default());

        assert_eq!(viewport_line_widths(&buffer), vec![3]);
    }

    #[test]
    fn viewport_width_does_not_treat_uniform_background_as_text() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 12, 1));
        for x in 0..buffer.area.width {
            buffer
                .cell_mut((x, 0))
                .expect("cell")
                .set_bg(RatatuiColor::Blue);
        }
        assert_eq!(viewport_line_widths(&buffer), vec![0]);

        buffer.cell_mut((5, 0)).expect("cell").set_symbol("x");
        assert_eq!(viewport_line_widths(&buffer), vec![6]);
    }

    #[test]
    fn resize_reflow_counts_wrapped_visual_rows() {
        assert_eq!(visual_row_count(60, 48), 2);
        assert_eq!(resize_reflow_geometry(&[1, 60, 1], 2, 0, 48, 24), (4, 3));
    }

    #[test]
    fn resize_reflow_accounts_for_rows_scrolled_above_the_screen() {
        assert_eq!(resize_reflow_geometry(&[1, 60, 1, 1], 2, 0, 20, 4), (4, 2));
    }

    #[test]
    fn history_scroll_excludes_the_bottom_regions() {
        assert_eq!(history_scroll_geometry(24, 15, 11, 12), (2, 20));
        assert_eq!(history_scroll_geometry(24, 4, 0, 1), (2, 20));
    }
}
