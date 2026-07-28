use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthStr;

pub(crate) fn copy_rows(source: &Buffer, destination: &mut Buffer, source_y: u16, target: Rect) {
    let height = target
        .height
        .min(source.area.height.saturating_sub(source_y))
        .min(destination.area.bottom().saturating_sub(target.y));
    let width = target
        .width
        .min(source.area.width)
        .min(destination.area.right().saturating_sub(target.x));
    for y in 0..height {
        for x in 0..width {
            let Some(cell) = source.cell((
                source.area.x.saturating_add(x),
                source.area.y.saturating_add(source_y).saturating_add(y),
            )) else {
                continue;
            };
            let Some(target) = destination.cell_mut((target.x + x, target.y + y)) else {
                continue;
            };
            *target = cell.clone();
        }
    }
}

/// Copies rows for Ratatui's direct-draw history path, where every cell is emitted without diffing.
/// Wide-character continuation cells must be empty or they become visible spaces.
pub(crate) fn copy_rows_for_direct_draw(
    source: &Buffer,
    destination: &mut Buffer,
    source_y: u16,
    target: Rect,
) {
    copy_rows(source, destination, source_y, target);

    let area = target.intersection(destination.area);
    for y in area.top()..area.bottom() {
        let mut continuation_columns = 0usize;
        for x in area.left()..area.right() {
            let Some(cell) = destination.cell_mut((x, y)) else {
                continue;
            };
            if continuation_columns > 0 || cell.skip {
                cell.set_symbol("");
                continuation_columns = continuation_columns.saturating_sub(1);
            } else {
                continuation_columns = UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::*;

    #[test]
    fn direct_draw_copy_suppresses_wide_character_continuation_cells() {
        let mut source = Buffer::empty(Rect::new(0, 0, 8, 1));
        source.set_string(0, 0, "中文a", Style::default());
        let mut destination = Buffer::empty(source.area);

        copy_rows_for_direct_draw(&source, &mut destination, 0, source.area);

        let symbols = destination.content[..5]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();
        assert_eq!(symbols, ["中", "", "文", "", "a"]);
        assert_eq!(symbols.concat(), "中文a");
    }
}
