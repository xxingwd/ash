use ratatui::{buffer::Buffer, layout::Rect};

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
