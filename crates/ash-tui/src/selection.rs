use std::sync::Arc;

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default)]
pub(crate) struct SelectableText {
    height: u16,
    regions: Vec<TextRegion>,
}

#[derive(Debug)]
struct TextRegion {
    top: u16,
    buffer: Arc<Buffer>,
}

#[derive(Clone, Copy)]
struct TextCell<'a> {
    column: u16,
    width: u16,
    symbol: &'a str,
}

impl SelectableText {
    pub(crate) fn new(height: u16) -> Self {
        Self {
            height,
            regions: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, top: u16, buffer: Arc<Buffer>) {
        if !buffer.area.is_empty() {
            self.regions.push(TextRegion { top, buffer });
        }
    }

    pub(crate) fn point(&self, position: Position) -> Option<Position> {
        self.point_on_row(position.y, position.x)
    }

    pub(crate) fn focus(&self, anchor: Position, position: Position) -> Option<Position> {
        let row = position.y.min(self.height.saturating_sub(1));
        if row >= anchor.y {
            (anchor.y..=row)
                .rev()
                .find_map(|row| self.point_on_row(row, position.x))
        } else {
            (row..=anchor.y).find_map(|row| self.point_on_row(row, position.x))
        }
    }

    pub(crate) fn text(&self, anchor: Position, focus: Position) -> String {
        let Some((start, end)) = selection_bounds(anchor, focus) else {
            return String::new();
        };
        let mut lines = Vec::with_capacity(usize::from(end.y.saturating_sub(start.y)) + 1);
        for row in start.y..=end.y {
            let Some(cells) = self.row_cells(row) else {
                continue;
            };
            let start_column = if row == start.y { start.x } else { 0 };
            let end_column = if row == end.y { end.x } else { u16::MAX };
            lines.push(selected_text(&cells, start_column, end_column));
        }
        lines.join("\n").trim_end_matches('\n').to_string()
    }

    pub(crate) fn highlight(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        scroll_top: u16,
        anchor: Position,
        focus: Position,
    ) {
        let Some((start, end)) = selection_bounds(anchor, focus) else {
            return;
        };
        let visible_bottom = scroll_top.saturating_add(area.height);
        let top = start.y.max(scroll_top);
        let bottom = end.y.min(visible_bottom.saturating_sub(1));
        if top > bottom {
            return;
        }

        for row in top..=bottom {
            let Some(cells) = self.row_cells(row) else {
                continue;
            };
            let start_column = if row == start.y { start.x } else { 0 };
            let end_column = if row == end.y { end.x } else { u16::MAX };
            let screen_row = area.y.saturating_add(row.saturating_sub(scroll_top));
            for cell in selected_cells(&cells, start_column, end_column) {
                for offset in 0..cell.width {
                    let screen_column = area.x.saturating_add(cell.column).saturating_add(offset);
                    if let Some(cell) = buffer.cell_mut((screen_column, screen_row)) {
                        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
    }

    fn point_on_row(&self, row: u16, column: u16) -> Option<Position> {
        let cells = self.row_cells(row)?;
        let first = cells
            .iter()
            .find(|cell| !cell.symbol.chars().all(char::is_whitespace))?;
        let last = cells
            .iter()
            .rfind(|cell| !cell.symbol.chars().all(char::is_whitespace))?;
        let right = last.column.saturating_add(last.width.saturating_sub(1));
        Some(Position::new(column.clamp(first.column, right), row))
    }

    fn row_cells(&self, row: u16) -> Option<Vec<TextCell<'_>>> {
        let index = self
            .regions
            .partition_point(|region| region.bottom() <= row);
        let region = self.regions.get(index)?;
        if row < region.top {
            return None;
        }
        let source_row = region
            .buffer
            .area
            .y
            .saturating_add(row.saturating_sub(region.top));
        Some(buffer_row(&region.buffer, source_row))
    }
}

impl TextRegion {
    fn bottom(&self) -> u16 {
        self.top.saturating_add(self.buffer.area.height)
    }
}

fn buffer_row(buffer: &Buffer, row: u16) -> Vec<TextCell<'_>> {
    let mut cells = Vec::with_capacity(usize::from(buffer.area.width));
    let mut hidden_columns = 0;
    for column in buffer.area.x..buffer.area.right() {
        let Some(cell) = buffer.cell((column, row)) else {
            continue;
        };
        if hidden_columns > 0 {
            hidden_columns -= 1;
            continue;
        }
        let width = u16::try_from(UnicodeWidthStr::width(cell.symbol()))
            .unwrap_or(u16::MAX)
            .max(1);
        hidden_columns = width.saturating_sub(1);
        if !crate::buffer::cell_is_skipped(cell) {
            cells.push(TextCell {
                column: column.saturating_sub(buffer.area.x),
                width,
                symbol: cell.symbol(),
            });
        }
    }
    while cells
        .last()
        .is_some_and(|cell| cell.symbol.chars().all(char::is_whitespace))
    {
        cells.pop();
    }
    cells
}

fn selected_text(cells: &[TextCell<'_>], start: u16, end: u16) -> String {
    selected_cells(cells, start, end)
        .map(|cell| cell.symbol)
        .collect()
}

fn selected_cells<'a, 'b>(
    cells: &'a [TextCell<'b>],
    start: u16,
    end: u16,
) -> impl Iterator<Item = &'a TextCell<'b>> {
    cells
        .iter()
        .filter(move |cell| cell.column >= start && cell.column <= end)
}

fn selection_bounds(anchor: Position, focus: Position) -> Option<(Position, Position)> {
    if anchor == focus {
        return None;
    }
    if (anchor.y, anchor.x) <= (focus.y, focus.x) {
        Some((anchor, focus))
    } else {
        Some((focus, anchor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_map(buffer: Buffer) -> SelectableText {
        let height = buffer.area.height;
        let mut text = SelectableText::new(height);
        text.push(0, Arc::new(buffer));
        text
    }

    #[test]
    fn extracts_text_without_trailing_layout_cells() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(0, 0, "hello", Style::default());
        buffer.set_string(0, 1, "world", Style::default());
        let text = text_map(buffer);

        assert_eq!(
            text.text(Position::new(1, 0), Position::new(2, 1)),
            "ello\nwor"
        );
    }

    #[test]
    fn omits_layout_gaps_but_preserves_content_blank_lines() {
        let mut first = Buffer::empty(Rect::new(0, 0, 8, 2));
        first.set_string(0, 0, "one", Style::default());
        let mut second = Buffer::empty(Rect::new(0, 0, 8, 1));
        second.set_string(0, 0, "two", Style::default());
        let mut text = SelectableText::new(4);
        text.push(0, Arc::new(first));
        text.push(3, Arc::new(second));

        assert_eq!(
            text.text(Position::new(0, 0), Position::new(2, 3)),
            "one\n\ntwo"
        );
    }

    #[test]
    fn wide_character_placeholders_are_not_copied() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(0, 0, "你好abc", Style::default());
        let text = text_map(buffer);

        assert_eq!(
            text.text(Position::new(0, 0), Position::new(6, 0)),
            "你好abc"
        );
        assert_eq!(text.text(Position::new(1, 0), Position::new(6, 0)), "好abc");
    }

    #[test]
    fn points_require_real_text_and_clamp_to_its_width() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(2, 0, "hello", Style::default());
        let text = text_map(buffer);

        assert_eq!(text.point(Position::new(0, 0)), Some(Position::new(2, 0)));
        assert_eq!(text.point(Position::new(11, 0)), Some(Position::new(6, 0)));
        assert_eq!(text.point(Position::new(0, 1)), None);
    }
}
